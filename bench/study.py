#!/usr/bin/env python3
"""Run a study: a JSON plan of groups, each = one store + swap mode + data set, loaded once, then a list of
workload points run on it. Results (kvbench JSON summaries + host state) go to <out>/results.jsonl.

plan.json:
{
  "name": "zram-pilot",
  "defaults": {"threads": 4, "conns": 16, "cpus": "4,5,10,11", "warmup": 60, "time": 120},
  "groups": [
    {"store": "valkey", "swap": "zram-lz4", "data": {"keys": 30000000, "value_size": "fixed:1024", "value_random_pct": 10},
     "points": [{"key_dist": "uniform"}, {"key_dist": "zipfian:0.99", "rate": 50000}]}
  ]
}
Usage: study.py plan.json [--out DIR] [--only GROUP_INDEX[,..]] [--dry-run]
"""
import argparse, datetime, json, os, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
KVB = os.environ.get("KVBENCH", "/root/kvbench/target/release/kvbench")
KVCTL = os.path.join(HERE, "kvctl.sh")
SWAPMODE = os.path.join(os.path.dirname(HERE), "host", "swapmode.sh")


def sh(cmd, check=True, capture=False):
    print("+", cmd, flush=True)
    r = subprocess.run(cmd, shell=True, check=check, text=True, capture_output=capture)
    return r.stdout if capture else None


def log(out, msg):
    line = f"{datetime.datetime.utcnow().isoformat(timespec='seconds')}Z {msg}"
    print(line, flush=True)
    with open(os.path.join(out, "study.log"), "a") as f:
        f.write(line + "\n")


def host_state():
    def rd(p):
        try:
            return open(p).read().strip()
        except OSError:
            return None
    st = {
        "kernel": os.uname().release,
        "swap": sh("swapon --noheadings --show=NAME,SIZE,USED,PRIO", capture=True).strip(),
        "zswap": {k: rd(f"/sys/module/zswap/parameters/{k}") for k in ("enabled", "compressor", "max_pool_percent", "zpool", "shrinker_enabled", "accept_threshold_percent")},
        "zram_algo": rd("/sys/block/zram0/comp_algorithm"),
        "zram_disksize": rd("/sys/block/zram0/disksize"),
        "sysctl": {k: rd(f"/proc/sys/vm/{k}") for k in ("swappiness", "page-cluster", "watermark_boost_factor", "watermark_scale_factor", "overcommit_memory")},
        "thp": rd("/sys/kernel/mm/transparent_hugepage/enabled"),
        "no_turbo": rd("/sys/devices/system/cpu/intel_pstate/no_turbo"),
    }
    return st


def data_args(d):
    a = f"--keys {d['keys']} --value-size {d.get('value_size', 'fixed:1024')} --value-random-pct {d.get('value_random_pct', 100)}"
    if d.get("client_compress"):
        a += f" --client-compress={d['client_compress']}"
    if d.get("key_prefix"):
        a += f" --key-prefix {d['key_prefix']}"
    return a


def settle(out, gi, pi, g, o):
    """Protocol v2: drive the point's own workload (same key distribution, same target rate if it has one) in chunks
    until it is steady: two consecutive chunks with throughput within settle_pct and disk writes within
    max(settle_disk_pct, settle_disk_mb) of each other (background flushes / compactions have settled at the level
    this workload produces). At least 2 chunks, at most settle_max seconds."""
    chunk, pct, mx = o.get("settle_chunk", 30), o.get("settle_pct", 2.0), o.get("settle_max", 600)
    dpct, dmb = o.get("settle_disk_pct", 25.0), o.get("settle_disk_mb", 5.0)
    if mx <= 0:
        return None
    pid = sh(f"{KVCTL} pid", capture=True).strip()
    hist = []
    t0 = time.time()

    def steady():
        if len(hist) < 2:
            return False
        (a, da), (b, db) = hist[-2], hist[-1]
        ok_ops = abs(a - b) <= pct / 100 * max(a, b)
        ok_disk = abs(da - db) <= max(dpct / 100 * max(da, db), dmb)
        return ok_ops and ok_disk

    while time.time() - t0 < mx:
        cmd = (f"{KVB} run {data_args(g['data'])} --threads {o['threads']} --conns {o['conns']} --cpus {o['cpus']} "
               f"--server-pid {pid} --time {chunk} --warmup 0 --get-ratio {o.get('get_ratio', 0.9)} "
               f"--key-dist {o.get('key_dist', 'uniform')} --quiet")
        if o.get("rate"):
            cmd += f" --rate {o['rate']}"
        if o.get("pipeline"):
            cmd += f" --pipeline {o['pipeline']}"
        r = subprocess.run(cmd, shell=True, text=True, capture_output=True)
        try:
            j = json.loads(r.stdout)
            hist.append((j["ops_s"], j["system"].get("disk_write_mb_s", 0.0)))
        except Exception:
            log(out, f"settle chunk failed: {r.stderr[-300:]}")
            break
        if steady():
            break
    res = {"chunks_ops_s": [round(x[0]) for x in hist], "chunks_disk_write_mb_s": [round(x[1], 1) for x in hist],
           "seconds": round(time.time() - t0), "steady": steady()}
    log(out, f"settle g{gi} p{pi}: {res}")
    return res


def run_point(out, gi, pi, g, p, dflt, rnd=0):
    o = dict(dflt)
    o.update(g.get("point_defaults", {}))
    o.update(p)
    if o.get("rate") and o.get("time_rate"):
        o["time"] = o["time_rate"]
    settled = settle(out, gi, pi, g, o)
    label = f"{g.get('name', g['store'])}|{g['swap']}|{g['data']['keys']}|{o.get('key_dist','uniform')}|{'rate'+str(o['rate']) if o.get('rate') else 'closed'}"
    if o.get("tag"):
        label += "|" + o["tag"]
    if o.get("pipeline"):
        label += f"|pipe{o['pipeline']}"
    if rnd:
        label += f"|r{rnd}"
    pid = sh(f"{KVCTL} pid", capture=True).strip()
    args = (f"{KVB} run {data_args(g['data'])} --threads {o['threads']} --conns {o['conns']} --cpus {o['cpus']} "
            f"--server-pid {pid} --time {o['time']} --warmup {o['warmup']} --get-ratio {o.get('get_ratio', 0.9)} "
            f"--key-dist {o.get('key_dist', 'uniform')} --label '{label}' --quiet "
            f"--out {out}/intervals.jsonl")
    if o.get("rate"):
        args += f" --rate {o['rate']}"
    if o.get("pipeline"):
        args += f" --pipeline {o['pipeline']}"
    if o.get("verify"):
        args += " --verify"
    log(out, f"point g{gi} p{pi}: {label}")
    r = subprocess.run(args, shell=True, text=True, capture_output=True)
    try:
        summ = json.loads(r.stdout)
    except json.JSONDecodeError:
        summ = {"error": "no summary", "stderr": r.stderr[-4000:]}
    rec = {"type": "point", "study": out, "group": gi, "point": pi, "store": g.get("name", g["store"]), "swap": g["swap"], "env": g.get("env", {}),
           "label": label, "ts": datetime.datetime.utcnow().isoformat(timespec="seconds") + "Z",
           "host": host_state(), "exit": r.returncode, "settle": settled, "round": rnd, "summary": summ}
    with open(os.path.join(out, "results.jsonl"), "a") as f:
        f.write(json.dumps(rec) + "\n")
    last = [l for l in r.stderr.splitlines() if l.startswith("SUMMARY") or l.startswith("WARNING")]
    log(out, " | ".join(last) if last else f"exit {r.returncode}: {r.stderr[-300:]}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("plan")
    ap.add_argument("--out")
    ap.add_argument("--only")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    plan = json.load(open(a.plan))
    out = a.out or f"/root/results/{plan['name']}"
    os.makedirs(out, exist_ok=True)
    sh(f"cp {a.plan} {out}/plan-{int(time.time())}.json")
    dflt = {"threads": 4, "conns": 16, "cpus": "4,5,10,11", "warmup": 60, "time": 120,
            "settle_chunk": 30, "settle_pct": 2.0, "settle_max": 600}
    dflt.update(plan.get("defaults", {}))
    only = set(int(x) for x in a.only.split(",")) if a.only else None
    for gi, g in enumerate(plan["groups"]):
        if only is not None and gi not in only:
            continue
        log(out, f"group {gi}: {g['store']} swap={g['swap']} data={g['data']}")
        if a.dry_run:
            continue
        sh(f"{KVCTL} stop")
        sh(f"{KVCTL} wipe {g['store']}")
        env = " ".join(f"{k}='{v}'" for k, v in g.get("env", {}).items())
        sh(f"{env} {SWAPMODE} {g['swap']}")
        sh("sync; echo 3 > /proc/sys/vm/drop_caches")
        sh(f"{env} {KVCTL} start {g['store']}")
        pid = sh(f"{KVCTL} pid", capture=True).strip()
        t0 = time.time()
        r = subprocess.run(f"{KVB} load {data_args(g['data'])} --threads 4 --conns 4 --cpus {dflt['cpus']} --server-pid {pid} "
                           f"--label 'load|{g.get('name', g['store'])}|{g['swap']}' --quiet --out {out}/loads.jsonl",
                           shell=True, text=True, capture_output=True)
        try:
            ls = json.loads(r.stdout)
            log(out, f"loaded {ls['keys_loaded']} keys in {time.time()-t0:.0f}s, used_memory={ls['server_info'].get('used_memory')} "
                     f"rss={ls['system'].get('server_rss_kb')}kB swap={ls['system'].get('server_swap_kb')}kB")
        except Exception:
            log(out, f"LOAD FAILED exit {r.returncode}: {r.stderr[-500:]}")
            continue
        rounds = int(plan.get("rounds", 1))
        for rnd in range(1, rounds + 1):
            order = list(range(len(g["points"])))
            if plan.get("shuffle") and rounds > 1:
                import random
                random.Random(f"{plan['name']}|{gi}|{rnd}").shuffle(order)
            log(out, f"group {gi} round {rnd}: order {order}")
            for pi in order:
                if os.path.exists(os.path.join(out, "STOP")):
                    log(out, "STOP file found, ending")
                    return
                run_point(out, gi, pi, g, g["points"][pi], dflt, rnd if rounds > 1 else 0)
        sh(f"{KVCTL} stop")
    sh(f"{KVCTL} stop")
    sh(f"{SWAPMODE} none")
    log(out, "STUDY-DONE")


if __name__ == "__main__":
    main()
