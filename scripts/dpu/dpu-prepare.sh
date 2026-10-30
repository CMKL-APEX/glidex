#!/bin/bash
# D0: bring a DPU's runtime OVS-DOCA configuration to the state in
# spec/31-dpu.md §9.3, as idempotent shell. This is the oracle `dpu_prepare`
# (D5) is later checked against; it is not what ships.
#
#   dpu-prepare.sh --ssh <user@dpu> --check                    # what would change (exit 3 if anything)
#   dpu-prepare.sh --ssh <user@dpu> --dry-run                  # same, with the commands
#   dpu-prepare.sh --ssh <user@dpu> --yes [options]            # apply
#   dpu-prepare.sh --local --yes [options]                     # on the DPU itself
#
# Options
#   --hugepages-2m N         2 MB pages to reserve (default 1024 = 2 GB)
#   --pmd-cpu-mask MASK      PMD cores (default 0xc0: cores 6 and 7)
#   --phy-bridge NAME        the uplink/underlay bridge (default ovsbr1); must exist
#   --int-bridge NAME        also create/keep the OVN integration bridge on the
#                            doca datapath (ovn-controller would otherwise
#                            recreate it on the kernel datapath)
#   --underlay-cidr A/LEN    re-apply the underlay address on the phy bridge
#   --underlay-gw ADDR       and the default route through it
#   --persist                write the tmpfiles.d + fstab entries for hugepages
#   --ovn-remote REMOTE      chassis: ovn-remote (ssl:... for a real central)
#   --ovn-encap-ip ADDR      chassis: ovn-encap-ip (geneve)
#   --ovn-bridge-mappings M  chassis: ovn-bridge-mappings (e.g. externalnet:ovsbr1)
#   --ssl-key/--ssl-cert/--ssl-ca PATH   OVS TLS files already on the DPU
#   --stop-local-central     stop+disable a DPU-local OVN central (never uninstalls)
#   --virtio-net             make the DPU ready for virtio-net-controller (needs
#                            the firmware values active): remove the stock SF 0
#                            from the bridge and the DPU, keep /etc/mellanox/
#                            mlnx-sf.conf from recreating it, and restart a
#                            controller that cannot serve (spec §0 finding 29)
#
# What it deliberately does not do: firmware (mlxconfig), resets, anything
# on the host. Restarting ovs-vswitchd drops the phy bridge's internal-port
# address (spec §0 finding 6): reach the DPU over the management link, not
# the underlay, or the restart cuts the session. --yes is required to
# apply because of that restart.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$here/lib.sh"

HUGE=1024; PMD=0xc0; PHY=ovsbr1; INT=""; UCIDR=""; UGW=""; PERSIST=0; YES=0
OVN_REMOTE=""; ENCAP=""; BMAP=""; SSL_KEY=""; SSL_CERT=""; SSL_CA=""; STOP_CENTRAL=0; VIRTIO=0
IP2=/opt/mellanox/iproute2/sbin; SF_CONF=/etc/mellanox/mlnx-sf.conf; STOCK_SF_NETDEV=""
HP_2M=/sys/kernel/mm/hugepages/hugepages-2048kB
HP_512M=/sys/kernel/mm/hugepages/hugepages-524288kB
HP_DIR=/hugepages-2M

parse_common "$@"; set -- "${ARGS[@]}"
while [ $# -gt 0 ]; do
  case "$1" in
    --hugepages-2m)         HUGE="${2:?}"; shift 2 ;;
    --pmd-cpu-mask)         PMD="${2:?}"; shift 2 ;;
    --phy-bridge)           PHY="${2:?}"; shift 2 ;;
    --int-bridge)           INT="${2:?}"; shift 2 ;;
    --underlay-cidr)        UCIDR="${2:?}"; shift 2 ;;
    --underlay-gw)          UGW="${2:?}"; shift 2 ;;
    --persist)              PERSIST=1; shift ;;
    --ovn-remote)           OVN_REMOTE="${2:?}"; shift 2 ;;
    --ovn-encap-ip)         ENCAP="${2:?}"; shift 2 ;;
    --ovn-bridge-mappings)  BMAP="${2:?}"; shift 2 ;;
    --ssl-key)              SSL_KEY="${2:?}"; shift 2 ;;
    --ssl-cert)             SSL_CERT="${2:?}"; shift 2 ;;
    --ssl-ca)               SSL_CA="${2:?}"; shift 2 ;;
    --stop-local-central)   STOP_CENTRAL=1; shift ;;
    --virtio-net)           VIRTIO=1; shift ;;
    --yes)                  YES=1; shift ;;
    -h|--help)              sed -n '2,42p' "$0"; exit 0 ;;
    *)                      die "unknown option: $1" ;;
  esac
done
require_target
[[ "$HUGE" =~ ^[0-9]+$ ]] || die "--hugepages-2m must be a number"
[[ "$PMD" =~ ^0x[0-9a-fA-F]+$ ]] || die "--pmd-cpu-mask must look like 0xc0"
[[ "$PHY" =~ ^[a-z0-9-]{1,15}$ ]] || die "--phy-bridge: name must be [a-z0-9-], at most 15 characters"
[ -z "$INT" ] || [[ "$INT" =~ ^[a-z0-9-]{1,15}$ ]] || die "--int-bridge: name must be [a-z0-9-], at most 15 characters"
if [ -n "$SSL_KEY$SSL_CERT$SSL_CA" ]; then
  [ -n "$SSL_KEY" ] && [ -n "$SSL_CERT" ] && [ -n "$SSL_CA" ] || die "give all of --ssl-key, --ssl-cert and --ssl-ca"
fi
if [ "$DRY_RUN" = 0 ] && [ "$YES" != 1 ]; then
  die "applying restarts ovs-vswitchd and drops the underlay address; re-run with --yes (or use --check / --dry-run)"
fi

oc_get() { rd ovs-vsctl --if-exists get Open_vSwitch . "other_config:$1" | tr -d '"'; }
ovs_get() { rd ovs-vsctl --if-exists get "$@" | tr -d '"'; }

# ---------------------------------------------------------------- 1. hugepages
step "1. hugepages (2 MB pool, dedicated mount)"
cur2m="$(rd cat "$HP_2M/nr_hugepages")"; cur2m="${cur2m:-0}"
if [ "$cur2m" -ge "$HUGE" ]; then ok "2 MB pool: $cur2m pages (wanted $HUGE)"
else mut_sh "reserve $HUGE x 2 MB pages (have $cur2m)" "echo $HUGE > $HP_2M/nr_hugepages"; fi

if [ -n "$(rd findmnt -n "$HP_DIR")" ]; then ok "$HP_DIR is mounted"
else mut_sh "mount a 2 MB hugetlbfs at $HP_DIR" "mkdir -p $HP_DIR && mount -t hugetlbfs -o pagesize=2M none $HP_DIR"; fi

t512="$(rd cat "$HP_512M/nr_hugepages")"; f512="$(rd cat "$HP_512M/free_hugepages")"
if [ "${t512:-0}" = 0 ]; then ok "512 MB pool is empty"
elif [ "${t512:-0}" = "${f512:-x}" ]; then mut_sh "release the unused 512 MB pool (${t512} pages)" "echo 0 > $HP_512M/nr_hugepages"
else warn "512 MB pages in use (total ${t512}, free ${f512}); leaving them"; fi

if [ "$PERSIST" = 1 ]; then
  if [ "$(rd_sh 'cat /etc/tmpfiles.d/ovs-doca-hugepages.conf 2>/dev/null | grep -c nr_hugepages')" = 2 ]; then ok "tmpfiles entry present"
  else mut_sh "persist the pools across reboots" "printf '%s\n' '# OVS-DOCA hugepages' 'w $HP_512M/nr_hugepages - - - - 0' 'w $HP_2M/nr_hugepages - - - - $HUGE' > /etc/tmpfiles.d/ovs-doca-hugepages.conf"; fi
  if [ "$(rd_sh "grep -c ' $HP_DIR ' /etc/fstab")" -ge 1 ] 2>/dev/null; then ok "fstab entry present"
  else mut_sh "persist the mount" "echo 'none $HP_DIR hugetlbfs pagesize=2M 0 0' >> /etc/fstab"; fi
else skip "persistence (pass --persist)"; fi

# ------------------------------------------------------- 2. OVS other_config
step "2. ovs other_config (doca-init, hw-offload, hugepage dir, PMD cores)"
need_restart=0
setcfg() {
  local k="$1" v="$2" cur
  cur="$(oc_get "$k")"
  if [ "$cur" = "$v" ]; then ok "$k=$v"; else
    mut "set $k=$v (was '${cur:-unset}')" ovs-vsctl set Open_vSwitch . "other_config:$k=$v"
    need_restart=1
  fi
}
setcfg doca-init true
setcfg hw-offload true
setcfg dpdk-hugepage-dir "$HP_DIR"
setcfg pmd-cpu-mask "$PMD"

step "3. restart ovs-vswitchd if the above changed"
if [ "$need_restart" = 1 ]; then
  mut "restart ovs-vswitchd (the phy bridge's internal-port address is lost)" systemctl restart ovs-vswitchd
  if [ "$DRY_RUN" = 0 ]; then sleep 8; fi
else ok "no restart needed"; fi

# ------------------------------------------------- 3b. virtio-net controller
step "3b. virtio-net-controller prerequisites"
if [ "$VIRTIO" = 1 ]; then
  fw="$(rd mlxconfig -d 03:00.0 q VIRTIO_NET_EMULATION_ENABLE | awk '$1=="VIRTIO_NET_EMULATION_ENABLE"{print $2}')"
  case "$fw" in
    True*) ok "VIRTIO_NET_EMULATION_ENABLE is active" ;;
    *) warn "VIRTIO_NET_EMULATION_ENABLE is '${fw:-unreadable}': firmware is not active (§9.1); skipping" ; VIRTIO=0 ;;
  esac
fi
if [ "$VIRTIO" = 1 ]; then
  # The controller creates its own SFs (sf_num 1000+) and fails to start
  # (`Failed to init IB port, err(-2)`) while the image's SF 0 exists.
  sf_line="$(rd "$IP2/mlxdevm" port show | awk '/flavour pcisf/ && / sfnum 0 /{print; exit}')"
  if [ -n "$sf_line" ]; then
    sf_port="${sf_line%%: *}"                                   # pci/0000:03:00.0/229408
    STOCK_SF_NETDEV="$(printf '%s\n' "$sf_line" | sed -n 's/.* netdev \([^ ]*\) .*/\1/p')"
    sf_dev="${sf_port#pci/}"; sf_dev="${sf_dev%/*}"
    [ -z "$STOCK_SF_NETDEV" ] || mut "remove the stock SF's netdev $STOCK_SF_NETDEV from its bridge" ovs-vsctl --if-exists del-port "$STOCK_SF_NETDEV"
    mut "delete the stock SF 0 ($sf_port)" env "PATH=$IP2:/usr/sbin:/usr/bin:/sbin:/bin" mlnx-sf --action delete --device "$sf_dev" --sfindex "$sf_port"
  else ok "no stock SF 0"; fi
  if [ -n "$(rd_sh "grep -s -E '^[^#]*--sfnum 0( |\$)' $SF_CONF")" ]; then
    mut_sh "keep $SF_CONF from recreating SF 0 at boot (backup: $SF_CONF.glidex-bak)" \
      "cp -n $SF_CONF $SF_CONF.glidex-bak && sed -i '/^[^#]*--sfnum 0\( \|\$\)/s/^/# glidex-disabled: /' $SF_CONF"
  else ok "$SF_CONF does not create SF 0"; fi
  if rd virtnet list | grep -q '"controller"'; then ok "virtnet list answers"
  else
    mut "restart virtio-net-controller (virtnet list does not answer)" systemctl restart virtio-net-controller
    if [ "$DRY_RUN" = 0 ]; then sleep 8; fi
  fi
else skip "no --virtio-net"; fi

# ------------------------------------------------------------- 4. bridges/ports
step "4. bridges and ports on the doca datapath"
[ "$(rd_sh "ovs-vsctl br-exists $PHY && echo yes")" = yes ] || die "bridge $PHY does not exist on the DPU (give --phy-bridge)"
bridge_doca() {
  local b="$1" dp
  dp="$(ovs_get bridge "$b" datapath_type)"
  if [ "$dp" = doca ]; then ok "$b datapath_type=doca"
  else mut "set $b datapath_type=doca (was '${dp:-system}')" ovs-vsctl set bridge "$b" datapath_type=doca; fi
}
bridge_doca "$PHY"
for p in $(rd ovs-vsctl list-ports "$PHY"); do
  [ -n "$STOCK_SF_NETDEV" ] && [ "$p" = "$STOCK_SF_NETDEV" ] && { skip "port $p (stock SF, removed in 3b)"; continue; }
  t="$(ovs_get Interface "$p" type)"
  case "$t" in
    doca) ok "port $p type=doca" ;;
    ""|dpdk)
      mut "port $p: type=${t:-system} -> doca" ovs-vsctl set Interface "$p" type=doca ;;
    internal|patch|geneve|vxlan|gre|stt|tap|system) skip "port $p type=$t (not a physical/representor port)" ;;
    *) warn "port $p has unexpected type '$t'; left alone" ;;
  esac
  if [ -n "$(rd ovs-vsctl --if-exists get Interface "$p" options:dpdk-devargs)" ]; then
    mut "port $p: drop the dpdk-devargs option (OVS-DOCA derives it)" ovs-vsctl remove Interface "$p" options dpdk-devargs
  fi
done
if [ -n "$INT" ]; then
  if [ "$(rd_sh "ovs-vsctl br-exists $INT && echo yes")" = yes ]; then bridge_doca "$INT"
  else mut "create $INT on the doca datapath, fail_mode=secure" ovs-vsctl --may-exist add-br "$INT" -- set bridge "$INT" datapath_type=doca fail_mode=secure; fi
fi

# ---------------------------------------------------------------- 5. underlay
step "5. underlay address and route on $PHY"
if [ -n "$UCIDR" ]; then
  if [ -n "$(rd_sh "ip -4 -o addr show dev $PHY | grep -F ' ${UCIDR%/*}/'")" ]; then ok "$PHY has the underlay address"
  else
    mut "bring $PHY up" ip link set "$PHY" up
    mut "add the underlay address to $PHY" ip addr add "$UCIDR" dev "$PHY"
  fi
  if [ -n "$UGW" ]; then
    if [ -n "$(rd_sh "ip -4 route show default | grep -F 'via $UGW dev $PHY'")" ]; then ok "default route via the gateway on $PHY"
    else mut "default route via the gateway on $PHY" ip route replace default via "$UGW" dev "$PHY"; fi
  fi
  warn "the address and route are runtime-only; persisting them is the operator's (netplan on the DPU)"
else skip "no --underlay-cidr (the restart above may have dropped the address)"; fi

# ------------------------------------------------------------- 6. OVN chassis
step "6. OVN chassis identity"
if [ -n "$OVN_REMOTE$ENCAP$BMAP" ]; then
  setid() {
    local k="$1" v="$2" cur
    [ -n "$v" ] || return 0
    cur="$(ovs_get Open_vSwitch . "external_ids:$k")"
    if [ "$cur" = "$v" ]; then ok "$k"; else mut "set external_ids:$k" ovs-vsctl set Open_vSwitch . "external_ids:$k=\"$v\""; fi
  }
  setid ovn-remote "$OVN_REMOTE"
  setid ovn-encap-type geneve
  setid ovn-encap-ip "$ENCAP"
  setid ovn-bridge-mappings "$BMAP"
else skip "no --ovn-* options"; fi
if [ -n "$SSL_KEY" ]; then
  want="Private key: $SSL_KEY"
  if [ "$(rd ovs-vsctl get-ssl | head -1)" = "$want" ]; then ok "OVS TLS files already set"
  else mut "point OVS at the TLS key, certificate and CA" ovs-vsctl set-ssl "$SSL_KEY" "$SSL_CERT" "$SSL_CA"; fi
fi
if [ "$STOP_CENTRAL" = 1 ]; then
  for u in ovn-central ovn-northd ovn-ovsdb-server-nb ovn-ovsdb-server-sb; do
    if [ "$(rd systemctl is-active "$u")" = active ]; then mut "stop $u" systemctl stop "$u"; else ok "$u not running"; fi
    case "$(rd systemctl is-enabled "$u")" in enabled) mut "disable $u" systemctl disable "$u" ;; *) : ;; esac
  done
else
  [ "$(rd systemctl is-active ovn-central)" = active ] && warn "a DPU-local ovn-central is running (pass --stop-local-central to stop it)"
fi

# ------------------------------------------------------------------ 7. verify
step "7. verification"
if [ "$DRY_RUN" = 1 ]; then skip "(dry run)"; finish; fi
rc=0
[ "$(ovs_get Open_vSwitch . doca_initialized)" = true ] && ok "doca_initialized=true (version $(ovs_get Open_vSwitch . doca_version))" || { warn "doca_initialized is not true"; rc=1; }
dp="$(rd ovs-appctl dpif/show)"
printf '%s\n' "$dp" | grep -q '^doca@' && ok "datapath doca@ovs-doca is up" || { warn "no doca datapath in dpif/show"; rc=1; }
for b in "$PHY" ${INT:+"$INT"}; do
  printf '%s\n' "$dp" | grep -q "^  $b:" && ok "$b is on the doca datapath" || { warn "$b not listed under dpif/show"; rc=1; }
done
errs="$(rd ovs-vsctl --columns=name,error list Interface | awk '/^error/ && $3!="[]"{c++} END{print c+0}')"
[ "$errs" = 0 ] && ok "no interface reports an error" || { warn "$errs interface(s) report an error"; rc=1; }
if [ "$VIRTIO" = 1 ]; then
  pfs="$(rd virtnet list | grep -c '"function_type": *"static PF"')"
  [ "$pfs" -ge 1 ] && ok "virtnet list answers: $pfs static PF(s)" || { warn "virtnet list shows no static PF"; rc=1; }
fi
log "pmd threads: $(rd ovs-appctl dpif-netdev/pmd-stats-show | grep -c '^pmd thread')"
log "(re-run with --check: it must report 0 changes)"
CHANGED_SUMMARY=$CHANGED
log "changes made: $CHANGED_SUMMARY"
exit "$rc"
