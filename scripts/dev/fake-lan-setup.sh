#!/bin/bash
# Dev/test only: a fake LAN for the uplink e2e tests (bridged_uplink_e2e,
# afxdp_uplink_e2e). Never touches real NICs. Run as root; undo with
# fake-lan-teardown.sh.
#
#   gxup0 <-> gxlan0 (netns gxlan): 192.0.2.0/24, gateway 192.0.2.1,
#             host holds 192.0.2.10 + a route (an "in-use" NIC to migrate)
#   gxup1 <-> gxlan1 (netns gxlan): 198.18.0.0/24, gateway 198.18.0.1,
#             host side unaddressed (AF_XDP uplink)
#   dnsmasq in the netns serves DHCP on both.
#
# It also gives the installed glidex-netd a 10 s uplink commit window,
# which the bridged test's rollback check relies on (the previous
# /etc/glidex/netd.json, if any, is kept and restored by teardown).
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "run as root (sudo $0)" >&2; exit 1; }

STATE=/run/glidex-fake-lan
NETD_CONF=/etc/glidex/netd.json

"$(dirname "$0")/fake-lan-teardown.sh" --keep-netd-config >/dev/null 2>&1 || true
mkdir -p "$STATE"

ip netns add gxlan
ip -n gxlan link set lo up
for i in 0 1; do
  ip link add "gxup$i" type veth peer name "gxlan$i"
  ip link set "gxlan$i" netns gxlan
  ip link set "gxup$i" up
  ip -n gxlan link set "gxlan$i" up
  # veth checksum offload leaves AF_XDP (generic mode) frames with bad
  # checksums, so the VM drops the DHCP replies. Compute them in software.
  ip netns exec gxlan ethtool -K "gxlan$i" tx off >/dev/null
done
ip -n gxlan addr add 192.0.2.1/24 dev gxlan0
ip -n gxlan addr add 198.18.0.1/24 dev gxlan1

# The in-use NIC: a static address and a route the migration must carry over.
ip addr add 192.0.2.10/24 dev gxup0
ip route add 198.51.100.0/24 via 192.0.2.1 dev gxup0

for i in 0 1; do
  if [ "$i" = 0 ]; then range=192.0.2.100,192.0.2.199; else range=198.18.0.100,198.18.0.199; fi
  ip netns exec gxlan dnsmasq --interface="gxlan$i" --bind-interfaces --except-interface=lo \
    --dhcp-range="$range,1h" --port=0 --log-dhcp \
    --dhcp-leasefile="$STATE/gxlan$i.leases" --pid-file="$STATE/gxlan$i.pid" \
    --log-facility="$STATE/gxlan$i.log"
done

# Short commit window for the tests.
mkdir -p /etc/glidex
if [ -f "$NETD_CONF" ] && [ ! -f "$STATE/netd.json.orig" ] && [ ! -f "$STATE/netd.json.none" ]; then
  cp -p "$NETD_CONF" "$STATE/netd.json.orig"
fi
[ -f "$STATE/netd.json.orig" ] || touch "$STATE/netd.json.none"
cat > "$NETD_CONF" <<'JSON'
{ "commit_window_secs": 10, "gateway_check_secs": 10 }
JSON
if systemctl is-active -q glidex-netd; then
  systemctl restart glidex-netd
fi

echo "fake LAN ready:"
ip -br addr show gxup0
ip -br link show gxup1
echo "run the tests with GLIDEX_TEST_LAN=1"
