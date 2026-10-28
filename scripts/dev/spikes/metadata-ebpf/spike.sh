#!/bin/bash
# Spike 3: tc/eBPF at the VM's port delivers metadata requests from a
# bridge with no host address or route (like OVN or isolated networks).
set -u
D=$(dirname "$0"); BR=gxtest-bpf; MD=gxtest-md0; PIN=/sys/fs/bpf/tc/globals
cleanup() {
  kill $SRV 2>/dev/null
  ip netns del bpfvm1 2>/dev/null; ip netns del bpfvm2 2>/dev/null
  ovs-vsctl --if-exists del-br $BR
  ip link del $MD 2>/dev/null
  ip route del 169.254.192.0/18 dev $MD 2>/dev/null
  rm -f $PIN/md_index $PIN/md_peer $PIN/md_dev
  echo "cleaned up: bridge $(ovs-vsctl br-exists $BR && echo LEFT || echo gone), maps $(ls $PIN/md_* 2>/dev/null | wc -l) left"
}
trap cleanup EXIT
ovs-vsctl add-br $BR -- set bridge $BR datapath_type=system   # no host address
ip link add $MD type dummy; ip link set $MD up
ip route add 169.254.192.0/18 dev $MD
MDIDX=$(cat /sys/class/net/$MD/ifindex); MDMAC=$(cat /sys/class/net/$MD/address)
for n in 1 2; do
  ip netns add bpfvm$n
  ip link add bv$n-h type veth peer name bv$n-g
  ip link set bv$n-g netns bpfvm$n
  ip -n bpfvm$n addr add 10.251.0.1$n/24 dev bv$n-g; ip -n bpfvm$n link set bv$n-g up; ip -n bpfvm$n link set lo up
  # No gateway anywhere: 169.254.169.254 on-link, a neighbour entry instead of ARP.
  ip -n bpfvm$n route add 169.254.169.254/32 dev bv$n-g
  ip -n bpfvm$n neigh add 169.254.169.254 lladdr 02:00:00:00:a9:fe dev bv$n-g
  ip link set bv$n-h up; ovs-vsctl add-port $BR bv$n-h
done
# Port 1 and 2's programs; the maps are pinned by the first load.
for n in 1 2; do
  tc qdisc add dev bv$n-h clsact
  tc filter add dev bv$n-h ingress bpf da obj $D/md.o sec tc/ingress
done
tc qdisc add dev $MD clsact
tc filter add dev $MD egress bpf da obj $D/md.o sec tc/egress
hex() { printf '%08x' "$1" | sed 's/\(..\)\(..\)\(..\)\(..\)/\4 \3 \2 \1/'; }
mac() { echo "$1" | tr ':' ' '; }
bpftool map update pinned $PIN/md_dev key hex 00 00 00 00 value hex $(hex $MDIDX) $(mac $MDMAC) 00 00
for n in 1 2; do
  bpftool map update pinned $PIN/md_index key hex $(hex $(cat /sys/class/net/bv$n-h/ifindex)) value hex $(hex $n)
done
setpriv --reuid=65534 --regid=65534 --clear-groups python3 - <<'PY' &
import socket
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("169.254.169.254", 18775)); s.listen(8)
while True:
    c,a=s.accept(); c.recv(1024); c.sendall(f"HTTP/1.0 200 OK\r\n\r\npeer={a[0]}\n".encode()); c.close()
PY
SRV=$!; sleep 1
for n in 1 2; do echo "VM$n: $(ip netns exec bpfvm$n curl -s --max-time 3 http://169.254.169.254/)"; done
# VM 2 claims VM 1's address: the port, not the address, decides.
ip -n bpfvm2 addr add 10.251.0.11/32 dev bv2-g
echo "VM2 as .11: $(ip netns exec bpfvm2 curl -s --max-time 3 --interface 10.251.0.11 http://169.254.169.254/)"
# A port without an index: dropped.
bpftool map delete pinned $PIN/md_index key hex $(hex $(cat /sys/class/net/bv2-h/ifindex))
echo "VM2 unindexed: '$(ip netns exec bpfvm2 curl -s --max-time 3 http://169.254.169.254/)'"
# The VMs' traffic to each other still goes through OVS.
echo "VM1 -> VM2 ping: $(ip netns exec bpfvm1 ping -c1 -W1 10.251.0.12 >/dev/null && echo ok || echo FAIL)"
