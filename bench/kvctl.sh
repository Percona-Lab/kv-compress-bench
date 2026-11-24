#!/bin/bash
# Start / stop one store at a time on 127.0.0.1:6379 as the systemd unit kvbsrv.
#   kvctl.sh start valkey|valkey-io|kvrocks|lavik   kvctl.sh stop   kvctl.sh pid   kvctl.sh wipe <store>
# CPU layout (E-2286G, cpu N and N+6 are SMT siblings): OS 0,6 | server 1-3,7-9 | client 4,5,10,11
# Every non-default setting is listed here (and copied into results by run scripts).
set -euo pipefail
SRV_CPUS=${SRV_CPUS:-1,2,3,7,8,9}
VALKEY=/opt/valkey-9.2.0/bin/valkey-server
KVROCKS=/opt/kvrocks-2.17.0/bin/kvrocks
LAVIK=/opt/lavik-0.3.2/lavik
LAVIK_GB=${LAVIK_GB:-300}
cmd=${1:?}; store=${2:-}

unit_run() {  # run "$@" as unit kvbsrv pinned to the server CPUs, protected from the OOM killer
  systemd-run --quiet --unit=kvbsrv --collect -p CPUAffinity="${SRV_CPUS//,/ }" -p OOMScoreAdjust=-900 \
    -p LimitNOFILE=1048576 -p LimitMEMLOCK=infinity "$@"
}
wait_up() {
  for i in $(seq 1 600); do
    if /opt/valkey-9.2.0/bin/valkey-cli -p 6379 ping 2>/dev/null | grep -q PONG; then return 0; fi
    systemctl is-active -q kvbsrv || { journalctl -u kvbsrv --no-pager | tail -20; echo "server died" >&2; exit 1; }
    sleep 0.5
  done
  echo "server did not come up" >&2; exit 1
}

case $cmd in
  start)
    systemctl is-active -q kvbsrv && { echo "kvbsrv already running" >&2; exit 1; }
    case $store in
      valkey|valkey-io)
        mkdir -p /data/valkey
        IO=1; [ $store = valkey-io ] && IO=${VALKEY_IO_THREADS:-4}
        # non-defaults: no persistence (save "", appendonly no), protected-mode off (loopback only), io-threads
        unit_run -p WorkingDirectory=/data/valkey $VALKEY --bind 127.0.0.1 --port 6379 --save "" --appendonly no \
          --protected-mode no --io-threads $IO --logfile /data/valkey/valkey.log ;;
      kvrocks)
        # shipped kvrocks.conf (rocksdb.compression snappy from level 2, 4 GB block cache, 16 KB blocks) with only
        # bind/port/dir/log-dir/workers changed, plus KVROCKS_OPTS="key=value key=value" overrides
        mkdir -p /data/kvrocks
        cp /opt/src/kvrocks-2.17.0/kvrocks.conf /data/kvrocks.conf
        set_conf() { if grep -q "^$1 " /data/kvrocks.conf; then sed -i "s|^$1 .*|$1 $2|" /data/kvrocks.conf; else echo "$1 $2" >> /data/kvrocks.conf; fi; }
        set_conf bind 127.0.0.1; set_conf port 6379; set_conf dir /data/kvrocks; set_conf log-dir /data/kvrocks
        set_conf workers ${KVROCKS_WORKERS:-6}; set_conf daemonize no
        for kv in ${KVROCKS_OPTS:-}; do set_conf "${kv%%=*}" "${kv#*=}"; done
        grep -E "^(bind|port|dir|workers|rocksdb\.(compression|block_cache_size|compression_start_level))" /data/kvrocks.conf
        unit_run $KVROCKS -c /data/kvrocks.conf ;;
      lavik)
        mkdir -p /data/lavik/logs
        [ -e /data/lavik/data ] || fallocate -l ${LAVIK_GB}G /data/lavik/data
        unit_run -p WorkingDirectory=/data/lavik $LAVIK --bind 127.0.0.1 --port 6379 --pin-workers --cpus ${SRV_CPUS//,/ } \
          --threads $(echo $SRV_CPUS | tr , '\n' | wc -l) --data-file /data/lavik/data --log-dir /data/lavik/logs ;;
      *) echo "unknown store $store" >&2; exit 2 ;;
    esac
    wait_up
    echo "$store up, pid $(systemctl show -p MainPID --value kvbsrv)" ;;
  stop)
    systemctl stop kvbsrv 2>/dev/null || true
    for i in $(seq 1 120); do systemctl is-active -q kvbsrv || break; sleep 0.5; done ;;
  pid) systemctl show -p MainPID --value kvbsrv ;;
  wipe)
    systemctl is-active -q kvbsrv && { echo "stop first" >&2; exit 1; }
    case $store in valkey|valkey-io) rm -rf /data/valkey ;; kvrocks) rm -rf /data/kvrocks ;; lavik) rm -rf /data/lavik ;; esac ;;
  *) echo "usage" >&2; exit 2 ;;
esac
