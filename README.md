# kv-compress-bench

A benchmark for Valkey and other Redis-compatible key-value stores (Kvrocks, Lavik, ...), built to study
compressed memory (zram, zswap) and stores that keep data on SSD instead of RAM.

> **AI disclosure.** This benchmark was designed, built, run and analyzed with an AI assistant (Claude).
> Results are spot-checked by a person, but not every number and explanation has been verified in detail,
> and they may contain mistakes. All code and raw data are published so you can check them; please open an
> issue if you find a problem.

## kvbench

`kvbench/` is a small Rust program that drives GET/SET traffic over plain RESP2.
It writes the protocol by hand and runs one epoll loop per thread with many connections, so it adds very
little CPU on the machine under test. Every run reports the client's own CPU per operation, so the client's
overhead is always visible.

### Data

* **Keys** are the integers `0 .. keys-1` as decimal strings, with an optional `--key-prefix`.
* **Value size** (`--value-size`):
  `fixed:N`, `uniform:MIN:MAX`, `normal:MEAN:STDDEV[:MIN:MAX]`, `lognormal:MEDIAN:SIGMA[:MIN:MAX]`.
* **Compressibility** (`--value-random-pct P`): values are cut from a shared pool made of 16-character groups,
  in which P% of the characters are random (64-symbol alphabet) and the rest is fixed filler text. This is the
  same idea as `--data-random-pct` in
  [Percona-Lab/sysbench-compressible](https://github.com/Percona-Lab/sysbench-compressible).
  `kvbench ratio` prints the ratio for 4 KiB pages (how zram/zswap compress), measured, not assumed:

  | `--value-random-pct` | lz4 | zstd -1 | zstd -3 |
  |---:|---:|---:|---:|
  | 100 | 1.00 | 1.31 | 1.31 |
  | 50 | 1.54 | 2.45 | 2.43 |
  | 25 | 2.49 | 4.45 | 4.38 |
  | 10 | 3.78 | 7.79 | 7.34 |
  | 5 | 4.72 | 12.25 | 12.27 |

  (1 KiB values. Random characters carry 6 bits each, so even 100% random compresses ~1.3x with an entropy
  coder such as zstd, but not with lz4.)
* Each value starts with `<key>:<version>:` and the rest is derived from a hash of (key, version), so any
  value can be regenerated; `--verify` checks every GET reply against the expected content.

### Workload

* `--get-ratio` (default 0.9): share of GETs; the rest are SETs that write a new version of the value.
* `--key-dist`:
  * `uniform`
  * `normal:CENTER:STDDEV`: fractions of the key space; `normal:0.5:0.05` puts ~68% of operations on the
    10% of keys around the middle;
  * `zipfian:THETA` (YCSB-style, default 0.99);
  * `hotspot:HOT_KEYS:HOT_OPS[:START]`: e.g. `hotspot:0.1:0.9` = 90% of operations on 10% of the keys.
* `--scramble auto|on|off`: map key ranks through a fixed permutation so hot keys are spread over the key
  space (auto = on for zipfian only, like YCSB).

### Load modes

* **Full load (closed loop)**, the default: `--threads` x `--conns` connections, each with `--pipeline`
  requests in flight; a new request is sent as soon as a reply arrives.
* **Target rate (open loop)**: `--rate N` total ops/s, evenly spaced or `--arrival poisson`. Latency is
  measured from the time each request was **scheduled**, not from when it was actually sent. A server that
  stalls therefore shows up as latency instead of silently lowering the request rate (no coordinated
  omission). Service time from the actual send is reported separately. The summary says whether the target
  rate was met.

### Output

Latency is recorded in HdrHistogram (3 significant digits): p50, p90, p99, p99.9, p99.99, max, for GET,
SET and all. Each interval (default 1 s) prints a line and, with `--out`, appends a JSON line; the run ends
with a JSON summary. Intervals and the summary also include:

* client CPU (getrusage) and server CPU / RSS / swapped-out memory (`--server-pid`, from /proc), plus the
  server's own `INFO` CPU and memory counters;
* host CPU busy / iowait, swap-in / swap-out pages per second, major faults, disk MB/s;
* `/proc/meminfo` (incl. `Zswap`, `Zswapped`), zram `mm_stat` (original vs compressed bytes), memory PSI.

### Usage

```bash
cargo build --release   # in kvbench/
# load 20M keys, 1 KiB values, 10% random (lz4 ~3.8x)
kvbench load --keys 20000000 --value-size fixed:1024 --value-random-pct 10 --threads 4 --conns 4 --flush
# full load, 90/10, zipfian
kvbench run  --keys 20000000 --value-size fixed:1024 --value-random-pct 10 \
             --key-dist zipfian:0.99 --threads 4 --conns 16 --time 60 --warmup 10 --server-pid $(pidof valkey-server)
# 100k ops/s target, normal distribution
kvbench run  --keys 20000000 --value-size fixed:1024 --value-random-pct 10 \
             --key-dist normal:0.5:0.05 --rate 100000 --threads 4 --conns 16
kvbench ratio --value-size lognormal:800:0.6 --value-random-pct 10
```

Use the same `--keys`, `--value-size`, `--value-random-pct`, `--pool-mb` and `--seed` for `load` and
`run` (they define the values; `--verify` relies on it).

## Test host

Vultr bare metal `vbm-6c-32gb`: Intel Xeon E-2286G (6 cores / 12 threads), 32 GB RAM, 2 x 960 GB Intel
S4610 SATA SSD; Ubuntu 26.04. Turbo off, performance governor (`host/02-tune-host.sh`). Swap / compressed
memory modes are set by `host/swapmode.sh`. Note that Lavik is designed for NVMe; this host has SATA SSDs.

## Results

**Web page: https://percona-lab.github.io/kv-compress-bench/** (interactive charts, generated from `data/` by `tools/build_site.py`).

**[RESULTS.md](RESULTS.md)**: Valkey 9.2 with 40 GB and 64 GB of data on 32 GB of RAM, using zram, zswap and disk
swap, lz4 and zstd (levels 3, 1, -1), fixed and variable value sizes. In short:

* zram with lz4 is the best option tested: at 40 GB (30% of memory compressed) it costs 27% of throughput with
  uniform access and 0-8% with skewed access; at 64 GB (2x RAM) Valkey still does 57-96% of its in-RAM throughput,
  versus 3-86% with disk swap.
* zstd's better compression ratio does not make up for its higher CPU cost per page at any level tried.
* zswap performs like zram unless its shrinker (on by default in recent kernels) writes compressed pages back to disk,
  which at 2x RAM cut zswap-zstd throughput by 70%.

* Kvrocks with lz4 gets more throughput than Valkey + zram on uniform/zipfian access at 4-7x the CPU per operation and higher
  tail latency; TidesKV is faster still but stalls for 5-10 s at a time; Lavik is limited by SATA SSDs on this host.

## Running a study

`bench/study.py plan.json` runs a plan (see `bench/plans/`): for each group it sets the swap mode
(`host/swapmode.sh`), starts the store (`bench/kvctl.sh`), loads the data, and for each workload point settles
the key distribution at full load before the timed run. Results go to `/root/results/<plan name>/`.
