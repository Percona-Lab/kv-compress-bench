#!/usr/bin/env python3
"""Generate the protocol-v2 plans for one host.
usage: make_plans.py HOSTTAG CLIENT_CPUS THREADS_JSON OUTDIR
THREADS_JSON: chosen thread counts, e.g. '{"valkey_io":4,"redis_io":4,"keydb":4,"pika_threads":2,"pika_pool":6}'"""
import json, sys
host, ccpus, th, outdir = sys.argv[1], sys.argv[2], json.loads(sys.argv[3]), sys.argv[4]
V = {"value_size": "fixed:1024", "value_random_pct": 10}
STD = [{"key_dist": "uniform"}, {"key_dist": "zipfian:0.99"}, {"key_dist": "hotspot:0.1:0.9"}, {"key_dist": "normal:0.5:0.05"},
       {"key_dist": "uniform", "rate": 50000}, {"key_dist": "zipfian:0.99", "rate": 50000}]
CAP = [{"key_dist": "uniform", "pipeline": 8}]
DEF = {"threads": 6, "conns": 12, "cpus": ccpus, "warmup": 30, "time": 120, "time_rate": 300,
       "settle_chunk": 30, "settle_pct": 2.0, "settle_disk_pct": 25.0, "settle_disk_mb": 5.0, "settle_max": 600}
SLOW = {"settle_max": 1800}
vio = {"VALKEY_IO_THREADS": th["valkey_io"]}

def grp(name, store, keys, swap="none", env=None, extra=None, slow=False, cc=None, points=STD):
    d = dict(V, keys=keys)
    if cc:
        d["client_compress"] = cc
    g = {"name": name, "store": store, "swap": swap, "data": d, "points": list(points) + (extra or [])}
    e = dict(env or {})
    if swap != "none":
        e.update({"ZRAM_GB": 96, "SWAP_GB": 96})
    if e:
        g["env"] = {k: str(v) for k, v in e.items()}
    if slow:
        g["point_defaults"] = SLOW
    return g

plans = {}
# 20 GB: everything in RAM (15M keys = 20 GB in Valkey)
k20 = 15_000_000
plans["20g"] = [
    grp("valkey", "valkey", k20, extra=CAP),
    grp("valkey-io", "valkey-io", k20, env=vio, extra=CAP),
    grp("redis", "redis", k20, extra=CAP),
    grp("redis-io", "redis-io", k20, env={"REDIS_IO_THREADS": th["redis_io"]}, extra=CAP),
    grp("keydb", "keydb", k20, env={"KEYDB_THREADS": th["keydb"]}, extra=CAP),
    grp("dragonfly", "dragonfly", k20, extra=CAP),
    grp("garnet", "garnet", k20, env={"GARNET_OPTS": "-m 24g"}, extra=CAP),
    grp("kvrocks", "kvrocks", k20, extra=CAP, slow=True),
    grp("kvrocks-lz4", "kvrocks", k20, env={"KVROCKS_OPTS": "rocksdb.compression=lz4"}, extra=CAP, slow=True),
    grp("pika", "pika", k20, env={"PIKA_THREADS": th["pika_threads"], "PIKA_POOL": th["pika_pool"]}, extra=CAP, slow=True),
    grp("lavik", "lavik", k20, extra=CAP, slow=True),
    grp("tideskv-nosync", "tideskv", k20, env={"TIDESKV_OPTS": "sync-mode=none"}, extra=CAP, slow=True),
]
for tag, keys in (("40g", 30_000_000), ("64g", 48_000_000)):
    plans[tag] = [
        grp("valkey", "valkey", keys, swap="zram-lz4"),
        grp("valkey", "valkey", keys, swap="zram-zstd-1"),
        grp("valkey", "valkey", keys, swap="zswap-lz4"),
        grp("valkey-zswap-noshrink", "valkey", keys, swap="zswap-lz4", env={"ZSWAP_SHRINKER": "N"}),
        grp("valkey", "valkey", keys, swap="disk", slow=True),
        grp("valkey-client-lz4", "valkey", keys, swap="disk", cc="lz4"),
        grp("valkey-client-zstd3", "valkey", keys, swap="disk", cc="zstd:3"),
        grp("valkey-io", "valkey-io", keys, swap="zram-lz4", env=vio),
        grp("kvrocks", "kvrocks", keys, slow=True),
        grp("kvrocks-lz4", "kvrocks", keys, env={"KVROCKS_OPTS": "rocksdb.compression=lz4"}, slow=True),
        grp("pika", "pika", keys, env={"PIKA_THREADS": th["pika_threads"], "PIKA_POOL": th["pika_pool"]}, slow=True),
        grp("lavik", "lavik", keys, slow=True),
        grp("tideskv-nosync", "tideskv", keys, env={"TIDESKV_OPTS": "sync-mode=none"}, slow=True),
        grp("dragonfly-tiered", "dragonfly", keys, env={"DRAGONFLY_OPTS": "--tiered_prefix=/data/dragonfly/tier"}, slow=True),
        grp("garnet-tiered", "garnet", keys, env={"GARNET_OPTS": "--storage-tier --logdir /data/garnet/log"}, slow=True),
    ]
for tag, groups in plans.items():
    p = {"name": f"v2-{host}-{tag}", "protocol": "v2", "rounds": 2, "shuffle": True, "defaults": DEF, "groups": groups,
         "threads_chosen": th}
    json.dump(p, open(f"{outdir}/v2-{host}-{tag}.json", "w"), indent=1)
    print(p["name"], len(groups), "groups")
