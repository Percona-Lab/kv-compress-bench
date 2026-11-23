#!/bin/bash
# Set the swap / compressed-memory mode for a test (same settings as Percona-Lab/zram-zswap-db-benchmark).
#   swapmode.sh none | disk | zswap[-lz4|-zstd] | zram[-lz4|-zstd] | show
#   none  - no swap at all
#   disk  - 64 GB swap file on the second SSD (/data), zswap off, default reclaim sysctls, swappiness 60
#   zswap - zswap (zsmalloc, max_pool_percent $ZSWAP_POOL_PCT, default 50) in front of the same swap file, swappiness 100
#   zram  - zram swap only ($ZRAM_GB GB device, default 64), zswap off, swappiness 180
#   compressed modes also: vm.page-cluster 0, watermark_boost_factor 0, watermark_scale_factor 125 (Arch wiki values)
set -euo pipefail
SWF=/data/kvbench-swap.img
mode=${1:?mode}; base=${mode%%-*}; comp=${mode#*-}; [ "$comp" = "$mode" ] && comp=lz4

show() {
  echo "swap: $(swapon --noheadings --show=NAME,SIZE | tr '\n' ' ')"
  echo "zswap: enabled=$(cat /sys/module/zswap/parameters/enabled) compressor=$(cat /sys/module/zswap/parameters/compressor) max_pool_percent=$(cat /sys/module/zswap/parameters/max_pool_percent)"
  [ -e /sys/block/zram0/comp_algorithm ] && echo "zram0: $(grep -o '\[[a-z0-9-]*\]' /sys/block/zram0/comp_algorithm) disksize=$(cat /sys/block/zram0/disksize)"
  echo "sysctl: swappiness=$(sysctl -n vm.swappiness) page-cluster=$(sysctl -n vm.page-cluster) watermark_boost_factor=$(sysctl -n vm.watermark_boost_factor) watermark_scale_factor=$(sysctl -n vm.watermark_scale_factor)"
}
[ "$mode" = show ] && { show; exit 0; }

# reset
swapoff -a 2>/dev/null || true
if [ -e /dev/zram0 ]; then swapoff /dev/zram0 2>/dev/null || true; echo 1 > /sys/block/zram0/reset; fi
echo N > /sys/module/zswap/parameters/enabled
if [ "$base" = disk ] || [ "$base" = none ]; then
  sysctl -q vm.page-cluster=3 vm.watermark_boost_factor=15000 vm.watermark_scale_factor=10
else
  sysctl -q vm.page-cluster=0 vm.watermark_boost_factor=0 vm.watermark_scale_factor=125
fi
mkswapfile() { [ -e $SWF ] || { fallocate -l 64G $SWF; chmod 600 $SWF; mkswap -q $SWF; }; }
case $base in
  none)  sysctl -q vm.swappiness=60 ;;
  disk)  sysctl -q vm.swappiness=60; mkswapfile; swapon $SWF ;;
  zswap) sysctl -q vm.swappiness=100; mkswapfile
         echo $comp > /sys/module/zswap/parameters/compressor
         echo zsmalloc > /sys/module/zswap/parameters/zpool 2>/dev/null || true   # zsmalloc is the only zpool on new kernels
         echo ${ZSWAP_POOL_PCT:-50} > /sys/module/zswap/parameters/max_pool_percent
         echo Y > /sys/module/zswap/parameters/enabled
         swapon $SWF ;;
  zram)  sysctl -q vm.swappiness=180; modprobe zram num_devices=1
         echo $comp > /sys/block/zram0/comp_algorithm
         echo ${ZRAM_GB:-64}G > /sys/block/zram0/disksize
         mkswap -q /dev/zram0; swapon -p 100 /dev/zram0 ;;
  *) echo "unknown mode $mode" >&2; exit 2 ;;
esac
show
