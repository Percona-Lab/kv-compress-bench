#!/usr/bin/env python3
"""Thread-count calibration for multi-threaded configurations (5M keys in RAM, uniform, 60 s).
usage: make_calib.py HOSTTAG CLIENT_CPUS OUTFILE"""
import json, sys
host, ccpus, outf = sys.argv[1], sys.argv[2], sys.argv[3]
V = {"keys": 5_000_000, "value_size": "fixed:1024", "value_random_pct": 10}
P = [{"key_dist": "uniform"}, {"key_dist": "uniform", "pipeline": 8}]
g = []
for n in (2, 3, 4, 6):
    g.append({"name": f"valkey-io{n}", "store": "valkey-io", "swap": "none", "data": V, "env": {"VALKEY_IO_THREADS": str(n)}, "points": P})
    g.append({"name": f"redis-io{n}", "store": "redis-io", "swap": "none", "data": V, "env": {"REDIS_IO_THREADS": str(n)}, "points": P})
    g.append({"name": f"keydb{n}", "store": "keydb", "swap": "none", "data": V, "env": {"KEYDB_THREADS": str(n)}, "points": P})
for t, pool in ((1, 12), (2, 6), (4, 6), (6, 12)):
    g.append({"name": f"pika-t{t}-p{pool}", "store": "pika", "swap": "none", "data": V, "env": {"PIKA_THREADS": str(t), "PIKA_POOL": str(pool)}, "points": P})
plan = {"name": f"v2-{host}-calib", "protocol": "v2-calib", "defaults": {"threads": 6, "conns": 12, "cpus": ccpus, "warmup": 15, "time": 60,
        "settle_chunk": 20, "settle_max": 120}, "groups": g}
json.dump(plan, open(outf, "w"), indent=1)
print(plan["name"], len(g), "groups")
