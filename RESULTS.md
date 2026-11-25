# Results: Valkey with zram, zswap and disk swap

> **AI disclosure.** This study was designed, run and analyzed with an AI assistant (Claude). Results were
> spot-checked by a person, but not every number and explanation has been verified in detail, and they may
> contain mistakes. Raw data is in [`data/`](data/) and the tables below are generated from it by
> [`tools/report.py`](tools/report.py). Please open an issue if you find a problem.

Valkey 9.2.0 (single main thread, default `io-threads 1`) with more data than RAM, on a 6-core Xeon E-2286G with
32 GB RAM (31 GB usable) and SATA SSDs, Ubuntu 26.04, kernel 7.0. Valkey keeps everything in memory; when it does not
fit, the kernel swaps pages out: to compressed RAM (zram, or zswap in front of a swap file) or to the SSD (disk swap).

## Summary

* **Compressing values in the client beats every kernel option.** With lz4 or zstd applied by the client before SET, 40 GB of data
  takes 9.5-11.8 GB in Valkey and 64 GB takes 14.5-18.2 GB, so it all fits in RAM and Valkey runs as fast as with uncompressed data in RAM,
  at 1-5 µs more client CPU per operation. See [client-side compression](#client-side-compression-instead-of-kernel-compression).
  The rest of this summary is about what to do when the client cannot be changed.
* **zram with lz4 is the best choice in every test.** With 40 GB of data on 31 GB of RAM (~30% of Valkey's memory
  compressed) it costs 27% of throughput with uniform key access, 5-8% with skewed access and ~0% with a tight
  hot set; at a fixed 50k ops/s p99 latency rises from 57 µs to 70-99 µs. At 64 GB (2x RAM) it still delivers
  57-96% of in-RAM throughput (uniform to normal), versus 3-86% for disk swap.
* **zstd is slower than lz4 at every level we tried (3, 1, -1).** It compresses Valkey's pages ~7.5-8.4x vs 4.6x
  for lz4, which keeps 4-5 GB more of Valkey resident at 64 GB, but that cuts page faults only ~10% while each
  fault costs roughly twice as much CPU. Level -1 is the best zstd level here.
* **zswap equals zram, unless its shrinker is on.** On this kernel `zswap.shrinker_enabled` defaults to `Y`, which
  writes compressed pages back to the disk swap file under memory pressure even when the pool has room. For a
  cache, those pages are still read, so they become disk reads: at 64 GB zswap-zstd dropped to 16k ops/s (54k with
  the shrinker off) and zswap-lz4 to 65k (82k off; zram 84k). See [the zswap shrinker](#the-zswap-shrinker).
* **Disk swap stalls Valkey.** Its single thread waits for each page read: 11k ops/s with uniform access at 40 GB
  and 5k at 64 GB, versus 107k / 84k with zram-lz4. It cannot hold 50k ops/s with uniform access at either size.
* **Stores that keep data on SSD:** Kvrocks with lz4 gets more throughput than Valkey with zram on uniform and zipfian access but
  uses 4-7x more CPU per operation and has higher tail latency; TidesKV is faster still but stalls for 5-10 s at a time;
  Lavik is limited by this host's SATA SSDs. See [Kvrocks, Lavik and TidesKV](#kvrocks-lavik-and-tideskv).
* **Variable value sizes (0-2000 B) use 14% less memory than fixed 1 KiB values** (jemalloc puts 1 KiB + header
  in the 1280-byte size class) and, at the same memory size, cause ~25% fewer page faults per operation than
  fixed values (likely because pages full of small values are hotter and stay resident; not verified).
  Fragmentation stayed at 1.02 with 10% of operations rewriting values at new random sizes.

## Method

* `kvbench` (this repository): 90% GET / 10% SET, integer keys, values 1 KiB with 10% random characters
  (lz4 ~3.8x, zstd ~7.3x on 4 KiB pages of bare values; Valkey's pages compress better, see the memory table).
  4 client threads x 16 connections, one request in flight per connection.
* Workloads: full load (closed loop) with uniform, zipfian 0.99, hotspot (90% of operations on 10% of keys) and
  normal (sd 5% of the key space) key access; fixed 50k ops/s (open loop, latency measured from the scheduled send
  time) with uniform and zipfian access.
* Each store / swap mode / data set is loaded once; before every workload point the runner drives that key
  distribution at full load until two 30 s chunks are within 2% (max 10 min, 30 min for disk swap), then runs 60 s
  warm-up + 120 s measured. Every run uses a fresh random request stream.
* Server on CPUs 1-3,7-9, client on 4,5,10,11 (OS on 0,6); turbo off, performance governor. The client needs about
  as much CPU per operation as the server (loopback TCP: the sender also does the receiver's network work).
  Results where the client threads were >90% busy are flagged in the data (none in the tables below).
* Run-to-run noise (in-RAM baseline, two runs): ~1% for throughput and p99, ~±15 µs for p99.9 at 50k ops/s.
  Each configuration was run once.
* This CPU (Coffee Lake) runs with PTI/IBRS/MDS mitigations, so syscalls are expensive; absolute numbers will be
  higher on newer hardware.

### Settings that are not defaults

| Component | Setting |
|---|---|
| Valkey 9.2.0 | `--save ""`, `--appendonly no` (no persistence), `--protected-mode no` (loopback only); everything else default (`io-threads 1`, no `maxmemory`) |
| Host | `vm.overcommit_memory=1`, turbo off, performance governor, conntrack skipped for loopback |
| disk swap | 64 GB (96 GB for the 64 GB data set) swap file on the second SSD, swappiness 60, other sysctls default |
| zram | one zram device 64 GB (96 GB for the 64 GB data set), no disk swap, swappiness 180, `page-cluster 0`, `watermark_boost_factor 0`, `watermark_scale_factor 125`; zstd level via `algorithm_params` (default 3) |
| zswap | `zsmalloc`, `max_pool_percent 50`, swap file as for disk swap, swappiness 100, same three sysctls as zram; `shrinker_enabled` kernel default `Y` unless marked "shrinker off" |

`host/swapmode.sh` sets each mode; every result line in `data/` records the swap, zswap and sysctl state.

## The zswap shrinker

zswap is a compressed cache in front of a normal swap device: pages being swapped out are compressed into a RAM pool
instead of being written to disk, and read back from the pool without disk I/O. Pages leave the pool for the swap
device only by **writeback**.

* **Shrinker off** (`/sys/module/zswap/parameters/shrinker_enabled = N`): writeback happens only when the pool
  reaches `max_pool_percent` of RAM. Until then everything stays compressed in RAM and zswap behaves like zram, with
  the disk as overflow. In our 64 GB runs the pool held 5-10 GB of a ~15.5 GB limit, so nothing was written back.
* **Shrinker on** (`Y`, the default on this kernel): the pool is registered with the kernel's memory reclaim, like
  other caches. Whenever memory is tight, the kernel asks zswap to shrink, and zswap writes its coldest compressed
  pages to the swap device even though the pool is far from its limit. The goal is to give the RAM to hot
  uncompressed pages, which works when swapped-out memory is really idle.

For a cache like Valkey the "cold" pages are still read, just less often (with uniform access, every key is equally
likely), and memory stays tight for as long as the data set exceeds RAM, so the shrinker keeps running and those
pages turn into synchronous disk reads in Valkey's single thread. At 64 GB, during the load the shrinker wrote back
~4.3k pages/s with lz4 and ~18.5k pages/s with zstd (slower compression keeps the system under pressure longer; our
best explanation, not confirmed); with zstd ~25% of page faults were then served from disk. At 40 GB it wrote back
fewer than 100 pages/s and made no difference. zram has no such mechanism (it only writes to a backing device if
one is configured and writeback is triggered).

For caches whose data set exceeds RAM, use zram or set `zswap.shrinker_enabled=0`.

## Tables

Values are throughput in ops/s with page faults per operation in parentheses (each fault = one page read back from
swap: decompressed for zram/zswap, read from the SSD for disk swap), or p99 / p99.9 latency in µs at 50k ops/s;
"missed" = the 50k ops/s target was not reached (achieved rate in parentheses).

### 40 GB (30M keys x 1 KiB, 31 GB RAM): throughput at full load (ops/s) (page faults per op)

In RAM = 15M keys (20 GB), no swap. Kernel defaults for zswap (shrinker on).

| workload | in RAM | zram lz4 | zram zstd 3 | zram zstd 1 | zram zstd -1 | zswap lz4 | zswap zstd 3 | disk swap |
|---|---|---|---|---|---|---|---|---|
| uniform | 147.2k | 107.2k (0.385) | 90.5k (0.340) | 88.3k (0.339) | 94.4k (0.346) | 107.0k (0.384) | 90.2k (0.338) | 11.3k (0.268) |
| zipfian 0.99 | 150.2k | 138.5k (0.072) | 131.4k (0.067) | 131.8k (0.063) | 133.1k (0.065) | 137.9k (0.071) | 130.4k (0.065) | 38.2k (0.062) |
| hotspot 10%/90% | 147.1k | 140.4k (0.043) | 135.7k (0.038) | 135.5k (0.038) | 135.5k (0.039) | 138.5k (0.043) | 134.8k (0.038) | 63.1k (0.030) |
| normal (sd 5%) | 146.9k | 146.7k (0.007) | 145.2k (0.005) | 144.2k (0.007) | 143.5k (0.007) | 145.1k (0.007) | 144.2k (0.006) | 133.0k (0.002) |

### 40 GB (30M keys x 1 KiB, 31 GB RAM): latency at a fixed 50k ops/s, p99 / p99.9 (µs)

| workload | in RAM | zram lz4 | zram zstd 3 | zram zstd 1 | zram zstd -1 | zswap lz4 | zswap zstd 3 | disk swap |
|---|---|---|---|---|---|---|---|---|
| uniform @ 50k ops/s | 57 / 83 | 99 / 205 | 109 / 212 | 113 / 245 | 111 / 268 | 87 / 204 | 112 / 199 | missed (16.7k) |
| zipfian @ 50k ops/s | 56 / 84 | 70 / 110 | 80 / 115 | 79 / 114 | 76 / 114 | 68 / 119 | 82 / 117 | 158728 / 259785 |

### 64 GB (48M keys x 1 KiB, 2x RAM): throughput at full load (ops/s) (page faults per op)

Disk swap at a fixed rate: latency capped at the 60 s histogram limit when the target is missed.

| workload | in RAM | zram lz4 | zram zstd 3 | zram zstd 1 | zram zstd -1 | zswap lz4 | zswap lz4, shrinker off | zswap zstd 3 | zswap zstd 3, shrinker off | disk swap |
|---|---|---|---|---|---|---|---|---|---|---|
| uniform | 147.2k | 83.7k (0.869) | 58.6k (0.763) | 62.4k (0.768) | 73.5k (0.779) | 65.4k (0.874) | 81.7k (0.875) | 16.0k (0.748) | 54.2k (0.765) | 4.9k (0.661) |
| zipfian 0.99 | 150.2k | 126.0k (0.193) | 112.8k (0.164) | 116.0k (0.159) | 120.1k (0.165) | 119.2k (0.194) | 124.7k (0.196) | 48.7k (0.181) | 113.2k (0.163) | 15.7k (0.186) |
| hotspot 10%/90% | 147.1k | 133.2k (0.099) | 124.9k (0.086) | 126.0k (0.086) | 128.4k (0.088) | 131.7k (0.099) | 133.1k (0.101) | 91.6k (0.085) | 123.0k (0.087) | 35.1k (0.070) |
| normal (sd 5%) | 146.9k | 140.6k (0.028) | 140.3k (0.016) | 141.0k (0.018) | 141.7k (0.016) | 139.9k (0.028) | 141.1k (0.028) | 135.9k (0.019) | 139.3k (0.019) | 127.0k (0.003) |

### 64 GB (48M keys x 1 KiB, 2x RAM): latency at a fixed 50k ops/s, p99 / p99.9 (µs)

| workload | in RAM | zram lz4 | zram zstd 3 | zram zstd 1 | zram zstd -1 | zswap lz4 | zswap lz4, shrinker off | zswap zstd 3 | zswap zstd 3, shrinker off | disk swap |
|---|---|---|---|---|---|---|---|---|---|---|
| uniform @ 50k ops/s | 57 / 83 | 103 / 242 | 165 / 313 | 179 / 318 | 124 / 415 | 263 / 463 | 103 / 253 | missed (35.4k) | 537 / 1649 | missed (6.6k) |
| zipfian @ 50k ops/s | 56 / 84 | 76 / 141 | 98 / 148 | 100 / 146 | 84 / 139 | 98 / 318 | 75 / 136 | 657 / 1143 | 100 / 142 | missed (25.0k) |

### Where Valkey's memory is (uniform, full load, end of run)

| data | mode | Valkey used_memory GB | resident GB | swapped GB | compression ratio |
|---|---|---|---|---|---|
| 40 GB | zram lz4 | 39.9 | 27.8 | 11.7 | 4.6x |
| 40 GB | zram zstd 3 | 39.9 | 29.3 | 10.1 | 8.1x |
| 40 GB | zram zstd 1 | 39.9 | 29.2 | 10.3 | 8.4x |
| 40 GB | zram zstd -1 | 39.9 | 28.7 | 10.7 | 7.5x |
| 40 GB | zswap lz4 | 39.9 | 27.8 | 11.6 | 4.4x |
| 40 GB | zswap zstd 3 | 39.9 | 29.1 | 10.3 | 7.9x |
| 64 GB | zram lz4 | 63.6 | 20.9 | 41.8 | 4.6x |
| 64 GB | zram zstd 3 | 63.6 | 25.1 | 37.6 | 8.1x |
| 64 GB | zram zstd 1 | 63.6 | 25.4 | 37.3 | 8.4x |
| 64 GB | zram zstd -1 | 63.6 | 25.0 | 37.7 | 7.5x |
| 64 GB | zswap lz4 | 63.6 | 20.4 | 42.3 | 4.4x |
| 64 GB | zswap lz4, shrinker off | 63.6 | 20.5 | 42.2 | 4.5x |
| 64 GB | zswap zstd 3 | 63.6 | 25.6 | 37.1 | 7.8x |
| 64 GB | zswap zstd 3, shrinker off | 63.6 | 25.6 | 37.1 | 7.8x |

### Fixed 1 KiB vs variable 0-2000 B values (ops/s (faults/op), or p99 / p99.9 µs)

| mode | workload | fixed, 30M keys (39.9 GB) | variable, 35M keys (39.9 GB) | variable, 30M keys (34.3 GB) |
|---|---|---|---|---|
| zram lz4 | uniform | 107.2k (0.385) | 112.7k (0.287) | 126.9k (0.135) |
| zram lz4 | zipfian 0.99 | 138.5k (0.072) | 138.5k (0.052) | 144.6k (0.024) |
| zram lz4 | uniform @ 50k ops/s | 99 / 205 | 85 / 176 | 73 / 128 |
| zram zstd 3 | uniform | 90.5k (0.340) | 93.4k (0.264) | 118.2k (0.109) |
| zram zstd 3 | zipfian 0.99 | 131.4k (0.067) | 135.0k (0.046) | 141.5k (0.020) |
| zram zstd 3 | uniform @ 50k ops/s | 109 / 212 | 101 / 179 | 86 / 130 |
| zswap lz4 | uniform | 107.0k (0.384) | 113.3k (0.288) | 126.7k (0.130) |
| zswap lz4 | zipfian 0.99 | 137.9k (0.071) | 139.6k (0.052) | 143.6k (0.022) |
| zswap lz4 | uniform @ 50k ops/s | 87 / 204 | 86 / 180 | 74 / 126 |
| zswap zstd 3 | uniform | 90.2k (0.338) | 85.4k (0.258) | 118.0k (0.112) |
| zswap zstd 3 | zipfian 0.99 | 130.4k (0.065) | 130.0k (0.042) | 142.0k (0.020) |
| zswap zstd 3 | uniform @ 50k ops/s | 112 / 199 | 207 / 374 | 86 / 130 |
| disk swap | uniform | 11.3k (0.268) | 15.4k (0.220) | 35.6k (0.079) |
| disk swap | zipfian 0.99 | 38.2k (0.062) | 55.2k (0.045) | 103.5k (0.014) |
| disk swap | uniform @ 50k ops/s | missed (16.7k) | missed (25.0k) | 244842 / 346030 |


### Data that is superseded or less certain

* `data/valkey-swap-r1` (first 40 GB run) is superseded: it had no settle phase (several skewed points, especially
  disk swap, were measured before throughput was steady) and every run reused the same request stream.
* Disk swap, 40 GB, normal access: throughput was still creeping up (settle 119k, measured 133k); treat as a lower bound.
* zswap zstd 3 at 64 GB with the shrinker off, uniform @ 50k ops/s (p99 537 µs vs 165 µs for zram zstd 3) and the
  variable-length 35M-key zswap zstd uniform @ 50k ops/s point (207 µs) are single outliers not yet explained.

## Client-side compression instead of kernel compression

The application compresses each value before SET and decompresses it after GET (`kvbench --client-compress`); Valkey stores the
compressed bytes. Disk swap was enabled only as overflow and was never used. Same data sets and workloads as above.

| Data | Mode | Valkey memory after load | Uniform (ops/s) | Zipfian (ops/s) | Uniform @ 50k p99 / p99.9 | Client CPU µs/op | Server CPU µs/op |
|---|---|---|---|---|---|---|---|
| - | in RAM, uncompressed (15M keys) | 20.1 GB | 147.2k | 150.2k | 57 / 83 µs | 11.1 | 6.8 |
| 40 GB | zram lz4 | 39.9 GB (11.7 GB swapped) | 107.2k | 138.5k | 99 / 205 µs | 11.5 | 9.3 |
| 40 GB | client lz4 | 11.8 GB | 150.6k | 153.3k | 57 / 66 µs | 12.1 | 6.6 |
| 40 GB | client zstd -1 | 11.1 GB | 153.4k | 156.1k | 58 / 65 µs | 14.5 | 6.5 |
| 40 GB | client zstd 3 | 9.5 GB | 155.6k | 157.5k | 59 / 65 µs | 16.0 | 6.4 |
| 64 GB | zram lz4 | 63.6 GB (41.8 GB swapped) | 83.7k | 126.0k | 103 / 242 µs | 11.7 | 12.0 |
| 64 GB | client lz4 | 18.2 GB | 150.1k | 153.0k | 58 / 70 µs | 12.1 | 6.7 |
| 64 GB | client zstd -1 | 17.0 GB | 153.6k | 156.1k | 59 / 80 µs | 14.4 | 6.5 |
| 64 GB | client zstd 3 | 14.5 GB | 155.8k | 157.9k | 59 / 78 µs | 15.9 | 6.4 |

* All data fits in RAM once compressed, so throughput and latency match (and slightly beat, by 2-6%) the uncompressed in-RAM run:
  Valkey moves ~300 bytes per value instead of 1 KiB. The cost moves to the client: +1.0 µs per operation with lz4, +3.4 µs with
  zstd -1, +4.9 µs with zstd 3.
* Compressing values one at a time gives a lower ratio than the kernel gets on 4 KiB pages: 3.2x with lz4 (zram on Valkey's pages
  4.6x), 3.9x with zstd -1 (7.5x), 4.2x with zstd 3 (8.1x). It still wins because all data is compressed and stays in RAM, instead
  of only the overflow being compressed and paged back in on access.
* Not measured here: values become opaque to the server (no APPEND, GETRANGE, INCR on them), every client must use the same codec,
  and small or already-compressed values gain little.

## Kvrocks, Lavik and TidesKV

The same data sets (30M / 48M keys x 1 KiB, 10% random) on stores that keep data on SSD and use RAM as a cache, each on the
same 6 server CPUs, no swap. Versions: Kvrocks 2.17.0, Lavik 0.3.2 (release binary), TidesKV 0.1.0 (commit `c598261`).

* **Kvrocks with lz4** compresses its files enough that they stay in the page cache even at 64 GB (SSD reads 0-3 MB/s), so it is
  CPU-bound, not I/O-bound. It gets more throughput than Valkey with zram on uniform (+8% at 40 GB, +36% at 64 GB) and zipfian access,
  but uses 4-7x more server CPU per operation (spread over its threads; Valkey uses one) and has higher tail latency (p99.9 at
  50k ops/s with uniform access 0.6-0.9 ms vs 0.2 ms). With hot spots Valkey with zram is faster. lz4 is better than both the shipped
  configuration (snappy from LSM level 2) and zstd on every workload; Kvrocks's tail latency varied a lot between the two 40 GB runs.
* **Lavik** reads every GET from the SSD and is capped at 69-79k ops/s by this host's SATA SSDs (~350 MB/s of reads), with p99
  latency of tens of milliseconds at 50k ops/s. It is designed for NVMe; this host does not show it at its best.
* **TidesKV** is the fastest store between stalls (217k ops/s uniform at 40 GB with p99 0.3 ms; 130-198k at 64 GB) with 26-40 µs
  of CPU per operation. In about half the runs it **stalls for 5-10 seconds**: throughput falls to 1-13% of normal for all requests,
  disk writes stop, then the backlog clears in a burst with a write burst of 240-310 MB/s. "Stall seconds" counts the measured seconds
  in which a request waited more than 100 ms. TidesKV ran with `sync-mode none`: its default `interval` mode committed at about
  the disk's sync rate here (~650 SET/s on this host; on a scratch VM 0.9k SET/s vs 9k with `none`, single connection), and
  `none` acknowledges a write before it leaves process memory, which is weaker durability than Kvrocks's default (WAL written,
  not fsynced). Why `interval` commits that slowly is not understood: in the source it does not fsync the WAL per commit.
* **Valkey with zram had no stalls in any run.**

Non-default settings: Kvrocks starts from its shipped `kvrocks.conf` with bind/port/dir/log-dir and `workers 6` changed (variants
set `rocksdb.compression lz4|zstd`); TidesKV defaults with bind/port/dir, `workers 6`, `sync-mode none` (variant `compression zstd`);
Lavik defaults with `--threads 6 --pin-workers --cpus 1,2,3,7,8,9` and a 300 GB data file. The 40 GB Kvrocks lz4 / zstd and Lavik
results are from a rerun (`data/stores-r1-rerun40`): the first run overlapped other work on the benchmark host.

### Stores, 40 GB: throughput at full load (ops/s), latency at 50k ops/s, CPU, stalls

| workload | Valkey + zram lz4 | Kvrocks shipped (snappy) | Kvrocks lz4 | Kvrocks zstd | Lavik | TidesKV lz4, sync none | TidesKV zstd, sync none |
|---|---|---|---|---|---|---|---|
| uniform | 107.2k | 91.7k | 115.5k | 87.7k | 75.6k | 217.4k | 165.1k |
| zipfian 0.99 | 138.5k | 146.5k | 158.9k | 128.2k | 77.6k | 230.0k | 148.3k |
| hotspot 10%/90% | 140.4k | 104.9k | 125.5k | 87.3k | 69.4k | 162.6k | 177.7k |
| normal (sd 5%) | 146.7k | 116.2k | 133.9k | 111.5k | 74.2k | 201.9k | 176.7k |
| uniform @ 50k ops/s p99 / p99.9 µs | 99 / 205 | 579 / 2820 | 201 / 556 | 2206 / 8139 | 98763 / 251527 | 7801405 / 8640266 | 131 / 155 |
| zipfian @ 50k ops/s p99 / p99.9 µs | 70 / 110 | 186 / 295 | 167 / 207 | 191 / 450 | 42631 / 104268 | 104 / 119 | 142 / 177 |
| server CPU µs/op (uniform) | 9.3 | 64.9 | 51.5 | 68.0 | 50.8 | 27.5 | 35.3 |
| stall seconds, all 6 points | 0 | 0 | 0 | 0 | 17 | 29 | 4 |

### Stores, 64 GB: throughput at full load (ops/s), latency at 50k ops/s, CPU, stalls

| workload | Valkey + zram lz4 | Kvrocks shipped (snappy) | Kvrocks lz4 | Kvrocks zstd | Lavik | TidesKV lz4, sync none | TidesKV zstd, sync none |
|---|---|---|---|---|---|---|---|
| uniform | 83.7k | 88.2k | 114.1k | 86.1k | 76.8k | 129.7k | 141.7k |
| zipfian 0.99 | 126.0k | 137.9k | 157.9k | 120.6k | 79.2k | 188.9k | 187.1k |
| hotspot 10%/90% | 133.2k | 95.5k | 115.8k | 77.7k | 75.3k | 176.1k | 129.6k |
| normal (sd 5%) | 140.6k | 105.6k | 125.4k | 93.9k | 75.9k | 198.1k | 156.8k |
| uniform @ 50k ops/s p99 / p99.9 µs | 103 / 242 | 1870 / 7029 | 191 / 906 | 2142 / 8921 | 10691 / 50725 | 89784 / 386924 | 236 / 1709 |
| zipfian @ 50k ops/s p99 / p99.9 µs | 76 / 141 | 230 / 567 | 166 / 210 | 200 / 3043 | 35226 / 67371 | 390 / 58884 | 3837788 / 4605346 |
| server CPU µs/op (uniform) | 11.9 | 67.5 | 52.2 | 69.3 | 50.7 | 32.8 | 39.8 |
| stall seconds, all 6 points | 0 | 0 | 0 | 0 | 1 | 11 | 27 |

