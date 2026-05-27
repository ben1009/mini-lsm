use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use bytes::Buf;

use super::{ALIGNMENT, HEADER_SIZE, ValuePointer, VlogEntry, VlogFileHeader};

/// Lightweight header-only entry metadata for GC analysis.
/// Contains the pointer, key, and value length without reading the value payload.
pub struct VlogEntryMeta {
    pub ptr: ValuePointer,
    pub key: Vec<u8>,
    pub value_len: u32,
    pub entry_size: usize,
}

/// Random-read vLog file reader.
///
/// The `File` is wrapped in a `Mutex` so that `read_entry` can be called
/// through `&self` (the reader is typically shared behind an `Arc`).
pub struct ValueLogReader {
    file: Mutex<File>,
}

impl ValueLogReader {
    /// Open a vLog file and validate its file header.
    /// Reads and verifies the 16-byte `VlogFileHeader` at offset 0.
    pub fn open(path: PathBuf) -> Result<Self> {
        let mut file = File::open(&path)?;
        let mut header_buf = [0u8; VlogFileHeader::SIZE];
        file.read_exact(&mut header_buf)?;
        VlogFileHeader::decode(&header_buf)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Read a single entry at the given offset with the given size.
    ///
    /// - Seeks to `offset`, reads `size` bytes
    /// - Parses the 24-byte `VlogEntryHeader`, then key and value
    /// - Validates `header_crc32` and `value_crc32`
    /// - Returns a `VlogEntry` with ptr, key, value, size
    pub fn read_entry(&self, offset: u64, size: u32) -> Result<VlogEntry> {
        let mut file = self
            .file
            .lock()
            .map_err(|e| anyhow!("lock poisoned: {}", e))?;
        file.seek(SeekFrom::Start(offset))?;

        let size = size as usize;
        anyhow::ensure!(
            size >= HEADER_SIZE,
            "entry size {} is smaller than header size {}",
            size,
            HEADER_SIZE
        );

        let mut buf = vec![0u8; size];
        file.read_exact(&mut buf)?;

        // Drop the lock early — all remaining work is CPU-bound on `buf`.
        drop(file);

        // Parse header (first 24 bytes)
        let mut hdr_bytes = &buf[..HEADER_SIZE];
        let header_crc32 = hdr_bytes.get_u32_le();
        let value_crc32 = hdr_bytes.get_u32_le();
        let value_len = hdr_bytes.get_u32_le() as usize;
        let key_len = hdr_bytes.get_u16_le() as usize;
        let flags = hdr_bytes.get_u16_le();
        let mut padding = [0u8; 8];
        hdr_bytes.copy_to_slice(&mut padding);

        // Bounds check
        anyhow::ensure!(
            HEADER_SIZE + key_len + value_len <= size,
            "entry data (header {} + key {} + value {}) exceeds entry size {}",
            HEADER_SIZE,
            key_len,
            value_len,
            size
        );

        let key = buf[HEADER_SIZE..HEADER_SIZE + key_len].to_vec();
        let value = buf[HEADER_SIZE + key_len..HEADER_SIZE + key_len + value_len].to_vec();

        // Validate header CRC32: covers value_crc32 + value_len + key_len + flags + padding + key
        let computed_header_crc = {
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&value_crc32.to_le_bytes());
            hasher.update(&(value_len as u32).to_le_bytes());
            hasher.update(&(key_len as u16).to_le_bytes());
            hasher.update(&flags.to_le_bytes());
            hasher.update(&padding);
            hasher.update(&key);
            hasher.finalize()
        };
        anyhow::ensure!(
            computed_header_crc == header_crc32,
            "header CRC32 mismatch: computed 0x{:08X}, stored 0x{:08X} at offset {}",
            computed_header_crc,
            header_crc32,
            offset
        );

        // Validate value CRC32
        let computed_value_crc = crc32fast::hash(&value);
        anyhow::ensure!(
            computed_value_crc == value_crc32,
            "value CRC32 mismatch: computed 0x{:08X}, stored 0x{:08X} at offset {}",
            computed_value_crc,
            value_crc32,
            offset
        );

        Ok(VlogEntry {
            ptr: ValuePointer {
                file_id: 0, // caller should fill in the correct file_id
                offset,
                size: size as u32,
            },
            key,
            value,
            size,
        })
    }

    /// Return an iterator that yields `VlogEntryMeta` for each entry,
    /// reading only headers + keys (skipping values for efficiency).
    pub fn iter_headers(&self) -> Result<VlogHeaderIterator> {
        let file = self
            .file
            .lock()
            .map_err(|e| anyhow!("lock poisoned: {}", e))?
            .try_clone()?;
        let file_size = file.metadata()?.len();
        Ok(VlogHeaderIterator {
            reader: file,
            offset: VlogFileHeader::SIZE as u64,
            file_size,
            file_id: 0, // caller can set via `with_file_id()`
        })
    }
}

/// Header-only iterator for GC analysis.
/// Reads only the header and key for each entry, skipping value payloads.
pub struct VlogHeaderIterator {
    reader: File,
    offset: u64,
    file_size: u64,
    file_id: u32,
}

impl VlogHeaderIterator {
    /// Set the file ID for generated `ValuePointer`s.
    pub fn with_file_id(mut self, file_id: u32) -> Self {
        self.file_id = file_id;
        self
    }
}

impl Iterator for VlogHeaderIterator {
    type Item = VlogEntryMeta;

    fn next(&mut self) -> Option<Self::Item> {
        // Stop at EOF
        if self.offset >= self.file_size {
            return None;
        }

        // Read the 24-byte entry header
        let mut hdr_buf = [0u8; HEADER_SIZE];
        if self.reader.seek(SeekFrom::Start(self.offset)).is_err() {
            return None;
        }
        if self.reader.read_exact(&mut hdr_buf).is_err() {
            return None;
        }

        let mut hdr_bytes: &[u8] = &hdr_buf;
        let _header_crc32 = hdr_bytes.get_u32_le();
        let _value_crc32 = hdr_bytes.get_u32_le();
        let value_len = hdr_bytes.get_u32_le();
        let key_len = hdr_bytes.get_u16_le() as usize;
        let _flags = hdr_bytes.get_u16_le();
        let _padding = hdr_bytes.get_u64_le(); // 8 bytes

        // Compute total entry size with alignment padding
        let raw_size = HEADER_SIZE + key_len + value_len as usize;
        let entry_size = raw_size.div_ceil(ALIGNMENT) * ALIGNMENT;

        // Sanity check: entry must not extend past EOF
        if self.offset + entry_size as u64 > self.file_size {
            return None;
        }

        // Read only the key bytes (skip the value payload for efficiency)
        let key_start = self.offset + HEADER_SIZE as u64;
        if self.reader.seek(SeekFrom::Start(key_start)).is_err() {
            return None;
        }
        let mut key = vec![0u8; key_len];
        if self.reader.read_exact(&mut key).is_err() {
            return None;
        }

        let current_offset = self.offset;
        self.offset += entry_size as u64;

        Some(VlogEntryMeta {
            ptr: ValuePointer {
                file_id: self.file_id,
                offset: current_offset,
                size: entry_size as u32,
            },
            key,
            value_len,
            entry_size,
        })
    }
}
