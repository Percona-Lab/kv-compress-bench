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
# TidesKV (pinned commit) and runtime deps for Garnet (.NET 10) and investigation tools
TIDESKV_SHA=c598261c0b80efe8807669531010b0606996123a
DEBIAN_FRONTEND=noninteractive apt-get -y -q install dotnet-runtime-10.0 elfutils ninja-build liblz4-dev libzstd-dev >/dev/null || true
if [ ! -x /opt/tideskv/bin/tideskv-server ]; then
  (rm -rf tideskv && git clone -q https://github.com/EvgeniyPatlan/tideskv.git && cd tideskv && git checkout -q $TIDESKV_SHA \
   && cmake --preset release && cmake --build --preset release -j"$(nproc)" && mkdir -p /opt/tideskv/bin \
   && cp build/release/src/tideskv-server /opt/tideskv/bin/ && git rev-parse HEAD > /opt/tideskv/REVISION)
fi
DEBIAN_FRONTEND=noninteractive apt-get -y -q install dotnet-runtime-10.0 elfutils ninja-build liblz4-dev libzstd-dev >/dev/null || true
echo STORES-DONE
