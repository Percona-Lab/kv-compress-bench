#!/bin/bash
# Garnet's native storage device (used by --storage-tier) looks for runtimes/linux-x64/native/libnative_device.so and needs
# liburing.so.2 + libaio.so.1; the release package and Ubuntu 26.04 (libaio1t64) lack these names. Idempotent.
GD=$(dirname "$1")
dpkg -s liburing2 >/dev/null 2>&1 && dpkg -s libaio1t64 >/dev/null 2>&1 || DEBIAN_FRONTEND=noninteractive apt-get -y -q install liburing2 libaio1t64 >/dev/null 2>&1
[ -e /usr/lib/x86_64-linux-gnu/libaio.so.1 ] || { ln -sf "$(ls /usr/lib/x86_64-linux-gnu/libaio.so.1t64* | head -1)" /usr/lib/x86_64-linux-gnu/libaio.so.1; ldconfig; }
mkdir -p "$GD/runtimes/linux-x64/native" && ln -sf ../../../libnative_device.so "$GD/runtimes/linux-x64/native/libnative_device.so"
