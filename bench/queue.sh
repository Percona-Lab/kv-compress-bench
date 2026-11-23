#!/bin/bash
# run plans one after another: queue.sh plan1 plan2 ...  (waits for any running study.py first)
while pgrep -f "bench/study.py" >/dev/null; do sleep 30; done
for p in "$@"; do n=$(python3 -c "import json;print(json.load(open(\"$p\"))[\"name\"])"); python3 /root/bench/study.py "$p" > /root/results/$n.out 2>&1; done
