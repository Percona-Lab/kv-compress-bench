#!/usr/bin/env python3
"""every 0.5 s: server CPU and the sum / max of Recv-Q and Send-Q on the server side (sport 6379) and client side (dport 6379)"""
import json, os, subprocess, sys, time
pid, out, secs = int(sys.argv[1]), sys.argv[2], float(sys.argv[3])
tck = os.sysconf("SC_CLK_TCK")
def cpu():
    f = open(f"/proc/{pid}/stat").read(); f = f[f.rindex(")") + 2:].split(); return (int(f[11]) + int(f[12])) / tck
def q(filt):
    o = subprocess.run(f"ss -tnH state established '{filt}'", shell=True, capture_output=True, text=True).stdout.splitlines()
    rq = [int(l.split()[0]) for l in o]; sq = [int(l.split()[1]) for l in o]
    return {"n": len(o), "recvq_sum": sum(rq), "recvq_nonzero": sum(1 for x in rq if x), "recvq_max": max(rq or [0]),
            "sendq_sum": sum(sq), "sendq_nonzero": sum(1 for x in sq if x)}
t0 = time.time(); c0 = cpu()
with open(out, "w") as fo:
    while time.time() - t0 < secs:
        time.sleep(0.5); c = cpu()
        fo.write(json.dumps({"t": round(time.time() - t0, 1), "srv_cpu": round((c - c0) / 0.5, 2), "server": q("( sport = :6379 )"), "client": q("( dport = :6379 )")}) + "\n")
        fo.flush(); c0 = c
