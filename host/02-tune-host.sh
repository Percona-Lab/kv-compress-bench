#!/bin/bash
# Host settings for kv-compress-bench (run once after the OS upgrade + reboot).
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get -y install build-essential cmake pkg-config git curl jq python3 python3-pip libssl-dev libevent-dev \
  autoconf automake libtool zlib1g-dev libpcre2-dev linux-tools-common linux-tools-$(uname -r) sysstat \
  zstd lz4 numactl xfsprogs util-linux

# second SSD -> /data (store data files, disk swap file)
if ! mountpoint -q /data; then
  DEV=/dev/sdb
  if ! blkid $DEV >/dev/null 2>&1; then mkfs.xfs -f $DEV; fi
  mkdir -p /data
  UUID=$(blkid -s UUID -o value $DEV)
  grep -q "$UUID" /etc/fstab || echo "UUID=$UUID /data xfs defaults,noatime 0 2" >> /etc/fstab
  mount /data
fi

# the image's default swap file is removed; swap is set per test mode by swapmode.sh
swapoff -a
sed -i '/swapfile/d' /etc/fstab
rm -f /swapfile

cat > /etc/sysctl.d/90-kvbench.conf <<'S'
vm.overcommit_memory = 1
net.core.somaxconn = 65535
net.ipv4.tcp_max_syn_backlog = 65535
net.ipv4.ip_local_port_range = 10000 65000
kernel.perf_event_paranoid = -1
S
sysctl --system >/dev/null

# stable clocks: performance governor, turbo off
cat > /etc/systemd/system/kvbench-cpu.service <<'S'
[Unit]
Description=kvbench CPU settings (performance governor, no turbo)
After=multi-user.target
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do echo performance > $g; done; [ -e /sys/devices/system/cpu/intel_pstate/no_turbo ] && echo 1 > /sys/devices/system/cpu/intel_pstate/no_turbo; true'
[Install]
WantedBy=multi-user.target
S
systemctl daemon-reload
systemctl enable --now kvbench-cpu.service
lscpu -e
