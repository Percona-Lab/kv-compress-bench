# Thread counts chosen for protocol v2 (calibration 2026-11-25, data/v2-*-calib)

Chosen on both hosts by full-load throughput with one request in flight per connection (5M keys, uniform, 60 s), same
values on both hosts:

| Server | Setting | Reason |
|---|---|---|
| Valkey 9.2 `valkey-io` | `--io-threads 6` | best throughput and p99 (io-threads 2/3/4/6 tested) |
| Redis 8.12 `redis-io` | `--io-threads 6` | best throughput (2/3/4/6 tested) |
| KeyDB 6.3.4 | `--server-threads 4` | 6 threads within 2%, still uses only 4 cores (2/3/4/6 tested) |
| Pika 4.0.4 | `thread-num 4`, `thread-pool-size 6` | within 6% of 6/12 with 2-3x lower p99 (1/12, 2/6, 4/6, 6/12 tested) |

Single-threaded Valkey and Redis (`io-threads 1`, the default) are measured as well.
