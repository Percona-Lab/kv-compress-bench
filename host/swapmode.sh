#!/bin/bash
# Set the swap / compressed-memory mode for a test (same settings as Percona-Lab/zram-zswap-db-benchmark).
#   swapmode.sh none | disk | zswap[-lz4|-zstd] | zram[-lz4|-zstd] | show
#   none  - no swap at all
#   disk  - $SWAP_GB GB (default 64) swap file on the second SSD (/data), zswap off, default reclaim sysctls, swappiness 60
#   zswap - zswap (zsmalloc, max_pool_percent $ZSWAP_POOL_PCT, default 50) in front of the same swap file, swappiness 100
#   zram  - zram swap only (zram-zstdN / zram-zstd-N: zstd level N via algorithm_params; default level 3)
#           ($ZRAM_GB GB device, default 64), zswap off, swappiness 180
#   compressed modes also: vm.page-cluster 0, watermark_boost_factor 0, watermark_scale_factor 125 (Arch wiki values)
set -euo pipefail
SWF=/data/kvbench-swap.img
mode=${1:?mode}; base=${mode%%-*}; comp=${mode#*-}; [ "$comp" = "$mode" ] && comp=lz4

show() {
  echo "swap: $(swapon --noheadings --show=NAME,SIZE | tr '\n' ' ')"
  echo "zswap: enabled=$(cat /sys/module/zswap/parameters/enabled) compressor=$(cat /sys/module/zswap/parameters/compressor) max_pool_percent=$(cat /sys/module/zswap/parameters/max_pool_percent) shrinker=$(cat /sys/module/zswap/parameters/shrinker_enabled)"
  [ -e /sys/block/zram0/comp_algorithm ] && echo "zram0: $(grep -o '\[[a-z0-9-]*\]' /sys/block/zram0/comp_algorithm) disksize=$(cat /sys/block/zram0/disksize)"
  echo "sysctl: swappiness=$(sysctl -n vm.swappiness) page-cluster=$(sysctl -n vm.page-cluster) watermark_boost_factor=$(sysctl -n vm.watermark_boost_factor) watermark_scale_factor=$(sysctl -n vm.watermark_scale_factor)"
}
[ "$mode" = show ] && { show; exit 0; }

# reset
swapoff -a 2>/dev/null || true
if [ -e /dev/zram0 ]; then
  # the device can stay busy for a while after a large swapoff / process exit: retry instead of failing
  for i in $(seq 1 120); do
    swapoff /dev/zram0 2>/dev/null || true
    echo 1 > /sys/block/zram0/reset 2>/dev/null && break
    sleep 1
  done
  [ $i -lt 120 ] || { echo "zram0 still busy after 120 s" >&2; exit 1; }
fi
for i in $(seq 1 30); do rmmod zram 2>/dev/null && break; [ -e /sys/module/zram ] || break; sleep 1; done   # fresh module: no algorithm_params (zstd level) carried over between modes
echo N > /sys/module/zswap/parameters/enabled
if [ "$base" = disk ] || [ "$base" = none ]; then
  sysctl -q vm.page-cluster=3 vm.watermark_boost_factor=15000 vm.watermark_scale_factor=10
else
  sysctl -q vm.page-cluster=0 vm.watermark_boost_factor=0 vm.watermark_scale_factor=125
fi
SWAP_GB=${SWAP_GB:-64}
mkswapfile() {
  if [ -e $SWF ] && [ "$(stat -c %s $SWF)" != "$((SWAP_GB*1024*1024*1024))" ]; then rm -f $SWF; fi
  [ -e $SWF ] || { fallocate -l ${SWAP_GB}G $SWF; chmod 600 $SWF; mkswap -q $SWF; }
}
case $base in
  none)  sysctl -q vm.swappiness=60 ;;
  disk)  sysctl -q vm.swappiness=60; mkswapfile; swapon $SWF ;;
  zswap) [[ $comp =~ ^zstd-?[0-9]+$ ]] && { echo "zswap cannot set a zstd level" >&2; exit 2; }
         sysctl -q vm.swappiness=100; mkswapfile
         echo $comp > /sys/module/zswap/parameters/compressor
         echo zsmalloc > /sys/module/zswap/parameters/zpool 2>/dev/null || true   # zsmalloc is the only zpool on new kernels
         echo ${ZSWAP_POOL_PCT:-50} > /sys/module/zswap/parameters/max_pool_percent
         echo ${ZSWAP_SHRINKER:-Y} > /sys/module/zswap/parameters/shrinker_enabled   # Y = kernel default (writes cold pages back to disk)
         echo Y > /sys/module/zswap/parameters/enabled
         swapon $SWF ;;
  zram)  sysctl -q vm.swappiness=180; modprobe zram num_devices=1
         if [[ $comp =~ ^zstd(-?[0-9]+)$ ]]; then   # zram-zstd1, zram-zstd-1: zstd at that level (default 3)
           echo zstd > /sys/block/zram0/comp_algorithm
           echo "algo=zstd level=${BASH_REMATCH[1]}" > /sys/block/zram0/algorithm_params
         else
           echo $comp > /sys/block/zram0/comp_algorithm
         fi
         echo ${ZRAM_GB:-64}G > /sys/block/zram0/disksize
         mkswap -q /dev/zram0; swapon -p 100 /dev/zram0 ;;
  *) echo "unknown mode $mode" >&2; exit 2 ;;
esac
show
