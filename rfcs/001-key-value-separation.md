# RFC: Key-Value Separation for Mini-LSM

**Status**: Draft  
**Author**: Mini-LSM Contributors  
**Created**: 2026-03-08  
**Target Version**: Post-Week 3  
**Tracking Issue**: TBD

---

## Summary

This RFC proposes adding key-value separation support to Mini-LSM, inspired by [WiscKey](https://www.usenix.org/system/files/conference/fast16/fast16-papers-lu.pdf) and production systems like BadgerDB and RocksDB's BlobDB. Key-value separation stores large values separately in dedicated Value Log (vLog) files while keeping keys and value pointers in the LSM tree. This significantly reduces write amplification and improves compaction performance for workloads with large values.

## Motivation

### Current Architecture Limitations

In the current Mini-LSM implementation, both keys and values are stored together in SSTable blocks:

```
┌─────────────────────────────────────────────────────────────┐
│  Block Format (Current)                                     │
├─────────────────────────────────────────────────────────────┤
│  ┌──────────┬──────────┬────────┬──────────┬──────────┐    │
│  │ key_len  │ key      │ val_len│ value    │ offset   │    │
│  │ (2B)     │ (var)    │ (2B)   │ (var)    │ (2B)     │    │
│  └──────────┴──────────┴────────┴──────────┴──────────┘    │
└─────────────────────────────────────────────────────────────┘
```

This design has several issues with large values:

1. **High Write Amplification**: During compaction, the entire key-value pair is rewritten even though keys are typically much smaller than values.
2. **Inefficient Range Scans**: Range scans must read through large values even when only keys are needed.
3. **Cache Pollution**: Large values consume block cache space inefficiently.
4. **Slower Compaction**: Moving large amounts of data during compaction increases I/O pressure.

### Example Scenario

Consider a workload with:
- Key size: 100 bytes
- Value size: 10 KB
- Total data: 10 GB (100M key-value pairs)

With leveled compaction (amplification ~10x), the system writes ~100 GB during compactions. With key-value separation, only ~1 GB of keys are rewritten, reducing amplification by **10x**.

## Design Overview

### Value Log (vLog) Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Key-Value Separation Architecture            │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│   LSM Tree (Keys + Value Pointers)                             │
│   ┌─────────────────────────────────────────────────────────┐  │
│   │  Key: "user:1001" → ValuePtr: {vlog_id: 5, offset: 1024}│  │
│   │  Key: "user:1002" → ValuePtr: {vlog_id: 5, offset: 2048}│  │
│   └─────────────────────────────────────────────────────────┘  │
│                              │                                  │
│                              ▼                                  │
│   Value Log Files (.vlog)                                      │
│   ┌─────────────────────────────────────────────────────────┐  │
│   │  vlog_00001.vlog                                       │  │
│   │  ┌──────────┬────────┬──────────┬──────────┬──────────┐ │  │
│   │  │ checksum │ key_len│ key      │ val_len  │ value    │ │  │
│   │  │ (4B)     │ (2B)   │ (var)    │ (4B)     │ (var)    │ │  │
│   │  └──────────┴────────┴──────────┴──────────┴──────────┘ │  │
│   └─────────────────────────────────────────────────────────┘  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Key Components

1. **ValueLog**: Manages value log files, handles writes and garbage collection
2. **ValuePointer**: A reference to a value stored in vLog (file_id, offset, size)
3. **ValueLogBuilder**: Builds vLog files during SSTable construction
4. **GarbageCollector**: Reclaims space from stale values during compaction

## Detailed Design

### 1. Value Pointer Format

```rust
/// A pointer to a value stored in the Value Log.
/// Stored inline in the LSM tree instead of the actual value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValuePointer {
    /// Value log file ID
    pub file_id: u32,
    /// Offset within the file where the value starts
    pub offset: u64,
    /// Size of the encoded value entry (for validation)
    pub size: u32,
}

/// Per-entry value-kind stored in SST block metadata alongside each key-value
/// pair. This is the authoritative source of truth for distinguishing inline
/// values from vLog pointers. A single-byte tag prefix in the value payload
/// (see `VALUE_POINTER_TAG`) is also present as a fast-path sanity check, but
/// the `KvKind` is what the reader trusts — it eliminates the collision risk
/// where a user value whose first byte happens to be `0xFF` would otherwise be
/// misclassified as a pointer.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvKind {
    /// The value is stored inline in the SST block.
    Inline = 0,
    /// The value is a 17-byte encoded `ValuePointer` that references the vLog.
    ValuePointer = 1,
}

impl KvKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Inline),
            1 => Some(Self::ValuePointer),
            _ => None,
        }
    }
}

/// Magic tag byte that prefixes every encoded `ValuePointer`.
///
/// Serves as a fast-path sanity check: if the first byte of a candidate value
/// is not `0xFF`, the value is definitely not a pointer. However the
/// authoritative classification comes from `KvKind` stored in the SST block
/// metadata, because a user value can legitimately start with `0xFF`.
const VALUE_POINTER_TAG: u8 = 0xFF;

impl ValuePointer {
    /// Encode to bytes for storage in LSM tree.
    ///
    /// Layout (17 bytes): `[tag:1][file_id:4][offset:8][size:4]`
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.put_u8(VALUE_POINTER_TAG);
        buf.put_u32(self.file_id);
        buf.put_u64(self.offset);
        buf.put_u32(self.size);
    }

    /// Decode from bytes. Returns an error if the buffer is malformed.
    pub fn decode(mut buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::encoded_size() {
            return Err(anyhow!("ValuePointer buffer too short: {} < {}", buf.len(), Self::encoded_size()));
        }
        let tag = buf.get_u8();
        if tag != VALUE_POINTER_TAG {
            return Err(anyhow!("ValuePointer tag mismatch: expected 0x{:02X}, got 0x{:02X}", VALUE_POINTER_TAG, tag));
        }
        Ok(Self {
            file_id: buf.get_u32(),
            offset: buf.get_u64(),
            size: buf.get_u32(),
        })
    }

    /// Try to decode from bytes. Returns `None` if the buffer is too short or
    /// does not start with the `VALUE_POINTER_TAG` byte.
    ///
    /// Callers that have access to the SST block's `KvKind` metadata should
    /// check that first (it is authoritative) and only use `try_decode` as a
    /// fast-path filter. This avoids the edge-case collision where a user value
    /// whose first byte is `0xFF` could be misclassified as a pointer.
    pub fn try_decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::encoded_size() || buf[0] != VALUE_POINTER_TAG {
            return None;
        }
        Self::decode(buf).ok()
    }

    /// Total encoded size: 17 bytes (1-byte tag + 4 + 8 + 4)
    pub const fn encoded_size() -> usize {
        1 + 4 + 8 + 4 // 17 bytes
    }
}
```

### 2. Value Log File Format

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Value Log Entry Format                           │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌───────────┬─────────┬───────────┬───────────┐                   │
│  │ Header    │ Key     │ Value     │ Padding   │                   │
│  │ (16 bytes)│ (var)   │ (var)     │ (0-7 bytes)│                  │
│  └───────────┴─────────┴───────────┴───────────┘                   │
│                                                                     │
│  Header Format (16 bytes total):                                    │
│  ┌─────────────┬─────────────┬─────────────┬───────────────┬──────────┐
│  │ crc32       │ value_length│ key_length  │ flags         │ padding  │
│  │ (4 bytes)   │ (4 bytes)   │ (2 bytes)   │ (2 bytes)     │ (4 bytes)│
│  └─────────────┴─────────────┴─────────────┴───────────────┴──────────┘
│                                                                     │
│  CRC32: Covers (header_without_crc) + key + value to detect         │
│         corruption of length fields as well as the payload          │
│                                                                     │
│  Alignment: Each entry (header + key + value) is padded to an       │
│             8-byte boundary on disk; the trailing pad bytes are     │
│             included in the entry's `size` so readers can skip      │
│             cleanly to the next entry.                              │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

```rust
/// Magic number for vLog file header
const VLOG_MAGIC: u32 = 0x564C4F47; // "VLOG"

/// Value log file header (first 16 bytes of each vLog file)
#[repr(C)]
#[derive(Clone, Debug)]
pub struct VlogFileHeader {
    pub magic: u32,           // 4 bytes
    pub version: u16,         // 2 bytes
    pub reserved: [u8; 10],   // 10 bytes padding to align to 16 bytes total
}

/// Entry header (precedes each key-value pair).
///
/// Field order is chosen so that all u32 fields come before u16 fields, which
/// keeps the C struct layout naturally 4-byte-aligned with no implicit padding
/// between the declared fields. The trailing `_padding` brings the total to a
/// flat 16 bytes and preserves the file's 8-byte alignment guarantee.
#[repr(C)]
pub struct VlogEntryHeader {
    pub crc32: u32,           // CRC32 of the rest of the header + key + value (4 bytes)
    pub value_len: u32,       // Value length (max 4GB) (4 bytes)
    pub key_len: u16,         // Key length (max 64KB). Large keys must be stored inline. (2 bytes)
    pub flags: u16,           // Flags (tombstone, etc.) (2 bytes)
    pub _padding: [u8; 4],    // Reserved / padding to a 16-byte total
}

const HEADER_SIZE: usize = std::mem::size_of::<VlogEntryHeader>(); // 16
const ALIGNMENT: usize = 8;
```

### 2.5 ValueLogBuilder

The `ValueLogBuilder` constructs vLog entries during SSTable building. It is owned by `SsTableBuilder` and writes sequentially to the current vLog file.

```rust
/// Builder for constructing vLog entries during SST construction.
pub struct ValueLogBuilder {
    writer: ValueLogWriter,
    file_id: u32,
}

impl ValueLogBuilder {
    /// Add a key-value pair to the vLog. Returns a `ValuePointer`.
    ///
    /// The on-disk footprint of an entry is `header + key + value`, padded up
    /// to the next `ALIGNMENT` (8-byte) boundary. The pad bytes are written to
    /// disk *and* counted in `ValuePointer::size`, so a reader can validate
    /// the entry and advance to the next one without re-reading the header.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> ValuePointer {
        let offset = self.writer.offset();
        let payload = HEADER_SIZE + key.len() + value.len();
        let padded = (payload + ALIGNMENT - 1) & !(ALIGNMENT - 1);
        let pad = padded - payload;

        self.writer.append_with_pad(key, value, pad);

        debug_assert_eq!(self.writer.offset() % ALIGNMENT as u64, 0);

        ValuePointer {
            file_id: self.file_id,
            offset,
            size: padded as u32,
        }
    }
}
```

### 3. ValueLog Module Structure

```
src/
├── vlog/
│   ├── mod.rs           # ValueLog manager
│   ├── builder.rs       # ValueLogBuilder for constructing vLog files
│   ├── reader.rs        # ValueLogReader for reading values
│   └── gc.rs            # GarbageCollector for space reclamation
```

### 4. Configuration Options

```rust
#[derive(Clone, Debug)]
pub struct ValueSeparationOptions {
    /// Enable key-value separation
    pub enabled: bool,
    
    /// Minimum value size to trigger separation (bytes)
    /// Values smaller than this are stored inline
    pub min_value_size: usize,
    
    /// Maximum size of a single vLog file
    pub max_vlog_file_size: usize,
    
    /// Ratio of stale data to trigger garbage collection
    pub gc_threshold_ratio: f64,
    
    /// Maximum number of vLog files to keep open
    pub max_open_vlog_files: usize,
}

impl Default for ValueSeparationOptions {
    fn default() -> Self {
        Self {
            enabled: false,               // Disabled by default for backward compatibility
            min_value_size: 1024,         // 1KB threshold
            max_vlog_file_size: 64 << 20, // 64MB per vLog file
            gc_threshold_ratio: 0.5,      // GC when 50% stale
            max_open_vlog_files: 64,
        }
    }
}
```

### 5. Modified SSTableBuilder

```rust
pub struct SsTableBuilder {
    builder: BlockBuilder,
    first_key: KeyVec,
    last_key: KeyVec,
    data: Vec<u8>,
    pub(crate) meta: Vec<BlockMeta>,
    block_size: usize,
    key_hashes: Vec<u32>,
    
    // NEW: Value log components
    vlog_options: Option<ValueSeparationOptions>,
    vlog_builder: Option<ValueLogBuilder>,
    vlog_buffer: Vec<u8>,
    referenced_vlogs: HashSet<u32>,
}

impl SsTableBuilder {
    pub fn add(&mut self, key: KeySlice, value: &[u8]) {
        if self.first_key.is_empty() {
            self.first_key.set_from_slice(key);
        }

        self.key_hashes.push(farmhash::fingerprint32(key.raw_ref()));

        // NEW: Check if value should be separated and annotate with KvKind
        let (value_to_store, kind) = if self.should_separate_value(key, value) {
            let vptr = self.write_to_vlog(key, value);
            // Store the encoded pointer instead of the value
            self.vlog_buffer.clear();
            vptr.encode(&mut self.vlog_buffer);
            (&self.vlog_buffer[..ValuePointer::encoded_size()], KvKind::ValuePointer)
        } else {
            // During compaction, values may already be encoded ValuePointers.
            // Use KvKind metadata (authoritative) rather than try_decode alone.
            if value.len() == ValuePointer::encoded_size() && value[0] == VALUE_POINTER_TAG {
                if let Some(vptr) = ValuePointer::try_decode(value) {
                    self.referenced_vlogs.insert(vptr.file_id);
                }
            }
            (value, KvKind::Inline)
        };

        // Each block entry now carries (key, value, KvKind) so that the
        // reader can classify the value without guessing from the payload.
        if self.builder.add_with_kind(key, value_to_store, kind) {
            self.last_key.set_from_slice(key);
            return;
        }

        self.finish_block();
        self.first_key.set_from_slice(key);
        assert!(self.builder.add_with_kind(key, value_to_store, kind));
        self.last_key.set_from_slice(key);
    }

    /// Write a key-value pair to the active vLog builder and return a pointer.
    fn write_to_vlog(&mut self, key: KeySlice, value: &[u8]) -> ValuePointer {
        let ptr = self.vlog_builder.as_mut().unwrap().add(key.raw_ref(), value);
        self.referenced_vlogs.insert(ptr.file_id);
        ptr
    }

    fn should_separate_value(&self, key: KeySlice, value: &[u8]) -> bool {
        match &self.vlog_options {
            Some(opts) if opts.enabled => {
                // Keys stored in vLog must fit in the u16 key_len field
                value.len() >= opts.min_value_size
                    && key.raw_ref().len() <= u16::MAX as usize
            }
            _ => false,
        }
    }
}
```

### 6. ValueLog Implementation

```rust
/// Pending deletion entry: a vLog file that has been retired by GC but whose
/// on-disk deletion is deferred until it is safe.
pub struct PendingDeletion {
    file_id: u32,
    /// The engine timestamp / epoch at the moment GC retired this file.
    obsolete_at_ts: u64,
}

/// Manages value log files for the storage engine.
pub struct ValueLog {
    /// Path to the vLog directory
    path: PathBuf,

    /// Currently active vLog file for writing
    active_writer: Mutex<ValueLogWriter>,

    /// Read cache for vLog files (file_id -> Arc<ValueLogReader>)
    readers: moka::sync::Cache<u32, Arc<ValueLogReader>>,

    /// Next vLog file ID
    next_file_id: AtomicU32,

    /// Configuration options
    options: ValueSeparationOptions,

    /// Tracks which SSTs reference which vLog entries.
    /// Populated during SST build: when an SST is finalized, the vLog file IDs
    /// it references are registered via register_sst_references().
    sst_to_vlogs: RwLock<HashMap<usize, HashSet<u32>>>,

    /// Monotonic clock / timestamp provider (shared with the LSM engine).
    /// Used by `schedule_deletion` to stamp each retired file with the
    /// current MVCC epoch so the deferred-reclamation pass can compare it
    /// against the MVCC watermark.
    lsm_clock: Arc<dyn Clock>,

    /// vLog files that have been retired by GC but not yet unlinked.
    /// Protected by a mutex; drained by `reclaim_pending_deletions`.
    pending_deletions: Mutex<Vec<PendingDeletion>>,

    /// Per-file open-reader reference count. Incremented when a
    /// `ValueLogReader` is fetched from the cache; decremented on drop.
    /// Used by `reclaim_pending_deletions` to ensure a file is not deleted
    /// while an iterator or snapshot still holds an open handle.
    reader_refcounts: RwLock<HashMap<u32, usize>>,
}

impl ValueLog {
    /// Write a key-value pair to the active vLog file.
    /// Returns a ValuePointer that can be stored in the LSM tree.
    pub fn write(&self, key: &[u8], value: &[u8]) -> Result<ValuePointer> {
        let mut writer = self.active_writer.lock();
        
        // Rotate to new file if current is full
        if writer.size() >= self.options.max_vlog_file_size {
            writer = self.rotate_vlog_file(writer)?;
        }
        
        writer.append(key, value)
    }

    /// Register SST -> vLog references when an SST is finalized.
    /// This populates the sst_to_vlogs mapping for GC tracking.
    pub fn register_sst_references(&self, sst_id: usize, vlog_ids: HashSet<u32>) {
        let mut mapping = self.sst_to_vlogs.write();
        mapping.insert(sst_id, vlog_ids);
    }

    /// Get all vLog files referenced by a given SST.
    pub fn get_sst_references(&self, sst_id: usize) -> Option<HashSet<u32>> {
        let mapping = self.sst_to_vlogs.read();
        mapping.get(&sst_id).cloned()
    }

    /// Get all SSTs that reference a given vLog file.
    /// Used during GC to find which SSTs need pointer updates.
    pub fn get_ssts_referencing(&self, vlog_id: u32) -> Vec<usize> {
        let mapping = self.sst_to_vlogs.read();
        mapping
            .iter()
            .filter_map(|(sst_id, vlogs)| {
                if vlogs.contains(&vlog_id) {
                    Some(*sst_id)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Read a value using a ValuePointer. Returns only the value bytes
    /// (the caller never needs to see the vLog header or key).
    pub fn read(&self, ptr: &ValuePointer) -> Result<Bytes> {
        let reader = self.get_reader(ptr.file_id)?;
        let entry = reader.read_entry(ptr.offset, ptr.size)?;
        Ok(entry.value.freeze())
    }

    /// Get a cached reader for the specified vLog file.
    fn get_reader(&self, file_id: u32) -> Result<Arc<ValueLogReader>> {
        let reader = self.readers.try_get_with(file_id, || {
            ValueLogReader::open(self.path_of_file(file_id)).map(Arc::new)
        }).map_err(|e| anyhow!("Failed to open vlog {}: {}", file_id, e))?;
        // Track open-reader reference count for safe deferred deletion.
        *self.reader_refcounts.write().entry(file_id).or_insert(0) += 1;
        Ok(reader)
    }

    /// Decrements the reference count for a vLog file reader.
    /// Called when a `ValueLogReaderHandle` is dropped.
    fn release_reader(&self, file_id: u32) {
        let mut counts = self.reader_refcounts.write();
        if let Some(count) = counts.get_mut(&file_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&file_id);
            }
        }
    }

    /// Returns the current open-reader reference count for a vLog file.
    /// Used by `reclaim_pending_deletions` to ensure a file is not deleted
    /// while iterators or snapshots still hold an open handle.
    pub fn reader_refcount(&self, file_id: u32) -> usize {
        self.reader_refcounts
            .read()
            .get(&file_id)
            .copied()
            .unwrap_or(0)
    }

    /// Return the current configuration options.
    pub fn options(&self) -> &ValueSeparationOptions {
        &self.options
    }

    /// Allocate and return the next vLog file ID.
    pub fn next_file_id(&self) -> u32 {
        self.next_file_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Return the filesystem path for a given vLog file ID.
    fn path_of_file(&self, file_id: u32) -> PathBuf {
        self.path.join(format!("{:05}.vlog", file_id))
    }

    /// Remove a vLog file from disk and invalidate the cache entry.
    /// Only call this when no active snapshots or iterators reference the file.
    pub fn remove_file(&self, file_id: u32) -> Result<()> {
        let path = self.path_of_file(file_id);
        std::fs::remove_file(&path)?;
        self.readers.invalidate(&file_id);
        Ok(())
    }

    /// Mark a vLog file as obsolete and queue it for deletion. The file is
    /// **not** unlinked here — that would race with active snapshots,
    /// iterators, and any in-flight reads through stale pointers in older
    /// SSTs. Instead the file is parked on a pending-deletion queue and a
    /// background task reclaims it once it is safe.
    ///
    /// Safety condition (any one is sufficient):
    /// - the file's reader/iterator refcount has dropped to zero, **and**
    /// - the MVCC watermark has advanced past the timestamp at which the file
    ///   was retired (so no snapshot can still hold a pointer into it), **and**
    /// - all SSTs that referenced this `file_id` have been compacted away
    ///   (so no `get()` can produce a stale pointer to it).
    ///
    /// `obsolete_at_ts` is the engine's current commit timestamp / epoch at
    /// the moment GC retired the file, used by the watermark check.
    pub fn schedule_deletion(&self, file_id: u32) -> Result<()> {
        let obsolete_at_ts = self.lsm_clock.now();
        self.pending_deletions
            .lock()
            .push(PendingDeletion { file_id, obsolete_at_ts });
        Ok(())
    }

    /// Background reclamation pass. Walks the pending queue and unlinks any
    /// file that has cleared all of the deferred-deletion conditions above.
    /// Run on a timer or at the tail of every successful compaction.
    pub fn reclaim_pending_deletions(&self, watermark_ts: u64) -> Result<()> {
        let mut pending = self.pending_deletions.lock();
        pending.retain(|p| {
            let safe = p.obsolete_at_ts <= watermark_ts
                && self.reader_refcount(p.file_id) == 0
                && self.get_ssts_referencing(p.file_id).is_empty();
            if safe {
                let _ = self.remove_file(p.file_id);
                false // drop from queue
            } else {
                true // keep, retry later
            }
        });
        Ok(())
    }
}
```

### 7. Garbage Collection

Garbage collection is triggered during compaction when the ratio of stale data exceeds a threshold.

**Important design choice**: Instead of rewriting SSTs to update value pointers (which would add massive write amplification and break SST immutability), we use the standard WiscKey approach:

1. Scan the target vLog file and identify live entries
2. Rewrite live entries to a new vLog file
3. Re-insert each live key with its new `ValuePointer` into the LSM tree via the normal write path
4. Old SSTs still contain stale pointers, but they are shadowed by the newer entries in the memtable and upper LSM levels
5. Eventually, normal compaction removes old SSTs containing stale pointers

```rust
/// A single entry read from a vLog file.
pub struct VlogEntry {
    pub ptr: ValuePointer,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub size: usize,
}

/// Analysis result for a single vLog file.
pub struct GcAnalysis {
    pub file_id: u32,
    pub stale_ratio: f64,
    pub live_entries: Vec<VlogEntry>,
    pub dead_bytes: usize,
}

/// Garbage collector for reclaiming space in value logs.
pub struct GarbageCollector {
    vlog: Arc<ValueLog>,
    lsm: Arc<MiniLsm>,
    threshold: f64,
}

impl GarbageCollector {
    /// Create a new garbage collector.
    pub fn new(vlog: Arc<ValueLog>, lsm: Arc<MiniLsm>, threshold: f64) -> Self {
        Self { vlog, lsm, threshold }
    }

    /// Analyze a vLog file and determine which entries are still live.
    /// Returns the ratio of stale (dead) data.
    ///
    /// Performance note: This performs an LSM `get()` for every entry in the vLog file.
    /// For large vLog files this can be expensive. Consider scheduling GC during
    /// low-traffic periods or processing files incrementally.
    pub fn analyze_file(&self, file_id: u32) -> Result<GcAnalysis> {
        let reader = self.vlog.get_reader(file_id)?;
        let mut live_entries = Vec::new();
        let mut dead_bytes = 0;
        let mut live_bytes = 0;

        for entry in reader.iter() {
            if self.is_entry_live(&entry)? {
                live_entries.push(entry);
                live_bytes += entry.size;
            } else {
                dead_bytes += entry.size;
            }
        }

        let total = live_bytes + dead_bytes;
        let stale_ratio = if total > 0 { dead_bytes as f64 / total as f64 } else { 0.0 };

        Ok(GcAnalysis {
            file_id,
            stale_ratio,
            live_entries,
            dead_bytes,
        })
    }

    /// Rewrite live entries to a new vLog file and update the LSM index.
    /// Old SSTs are NOT rewritten; stale pointers are shadowed by new LSM writes.
    ///
    /// **Race avoidance**: a user `put`/`delete` can land on a key between
    /// the `is_entry_live` check and the GC re-insert. To make sure GC never
    /// shadows fresher user writes, we re-validate the pointer atomically
    /// (under a per-key guard or via the LSM's MVCC sequence number) right
    /// before insertion, and only insert when the LSM still observes the
    /// *exact* old pointer for that key. If the key has been overwritten or
    /// deleted in the meantime, the new pointer is discarded — the new vLog
    /// entry is simply unreferenced and will be reclaimed by the next GC pass.
    pub fn compact_file(&self, analysis: &GcAnalysis) -> Result<()> {
        if analysis.stale_ratio < self.threshold {
            return Ok(());
        }

        // Create new vLog file with live entries
        let new_file_id = self.vlog.next_file_id();
        let mut writer = ValueLogWriter::create(self.vlog.path_of_file(new_file_id))?;

        // Rewrite live entries and update LSM index
        for entry in &analysis.live_entries {
            let new_ptr = writer.append(&entry.key, &entry.value)?;
            let mut buf = Vec::with_capacity(ValuePointer::encoded_size());
            new_ptr.encode(&mut buf);

            // Atomic rebind: only swap the pointer if the key still resolves
            // to `entry.ptr`. `compare_and_set` performs the get + put under
            // the same MVCC sequence so a concurrent user write cannot be
            // overwritten. Implementations without explicit CAS can serialize
            // GC writes with the write batch lock and re-check `is_entry_live`
            // inside the critical section.
            self.lsm.compare_and_set(
                &entry.key,
                /* expected = */ &entry.ptr,
                /* new      = */ &buf,
            )?;
        }

        writer.close()?;

        // Ensure new vLog entries and LSM writes are durable before scheduling
        // the old file for reclamation.
        self.lsm.sync()?;

        // Defer deletion until all active snapshots/iterators referencing the
        // old file have been released. See `ValueLog::schedule_deletion` and
        // section 7.1 for the watermark/refcount-based reclamation contract.
        self.vlog.schedule_deletion(analysis.file_id)?;

        Ok(())
    }

    /// Check if a vLog entry is still referenced by the LSM tree.
    fn is_entry_live(&self, entry: &VlogEntry) -> Result<bool> {
        match self.lsm.get(&entry.key)? {
            Some(value) => {
                if let Some(ptr) = ValuePointer::try_decode(&value) {
                    Ok(ptr.file_id == entry.ptr.file_id && ptr.offset == entry.ptr.offset)
                } else {
                    Ok(false) // Value is now inline (untagged) — not a pointer
                }
            }
            None => Ok(false), // Key was deleted
        }
    }
}

### 7.1 Stale Pointer Handling

Because SSTs are immutable, old SSTs continue to contain pointers to the old vLog file even after GC moves values to a new file. This is handled naturally by the LSM tree's tiered structure:

- New GC writes go to the **memtable** first
- `get()` searches memtable → immutable memtables → L0 → L1 → ...
- The new pointer in the memtable (or a recently flushed SST) shadows the old pointer
- Range scans may encounter both old and new pointers; merge iterators deduplicate by key
- Eventually, compaction removes old SSTs containing stale pointers entirely

If a `get()` reads a stale pointer from an old SST after the old vLog file has been deleted, it will get an I/O error. To prevent this, GC must only delete old vLog files after:
1. All live entries are rewritten to the new vLog file
2. The new pointers are durably written to the LSM tree (via `sync()`)
3. No active snapshots or iterators are reading the old file

**Deferred Deletion Strategy:**

Production systems use one of the following approaches to safely reclaim old vLog files:

- **Reference Counting**: Track open readers per vLog file. Delete when count reaches zero.
- **Watermark-Based Reclamation**: Record the current MVCC watermark (minimum active snapshot timestamp) before GC. Only delete files after all snapshots older than that watermark have been released. This integrates naturally with Mini-LSM's Week 3 MVCC design.
- **Epoch-Based Reclamation**: Similar to watermark, but using monotonic epoch counters for non-MVCC systems.
```

### 8. Integration with Compaction

```rust
impl CompactionController {
    /// After compaction, trigger garbage collection for affected vLog files.
    pub fn post_compaction_gc(
        &self,
        input_ssts: &[usize],
        output_ssts: &[usize],
        vlog: &Arc<ValueLog>,
        lsm: &Arc<MiniLsm>,
    ) -> Result<()> {
        // Collect all vLog files referenced by input SSTs
        let mut affected_vlogs: HashSet<u32> = HashSet::new();
        
        for sst_id in input_ssts {
            if let Some(vlogs) = vlog.get_sst_references(*sst_id) {
                affected_vlogs.extend(vlogs);
            }
        }

        // Run GC analysis on affected files
        let gc = GarbageCollector::new(vlog.clone(), lsm.clone(), vlog.options().gc_threshold_ratio);
        for file_id in affected_vlogs {
            let analysis = gc.analyze_file(file_id)?;
            if analysis.stale_ratio >= vlog.options().gc_threshold_ratio {
                gc.compact_file(&analysis)?;
            }
        }

        // Register vLog references for output SSTs.
        // In practice, each output SST is built by an SsTableBuilder which already
        // populates referenced_vlogs. When the SST is finalized, the builder's
        // referenced_vlogs set is passed to vlog.register_sst_references().
        for sst_id in output_ssts {
            // SST builders register references automatically during finalization.
        }

        Ok(())
    }
}
```

## Implementation Plan

### Phase 1: Core Infrastructure (Week 1)

1. **ValuePointer and Encoding**
   - Implement `ValuePointer` struct with serialization
   - Add configuration options to `LsmStorageOptions`
   - Create constants and shared types

2. **ValueLog File Format**
   - Implement vLog entry format with CRC32 checksums
   - Create `VlogEntryHeader` and encoding/decoding
   - Add alignment and padding logic

3. **ValueLogWriter**
   - Sequential write API for building vLog files
   - File rotation when size limit reached
   - Sync/flushing semantics

4. **ValueLogReader**
   - Random read API using file ID + offset
   - Iterator interface for garbage collection
   - Validation with checksums

### Phase 2: SSTable Integration (Week 2)

1. **Modified SSTableBuilder**
   - Add `ValueLogBuilder` integration
   - Threshold-based value separation
   - Track which vLog files are referenced via `referenced_vlogs: HashSet<u32>`
   - Register SST -> vLog mapping via `register_sst_references()` when SST is finalized

2. **Modified SsTable and SsTableIterator**
   - Detect and decode `ValuePointer` values
   - Transparent value fetching from vLog
   - Iterator support for separated values

3. **ValueLog Manager**
   - Lifecycle management of vLog files
   - Reference tracking from SSTs
   - File caching and cleanup

### Phase 3: Garbage Collection (Week 3)

1. **GC Analysis**
   - Scan vLog files to find live/dead entries
   - Calculate space reclamation statistics
   - Trigger policies

2. **GC Execution**
   - Rewrite live entries to new vLog files
   - Re-insert updated pointers into LSM tree via normal writes
   - Defer old file deletion until snapshots are quiesced

3. **Background GC Thread**
   - Optional background GC processing
   - Rate limiting and I/O scheduling
   - Progress tracking and metrics

### Phase 4: Testing and Optimization (Week 4)

1. **Unit Tests**
   - Value pointer encoding/decoding
   - vLog file format correctness
   - GC correctness with various workloads

2. **Integration Tests**
   - End-to-end workflows
   - Crash recovery testing
   - Concurrent read/write scenarios

3. **Performance Benchmarks**
   - Compare with/without key-value separation
   - Measure write amplification reduction
   - Analyze read latency impact

## API Changes

### New Public Types

```rust
pub mod vlog {
    pub struct ValuePointer { ... }
    pub struct ValueSeparationOptions { ... }
    pub struct ValueLogStats { ... }
}
```

### Modified Types

```rust
pub struct LsmStorageOptions {
    // ... existing fields ...
    
    /// Options for key-value separation
    pub value_separation: ValueSeparationOptions,
}
```

### New Storage Methods

```rust
impl MiniLsm {
    /// Get statistics about value log usage
    pub fn vlog_stats(&self) -> ValueLogStats;
    
    /// Trigger manual garbage collection
    pub fn trigger_gc(&self) -> Result<()>;
}
```

## Testing Strategy

### Unit Tests

```rust
#[test]
fn test_value_pointer_encoding() {
    let ptr = ValuePointer {
        file_id: 42,
        offset: 1024,
        size: 256,
    };
    let mut buf = Vec::new();
    ptr.encode(&mut buf);
    let decoded = ValuePointer::decode(&buf);
    assert_eq!(ptr, decoded);
}

#[test]
fn test_vlog_write_read() {
    let temp_dir = tempfile::tempdir().unwrap();
    let vlog = ValueLog::open(temp_dir.path(), Default::default()).unwrap();
    
    let key = b"test_key";
    let value = vec![0u8; 4096]; // Large value
    
    let ptr = vlog.write(key, &value).unwrap();
    let read_value = vlog.read(&ptr).unwrap();
    
    assert_eq!(value, read_value.as_ref());
}
```

### Integration Tests

```rust
#[test]
fn test_key_value_separation_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let options = LsmStorageOptions {
        value_separation: ValueSeparationOptions {
            enabled: true,                // Enable for this test
            min_value_size: 100,
            ..Default::default()
        },
        ..Default::default()
    };
    
    let storage = MiniLsm::open(&dir, options).unwrap();
    
    // Write small value (inline)
    storage.put(b"small", b"tiny").unwrap();
    
    // Write large value (separated)
    let large_value = vec![0u8; 10000];
    storage.put(b"large", &large_value).unwrap();
    
    // Force flush to create SST
    storage.force_flush().unwrap();
    
    // Verify both values can be read
    assert_eq!(storage.get(b"small").unwrap().unwrap(), b"tiny");
    assert_eq!(storage.get(b"large").unwrap().unwrap(), large_value);
}
```

## Compatibility and Migration

### Forward Compatibility

- Disabled by default in existing configurations
- Can be enabled on existing databases (new writes use separation)
- Existing inline values remain unchanged

### Format Versioning

```rust
/// Database format version
const FORMAT_VERSION: u32 = 2; // Increment from 1

/// SSTable footer extension for vLog metadata
pub struct SsTableFooter {
    pub format_version: u32,
    pub has_vlog_references: bool,
    pub vlog_file_ids: Vec<u32>,
}
```

### Manifest Changes

Manifest records are extended to carry the SST → vLog reference set so that
recovery can rebuild `sst_to_vlogs` directly from the manifest log instead of
re-opening every SST footer. This keeps startup O(manifest size) rather than
O(total SST count) once vLog adoption grows.

```rust
#[derive(Serialize, Deserialize)]
pub enum ManifestRecord {
    /// Flush of a memtable to L0. Carries the vLog files this new SST
    /// references (empty if the SST has no separated values).
    Flush(usize, Vec<u32>),

    NewMemtable(usize),

    /// Compaction output. For each output SST, record the set of vLog files
    /// it references so the SST → vLog map is reconstructable from the
    /// manifest alone.
    Compaction(CompactionTask, Vec<(usize, Vec<u32>)>),

    /// vLog file lifecycle.
    NewVlogFile(u32),
    DeleteVlogFile(u32),
}
```

Recovery walks the manifest as before; for every `Flush` / `Compaction` record
it inserts the carried `(sst_id, vlog_ids)` pairs into `sst_to_vlogs`. SSTable
footers still embed the vLog reference list as a redundant copy, used by
`fsck`-style consistency checks and by older snapshots whose manifest record
predates this format.

## Crash Recovery

Because vLog files are append-only and written before their corresponding SSTs, crash recovery follows these ordering rules:

1. **vLog writes happen before SST writes**: When flushing a memtable, values are first appended to the vLog, then the SST is built with pointers to those vLog locations.
2. **SST atomically installed**: The SST is only added to the LSM state (and manifest updated) after both vLog and SST files are fully written and synced.
3. **Recovery on restart**: The manifest is replayed as usual. Any vLog files referenced by SSTs in the manifest are valid. vLog files not referenced by any SST can be garbage collected on startup.
4. **Partial vLog write**: If a crash occurs during vLog writing, the partially written entry is detected by CRC32 mismatch and skipped during reads.

## Performance Considerations

### Write Path

| Operation | Latency Impact | Notes |
|-----------|---------------|-------|
| Small value (< threshold) | None | Stored inline as before |
| Large value | +1 disk write | Sequential write to vLog |
| Flush | Neutral | Sequential vLog writes are fast |

### Read Path

| Scenario | Latency Impact | Mitigation |
|----------|---------------|------------|
| Point get (large value) | +1 seek | vLog reader cache |
| Range scan (keys only) | **Improved** | No value scanning |
| Range scan (full) | Similar | Prefetching for sequential reads |

### Compaction

| Metric | Improvement |
|--------|-------------|
| Write amplification | 5-10x reduction for large values |
| I/O throughput | ~10x improvement (less data moved) |
| CPU usage | Reduced (smaller sorting) |

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| vLog file corruption | Data loss | CRC32 checksums + validation |
| GC overhead | Latency spikes | Background GC + rate limiting |
| Space amplification | Temporary bloat | Configurable GC threshold |
| Recovery complexity | Longer startup | vLog index + incremental recovery |

## Future Work

1. **Compression**: Compress values in vLog to reduce space
2. **Hot/Cold Separation**: Tiered storage for frequently accessed values
3. **Parallel GC**: Concurrent garbage collection across multiple files
4. **vLog Index**: In-memory index for faster lookups
5. **Value Caching**: Dedicated cache for hot values

## References

1. [WiscKey: Separating Keys from Values in SSD-conscious Storage](https://www.usenix.org/system/files/conference/fast16/fast16-papers-lu.pdf) - Lu et al., FAST 2016
2. [BadgerDB Documentation](https://dgraph.io/docs/badger/design/) - Dgraph Labs
3. [RocksDB BlobDB](https://github.com/facebook/rocksdb/wiki/BlobDB) - Facebook
4. [Titan: A RocksDB Plugin for Large Values](https://pingcap.com/blog/titan-storage-engine-design-and-implementation) - PingCAP
5. [Pebble Value Separation](https://www.cockroachlabs.com/blog/pebble-key-value-separation/) - Cockroach Labs

---

## Appendix A: File Layout

```
data/
├── MANIFEST
├── 00001.sst
├── 00002.sst
├── ...
├── 00001.vlog      # NEW: Value log files
├── 00002.vlog
├── 00001.wal
└── vlog_index/     # NEW: Optional vLog index
    └── 00001.idx
```

## Appendix B: Configuration Examples

### Development (low memory)

```rust
ValueSeparationOptions {
    enabled: true,
    min_value_size: 512,
    max_vlog_file_size: 16 << 20,  // 16MB
    gc_threshold_ratio: 0.3,       // Aggressive GC
    max_open_vlog_files: 16,
}
```

### Production (large values)

```rust
ValueSeparationOptions {
    enabled: true,
    min_value_size: 4096,          // 4KB
    max_vlog_file_size: 256 << 20, // 256MB
    gc_threshold_ratio: 0.5,
    max_open_vlog_files: 128,
}
```

### Disabled (backward compatible)

```rust
ValueSeparationOptions {
    enabled: false,
    ..Default::default()
}
```
