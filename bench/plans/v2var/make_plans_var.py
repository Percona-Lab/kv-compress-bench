#!/usr/bin/env python3
"""Follow-up to protocol v2: log-normal value sizes (same compressibility). Key counts come from a sizing run so that
Valkey's memory matches the fixed-size v2 data sets.
usage: make_plans_var.py HOSTTAG CLIENT_CPUS THREADS_JSON BYTES_PER_KEY OUTDIR"""
import json, sys
sys.path.insert(0, __import__("os").path.dirname(__file__) + "/../v2")
host, ccpus, th, bpk, outdir = sys.argv[1], sys.argv[2], json.loads(sys.argv[3]), float(sys.argv[4]), sys.argv[5]
VS = "lognormal:700:0.8:32:8192"
# Valkey used_memory right after loading the fixed 1 KiB v2 data sets
TARGET = {"20g": 20.12e9, "40g": 40.25e9, "64g": 63.63e9}
V = {"value_size": VS, "value_random_pct": 10}
STD = [{"key_dist": "uniform"}, {"key_dist": "zipfian:0.99"}, {"key_dist": "hotspot:0.1:0.9"}, {"key_dist": "normal:0.5:0.05"},
       {"key_dist": "uniform", "rate": 50000}, {"key_dist": "zipfian:0.99", "rate": 50000}]
CAP = [{"key_dist": "uniform", "pipeline": 8}]
DEF = {"threads": 6, "conns": 12, "cpus": ccpus, "warmup": 30, "time": 120, "time_rate": 300,
       "settle_chunk": 30, "settle_pct": 2.0, "settle_disk_pct": 25.0, "settle_disk_mb": 5.0, "settle_max": 600}
SLOW = {"settle_max": 1800}
vio = {"VALKEY_IO_THREADS": th["valkey_io"]}
pk = {"PIKA_THREADS": th["pika_threads"], "PIKA_POOL": th["pika_pool"]}

def grp(name, store, keys, swap="none", env=None, extra=None, slow=False, cc=None):
    d = dict(V, keys=keys)
    if cc:
        d["client_compress"] = cc
    g = {"name": name, "store": store, "swap": swap, "data": d, "points": STD + (extra or [])}
    e = dict(env or {})
    if swap != "none":
        e.update({"ZRAM_GB": 96, "SWAP_GB": 96})
    if e:
        g["env"] = {k: str(v) for k, v in e.items()}
    if slow:
        g["point_defaults"] = SLOW
    return g

plans = {}
k = {t: int(round(TARGET[t] / bpk / 1e5) * 1e5) for t in TARGET}
plans["20g"] = [
    grp("valkey", "valkey", k["20g"], extra=CAP), grp("valkey-io", "valkey-io", k["20g"], env=vio, extra=CAP),
    grp("redis", "redis", k["20g"], extra=CAP), grp("redis-io", "redis-io", k["20g"], env={"REDIS_IO_THREADS": th["redis_io"]}, extra=CAP),
    grp("keydb", "keydb", k["20g"], env={"KEYDB_THREADS": th["keydb"]}, extra=CAP), grp("dragonfly", "dragonfly", k["20g"], extra=CAP),
    grp("garnet", "garnet", k["20g"], env={"GARNET_OPTS": "-m 24g"}, extra=CAP),
    grp("kvrocks-lz4", "kvrocks", k["20g"], env={"KVROCKS_OPTS": "rocksdb.compression=lz4"}, extra=CAP, slow=True),
    grp("pika", "pika", k["20g"], env=pk, extra=CAP, slow=True), grp("lavik", "lavik", k["20g"], extra=CAP, slow=True),
    grp("tideskv-nosync", "tideskv", k["20g"], env={"TIDESKV_OPTS": "sync-mode=none"}, extra=CAP, slow=True),
    grp("kvrocks", "kvrocks", k["20g"], extra=CAP, slow=True),
]
for t in ("40g", "64g"):
    n = k[t]
    plans[t] = [
        grp("valkey", "valkey", n, swap="zram-lz4"),
        grp("valkey-zswap-noshrink", "valkey", n, swap="zswap-lz4", env={"ZSWAP_SHRINKER": "N"}),
        grp("valkey", "valkey", n, swap="disk", slow=True),
        grp("valkey-client-lz4", "valkey", n, swap="disk", cc="lz4"),
        grp("valkey-client-zstd3", "valkey", n, swap="disk", cc="zstd:3"),
        grp("valkey-io", "valkey-io", n, swap="zram-lz4", env=vio),
        grp("kvrocks-lz4", "kvrocks", n, env={"KVROCKS_OPTS": "rocksdb.compression=lz4"}, slow=True),
        grp("pika", "pika", n, env=pk, slow=True), grp("lavik", "lavik", n, slow=True),
        grp("tideskv-nosync", "tideskv", n, env={"TIDESKV_OPTS": "sync-mode=none"}, slow=True),
        grp("dragonfly-tiered", "dragonfly", n, env={"DRAGONFLY_OPTS": "--tiered_prefix=/data/dragonfly/tier"}, slow=True),
        grp("garnet-tiered", "garnet", n, env={"GARNET_OPTS": "--storage-tier --logdir /data/garnet/log"}, slow=True),
    ]
for t, groups in plans.items():
    p = {"name": f"v2var-{host}-{t}", "protocol": "v2", "value_size": VS, "bytes_per_key": bpk, "rounds": 2, "shuffle": True,
         "defaults": DEF, "groups": groups, "threads_chosen": th}
    json.dump(p, open(f"{outdir}/v2var-{host}-{t}.json", "w"), indent=1)
    print(p["name"], len(groups), "groups,", k[t], "keys")
