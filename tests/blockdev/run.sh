#!/bin/sh
# Block-device path validation for thindd. Runs as root inside a privileged
# container. Every device it touches is a loop device it created itself from a
# file under /scratch, and every one is detached again on exit.
set -eu

BM=${THINDD:-/work/thindd}
W=/scratch
FAILED=0
LOOPFILE=/tmp/thindd-loops
MOUNTED=""
: > $LOOPFILE

step() { printf '\n### %s\n' "$*"; }
ok()   { printf '  ok   %s\n' "$*"; }
bad()  { printf '  FAIL %s\n' "$*"; FAILED=1; }

cleanup() {
    for m in $MOUNTED; do umount -l "$m" 2>/dev/null || true; done
    # mkloop runs inside a command substitution, so it records what it created
    # in a file rather than a variable the parent shell would never see.
    while read -r l; do losetup -d "$l" 2>/dev/null || true; done < $LOOPFILE
    printf '\n--- loop devices still attached (should be empty) ---\n'
    losetup -a || true
}
trap cleanup EXIT

# Docker gives the container a tmpfs /dev with only a couple of loop nodes, and
# nothing creates the rest. Make them by hand.
for n in $(seq 0 15); do [ -b "/dev/loop$n" ] || mknod "/dev/loop$n" b 7 "$n"; done

mkloop() {  # mkloop <size-MiB> [extra losetup args...]
    _size=$1; shift
    _f="$W/back.$(date +%s%N).raw"
    truncate -s "${_size}M" "$_f"
    _l=$(losetup --show -f "$@" "$_f")
    echo "$_l" >> $LOOPFILE
    echo "$_l"
}

# Make the node for a partition the kernel just created, reading its device
# numbers out of sysfs since devtmpfs is not mounted here.
mkpart_node() {  # mkpart_node <disk> <part-name>
    _sys="/sys/block/$(basename "$1")/$2/dev"
    [ -r "$_sys" ] || return 1
    _mm=$(cat "$_sys"); _maj=${_mm%%:*}; _min=${_mm##*:}
    [ -b "/dev/$2" ] || mknod "/dev/$2" b "$_maj" "$_min"
}

sectors_written() { awk '{print $7}' "/sys/block/$(basename "$1")/stat"; }
kib_written()     { echo $(( ($2 - $1) / 2 )); }

verify() {  # verify <blockdev> <expected-sha>
    _got=$(dd if="$1" bs=1M count=256 status=none | sha256sum | cut -d' ' -f1)
    [ "$_got" = "$2" ]
}

# ---------------------------------------------------------------- fixtures
step "fixtures"
IMG=$W/test.img
dd if=/dev/zero of=$IMG bs=1M count=256 status=none          # dense, not sparse
dd if=/dev/urandom of=$IMG bs=1M count=16 seek=0   conv=notrunc status=none
dd if=/dev/urandom of=$IMG bs=1M count=32 seek=100 conv=notrunc status=none
dd if=/dev/urandom of=$IMG bs=1M count=8  seek=248 conv=notrunc status=none
IMG_SHA=$(sha256sum $IMG | cut -d' ' -f1)
echo "  256 MiB dense image, 56 MiB of real data"
$BM create -q $IMG
$BM info $IMG.bmap | sed 's/^/  /'
gzip -1 -k -f -c $IMG > $IMG.gz

# ---------------------------------------------------------------- A
step "A. copy to a blank block device"
LOOP=$(mkloop 512)
echo "  device: $LOOP"
B=$(sectors_written "$LOOP")
$BM copy --no-progress $IMG "$LOOP" 2>&1 | sed 's/^/  /'
A=$(sectors_written "$LOOP")
verify "$LOOP" "$IMG_SHA" && ok "device content matches the image" || bad "device content differs"
W1=$(kib_written "$B" "$A")
echo "  block layer saw ${W1} KiB written"
[ "$W1" -lt 80000 ] && ok "wrote ~56 MiB, not 256 MiB" || bad "wrote ${W1} KiB, expected ~57344"

step "A2. dd to the same device, for comparison"
B=$(sectors_written "$LOOP")
dd if=$IMG of="$LOOP" bs=8M conv=fsync status=none
A=$(sectors_written "$LOOP")
W2=$(kib_written "$B" "$A")
echo "  block layer saw ${W2} KiB written"
[ "$W2" -gt "$W1" ] && ok "thindd moved $(( W2 * 100 / (W1 + 1) ))% less data through the block layer" \
                    || bad "dd wrote no more than thindd"

# ---------------------------------------------------------------- B
step "B. destination smaller than the image"
SMALL=$(mkloop 64)
if $BM copy --no-progress $IMG "$SMALL" >$W/b.out 2>&1; then
    bad "copy onto a too-small device succeeded"
else
    sed 's/^/  /' $W/b.out
    grep -q "will not fit\|only holds" $W/b.out && ok "refused with a size error" \
                                                || bad "refused, but not with a size error"
fi

# ---------------------------------------------------------------- C
step "C. O_EXCL guard against a claimed device"
BUSY=$(mkloop 512)
mkfs.ext4 -q -F "$BUSY" 2>/dev/null
mkdir -p $W/mnt && mount -o ro "$BUSY" $W/mnt && MOUNTED="$MOUNTED $W/mnt"
echo "  mounted $BUSY read-only on $W/mnt"
if $BM copy --no-progress $IMG "$BUSY" >$W/c.out 2>&1; then
    bad "wrote to a mounted device without --force"
else
    sed 's/^/  /' $W/c.out
    grep -qi "busy" $W/c.out && ok "refused a mounted device (O_EXCL)" \
                             || bad "refused, but not as busy"
fi

step "C2. --force bypasses the guard"
if $BM copy --no-progress --force $IMG "$BUSY" >$W/c2.out 2>&1; then
    sed 's/^/  /' $W/c2.out
    verify "$BUSY" "$IMG_SHA" && ok "--force wrote through, content matches" \
                              || bad "--force wrote, but content differs"
else
    sed 's/^/  /' $W/c2.out; bad "--force still refused"
fi
umount -l $W/mnt; MOUNTED=""; sleep 1

step "C3. the same device once nothing holds it"
$BM copy --no-progress $IMG "$BUSY" >/dev/null 2>&1 && ok "accepted after umount" \
                                                    || bad "still refused after umount"

# ---------------------------------------------------------------- D
step "D. --mode on a device that already holds data"
DIRTY=$(mkloop 512)
dd if=/dev/urandom of="$DIRTY" bs=1M count=300 status=none conv=fsync
$BM copy --no-progress $IMG "$DIRTY" >/dev/null 2>&1
verify "$DIRTY" "$IMG_SHA" && bad "skip mode unexpectedly cleared the stale data" \
                           || ok "skip mode leaves stale bytes (documented default)"

dd if=/dev/urandom of="$DIRTY" bs=1M count=300 status=none conv=fsync
B=$(sectors_written "$DIRTY")
$BM copy -vv --no-progress --mode zero $IMG "$DIRTY" >$W/d.out 2>&1
grep -v "^ *$" $W/d.out | sed 's/^/  /'
A=$(sectors_written "$DIRTY")
verify "$DIRTY" "$IMG_SHA" && ok "zero mode reproduces the image exactly" \
                           || bad "zero mode left stale bytes"
echo "  block layer saw $(kib_written "$B" "$A") KiB written while clearing 200 MiB of stale data"
if grep -q "fallocate zeroing unavailable" $W/d.out; then
    ok "kernel refused fallocate on this device; the explicit-write fallback carried it"
else
    ok "the kernel accepted fallocate(ZERO_RANGE) — no zero pages came out of user space"
    echo "       (a loop device turns that into REQ_OP_WRITE_ZEROES, which the block"
    echo "        layer still counts as written sectors; on real flash the controller"
    echo "        executes it internally)"
fi

# ---------------------------------------------------------------- E
step "E. sysfs tuning is applied and restored"
SYS=/sys/block/$(basename "$LOOP")
SCHED_BEFORE=$(cat $SYS/queue/scheduler 2>/dev/null || echo n/a)
RATIO_BEFORE=$(cat $SYS/bdi/max_ratio 2>/dev/null || echo n/a)
echo "  before: scheduler='$SCHED_BEFORE' max_ratio='$RATIO_BEFORE'"
$BM copy -vv --no-progress $IMG "$LOOP" 2>&1 | grep -i "tuned block device\|could not tune\|restore" | sed 's/^/  /' || true
SCHED_AFTER=$(cat $SYS/queue/scheduler 2>/dev/null || echo n/a)
RATIO_AFTER=$(cat $SYS/bdi/max_ratio 2>/dev/null || echo n/a)
echo "  after:  scheduler='$SCHED_AFTER' max_ratio='$RATIO_AFTER'"
[ "$SCHED_BEFORE" = "$SCHED_AFTER" ] && ok "scheduler restored" || bad "scheduler left as $SCHED_AFTER"
[ "$RATIO_BEFORE" = "$RATIO_AFTER" ] && ok "bdi/max_ratio restored" || bad "max_ratio left as $RATIO_AFTER"

# ---------------------------------------------------------------- F
step "F. writing to a partition (sysfs has to walk up to the parent disk)"
MAXPART=$(cat /sys/module/loop/parameters/max_part 2>/dev/null || echo 0)
echo "  loop.max_part=$MAXPART"
if [ "$MAXPART" = "0" ]; then
    echo "  SKIPPED: this host's loop module was loaded with max_part=0, so a loop"
    echo "           device has a single minor and can never expose a partition."
    echo "           The partition case is covered for real by tests/vm/, which"
    echo "           boots a QEMU guest with its own virtio disk."
else
    PART=$(mkloop 512 -P)
    sfdisk -q "$PART" >/dev/null 2>&1 <<'PT' || true
label: dos
start=2048, size=819200, type=83
PT
    losetup -c "$PART" 2>/dev/null || true
    sleep 1
    PNAME="$(basename "$PART")p1"
    if mkpart_node "$PART" "$PNAME"; then
        echo "  partition: /dev/$PNAME"
        $BM copy -vv --no-progress $IMG "/dev/$PNAME" 2>&1 | grep -i "tuned block device\|could not tune" | sed 's/^/  /' || true
        verify "/dev/$PNAME" "$IMG_SHA" && ok "partition content matches" || bad "partition content differs"
    else
        echo "  (kernel exposed no partition node; skipping)"
    fi
fi

# ---------------------------------------------------------------- G
step "G. gzipped image straight to a block device"
GZL=$(mkloop 512)
$BM copy --no-progress $IMG.gz "$GZL" 2>&1 | sed 's/^/  /'
verify "$GZL" "$IMG_SHA" && ok "gz -> block device matches" || bad "gz -> block device differs"

# ---------------------------------------------------------------- H
step "H. a block device as the *image* (stat reports size 0 for these)"
$BM create -q --detect zeros --checksum none -o $W/dev.bmap "$GZL"
$BM info $W/dev.bmap | sed 's/^/  /'
grep -q "536870912" $W/dev.bmap && ok "sized the device with lseek, not stat" || bad "device size wrong"

# ---------------------------------------------------------------- I
step "I. tighter sync watermark"
BIG=$(mkloop 512)
$BM copy --no-progress --sync-every 8M $IMG "$BIG" >/dev/null 2>&1
verify "$BIG" "$IMG_SHA" && ok "8 MiB watermark still produces the right image" \
                         || bad "sync watermark broke the copy"

step "J. --wipe clears what the image does not describe"
# The image is 256 MiB and the device is 512 MiB, so half of it is past the end
# of the image — territory no --mode setting reaches, because a bmap says nothing
# about the space after the image.
WIPE=$(mkloop 512)
dd if=/dev/urandom of="$WIPE" bs=1M count=512 status=none conv=fsync
tail_nonzero() { dd if="$1" bs=1M skip=256 count=256 status=none | tr -d '\000' | wc -c; }
gap_nonzero()  { dd if="$1" bs=1M skip=32 count=64 status=none | tr -d '\000' | wc -c; }

$BM copy --no-progress --mode zero $IMG "$WIPE" >/dev/null 2>&1
[ "$(gap_nonzero "$WIPE")" = 0 ] && ok "zero mode cleared the gaps inside the image" \
                                 || bad "zero mode left stale bytes in the image"
[ "$(tail_nonzero "$WIPE")" != 0 ] && ok "zero mode left the space past the image untouched, as designed" \
                                   || bad "zero mode unexpectedly reached past the image"

dd if=/dev/urandom of="$WIPE" bs=1M count=512 status=none conv=fsync
$BM copy --no-progress --wipe $IMG "$WIPE" 2>&1 | grep -E "wiped|clearing" | sed 's/^/  /'
verify "$WIPE" "$IMG_SHA" && ok "image region matches after a wipe" || bad "image region differs"
[ "$(gap_nonzero "$WIPE")" = 0 ] && ok "wipe cleared the gaps inside the image" \
                                 || bad "wipe left stale bytes in the image"
[ "$(tail_nonzero "$WIPE")" = 0 ] && ok "wipe cleared the space past the image too" \
                                  || bad "wipe left $(tail_nonzero "$WIPE") stale bytes past the image"

step "K. --seek writes the image at an offset, the way a bootloader is flashed"
SEEKDEV=$(mkloop 512)
dd if=/dev/urandom of="$SEEKDEV" bs=1M count=512 status=none conv=fsync
HEAD_BEFORE=$(dd if="$SEEKDEV" bs=8K count=1 status=none | sha256sum | cut -d' ' -f1)
TAIL_BEFORE=$(dd if="$SEEKDEV" bs=1M skip=257 count=255 status=none | sha256sum | cut -d' ' -f1)

# --mode zero so the image region compares exactly; the device is full of
# random data, and skip mode would quite correctly leave it in the gaps.
$BM copy --no-progress --seek 8K --mode zero $IMG "$SEEKDEV" 2>&1 | grep -E "offset|wrote" | sed 's/^/  /'

AT=$(dd if="$SEEKDEV" bs=8K skip=1 count=32768 status=none | head -c 268435456 | sha256sum | cut -d' ' -f1)
[ "$AT" = "$IMG_SHA" ] && ok "image landed at offset 8 KiB" || bad "image is not at the offset"
[ "$(dd if="$SEEKDEV" bs=8K count=1 status=none | sha256sum | cut -d' ' -f1)" = "$HEAD_BEFORE" ] \
    && ok "the 8 KiB before the offset was left alone" || bad "bytes before the offset changed"
[ "$(dd if="$SEEKDEV" bs=1M skip=257 count=255 status=none | sha256sum | cut -d' ' -f1)" = "$TAIL_BEFORE" ] \
    && ok "everything past the image was left alone" || bad "bytes past the image changed"

step "K2. an image that no longer fits once shifted is refused"
if $BM copy --no-progress --seek 300M $IMG "$SEEKDEV" >$W/k.out 2>&1; then
    bad "a copy that runs off the end of the device was accepted"
else
    sed 's/^/  /' $W/k.out | tail -2
    grep -q "will not fit\|only holds" $W/k.out && ok "refused: offset + image exceeds the device" \
                                                 || bad "refused, but not with a size error"
fi

step "N. --verify reads the device back and compares it against the image"
VDEV=$(mkloop 512)
dd if=/dev/urandom of="$VDEV" bs=1M count=512 status=none conv=fsync
echo "  a device that already holds data, flashed with the default --mode skip:"
if $BM copy --no-progress --verify $IMG "$VDEV" >$W/n1.out 2>&1; then
    bad "--verify passed on a device whose gaps still hold old data"
else
    grep -E "differs from the image|--mode skip leaves" $W/n1.out | sed 's/^/    /'
    grep -q "differs from the image at byte" $W/n1.out \
        && ok "caught it, and pointed at the byte" || bad "failed, but not with a useful message"
fi
echo "  the same device with the gaps cleared:"
if $BM copy --no-progress --verify --mode zero $IMG "$VDEV" >$W/n2.out 2>&1; then
    grep "verified" $W/n2.out | sed 's/^/    /'
    ok "--verify passed once the device actually holds the image"
else
    sed 's/^/    /' $W/n2.out | tail -3; bad "--verify failed on a copy that should match"
fi

step "M. --zap clears the ends where the partition table lives, and nothing else"
ZAPDEV=$(mkloop 512)
dd if=/dev/urandom of="$ZAPDEV" bs=1M count=512 status=none conv=fsync
# Sample past the image (256 MiB) and before the zapped tail (508 MiB): the
# region only --wipe would ever reach.
MID_BEFORE=$(dd if="$ZAPDEV" bs=1M skip=300 count=200 status=none | sha256sum | cut -d' ' -f1)
nz() { dd if="$ZAPDEV" bs=1M skip="$1" count="$2" status=none | tr -d '\000' | wc -c; }

START=$(date +%s%N)
$BM copy --no-progress --zap --mode zero $IMG "$ZAPDEV" 2>&1 | grep -E "clearing|wiped|wrote" | sed 's/^/  /'
echo "  wall $(( ($(date +%s%N) - START) / 1000000 )) ms"

verify "$ZAPDEV" "$IMG_SHA" && ok "image region matches" || bad "image region differs"
[ "$(nz 508 4)" = 0 ] && ok "the last 4 MiB is cleared — a stale GPT backup cannot survive there" \
                      || bad "the tail still holds $(nz 508 4) non-zero bytes"
[ "$(dd if="$ZAPDEV" bs=1M skip=300 count=200 status=none | sha256sum | cut -d' ' -f1)" = "$MID_BEFORE" ] \
    && ok "the 200 MiB between the image and the tail was left alone, as designed" \
    || bad "zap touched the middle of the device"

step "M2. --zap and --wipe are mutually exclusive"
$BM copy --no-progress --zap --wipe $IMG "$ZAPDEV" >$W/m.out 2>&1 \
    && bad "--zap --wipe together was accepted" \
    || { grep -qi "cannot be used with" $W/m.out && ok "rejected at the command line" \
         || { sed 's/^/  /' $W/m.out | tail -2; bad "rejected, but not by clap"; }; }

step "L. a destination that reports no size is refused a wipe, not silently skipped"
# The regression this guards: --wipe used to return success having done nothing
# on any character device, which on macOS meant a raw disk (/dev/rdiskN).
if $BM copy --no-progress --wipe $IMG /dev/null >$W/l.out 2>&1; then
    bad "--wipe on /dev/null reported success without clearing anything"
else
    sed 's/^/  /' $W/l.out | tail -2
    grep -q "reports no size" $W/l.out && ok "refused with a clear reason" \
                                       || bad "refused, but not for the right reason"
fi
$BM copy --no-progress $IMG /dev/null >/dev/null 2>&1 \
    && ok "a plain copy to /dev/null still works" || bad "plain copy to /dev/null broke"

printf '\n=================================\n'
[ "$FAILED" = 0 ] && echo "ALL BLOCK-DEVICE TESTS PASSED" || echo "SOME TESTS FAILED"
exit $FAILED
