# Block-device tests

The paths that only exist when the destination is a real block device — the
`O_EXCL` guard, the capacity check, sysfs tuning, `fallocate` zeroing — cannot
be exercised from `cargo test`: they need loop devices, `mount`, and root.

`run.sh` covers them against loop devices it creates and detaches itself. It
needs a privileged container, because Docker gives an unprivileged one a `/dev`
with no loop nodes in it.

```bash
BD=$PWD/.scratch/bd                       # see the note on bind mounts below
cargo build --profile dist --target x86_64-unknown-linux-musl
mkdir -p "$BD/scratch"
cp target/x86_64-unknown-linux-musl/dist/thindd tests/blockdev/run.sh "$BD/"

docker run --rm --privileged \
  -v "$BD:/work:ro" -v "$BD/scratch:/scratch" \
  debian:bookworm-slim /work/run.sh
```

A static musl build is used so the script runs on any base image. Point
`THINDD` at a different binary to override.

**On bind mounts.** `-v` paths are resolved by the Docker *daemon*, not by
whoever runs the command. If you are driving Docker from inside another
container over a mounted `/var/run/docker.sock`, only paths that the daemon
sees at the same location work — typically the one directory that is
bind-mounted into your container one-to-one. Pick `$BD` accordingly; `/tmp/...`
inside your container is almost certainly not `/tmp/...` on the daemon's host,
and the run fails with `stat /work/run.sh: no such file or directory`.

## What it checks

| | |
|---|---|
| A | a copy onto a blank device reproduces the image, and the **block layer** confirms only the mapped bytes were written |
| A2 | the same copy with `dd`, for a like-for-like sector count |
| B | an image larger than the device is refused with a size error, before anything is written |
| C | a mounted device is refused: `O_EXCL` fails with `EBUSY` |
| C2 | `--force` writes through the same claim, and the content still matches |
| C3 | the device is accepted again once it is unmounted |
| D | `--mode skip` leaves stale bytes; `--mode zero` reproduces the image exactly, and the log says whether `fallocate` or the write fallback carried it |
| E | `queue/scheduler` and `bdi/max_ratio` are set during the copy and restored after |
| F | writing to a *partition*, where the sysfs knobs live on the parent disk |
| G | a gzipped image straight onto a device |
| H | a block device used as the *image*, which `stat` reports as zero-sized |
| I | a tighter `--sync-every` watermark still produces the right image |
| J | `--mode zero` deliberately stops at the end of the image, and `--wipe` clears the whole device including the space beyond it |
| K | `--seek` puts the image at an offset, leaving the bytes before it and after it untouched; an offset that runs off the end of the device is refused |

## Caveats

**F is skipped when `/sys/module/loop/parameters/max_part` is `0`**, which is the
default on most distributions: with `max_part=0` a loop device has one minor and
can never expose a partition, whatever `losetup -P` is asked to do. Loading the
module with `max_part=8` makes it run here, but the partition case does not
depend on that — [`../vm/`](../vm/) covers it properly by booting a QEMU guest
with its own virtio disk and writing to a real partition of it.

The loop devices are host-kernel objects even when created from inside a
container. The script detaches everything it created on exit and prints what is
still attached, so a leak is visible rather than silent.
