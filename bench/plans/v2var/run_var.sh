#!/bin/bash
# Sizing run, then the log-normal follow-up plans. Waits for any running study first.
set -u
HOST=$1; CCPUS=${2:-0,4,5,6,10,11}; TH=${3:-'{"valkey_io":6,"redis_io":6,"keydb":4,"pika_threads":4,"pika_pool":6}'}
while pgrep -f "^/bin/bash /root/bench/queue.sh" >/dev/null || pgrep -f "^python3 /root/bench/study.py" >/dev/null; do sleep 60; done
OUT=/root/results/v2var-$HOST-sizing; mkdir -p $OUT
K=/root/kvbench/target/release/kvbench
bash /root/bench/kvctl.sh stop; bash /root/host/swapmode.sh none; bash /root/bench/kvctl.sh wipe valkey
bash /root/bench/kvctl.sh start valkey
$K load --keys 3000000 --value-size lognormal:700:0.8:32:8192 --value-random-pct 10 --threads 6 --conns 4 --cpus $CCPUS --quiet --out $OUT/load.jsonl > $OUT/load.json
bash /root/bench/kvctl.sh stop
BPK=$(python3 -c "import json;d=json.load(open('$OUT/load.json'));print(d['server_info']['used_memory']/d['keys_loaded'])")
echo "bytes per key: $BPK" | tee $OUT/sizing.txt
python3 /root/bench/plans/v2var/make_plans_var.py $HOST $CCPUS "$TH" $BPK /root/bench/plans/v2var | tee -a $OUT/sizing.txt
for t in 20g 40g 64g; do python3 /root/bench/study.py /root/bench/plans/v2var/v2var-$HOST-$t.json > /root/results/v2var-$HOST-$t.out 2>&1; done
