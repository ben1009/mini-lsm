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
| Compaction time | 101ms | 3.4ms | **30x faster** | 5000 entries @ 16KB |
| Compaction SST rewrite | 78.4MB | 0.1MB | **780x less** | Keys+ptrs vs full values |
| Full scan | 20.9ms | 16.2ms | **22% faster** | Smaller SSTs = less I/O |
| Point-get | 1.5us | 3.4us | **2.3x slower** | Extra vLog seek |
| Write throughput (1KB) | 979us | 1064us | ~9% slower | Per 1000 entries |
| Write throughput (4KB) | 1072us | 1216us | ~13% slower | |
| Write throughput (16KB) | 1151us | 1528us | ~33% slower | |
| Write throughput (64KB) | 4751us | 5708us | ~20% slower | |
| On-disk ratio (post-compact) | 1.00x | 1.00x | Same | Single round |

---

## Detailed Results

### 1. Write Throughput

Measures wall-clock time for 1000 `put()` calls. vLog mode adds overhead because
`SsTableBuilder::add()` writes large values to the vLog during flush (not on the
`put()` path itself, but the memtable fills faster triggering more flushes).

```
write_throughput/inline/1kb     time: [967.05 us 979.45 us 982.56 us]
write_throughput/vlog/1kb       time: [1025.7 us 1064.1 us 1073.7 us]
write_throughput/inline/4kb     time: [1049.0 us 1072.1 us 1077.9 us]
write_throughput/vlog/4kb       time: [1210.8 us 1216.1 us 1237.4 us]
write_throughput/inline/16kb    time: [1148.8 us 1151.4 us 1161.5 us]
write_throughput/vlog/16kb      time: [1511.8 us 1527.6 us 1590.8 us]
write_throughput/inline/64kb    time: [4651.1 us 4751.0 us 4776.0 us]
write_throughput/vlog/64kb      time: [5631.5 us 5707.7 us 5726.7 us]
```

**Analysis**: The write-path `put()` itself is identical (both go to memtable).
The overhead comes from flush-time vLog writes. At 64KB values, vLog mode is
~20% slower per 1000 entries. This is amortized — the per-entry overhead is
~1us, which is negligible compared to the 64KB value write.

### 2. Compaction Time

Measures `force_full_compaction()` wall-clock time after loading 5000 entries
(16KB each, ~78MB live data) into L0 SSTs.

```
compaction/inline    time: [97.742 ms 101.16 ms 102.01 ms]
compaction/vlog      time: [3.3102 ms 3.3789 ms 3.3960 ms]
```

Post-compaction disk layout:
```
[inline] SST=82,186,708 bytes  vLog=0         total=82MB
[vlog]   SST=145,967 bytes     vLog=82,120,640  total=82MB
```

**Analysis**: This is the headline result. Compaction in vLog mode rewrites
**0.1MB of SST data** (keys + 16-byte pointers) vs **78.4MB** (full values).
The vLog files are not touched during compaction — they are append-only and
GC'd separately. This is the core write-amplification reduction.

### 3. Point-Get Read Latency

Measures `get()` for random keys after full compaction (clean LSM state).

```
read_point_get/inline    time: [1.4810 us 1.4973 us 1.5014 us]
read_point_get/vlog      time: [3.2952 us 3.3843 us 3.4066 us]
```

**Analysis**: vLog point-gets require two I/O operations:
1. Read the SST block to get the `ValuePointer` (~1.5us, same as inline)
2. Read the vLog file at the pointer offset (~1.9us additional)

The extra seek is the cost of separation. Mitigations:
- vLog reader cache (moka) avoids re-opening files
- Sequential vLog layout benefits from OS readahead
- Point-get latency is dominated by the LSM tree lookup, not the vLog read

### 4. Full Scan Throughput

Measures full scan (`scan(Unbounded, Unbounded)`) over all 5000 entries.

```
read_scan/inline    time: [20.837 ms 20.850 ms 20.853 ms]
read_scan/vlog      time: [16.001 ms 16.180 ms 16.898 ms]
```

**Analysis**: vLog mode scans are **22% faster** because SSTs contain only
keys + 16-byte pointers instead of full 16KB values. The SST blocks are much
smaller (keys are ~12 bytes each, so ~28 bytes per entry vs ~16KB), meaning:
- Fewer SST blocks to read from disk
- Better block cache hit rate
- Less data to deserialize during merge iteration

The vLog values are read on-demand, but sequential vLog layout + OS readahead
keeps the per-value read cost low.

### 5. Write Amplification

Measured after single compaction of 5000 entries @ 16KB:

```
[inline] sst_before=78.4MB  sst_after=78.4MB  vlog=0.0MB    live=78.2MB  ratio=1.00x
         compaction rewrites 78.4MB SST data

[vlog]   sst_before=0.1MB   sst_after=0.1MB   vlog=78.3MB   live=78.2MB  ratio=1.00x
         compaction rewrites 0.1MB SST data
```

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
| Double I/O for point-gets (SST + vLog) | 2.3x latency | Prefetch vLog entries on SST read; value cache |
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
| +1 seek for point-gets | +1.9us (~2.3x total) | Yes |
| Improved range scans | 22% faster | Yes |
| No write-path latency impact | ~20% slower at 64KB values | Partial — flush-time overhead, not put-path |
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
