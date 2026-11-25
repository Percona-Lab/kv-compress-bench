#!/bin/bash
# Correctness check + stall investigation for TidesKV (investigation run, not a published benchmark).
# usage: tideskv-check.sh <name> "<TIDESKV_OPTS>"
set -u
NAME=$1; OPTS=$2; OUT=/root/results/investigate-tideskv/$NAME; mkdir -p $OUT
K=/root/kvbench-check; D="--keys 30000000 --value-size fixed:1024 --value-random-pct 10"; C="--threads 4 --conns 16 --cpus 4,5,10,11"
bash /root/bench/kvctl.sh stop; bash /root/host/swapmode.sh none >/dev/null; bash /root/bench/kvctl.sh wipe tideskv
sync; echo 3 > /proc/sys/vm/drop_caches
TIDESKV_OPTS="$OPTS" bash /root/bench/kvctl.sh start tideskv
PID=$(bash /root/bench/kvctl.sh pid)
$K load $D --threads 4 --conns 4 --cpus 4,5,10,11 --quiet --out $OUT/load.jsonl > $OUT/load.json
python3 /root/bench/investigate/collect.py $PID $OUT/collect.jsonl --stack-every 2 &
CP=$!
for w in "hotspot:0.1:0.9" "uniform --rate 50000" "uniform" "zipfian:0.99"; do
  tag=$(echo $w | tr ' :.' '___')
  $K run $D $C --server-pid $PID --key-dist $w --time 300 --warmup 30 --check --label "$NAME|$w" --quiet --out $OUT/intervals.jsonl > $OUT/run-$tag.json 2> $OUT/run-$tag.err
  echo "$w exit $?" >> $OUT/done.txt; grep -E "CHECK|SUMMARY" $OUT/run-$tag.err >> $OUT/done.txt
done
touch $OUT/collect.jsonl.STOP; wait $CP
journalctl -u kvbsrv --no-pager > $OUT/server.log 2>&1
bash /root/bench/kvctl.sh stop
echo INVESTIGATION-DONE >> $OUT/done.txt
