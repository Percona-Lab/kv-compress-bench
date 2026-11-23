#!/bin/bash
# Build / install the stores and tools. Versions are pinned here.
set -euxo pipefail
VALKEY=9.2.0
KVROCKS=2.17.0
LAVIK=0.3.2
MEMTIER=2.6.1
mkdir -p /opt/src && cd /opt/src

# Valkey (source, default build options + jemalloc)
if [ ! -x /opt/valkey-$VALKEY/bin/valkey-server ]; then
  curl -fsSL https://github.com/valkey-io/valkey/archive/refs/tags/$VALKEY.tar.gz | tar xz
  make -C valkey-$VALKEY -j"$(nproc)"
  make -C valkey-$VALKEY PREFIX=/opt/valkey-$VALKEY install
fi

# Lavik (release binary)
if [ ! -d /opt/lavik-$LAVIK ]; then
  curl -fsSLO https://github.com/eloqdata/lavik/releases/download/v$LAVIK/lavik-v$LAVIK-linux-x86_64.tar.gz
  curl -fsSLO https://github.com/eloqdata/lavik/releases/download/v$LAVIK/lavik-v$LAVIK-linux-x86_64.tar.gz.sha256
  sha256sum -c lavik-v$LAVIK-linux-x86_64.tar.gz.sha256
  mkdir -p /opt/lavik-$LAVIK && tar xzf lavik-v$LAVIK-linux-x86_64.tar.gz -C /opt/lavik-$LAVIK --strip-components=1
fi

# memtier_benchmark (cross-check of kvbench numbers)
if ! command -v memtier_benchmark >/dev/null; then
  curl -fsSL https://github.com/RedisLabs/memtier_benchmark/archive/refs/tags/$MEMTIER.tar.gz | tar xz
  (cd memtier_benchmark-$MEMTIER && autoreconf -ivf && ./configure && make -j"$(nproc)" && make install)
fi

# Kvrocks (source; long build, fetches its own RocksDB etc.)
if [ ! -x /opt/kvrocks-$KVROCKS/bin/kvrocks ]; then
  curl -fsSL https://github.com/apache/kvrocks/archive/refs/tags/v$KVROCKS.tar.gz | tar xz
  (cd kvrocks-$KVROCKS && ./x.py build -j"$(nproc)" build && mkdir -p /opt/kvrocks-$KVROCKS/bin && cp build/kvrocks /opt/kvrocks-$KVROCKS/bin/)
fi

# Rust toolchain for kvbench
if [ ! -x /root/.cargo/bin/cargo ]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
echo STORES-DONE
