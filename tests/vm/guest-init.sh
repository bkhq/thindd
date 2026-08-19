#!/bin/busybox sh
# PID 1 inside the QEMU guest. Everything here runs on a virtual virtio disk
# that exists only for the length of this boot; no host device is touched.
#
# What this proves, and nothing else does: writing to a *partition*. A partition
# has no `queue/` directory of its own, so thindd has to walk up to the parent
# disk to find the I/O knobs. Loop devices cannot exercise that — a distribution
# loads the loop module with max_part=0, so a loop device can never have
# partitions — which is why this test needs a VM and its own virtio disk.

export PATH=/bin:/sbin:/usr/bin:/usr/sbin

/bin/busybox --install -s /bin 2>/dev/null

mount -t proc     proc     /proc
mount -t sysfs    sysfs    /sys
mount -t devtmpfs devtmpfs /dev
mount -t tmpfs    tmpfs    /tmp

FAILED=0
ok()  { echo "  ok   $*"; }
bad() { echo "  FAIL $*"; FAILED=1; }

finish() {
    echo
    if [ "$FAILED" = 0 ]; then echo "VMTEST-RESULT: PASS"; else echo "VMTEST-RESULT: FAIL"; fi
    echo 1 > /proc/sys/kernel/sysrq
    sync
    echo o > /proc/sysrq-trigger
    sleep 5
}
trap finish EXIT

echo
echo "### guest: bringing up the virtio disk"
for m in /lib/modules/*.ko; do insmod "$m" 2>/dev/null; done
for i in 1 2 3 4 5 6 7 8 9 10; do
    [ -b /dev/vda1 ] && break
    sleep 1
done
if [ ! -b /dev/vda1 ]; then
    echo "  no /dev/vda1 appeared; block devices present:"
    ls /dev/vd* /dev/sd* 2>/dev/null
    bad "the guest never saw a partitioned disk"
    exit 1
fi

DISK_DEV=$(cat /sys/class/block/vda/dev)
PART_DEV=$(cat /sys/class/block/vda1/dev)
echo "  /dev/vda  = $DISK_DEV   $(( $(cat /sys/class/block/vda/size) / 2048 )) MiB"
echo "  /dev/vda1 = $PART_DEV   $(( $(cat /sys/class/block/vda1/size) / 2048 )) MiB"

echo
echo "### the layout that makes this test necessary"
[ -d "/sys/dev/block/$PART_DEV/queue" ] \
    && bad "the partition has its own queue/, so nothing would need walking" \
    || ok "partition has no queue/ of its own"
[ -f "/sys/dev/block/$DISK_DEV/queue/scheduler" ] \
    && ok "the parent disk is the one holding queue/scheduler" \
    || bad "parent disk has no scheduler knob; the rest of this test is moot"

SCHED_BEFORE=$(cat "/sys/dev/block/$DISK_DEV/queue/scheduler" 2>/dev/null)
RATIO_BEFORE=$(cat "/sys/dev/block/$DISK_DEV/bdi/max_ratio" 2>/dev/null)
echo "  before: scheduler='$SCHED_BEFORE' max_ratio='$RATIO_BEFORE'"

echo
echo "### building a 64 MiB dense image with 16 MiB of data"
IMG=/tmp/test.img
dd if=/dev/zero    of=$IMG bs=1M count=64 2>/dev/null
dd if=/dev/urandom of=$IMG bs=1M count=8 seek=0  conv=notrunc 2>/dev/null
dd if=/dev/urandom of=$IMG bs=1M count=8 seek=40 conv=notrunc 2>/dev/null
IMG_SHA=$(sha256sum $IMG | cut -d' ' -f1)
echo "  sha256 $IMG_SHA"

echo
echo "### thindd copy -> /dev/vda1"
/bin/thindd create -q $IMG 2>&1 | sed 's/^/  /'
/bin/thindd copy -vv --no-progress $IMG /dev/vda1 > /tmp/copy.log 2>&1
RC=$?
sed 's/^/  /' /tmp/copy.log
[ "$RC" = 0 ] && ok "copy exited 0" || bad "copy exited $RC"

GOT=$(dd if=/dev/vda1 bs=1M count=64 2>/dev/null | sha256sum | cut -d' ' -f1)
[ "$GOT" = "$IMG_SHA" ] && ok "partition content matches the image" \
                        || bad "partition content differs ($GOT)"

echo
echo "### did the tuning reach the parent disk rather than the partition?"
# The tool resolves the sysfs base and logs it; for a partition that base has to
# be the disk's directory, which ends in /vda, never /vda1.
BASE=$(grep -o "base=[^ ]*" /tmp/copy.log | head -1 | cut -d= -f2)
echo "  resolved base: ${BASE:-<none logged>}"
case "$BASE" in
    "")        bad "no sysfs base was logged; tuning never got that far" ;;
    */vda1)    bad "resolved to the partition, which has no knobs" ;;
    */vda)     ok "resolved to the parent disk by walking up from the partition" ;;
    *)         bad "resolved somewhere unexpected: $BASE" ;;
esac
grep -q "tuned block device.*queue/scheduler" /tmp/copy.log \
    && ok "scheduler knob on the parent disk was written" \
    || bad "no scheduler knob was written"
grep -q "tuned block device.*bdi/max_ratio" /tmp/copy.log \
    && ok "bdi/max_ratio on the parent disk was written" \
    || bad "no max_ratio knob was written"

SCHED_AFTER=$(cat "/sys/dev/block/$DISK_DEV/queue/scheduler" 2>/dev/null)
RATIO_AFTER=$(cat "/sys/dev/block/$DISK_DEV/bdi/max_ratio" 2>/dev/null)
echo "  after:  scheduler='$SCHED_AFTER' max_ratio='$RATIO_AFTER'"
[ "$SCHED_BEFORE" = "$SCHED_AFTER" ] && ok "parent disk scheduler restored" \
                                     || bad "scheduler left as '$SCHED_AFTER'"
[ "$RATIO_BEFORE" = "$RATIO_AFTER" ] && ok "parent disk bdi/max_ratio restored" \
                                     || bad "max_ratio left as '$RATIO_AFTER'"

echo
echo "### the whole disk, for contrast: no walk needed"
# --mode zero so the comparison is exact: skip mode would leave the
# partition table and the bytes the previous copy put at offset 1 MiB, which is
# correct behaviour but not comparable against the image.
/bin/thindd copy -vv --no-progress --force --mode zero $IMG /dev/vda > /tmp/copy2.log 2>&1
RC2=$?
grep -E "resolved block-device|ERROR" /tmp/copy2.log | sed 's/^/  /'
[ "$RC2" = 0 ] && ok "whole-disk copy exited 0" || bad "whole-disk copy exited $RC2"
BASE2=$(grep -o "base=[^ ]*" /tmp/copy2.log | head -1 | cut -d= -f2)
case "$BASE2" in
    */vda) ok "whole disk resolved to itself: $BASE2" ;;
    "")    bad "no sysfs base logged for the whole disk" ;;
    *)     bad "whole disk resolved to $BASE2" ;;
esac
GOT2=$(dd if=/dev/vda bs=1M count=64 2>/dev/null | sha256sum | cut -d" " -f1)
[ "$GOT2" = "$IMG_SHA" ] && ok "whole-disk content matches the image" \
                         || bad "whole-disk content differs"

exit 0
