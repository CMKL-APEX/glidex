#!/bin/bash
# D0: collect the spec/31-dpu.md §9.2 capability fields from a DPU (and, with
# --host, the host side) as one JSON document on stdout. Read-only.
#
#   dpu-probe.sh --ssh <user@dpu> [--host] [--fixtures DIR] [--rshim rshim0]
#   dpu-probe.sh --local            # run on the DPU itself
#
# --fixtures DIR also writes every raw command output, scrubbed of
# addresses, MACs, identifiers and host names, one file per command: the
# canned inputs for the glidex-dpu probe-parsing tests (D1). Review the
# files before committing them.
#
# Fields that cannot be read are null; the script never fails because a
# tool is missing, since "not installed" is itself a finding.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$here/lib.sh"
need jq

WITH_HOST=0
FIXTURES=""
RSHIM="rshim0"
parse_common "$@"; set -- "${ARGS[@]}"
while [ $# -gt 0 ]; do
  case "$1" in
    --host)      WITH_HOST=1; shift ;;
    --fixtures)  FIXTURES="${2:?--fixtures needs a directory}"; shift 2 ;;
    --rshim)     RSHIM="${2:?--rshim needs a device name}"; shift 2 ;;
    -h|--help)   sed -n '2,15p' "$0"; exit 0 ;;
    *)           die "unknown option: $1" ;;
  esac
done
require_target
[ -z "$FIXTURES" ] || { umask 077; mkdir -p "$FIXTURES"; }

HOSTNAMES=()
[ -n "$DPU_SSH" ] && HOSTNAMES+=("${DPU_SSH#*@}")
HOSTNAMES+=("$(hostname 2>/dev/null || true)")

# cap NAME  -> runs the command that follows on the DPU, keeps the output
# in $OUT_<NAME> and (if asked) as a scrubbed fixture file.
cap() {
  local name="$1"; shift
  local out
  out="$("$@" 2>/dev/null || true)"
  printf -v "OUT_$name" '%s' "$out"
  if [ -n "$FIXTURES" ]; then
    printf '%s\n' "$out" | scrub "${HOSTNAMES[@]}" >"$FIXTURES/dpu-$name.txt"
  fi
}
cap_sh() {
  local name="$1" snippet="$2" out
  out="$(dpu_sh "$snippet" 2>/dev/null || true)"
  printf -v "OUT_$name" '%s' "$out"
  if [ -n "$FIXTURES" ]; then
    printf '%s\n' "$out" | scrub "${HOSTNAMES[@]}" >"$FIXTURES/dpu-$name.txt"
  fi
}
jstr() { if [ -n "$1" ]; then jq -n --arg v "$1" '$v'; else echo null; fi; }

cap ovs_version     dpu ovs-vsctl --version
cap other_config    dpu ovs-vsctl --if-exists get Open_vSwitch . other_config
cap doca_init       dpu ovs-vsctl --if-exists get Open_vSwitch . doca_initialized
cap doca_ver        dpu ovs-vsctl --if-exists get Open_vSwitch . doca_version
cap iface_types     dpu ovs-vsctl --if-exists get Open_vSwitch . iface_types
cap datapath_types  dpu ovs-vsctl --if-exists get Open_vSwitch . datapath_types
cap external_ids    dpu ovs-vsctl --if-exists get Open_vSwitch . external_ids
cap bridges         dpu ovs-vsctl --columns=name,datapath_type,fail_mode list Bridge
cap interfaces      dpu ovs-vsctl --columns=name,type,error,options list Interface
cap dpif_show       dpu ovs-appctl dpif/show
cap pmd_stats       dpu ovs-appctl dpif-netdev/pmd-stats-show
cap_sh hugepages    'for d in /sys/kernel/mm/hugepages/hugepages-*; do echo "$(basename $d) total=$(cat $d/nr_hugepages) free=$(cat $d/free_hugepages)"; done'
cap_sh hugetlbfs    'mount -t hugetlbfs'
cap_sh eswitch      'for d in $(devlink dev show 2>/dev/null | grep -E "^pci/" | head -1); do devlink dev eswitch show $d; done'
cap mlxconfig       dpu mlxconfig -d /dev/mst/mt41692_pciconf0 q VIRTIO_NET_EMULATION_ENABLE VIRTIO_NET_EMULATION_NUM_PF VIRTIO_NET_EMULATION_NUM_VF VIRTIO_NET_EMULATION_NUM_MSIX VIRTIO_NET_EMULATION_NUM_VF_MSIX PCI_SWITCH_EMULATION_ENABLE PER_PF_NUM_SF PF_TOTAL_SF PF_SF_BAR_SIZE SRIOV_EN NUM_OF_VFS INTERNAL_CPU_MODEL
cap virtnet_list    dpu virtnet list
cap_sh virtnet_svc  'systemctl is-active virtio-net-controller; systemctl is-enabled virtio-net-controller'
cap ovn_ctl_ver     dpu ovn-controller --version
cap_sh ovn_central  'systemctl is-active ovn-central ovn-northd ovn-ovsdb-server-nb ovn-ovsdb-server-sb ovn-controller'
cap bfver           dpu bfver
cap bfb_version     dpu cat /etc/bfb_version.json
cap_sh os_release   '. /etc/os-release && echo "$PRETTY_NAME"; uname -r'
cap_sh nvme_boot    'findmnt -no SOURCE /; lsblk -dno NAME,SIZE,TYPE'

# --- derive the §9.2 fields -------------------------------------------------
kv() { printf '%s\n' "$1" | sed -nE "s/.*[{\", ]$2=\"?([^\",}]*)\"?.*/\1/p" | head -1; }

bridges_json=$(printf '%s\n' "$OUT_bridges" | awk '
  /^name/ {n=$3; gsub(/"/,"",n)} /^datapath_type/ {d=$3; gsub(/"/,"",d)} /^fail_mode/ {f=$3; gsub(/"/,"",f); printf "%s %s %s\n", n, d, f}' |
  jq -R -s -c 'split("\n")|map(select(length>0)|split(" ")|{name:.[0],datapath_type:.[1],fail_mode:(if .[2]=="[]" then null else (.[2]//null) end)})')
[ -n "$bridges_json" ] || bridges_json='[]'

pmd_threads=$(printf '%s\n' "$OUT_pmd_stats" | grep -c '^pmd thread' || true)
offloaded=$(printf '%s\n' "$OUT_dpif_show" | sed -nE 's/.*offloaded flows: ([0-9]+).*/\1/p' | paste -sd+ - | bc 2>/dev/null || true)
eswitch_mode=$(printf '%s\n' "$OUT_eswitch" | sed -nE 's/.* mode ([a-z]+) .*/\1/p' | head -1)
doca_init=$(printf '%s' "$OUT_doca_init" | tr -d '"')
types_have_doca=false; printf '%s' "$OUT_iface_types" | grep -qw doca && types_have_doca=true
iface_dpdk_listed=false; printf '%s' "$OUT_iface_types" | grep -qw dpdk && iface_dpdk_listed=true
virtio_active=$(printf '%s\n' "$OUT_virtnet_svc" | sed -n 1p)

fw_json=$(printf '%s\n' "$OUT_mlxconfig" | awk '/^ +[A-Z_]+ +/ {gsub(/\(.*\)/,"",$2); printf "%s %s\n", $1, $2}' |
  jq -R -s -c 'split("\n")|map(select(length>0)|split(" ")|{(.[0]):.[1]})|add // {}')

central_json=$(printf '%s\n' "$OUT_ovn_central" | awk '{print}' | paste -sd, - | jq -R -c 'split(",")')

# --- host side ---------------------------------------------------------------
host_json=null
if [ "$WITH_HOST" = 1 ]; then
  misc="$( (cat "/dev/$RSHIM/misc" 2>/dev/null || sudo -n cat "/dev/$RSHIM/misc" 2>/dev/null) || true)"
  [ -z "$FIXTURES" ] || printf '%s\n' "$misc" | scrub "${HOSTNAMES[@]}" >"$FIXTURES/host-rshim-misc.txt"
  mods="$(lsmod 2>/dev/null | awk '{print $1}' | grep -E '^(vfio|vfio_pci|vfio_iommu_type1|iommufd|vdpa|vhost_vdpa|mlx5_vdpa|virtio_vdpa|virtio_vfio_pci|vhost_net)$' | paste -sd, - || true)"
  groups=$(ls /sys/kernel/iommu_groups 2>/dev/null | wc -l)
  cmdline="$(tr ' ' '\n' </proc/cmdline | grep -E '^(iommu|intel_iommu|amd_iommu|pci)=' | paste -sd' ' - || true)"
  host_json=$(jq -n \
    --arg bf_mode "$(printf '%s\n' "$misc" | sed -nE 's/^BF_MODE +//p' | head -1)" \
    --arg boot_mode "$(printf '%s\n' "$misc" | sed -nE 's/^BOOT_MODE +//p' | head -1)" \
    --arg dev_info "$(printf '%s\n' "$misc" | sed -nE 's/^DEV_INFO +//p' | head -1)" \
    --arg rshim_active "$(systemctl is-active rshim 2>/dev/null || true)" \
    --arg mods "$mods" --arg cmdline "$cmdline" --argjson groups "$groups" \
    --arg kernel "$(uname -r)" \
    --arg bfb_install "$(command -v bfb-install || true)" \
    'def nz: if . == "" then null else . end;
     {rshim:{device:"'"$RSHIM"'",service:$rshim_active,bf_mode:$bf_mode,boot_mode:$boot_mode,dev_info:$dev_info},
      modules:($mods|split(",")|map(select(length>0))), iommu_groups:$groups,
      cmdline:$cmdline, kernel:$kernel, bfb_install:($bfb_install|nz)}')
fi

jq -n \
  --argjson host "$host_json" \
  --argjson bridges "$bridges_json" \
  --argjson fw "$fw_json" \
  --argjson central "$central_json" \
  --argjson doca_listed "$types_have_doca" \
  --argjson dpdk_listed "$iface_dpdk_listed" \
  --arg ovs_version "$(printf '%s\n' "$OUT_ovs_version" | sed -nE 's/.*\) ([0-9.]+).*/\1/p' | head -1)" \
  --arg doca_initialized "$doca_init" \
  --arg doca_version "$(printf '%s' "$OUT_doca_ver" | tr -d '"' | sed 's/^\[\]$//')" \
  --arg dpdk_hugepage_dir "$(kv "$OUT_other_config" dpdk-hugepage-dir)" \
  --arg pmd_cpu_mask "$(kv "$OUT_other_config" pmd-cpu-mask)" \
  --arg hw_offload "$(kv "$OUT_other_config" hw-offload)" \
  --arg doca_init_cfg "$(kv "$OUT_other_config" doca-init)" \
  --argjson pmd_threads "${pmd_threads:-0}" \
  --arg offloaded "${offloaded:-}" \
  --arg eswitch_mode "$eswitch_mode" \
  --arg virtio_ctl "$virtio_active" \
  --arg ovn_version "$(printf '%s\n' "$OUT_ovn_ctl_ver" | sed -nE 's/^ovn-controller +//p' | head -1)" \
  --arg bundle "$(printf '%s\n' "$OUT_bfver" | head -3 | paste -sd' ' -)" \
  --arg os "$(printf '%s\n' "$OUT_os_release" | paste -sd' ' -)" \
  --arg hugepages "$OUT_hugepages" \
  'def nz: if . == "" then null else . end;
   {
     ovs_version: ($ovs_version|nz),
     doca_initialized: ($doca_initialized == "true"),
     doca_version: ($doca_version|nz),
     iface_types_has_doca: $doca_listed,
     iface_types_lists_dpdk: $dpdk_listed,
     other_config: {doca_init: ($doca_init_cfg|nz), hw_offload: ($hw_offload|nz),
                    dpdk_hugepage_dir: ($dpdk_hugepage_dir|nz), pmd_cpu_mask: ($pmd_cpu_mask|nz)},
     hugepages: ($hugepages|split("\n")|map(select(length>0))),
     pmd_threads: $pmd_threads,
     offloaded_flows: ($offloaded|nz|if .==null then null else tonumber end),
     eswitch_mode: ($eswitch_mode|nz),
     bridges: $bridges,
     virtio_controller: ($virtio_ctl|nz),
     firmware: $fw,
     ovn: {version: ($ovn_version|nz), units: $central},
     bundle: ($bundle|nz),
     os: ($os|nz),
     host: $host
   }'
