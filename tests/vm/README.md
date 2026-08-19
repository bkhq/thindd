# VM test — writing to a partition

One code path cannot be reached from `cargo test` or even from the loop-device
suite in [`../blockdev/`](../blockdev/): **writing to a partition**.

A partition has no `queue/` directory of its own in sysfs — the I/O knobs live
on the parent disk — so `thindd` has to resolve `/sys/dev/block/<maj>:<min>/..`
to find them. Loop devices cannot exercise that, because distributions load the
loop module with `max_part=0`, which gives a loop device a single minor and
therefore no partitions, whatever `losetup -P` is asked to do.

So this test brings its own kernel. `run.sh` boots a throwaway QEMU guest with a
virtio disk, and the guest writes to a real partition of it.

Nothing touches a host block device: the "disk" is a sparse file in the work
directory, and the guest is a kernel plus a from-scratch initramfs — busybox,
the `thindd` binary under test, and the six virtio modules needed to see the
disk.

```bash
WORK=$PWD/.scratch/vm                 # see the note on bind mounts below
cargo build --profile dist --target x86_64-unknown-linux-musl
mkdir -p "$WORK/payload" "$WORK/scratch"
cp target/x86_64-unknown-linux-musl/dist/thindd "$WORK/payload/"
cp tests/vm/run.sh tests/vm/guest-init.sh "$WORK/payload/"

docker run --rm --privileged --device /dev/kvm \
  -v "$WORK/payload:/work:ro" -v "$WORK/scratch:/w" -w /w \
  debian:bookworm-slim /work/run.sh
```

The container installs qemu and downloads a Debian kernel on first run, then
caches both in `/w`. With `/dev/kvm` the whole thing takes a few seconds; without
it, QEMU falls back to emulation and it still works, just slower.

## What it checks

| | |
|---|---|
| the premise | the partition really has no `queue/`, and the parent disk really does — otherwise the rest proves nothing |
| the walk | `thindd` logs the sysfs base it resolved: starting from `254:1` it must land on `…/block/vda`, never on `vda1` |
| the writes | `queue/scheduler` and `bdi/max_ratio` on the **parent disk** are actually written |
| the restore | both are back to their original values once the copy finishes |
| the copy | the partition's contents match the image byte for byte |
| the contrast | the same copy onto the whole disk resolves to the same base without any walk, and matches the image under `--mode zero` |

## Notes

* The partition table is written on the host with `sfdisk` before boot, so the
  guest needs no partitioning tool — busybox `fdisk` scripting is not worth the
  trouble.
* Debian ships virtio as xz-compressed modules and busybox `insmod` cannot read
  those, so `run.sh` decompresses them into the initramfs and numbers them so
  they load in dependency order.
* The guest always powers off through sysrq, and the host wraps QEMU in a
  timeout, so a hung guest fails the run rather than hanging the terminal.
* **On bind mounts:** `-v` paths are resolved by the Docker *daemon*, not by
  whoever runs the command. If you drive Docker from inside another container
  over a mounted `/var/run/docker.sock`, only paths the daemon sees at the same
  location work. Pick `WORK` accordingly.
