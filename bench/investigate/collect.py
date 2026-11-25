#!/usr/bin/env python3
"""Sample a server process while a benchmark runs: per-thread CPU every second, INFO every second, and all
thread stacks (eu-stack) every --stack-every seconds. Writes JSON lines to OUT. Stops when the pid exits or STOP file appears.
usage: collect.py PID OUT [--stack-every 2]"""
import json, os, subprocess, sys, time
pid, out = int(sys.argv[1]), sys.argv[2]
every = int(sys.argv[sys.argv.index("--stack-every") + 1]) if "--stack-every" in sys.argv else 2
tck = os.sysconf("SC_CLK_TCK")
def threads():
    d = {}
    for t in os.listdir(f"/proc/{pid}/task"):
        try:
            st = open(f"/proc/{pid}/task/{t}/stat").read()
            comm = st[st.index("(") + 1:st.rindex(")")]
            f = st[st.rindex(")") + 2:].split()
            d[int(t)] = (comm, int(f[11]) + int(f[12]), f[0], open(f"/proc/{pid}/task/{t}/wchan").read())
        except OSError:
            pass
    return d
prev = threads(); n = 0
with open(out, "a") as fo:
    while os.path.exists(f"/proc/{pid}") and not os.path.exists(out + ".STOP"):
        time.sleep(1); n += 1
        now = threads(); ts = time.time()
        cpu = sorted(((round((v[1] - prev.get(t, (0, v[1]))[1]) / tck, 2), t, v[0], v[2], v[3]) for t, v in now.items()), reverse=True)
        rec = {"ts": ts, "threads": [{"cpu": c, "tid": t, "comm": cm, "state": s, "wchan": w} for c, t, cm, s, w in cpu if c > 0.01 or s != "S"][:20],
               "states": {}}
        for v in now.values():
            rec["states"][v[2] + ":" + v[3]] = rec["states"].get(v[2] + ":" + v[3], 0) + 1
        try:
            rec["info"] = subprocess.run(["/opt/valkey-9.2.0/bin/valkey-cli", "-p", "6379", "info", "all"], capture_output=True, text=True, timeout=0.8).stdout
        except Exception as e:
            rec["info"] = f"ERR {e}"
        if n % every == 0:
            try:
                rec["stacks"] = subprocess.run(["eu-stack", "-p", str(pid)], capture_output=True, text=True, timeout=5).stdout
            except Exception as e:
                rec["stacks"] = f"ERR {e}"
        fo.write(json.dumps(rec) + "\n"); fo.flush()
        prev = now
