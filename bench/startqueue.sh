#!/bin/bash
nohup /root/bench/queue.sh "$@" > /root/results/queue.out 2>&1 < /dev/null &
echo "queue pid $!"
