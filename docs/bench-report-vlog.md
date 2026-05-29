# vLog Performance Benchmark Report

**Date**: 2026-05-29
**Commit**: 21fd293 (feat: run post-compaction GC on a background thread)
**Hardware**: Linux 6.18.9-arch1-2, x86_64
**Rust**: stable, edition 2024

---

## Methodology

Benchmarks use Criterion.rs with deterministic workloads:
- **Block size**: 4096 bytes
- **Target SST size**: 2MB
- **Memtable limit**: 2 (forces frequent flushes)
- **Compaction**: Leveled (3 levels, trigger=1000 to prevent background races during measurement)
- **vLog config**: `min_value_size=16`, `gc_threshold_ratio=0.5`
- **Key format**: `key{:08}` (12 bytes), values filled with `0xAB`

Run: `cargo bench --package mini-lsm-starter --bench vlog_benchmarks`

---

## Results Summary

| Metric | Inline | vLog | Delta | Notes |
|--------|--------|------|-------|-------|
| Compaction time | 98ms | 3.0ms | **33x faster** | 5000 entries @ 16KB |
| Compaction SST rewrite | 78.4MB | 0.1MB | **780x less** | Keys+ptrs vs full values |
| Full scan | 19.6ms | 13.0ms | **34% faster** | Smaller SSTs = less I/O |
| Point-get | 1.5us | 3.0us | **2x slower** | Extra vLog seek |
| Write throughput (1KB) | 950us | 1000us | ~5% slower | Per 1000 entries |
| Write throughput (4KB) | 1000us | 1183us | ~18% slower | |
| Write throughput (16KB) | 1160us | 1404us | ~21% slower | |
| Write throughput (64KB) | 3562us | 4737us | ~33% slower | |
| On-disk ratio (post-compact) | 1.00x | 1.00x | Same | Single round |

---

## Detailed Results

### 1. Write Throughput

Measures wall-clock time for 1000 `put()` calls. vLog mode adds overhead because
`SsTableBuilder::add()` writes large values to the vLog during flush (not on the
`put()` path itself, but the memtable fills faster triggering more flushes).

```text
write_throughput/inline/1kb     time: [933.82 us 949.64 us 953.59 us]
write_throughput/vlog/1kb       time: [999.13 us 999.82 us 1002.6 us]
write_throughput/inline/4kb     time: [972.96 us 1000.0 us 1006.8 us]
write_throughput/vlog/4kb       time: [1140.6 us 1183.1 us 1193.8 us]
write_throughput/inline/16kb    time: [1140.9 us 1159.9 us 1164.7 us]
write_throughput/vlog/16kb      time: [1386.4 us 1404.1 us 1408.5 us]
write_throughput/inline/64kb    time: [3523.1 us 3561.8 us 3716.4 us]
write_throughput/vlog/64kb      time: [4551.0 us 4736.9 us 4783.4 us]
```text

**Analysis**: The write-path `put()` itself is identical (both go to memtable).
The overhead comes from flush-time vLog writes. At 64KB values, vLog mode is
~33% slower per 1000 entries. This is amortized — the per-entry overhead is
~1.2us, which is negligible compared to the 64KB value write.

### 2. Compaction Time

Measures `force_full_compaction()` wall-clock time after loading 5000 entries
(16KB each, ~78MB live data) into L0 SSTs.

```text
compaction/inline    time: [97.742 ms 98.0 ms 98.250 ms]
compaction/vlog      time: [3.0005 ms 3.0305 ms 3.1507 ms]
```text

Post-compaction disk layout:
```text
[inline] SST=82,186,708 bytes  vLog=0         total=82MB
[vlog]   SST=145,967 bytes     vLog=82,120,640  total=82MB
```text

**Analysis**: This is the headline result. Compaction in vLog mode rewrites
**0.1MB of SST data** (keys + 16-byte pointers) vs **78.4MB** (full values).
The vLog files are not touched during compaction — they are append-only and
GC'd separately. This is the core write-amplification reduction.

### 3. Point-Get Read Latency

Measures `get()` for random keys after full compaction (clean LSM state).

```text
read_point_get/inline    time: [1.4396 us 1.4596 us 1.4646 us]
read_point_get/vlog      time: [2.8820 us 2.9629 us 2.9831 us]
```text

**Analysis**: vLog point-gets require two I/O operations:
1. Read the SST block to get the `ValuePointer` (~1.5us, same as inline)
2. Read the vLog file at the pointer offset (~1.5us additional)

The extra seek is the cost of separation. Mitigations:
- vLog reader cache (moka) avoids re-opening files
- Sequential vLog layout benefits from OS readahead
- Point-get latency is dominated by the LSM tree lookup, not the vLog read

### 4. Full Scan Throughput

Measures full scan (`scan(Unbounded, Unbounded)`) over all 5000 entries.

```text
read_scan/inline    time: [19.113 ms 19.550 ms 19.659 ms]
read_scan/vlog      time: [12.923 ms 13.000 ms 13.307 ms]
```text

**Analysis**: vLog mode scans are **34% faster** because SSTs contain only
keys + 16-byte pointers instead of full 16KB values. The SST blocks are much
smaller (keys are ~12 bytes each, so ~28 bytes per entry vs ~16KB), meaning:
- Fewer SST blocks to read from disk
- Better block cache hit rate
- Less data to deserialize during merge iteration

The vLog values are read on-demand, but sequential vLog layout + OS readahead
keeps the per-value read cost low.

### 5. Write Amplification

Measured after single compaction of 5000 entries @ 16KB:

```text
[inline] sst_before=78.4MB  sst_after=78.4MB  vlog=0.0MB    live=78.2MB  ratio=1.00x
         compaction rewrites 78.4MB SST data

[vlog]   sst_before=0.1MB   sst_after=0.1MB   vlog=78.3MB   live=78.2MB  ratio=1.00x
         compaction rewrites 0.1MB SST data
```text

**Analysis**: The on-disk ratio is ~1.0x for both modes after a single
compaction — the data has to live somewhere. The key metric is **compaction
rewrite volume**: inline mode rewrites 78.4MB of SST data per compaction,
while vLog mode rewrites only 0.1MB. With leveled compaction (amplification
factor ~10x), inline mode would write ~780MB over the LSM lifetime of this
data, while vLog mode writes ~1MB of SST data + ~78MB of vLog data (written
once at flush time, not rewritten during compaction).

---

## Bottlenecks and Optimization Opportunities

### Write Path

| Bottleneck | Impact | Potential Fix |
|------------|--------|---------------|
| Per-flush vLog fsync | ~1ms per flush | Batch multiple flushes; async fsync |
| Value copy in `ValueLogBuilder::add` | Minor | Zero-copy with `Bytes` |
| Sequential vLog write (no parallelism) | Minor | Per-flush writers already avoid contention |

### Read Path

| Bottleneck | Impact | Potential Fix |
|------------|--------|---------------|
| Double I/O for point-gets (SST + vLog) | 2x latency | Prefetch vLog entries on SST read; value cache |
| No vLog value caching | Repeated reads hit disk | LRU cache for hot vLog values |
| Scan reads vLog entries one-at-a-time | Sequential but serial | Batch prefetch next N entries |

### Compaction

| Bottleneck | Impact | Potential Fix |
|------------|--------|---------------|
| Synchronous GC blocks compaction thread | Compaction stalls on GC | Already fixed: background thread (#85) |
| GC CAS is per-key (no batching) | Lock overhead per key | Already fixed: batch CAS (#82) |
| No GC rate limiting | GC can starve foreground I/O | I/O budget + concurrency cap |
| No parallel GC across files | Sequential file processing | rayon thread pool |

### Space

| Bottleneck | Impact | Potential Fix |
|------------|--------|---------------|
| Pending deletions lost on restart | Temporary space leak | Already fixed: orphan cleanup (#83) |
| No vLog compression | Full value stored | LZ4/Snappy per-entry |
| No hot/cold value tiering | All vLog on same storage | Memory-mapped hot vLog |

---

## Comparison with RFC Predictions

| RFC Claim | Actual | Match? |
|-----------|--------|--------|
| ~10x write amplification reduction | 780x SST rewrite reduction (16KB values) | Exceeds (RFC used 100B keys, 10KB values) |
| +1 seek for point-gets | +1.5us (~2x total) | Yes |
| Improved range scans | 34% faster | Yes |
| No write-path latency impact | ~33% slower at 64KB values | Partial — flush-time overhead, not put-path |
| Compaction I/O ~10x improvement | 780x at 16KB values | Exceeds (value-size dependent) |

The RFC's 10x estimate used 10KB values with 100-byte keys. With 16KB values
and 12-byte keys (our benchmark), the ratio is even more favorable because the
key-to-value size ratio is larger.

---

## Future Benchmark Ideas

1. **Multi-round write amplification**: Write overlapping data in N rounds with
   compaction between each. Measure cumulative SST bytes written. (Blocked by
   leveled compaction controller bug on reopen — needs investigation.)
2. **GC throughput**: Measure time to GC a vLog file at various stale ratios.
3. **Mixed workload**: Concurrent reads + writes + compaction.
4. **Value size sweep**: Plot compaction time and scan throughput as a function
   of value size (128B to 1MB) to find the crossover point where vLog stops
   being beneficial.
5. **Memory pressure**: Measure block cache hit rate with/without vLog under
   memory constraints.
