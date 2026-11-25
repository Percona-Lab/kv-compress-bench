#!/usr/bin/env python3
"""Build the GitHub Pages site (docs/index.html) from data/*/results.jsonl.
The page template is tools/site_template.html; the data is embedded as JSON so the page works offline too.
Usage: python3 tools/build_site.py"""
import json, os

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.join(HERE, "..")
DATA = os.path.join(ROOT, "data")

# (study directory, data-set id, mode override)
STUDIES = [
    ("valkey-swap-r2", "40g", None),
    ("valkey-swap-zstdfast-40g", "40g", None),
    ("valkey-swap-64g-r1", "64g", None),
    ("valkey-swap-64g-zswap-noshrink", "64g", "noshrink"),
    ("valkey-swap-varlen-35m", "var40g", None),
    ("valkey-swap-varlen-r1", "var30m", None),
    ("valkey-client-compress", "auto", "client"),
]
# later entries replace earlier ones for the same (data set, mode, workload): the 40 GB reruns replace the stores-r1
# groups that overlapped side work on the benchmark host
STORES = [("stores-r1", None, None), ("stores-r1-rerun40", None, None), ("stores-tideskv", None, None)]


def stalls(study):
    """seconds (measured 1 s intervals) in which some request waited more than 100 ms, per label"""
    out = {}
    p = os.path.join(DATA, study, "intervals.jsonl")
    if not os.path.exists(p):
        return out
    for line in open(p):
        r = json.loads(line)
        if r.get("type") != "interval" or r.get("phase") != "measure":
            continue
        d = out.setdefault(r["label"], [0, 0])
        d[1] += 1
        if r.get("max_us", 0) > 100_000:
            d[0] += 1
    return out


def rows():
    out = {}
    for study, ds, tag in STUDIES + STORES:
        p = os.path.join(DATA, study, "results.jsonl")
        if not os.path.exists(p):
            continue
        st = stalls(study)
        for line in open(p):
            r = json.loads(line)
            s = r["summary"]
            if "latency" not in s:
                continue
            label = r["label"].split("|")
            wl = label[3] + "|" + label[4]
            mode = r["swap"] + ("-" + tag if tag else "")
            store = r.get("store", "valkey")
            dset = ds
            if tag == "client":  # client-side compression study: mode from the group name, data set from the key count
                mode = store.replace("valkey-", "")          # client-lz4, client-zstd:-1, client-zstd:3
                store = "valkey"
                dset = "40g" if int(label[2]) == 30000000 else "64g"
            if dset is None:  # stores study: data set from key count
                dset = "40g" if int(label[2]) == 30000000 else "64g"
                mode = store
                store = store.split("-")[0]
            sy = s.get("system", {})
            ie = s.get("server_info_end") or {}
            L = s["latency"]["all"]
            ratio = None
            if sy.get("zram0_compr_bytes"):
                ratio = sy["zram0_orig_bytes"] / sy["zram0_compr_bytes"]
            elif sy.get("Zswap_kb"):
                ratio = sy["Zswapped_kb"] / sy["Zswap_kb"]
            sd = st.get(r["label"], [0, 0])
            out[(dset, store, mode, wl)] = {
                "stall_s": sd[0], "intervals": sd[1],
                "study": study, "ds": dset, "store": store, "mode": mode, "wl": wl, "keys": int(label[2]),
                "ops": round(s["ops_s"]), "met": s.get("target_rate_met"),
                "p50": round(L["p50_us"], 1), "p99": round(L["p99_us"], 1), "p999": round(L["p999_us"], 1),
                "faults": round(s["cpu"].get("server_majflt_per_op", 0), 4),
                "srv_us": round(s["cpu"].get("server_cpu_us_per_op", 0), 2),
                "used_gb": round(ie.get("used_memory", 0) / 1e9, 1) if ie.get("used_memory") else None,
                "rss_gb": round(sy.get("server_rss_kb", 0) / 1e6, 1),
                "swap_gb": round(sy.get("server_swap_kb", 0) / 1e6, 1),
                "ratio": round(ratio, 2) if ratio else None,
                "zswap_wb_s": round(sy.get("zswap_writeback_s", 0)),
                "disk_read_mb_s": round(sy.get("disk_read_mb_s", 0), 1),
                "cli_us": round(s["cpu"].get("client_cpu_us_per_op", 0), 2),
                "cc_ratio": round(s["client_compress"]["ratio"], 2) if s.get("client_compress") else None,
            }
    return list(out.values())


def main():
    data = rows()
    tpl = open(os.path.join(HERE, "site_template.html")).read()
    html = tpl.replace("/*__DATA__*/[]", json.dumps(data, separators=(",", ":")))
    os.makedirs(os.path.join(ROOT, "docs"), exist_ok=True)
    open(os.path.join(ROOT, "docs", "index.html"), "w").write(html)
    json.dump(data, open(os.path.join(ROOT, "docs", "results.json"), "w"), indent=0)
    open(os.path.join(ROOT, "docs", ".nojekyll"), "w").write("")
    print(f"docs/index.html: {len(data)} result rows")


if __name__ == "__main__":
    main()
