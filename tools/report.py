#!/usr/bin/env python3
"""Build RESULTS.md tables from data/*/results.jsonl. Usage: python3 tools/report.py > RESULTS-tables.md"""
import json, os, sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "data")
WL_CLOSED = ["uniform|closed", "zipfian:0.99|closed", "hotspot:0.1:0.9|closed", "normal:0.5:0.05|closed"]
WL_RATE = ["uniform|rate50000", "zipfian:0.99|rate50000"]
WL_NAME = {"uniform|closed": "uniform", "zipfian:0.99|closed": "zipfian 0.99", "hotspot:0.1:0.9|closed": "hotspot 10%/90%",
           "normal:0.5:0.05|closed": "normal (sd 5%)", "uniform|rate50000": "uniform @ 50k ops/s",
           "zipfian:0.99|rate50000": "zipfian @ 50k ops/s"}
MODE_NAME = {"none": "in RAM", "zram-lz4": "zram lz4", "zram-zstd": "zram zstd 3", "zram-zstd1": "zram zstd 1",
             "zram-zstd-1": "zram zstd -1", "zswap-lz4": "zswap lz4", "zswap-zstd": "zswap zstd 3", "disk": "disk swap"}


def load(study, key="swap"):
    d = {}
    p = os.path.join(ROOT, study, "results.jsonl")
    for line in open(p):
        r = json.loads(line)
        wl = r["label"].split("|", 3)[3]
        name = r.get(key) or r["swap"]
        d.setdefault(name, {})[wl] = r
    return d


def thr(r):
    return f"{r['summary']['ops_s']/1000:.1f}k" if r else ""


def lat(r):
    if not r:
        return ""
    s = r["summary"]
    if not s.get("target_rate_met"):
        return f"missed ({s['ops_s']/1000:.1f}k)"
    L = s["latency"]["all"]
    return f"{L['p99_us']:.0f} / {L['p999_us']:.0f}"


def flt(r):
    return f"{r['summary']['cpu'].get('server_majflt_per_op', 0):.3f}" if r else ""


def table(title, cols, rows, note=None):
    out = [f"### {title}", ""]
    if note:
        out += [note, ""]
    out.append("| " + " | ".join(cols) + " |")
    out.append("|" + "---|" * len(cols))
    for row in rows:
        out.append("| " + " | ".join(row) + " |")
    out.append("")
    return "\n".join(out)


def matrix(title, data, modes, note=None, with_faults=False):
    rows = []
    for wl in WL_CLOSED:
        rows.append([WL_NAME[wl]] + [thr(data.get(m, {}).get(wl)) + (f" ({flt(data[m][wl])})" if with_faults and data.get(m, {}).get(wl) and m != "none" else "") for m in modes])
    t1 = table(title + ": throughput at full load (ops/s)" + (" (page faults per op)" if with_faults else ""),
               ["workload"] + [MODE_NAME.get(m, m) for m in modes], rows, note)
    rows = [[WL_NAME[wl]] + [lat(data.get(m, {}).get(wl)) for m in modes] for wl in WL_RATE]
    t2 = table(title + ": latency at a fixed 50k ops/s, p99 / p99.9 (µs)", ["workload"] + [MODE_NAME.get(m, m) for m in modes], rows)
    return t1 + "\n" + t2


def memrow(r):
    s = r["summary"]; sy = s["system"]; ie = s.get("server_info_end") or {}
    used = ie.get("used_memory", 0) / 1e9
    rss = sy.get("server_rss_kb", 0) / 1e6
    sw = sy.get("server_swap_kb", 0) / 1e6
    ratio = ""
    if sy.get("zram0_compr_bytes"):
        ratio = f"{sy['zram0_orig_bytes']/sy['zram0_compr_bytes']:.1f}x"
    elif sy.get("Zswap_kb"):
        ratio = f"{sy['Zswapped_kb']/sy['Zswap_kb']:.1f}x"
    return used, rss, sw, ratio


def main():
    out = []
    r2 = load("valkey-swap-r2")
    for k, v in load("valkey-swap-zstdfast-40g").items():
        r2[k] = v
    modes40 = ["none", "zram-lz4", "zram-zstd", "zram-zstd1", "zram-zstd-1", "zswap-lz4", "zswap-zstd", "disk"]
    out.append(matrix("40 GB (30M keys x 1 KiB, 31 GB RAM)", r2, modes40,
                      "In RAM = 15M keys (20 GB), no swap. Kernel defaults for zswap (shrinker on).", with_faults=True))

    g64 = load("valkey-swap-64g-r1")
    ns = load("valkey-swap-64g-zswap-noshrink")
    for k, v in ns.items():
        g64[k + "-noshrink"] = v
    g64["none"] = r2["none"]
    MODE_NAME.update({"zswap-lz4-noshrink": "zswap lz4, shrinker off", "zswap-zstd-noshrink": "zswap zstd 3, shrinker off"})
    modes64 = ["none", "zram-lz4", "zram-zstd", "zram-zstd1", "zram-zstd-1", "zswap-lz4", "zswap-lz4-noshrink", "zswap-zstd", "zswap-zstd-noshrink", "disk"]
    out.append(matrix("64 GB (48M keys x 1 KiB, 2x RAM)", g64, modes64,
                      "Disk swap at a fixed rate: latency capped at the 60 s histogram limit when the target is missed.", with_faults=True))

    # memory split
    rows = []
    for size, data, modes in (("40 GB", r2, modes40[1:-1]), ("64 GB", g64, modes64[1:-1])):
        for m in modes:
            r = data.get(m, {}).get("uniform|closed")
            if not r:
                continue
            used, rss, sw, ratio = memrow(r)
            rows.append([size, MODE_NAME.get(m, m), f"{used:.1f}", f"{rss:.1f}", f"{sw:.1f}", ratio])
    out.append(table("Where Valkey's memory is (uniform, full load, end of run)",
                     ["data", "mode", "Valkey used_memory GB", "resident GB", "swapped GB", "compression ratio"], rows))

    # variable length
    v30 = load("valkey-swap-varlen-r1"); v35 = load("valkey-swap-varlen-35m")
    rows = []
    for m in ["zram-lz4", "zram-zstd", "zswap-lz4", "zswap-zstd", "disk"]:
        for wl in WL_CLOSED[:2] + WL_RATE[:1]:
            f = (lambda r: thr(r) + f" ({flt(r)})") if "closed" in wl else lat
            rows.append([MODE_NAME[m], WL_NAME[wl], f(r2.get(m, {}).get(wl)), f(v35.get(m, {}).get(wl)), f(v30.get(m, {}).get(wl))])
    lr = lambda d, m: (d[m]["uniform|closed"]["summary"].get("server_info_end") or {}).get("used_memory", 0) / 1e9
    out.append(table("Fixed 1 KiB vs variable 0-2000 B values (ops/s (faults/op), or p99 / p99.9 µs)",
                     ["mode", "workload", f"fixed, 30M keys ({lr(r2,'zram-lz4'):.1f} GB)", f"variable, 35M keys ({lr(v35,'zram-lz4'):.1f} GB)",
                      f"variable, 30M keys ({lr(v30,'zram-lz4'):.1f} GB)"], rows))
    print("\n".join(out))


if __name__ == "__main__":
    main()
