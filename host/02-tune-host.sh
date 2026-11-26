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
  ROOTDEV=$(lsblk -no PKNAME $(findmnt -no SOURCE /) | head -1)
  DEV=/dev/$(lsblk -dno NAME,TYPE | awk '$2=="disk"{print $1}' | grep -v "^$ROOTDEV$" | grep -v "^zram" | head -1)
  echo "data disk: $DEV (root on $ROOTDEV)"
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

# the Intel RDMA driver (loaded for ice / i40e NICs) reserves ~5.6 GB of RAM; nothing here uses RDMA
cp "$(dirname "$0")/kvbench-no-irdma.conf" /etc/modprobe.d/ && update-initramfs -u >/dev/null 2>&1 || true
rmmod irdma 2>/dev/null || true

# stable clocks: performance governor, turbo off
cat > /etc/systemd/system/kvbench-cpu.service <<'S'
[Unit]
Description=kvbench CPU settings (performance governor, no turbo / boost)
After=multi-user.target
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do echo performance > $g; done; [ -e /sys/devices/system/cpu/intel_pstate/no_turbo ] && echo 1 > /sys/devices/system/cpu/intel_pstate/no_turbo; [ -e /sys/devices/system/cpu/cpufreq/boost ] && echo 0 > /sys/devices/system/cpu/cpufreq/boost; true'
[Install]
WantedBy=multi-user.target
S
systemctl daemon-reload
systemctl enable --now kvbench-cpu.service
lscpu -e
