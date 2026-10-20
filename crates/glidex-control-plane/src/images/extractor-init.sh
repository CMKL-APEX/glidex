#!/bin/busybox sh
# glidex kernel extractor (spec/kernel-boot.md §10.3): /init of the
# extractor appliance. Mounts every partition of the image on /dev/vda
# read-only and writes its kernels, initramfs files and boot configs to
# /dev/vdb as one stream:
#
#   GXFILE <partition>:<path> <size>\n<size bytes>  ...  GXEND\n
#
# Diagnostics go to the serial console, which glidex keeps for errors.
/bin/busybox --install -s /bin
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
for m in $(cat /lib/modules/order); do
  insmod "/lib/modules/$m.ko" || echo "gx: insmod $m failed" >&2
done
for i in $(seq 1 25); do
  [ -b /dev/vdb ] && break
  sleep 0.2
done
mkdir -p /mnt
{
  for p in /dev/vda[0-9]*; do
    ok=
    for t in "ext4 ro,noload" "xfs ro,norecovery" "btrfs ro" "vfat ro"; do
      set -- $t
      mount -t "$1" -o "$2" "$p" /mnt 2>/dev/null && { ok=$1; break; }
    done
    [ -n "$ok" ] || continue
    echo "gx: ${p#/dev/} is $ok" >&2
    find /mnt -maxdepth 4 -type f \( -name 'vmlinuz-*' -o -name 'initrd.img-*' \
        -o -name 'initramfs-*.img' -o -name 'grub.cfg' -o -path '*/loader/entries/*.conf' \) \
        2>/dev/null | grep -v rescue | while read -r f; do
      # The stream's header is space-separated.
      case "$f" in *' '*) continue ;; esac
      sz=$(stat -c %s "$f")
      printf 'GXFILE %s:%s %s\n' "${p#/dev/}" "${f#/mnt}" "$sz"
      cat "$f"
    done
    umount /mnt
  done
  printf 'GXEND\n'
} > /dev/vdb
sync
echo "gx: done" >&2
poweroff -f
