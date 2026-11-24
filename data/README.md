# Raw results

Each directory is one study (`bench/study.py`): `results.jsonl` (one line per point: config, host/swap state, settle
history, kvbench summary), `intervals.jsonl` (per-second lines), `loads.jsonl`, `study.log`, and the plan used.

| Directory | What | Valid |
|---|---|---|
| `calib`, `pilot` | first calibration / pilot runs | indicative only |
| `valkey-swap-r1` | 40 GB, all modes, fixed 60 s warm-up | **superseded**: no settle phase (skewed disk points not steady), identical request stream in every run |
| `valkey-swap-r2` | 40 GB rerun with settle + per-run stream seed | yes |
| `valkey-swap-zstdfast-40g` | 40 GB zram zstd level 1 / -1 | yes |
| `valkey-swap-64g-r1` | 64 GB (2x RAM), all modes, kernel defaults | yes |
| `valkey-swap-64g-zswap-noshrink` | 64 GB zswap with `shrinker_enabled=N` | yes |
| `valkey-swap-varlen-r1` | values uniform 0-2000 B, 30M keys (34.6 GB) | yes |
| `valkey-swap-varlen-35m` | values uniform 0-2000 B, 35M keys (~40 GB, matches r2 memory) | yes |
| `aborted-*` | stopped runs (before the fixes) | no |
