#!/bin/bash
# Boot a throwaway QEMU guest with its own virtio disk and run the one test that
# needs a real partition. See tests/vm/README.md for how to invoke this.
#
# Nothing here touches a host block device: the "disk" is a sparse file in the
# work directory, and the guest is a kernel plus an initramfs built from
# scratch. The host side only needs qemu, and /dev/kvm if you want it quick.
set -euo pipefail

WORK=${WORK:-/w}
THINDD=${THINDD:-/work/thindd}
INIT=${INIT:-/work/guest-init.sh}
DISK_MIB=${DISK_MIB:-256}
TIMEOUT=${TIMEOUT:-300}

cd "$WORK"

say() { printf '\n\033[1m### %s\033[0m\n' "$*"; }

say "host: tools"
# The container is fresh every run, so packages are always installed; only the
# kernel download is cached in the work directory.
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    qemu-system-x86 qemu-utils busybox-static cpio xz-utils fdisk >/dev/null 2>&1
echo "  qemu $(qemu-system-x86_64 --version | head -1 | awk '{print $4}')"

say "host: fetching a kernel"
if [ ! -f vmlinuz ]; then
    apt-get download "$(apt-cache depends linux-image-amd64 |
        awk '/Depends: linux-image-6/{print $2}' | head -1)" >/dev/null
    dpkg-deb -x linux-image-6*.deb kernel
    cp kernel/boot/vmlinuz-* vmlinuz
    echo "  downloaded"
else
    echo "  cached"
fi
KVER=$(basename kernel/lib/modules/*)
echo "  kernel $KVER"

say "host: building an initramfs"
rm -rf irfs && mkdir -p irfs/{bin,sbin,proc,sys,dev,tmp,lib/modules}
cp /bin/busybox irfs/bin/busybox
cp "$THINDD" irfs/bin/thindd
cp "$INIT" irfs/init
chmod +x irfs/init irfs/bin/busybox irfs/bin/thindd

# virtio is modular in a Debian kernel, and Debian ships modules xz-compressed,
# which busybox insmod cannot read. Decompress the handful we need; the guest
# insmods them in filename order, which is why they are numbered.
n=0
for m in virtio virtio_ring virtio_pci_legacy_dev virtio_pci_modern_dev virtio_pci virtio_blk; do
    src=$(find kernel/lib/modules/"$KVER" -name "$m.ko*" | head -1) || true
    [ -n "$src" ] || continue
    n=$((n + 1))
    case "$src" in
        *.xz) xz -dc "$src" > "irfs/lib/modules/$(printf '%02d' $n)-$m.ko" ;;
        *)    cp "$src" "irfs/lib/modules/$(printf '%02d' $n)-$m.ko" ;;
    esac
done
echo "  $(ls irfs/lib/modules | wc -l) modules, thindd $(du -h irfs/bin/thindd | cut -f1)"
( cd irfs && find . | cpio -o -H newc --quiet | gzip -1 ) > initramfs.gz
echo "  initramfs $(du -h initramfs.gz | cut -f1)"

say "host: creating and partitioning a virtual disk"
rm -f disk.raw && truncate -s "${DISK_MIB}M" disk.raw
# One primary partition covering all but the first megabyte. The guest sees a
# ready-made table, so it needs no partitioning tool of its own.
sfdisk -q disk.raw >/dev/null <<PT
label: dos
start=2048, type=83
PT
sfdisk -l disk.raw | sed 's/^/  /'

say "host: booting the guest"
ACCEL=(-cpu qemu64)
if [ -w /dev/kvm ]; then
    ACCEL=(-enable-kvm -cpu host)
    echo "  KVM available"
else
    echo "  no /dev/kvm, falling back to emulation (slower)"
fi

set +e
timeout "$TIMEOUT" qemu-system-x86_64 \
    "${ACCEL[@]}" \
    -m 1024 -smp 2 \
    -kernel vmlinuz \
    -initrd initramfs.gz \
    -append "console=ttyS0 panic=1 loglevel=4" \
    -drive file=disk.raw,format=raw,if=virtio \
    -nographic -no-reboot -display none \
    -serial mon:stdio </dev/null | tee guest.log
QEMU_RC=$?
set -e

say "host: result"
if grep -q "^VMTEST-RESULT: PASS" guest.log; then
    echo "  VM PARTITION TEST PASSED"
    exit 0
fi
if grep -q "^VMTEST-RESULT: FAIL" guest.log; then
    echo "  VM PARTITION TEST FAILED"
    exit 1
fi
echo "  the guest never reported a result (qemu exit $QEMU_RC)"
exit 1
