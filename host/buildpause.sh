#!/bin/bash
# stop|cont the background store build (process group of 03-stores.sh)
P=$(pgrep -o -f "bash /root/03-stores.sh") || exit 0
G=$(ps -o pgid= -p $P | tr -d ' ')
[ "$G" = "$(ps -o pgid= -p $$ | tr -d ' ')" ] && { echo "same pgid as me, refusing"; exit 1; }
kill -$( [ "$1" = stop ] && echo STOP || echo CONT ) -- -$G && echo "$1 pgid $G"
