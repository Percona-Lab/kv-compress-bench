#!/bin/bash
# Skip connection tracking for loopback traffic (client and server share the host; ufw's conntrack would
# otherwise add per-packet cost to both). Idempotent.
nft list table inet kvbench_raw >/dev/null 2>&1 && nft delete table inet kvbench_raw
nft -f - <<'N'
table inet kvbench_raw {
  chain pre { type filter hook prerouting priority raw; iif "lo" notrack; }
  chain out { type filter hook output priority raw; oif "lo" notrack; }
}
N
nft list table inet kvbench_raw
