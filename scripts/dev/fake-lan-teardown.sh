#!/bin/bash
# Undo fake-lan-setup.sh: stop its dnsmasq, remove the netns, veths and
# route, and restore glidex-netd's previous config. Run as root.
# Tear down the uplinks/bridges first (the tests clean up after themselves;
# `gxctl uplink-rm` otherwise) so netd doesn't keep state about gxup0/1.
set -u
[ "$(id -u)" = 0 ] || { echo "run as root (sudo $0)" >&2; exit 1; }

STATE=/run/glidex-fake-lan
NETD_CONF=/etc/glidex/netd.json

for i in 0 1; do
  pidf="$STATE/gxlan$i.pid"
  [ -f "$pidf" ] && kill "$(cat "$pidf")" 2>/dev/null
done
ip route del 198.51.100.0/24 2>/dev/null
ip link del gxup0 2>/dev/null
ip link del gxup1 2>/dev/null
ip netns del gxlan 2>/dev/null

if [ "${1:-}" != "--keep-netd-config" ]; then
  if [ -f "$STATE/netd.json.orig" ]; then
    cp -p "$STATE/netd.json.orig" "$NETD_CONF"
  elif [ -f "$STATE/netd.json.none" ]; then
    rm -f "$NETD_CONF"
    rmdir /etc/glidex 2>/dev/null
  fi
  rm -rf "$STATE"
  if systemctl is-active -q glidex-netd; then
    systemctl restart glidex-netd
  fi
fi
echo "fake LAN removed"
