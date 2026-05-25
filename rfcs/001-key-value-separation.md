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

impl ValuePointer {
    /// Encode to bytes for storage in LSM tree
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.put_u32(self.file_id);
        buf.put_u64(self.offset);
        buf.put_u32(self.size);
    }

    /// Decode from bytes. Panics if the buffer is shorter than 16 bytes.
    pub fn decode(mut buf: &[u8]) -> Self {
        assert!(buf.len() >= Self::encoded_size(), "ValuePointer buffer too short");
        Self {
            file_id: buf.get_u32(),
            offset: buf.get_u64(),
            size: buf.get_u32(),
        }
    }

    /// Try to decode from bytes. Returns None if the buffer is too short.
    /// 
    /// Note: In production, a type tag or prefix byte should be used to
    /// unambiguously distinguish ValuePointer from inline 16-byte values.
    pub fn try_decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::encoded_size() {
            return None;
        }
        Some(Self::decode(buf))
    }

    /// Total encoded size: 16 bytes
    pub const fn encoded_size() -> usize {
        4 + 8 + 4 // 16 bytes
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
│  ┌─────────────┬───────────────┬─────────────┬─────────────┬──────────┐
│  │ crc32       │ key_length    │ value_length│ flags       │ padding  │
│  │ (4 bytes)   │ (2 bytes)     │ (4 bytes)   │ (2 bytes)   │ (4 bytes)│
│  └─────────────┴───────────────┴─────────────┴─────────────┴──────────┘
│                                                                     │
│  CRC32: Covers key + value payload for integrity validation         │
│                                                                     │
│  Alignment: Entries are 8-byte aligned for efficient disk access    │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

```rust
/// Magic number for vLog file header
const VLOG_MAGIC: u32 = 0x564C4F47; // "VLOG"

/// Value log file header (first 16 bytes of each vLog file)
#[derive(Clone, Debug)]
pub struct VlogFileHeader {
    pub magic: u32,           // 4 bytes
    pub version: u16,         // 2 bytes
    pub reserved: [u8; 10],   // 10 bytes padding to align to 16 bytes total
}

/// Entry header (precedes each key-value pair)
/// Total size: 16 bytes for 8-byte alignment
#[repr(C)]
pub struct VlogEntryHeader {
    pub crc32: u32,           // CRC32 of key + value (4 bytes)
    pub key_len: u16,         // Key length (max 64KB). Large keys must be stored inline. (2 bytes)
    pub value_len: u32,       // Value length (max 4GB) (4 bytes)
    pub flags: u16,           // Flags (tombstone, etc.) (2 bytes)
    pub _padding: [u8; 4],    // Padding to 16 bytes for alignment
}

const HEADER_SIZE: usize = std::mem::size_of::<VlogEntryHeader>();
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
    /// Add a key-value pair to the vLog. Returns a ValuePointer.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> ValuePointer {
        let offset = self.writer.offset();
        self.writer.append(key, value);
        ValuePointer {
            file_id: self.file_id,
            offset,
            size: (key.len() + value.len() + HEADER_SIZE) as u32,
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

        // NEW: Check if value should be separated
        let value_to_store = if self.should_separate_value(value) {
            let vptr = self.write_to_vlog(key, value);
            // Store the encoded pointer instead of the value
            self.vlog_buffer.clear();
            vptr.encode(&mut self.vlog_buffer);
            &self.vlog_buffer[..ValuePointer::encoded_size()]
        } else {
            // During compaction, values may already be encoded ValuePointers.
            // Track their vLog references to prevent premature GC.
            if let Some(vptr) = ValuePointer::try_decode(value) {
                self.referenced_vlogs.insert(vptr.file_id);
            }
            value
        };

        if self.builder.add(key, value_to_store) {
            self.last_key.set_from_slice(key);
            return;
        }

        self.finish_block();
        self.first_key.set_from_slice(key);
        assert!(self.builder.add(key, value_to_store));
        self.last_key.set_from_slice(key);
    }

    /// Write a key-value pair to the active vLog builder and return a pointer.
    fn write_to_vlog(&mut self, key: KeySlice, value: &[u8]) -> ValuePointer {
        let ptr = self.vlog_builder.as_mut().unwrap().add(key.raw_ref(), value);
        self.referenced_vlogs.insert(ptr.file_id);
        ptr
    }

    fn should_separate_value(&self, value: &[u8]) -> bool {
        match &self.vlog_options {
            Some(opts) if opts.enabled => value.len() >= opts.min_value_size,
            _ => false,
        }
    }
}
```

### 6. ValueLog Implementation

```rust
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
    
    /// Tracks which SSTs reference which vLog entries
    /// Used for garbage collection
    /// Populated during SST build: when an SST is finalized, the vLog file IDs
    /// it references are registered via register_sst_references()
    sst_to_vlogs: RwLock<HashMap<usize, HashSet<u32>>>,
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

    /// Read a value using a ValuePointer.
    pub fn read(&self, ptr: &ValuePointer) -> Result<Bytes> {
        let reader = self.get_reader(ptr.file_id)?;
        reader.read_at(ptr.offset, ptr.size)
    }

    /// Get a cached reader for the specified vLog file.
    fn get_reader(&self, file_id: u32) -> Result<Arc<ValueLogReader>> {
        self.readers.try_get_with(file_id, || {
            ValueLogReader::open(self.path_of_file(file_id))
        }).map_err(|e| anyhow!("Failed to open vlog {}: {}", file_id, e))
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

    /// Schedule a vLog file for deletion once all readers are quiesced.
    /// In production, this uses watermark-based epoch reclamation or reference counting.
    pub fn schedule_deletion(&self, file_id: u32) -> Result<()> {
        // TODO: integrate with snapshot watermark / epoch-based reclamation
        // For now, defer to a background cleanup task that checks active snapshots.
        self.remove_file(file_id)
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
            // Re-insert key with new pointer into LSM tree. Old SSTs still contain
            // stale pointers, but LSM levels above them (memtable, newer SSTs)
            // shadow those old entries with the updated pointer.
            let mut buf = Vec::with_capacity(ValuePointer::encoded_size());
            new_ptr.encode(&mut buf);
            self.lsm.put(&entry.key, &buf)?;
        }

        writer.close()?;

        // Ensure new vLog entries and LSM writes are durable before removing old file
        self.lsm.sync()?;

        // Defer deletion until all active snapshots/iterators referencing the old
        // file have been released. In a production system, this uses reference
        // counting or watermark-based epoch reclamation (see Stale Pointer Handling).
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
                    Ok(false) // Value is now inline (or ambiguous 16-byte value)
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

```rust
#[derive(Serialize, Deserialize)]
pub enum ManifestRecord {
    Flush(usize),
    NewMemtable(usize),
    Compaction(CompactionTask, Vec<usize>),
    // NEW: Track vLog file lifecycle
    NewVlogFile(u32),
    DeleteVlogFile(u32),
}
```

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
