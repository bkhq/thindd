# thindd

**English** · [简体中文](README.zh-CN.md)

[![CI](https://github.com/bkhq/thindd/actions/workflows/ci.yml/badge.svg)](https://github.com/bkhq/thindd/actions/workflows/ci.yml)
[![Release](https://github.com/bkhq/thindd/actions/workflows/release.yml/badge.svg)](https://github.com/bkhq/thindd/releases)

> Thin `dd` for disk images: writes the bytes that carry data, not the zeroes
> between them.

A 2 GiB image holding 316 MiB of payload flashes in the time it takes to write
316 MiB. The name is the thin-provisioning idea applied to `dd` — space that
holds nothing costs nothing.

Built on the block-map (bmap) format from the Yocto Project's
[`bmaptool`](https://github.com/yoctoproject/bmaptool) and file-format
compatible with it in both directions, with one substantial addition:
**zero-run elision**. Gzipped images are decoded transparently, so a `.img.gz`
flashes in one step.

## The problem this solves

Upstream `bmaptool` speeds up flashing by writing only the blocks of an image
that carry data. It finds those blocks by asking the file system which parts of
the image file are backed by real extents (`SEEK_HOLE` / `FIEMAP`). That works
beautifully for an image that was *built* as a sparse file, and not at all for
the image you usually have in your hands:

* one you downloaded and decompressed — decompression writes every byte, zeroes
  included;
* one you `dd`-ed off a device;
* one that came out of a build system that did not preserve sparseness;
* anything that travelled through `tar` without `--sparse`, `scp`, or a USB stick.

Those images are **dense**: every zero byte is really on disk. `SEEK_HOLE`
reports 100% mapped, and `bmaptool` degenerates into `dd`.

`thindd` detects all-zero blocks **by content** as well, so those images flash
at the speed of their actual payload.

## Measured

A 2 GiB dense image containing 316 MiB of real data, written to a file on NVMe
(`--mode zero` also clears the destination, which is why it does more work):

| | wall clock | bytes written | destination on disk |
|---|---|---|---|
| `dd bs=8M conv=fsync` | 22.7 s | 2.0 GiB | 2.1 GiB |
| upstream-style bmap (holes only) | — | 2.0 GiB (100% mapped) | 2.1 GiB |
| `thindd copy` **with** a bmap | **1.9 s** | 316 MiB | 317 MiB |
| `thindd copy` **without** a bmap (scan on the fly) | 13.1 s | 316 MiB | 317 MiB |
| `thindd copy --mode zero` | 3.4 s | 316 MiB + hole punch | 317 MiB |

Same image compressed with `gzip -1` (324 MiB on disk):

| | wall clock | bytes written | destination on disk |
|---|---|---|---|
| `gzip -dc image.gz \| dd bs=8M conv=fsync` | 25.2 s | 2.0 GiB | 2.1 GiB |
| `thindd copy image.gz out` (no map) | **2.0 s** | 316 MiB | 317 MiB |
| `thindd copy image.gz out` (with map) | 2.2 s | 316 MiB | 317 MiB |
| `thindd create image.gz` | 0.6 s | — | — |

All outputs are byte-identical to the source image. Against a loop device, the
block layer confirms it: 57344 KiB of sectors written where `dd` sends 262144.

The two rows worth understanding:

* **with a bmap**: the map already says which 316 MiB matter, so the image is
  neither fully read nor fully written. This is the flow you want in CI or on a
  production line.
* **without a bmap**: the whole image still has to be *read* (that is how the
  zeroes are found), but only 316 MiB are *written*. On the media people
  actually flash — SD cards, eMMC, USB sticks at 10–40 MB/s — writing is the
  bottleneck by an order of magnitude, so this is very close to the with-bmap
  number in practice, with nothing to prepare in advance.

## Install

Prebuilt binaries for every tagged release:

| platform | archive |
|---|---|
| Linux x86_64 | `thindd-<tag>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `thindd-<tag>-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel | `thindd-<tag>-x86_64-apple-darwin.tar.gz` |
| macOS Apple silicon | `thindd-<tag>-aarch64-apple-darwin.tar.gz` |

```bash
# Linux x86_64, latest release
curl -fsSL https://github.com/bkhq/thindd/releases/latest/download/thindd-$(
  curl -fsSL https://api.github.com/repos/bkhq/thindd/releases/latest | grep -o '"tag_name": "[^"]*' | cut -d'"' -f4
)-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo install -m755 thindd /usr/local/bin/
```

The Linux archives are statically linked against musl: one file, no glibc
version to match, runs on any distribution. Each archive ships with a
`.sha256` alongside it.

From source:

```bash
cargo install --git https://github.com/bkhq/thindd thindd
# or, in a clone:
cargo build --release          # target/release/thindd
```

No C dependencies, no `openssl`, no `ioctl`, no `unsafe`. Hole detection is
`lseek(SEEK_DATA/SEEK_HOLE)`, fast zeroing is `fallocate`, gzip is
[`zlib-rs`](https://crates.io/crates/zlib-rs) — all reached through safe
wrappers.

## Use

```bash
# Flash an image. A sibling <IMAGE>.bmap is picked up automatically.
thindd copy core-image.wic /dev/sdb

# No bmap file anywhere? Still fast — zero runs are found while reading.
thindd copy --no-bmap core-image.wic /dev/sdb

# Also clear the parts that are not in the image, instead of leaving whatever
# the device held before. Uses fallocate/BLKZEROOUT, so it is nearly free.
thindd copy --mode zero core-image.wic /dev/sdb

# Precompute a map, so later flashes neither read nor write the zeroes.
thindd create core-image.wic            # writes core-image.wic.bmap
thindd info core-image.wic.bmap --ranges

# Compressed images are decoded on the fly — no scratch file, no pipeline.
thindd copy core-image.wic.gz /dev/sdb
thindd create core-image.wic.gz         # writes core-image.wic.bmap

# Streaming works too, compressed or not.
zstd -dc core-image.wic.zst | thindd copy --no-bmap - /dev/sdb
cat core-image.wic.gz | thindd copy --no-bmap - /dev/sdb
```

### `copy`

| flag | default | meaning |
|---|---|---|
| `--bmap FILE` | `<IMAGE>.bmap` if it exists | map to use |
| `--no-bmap` | | ignore any map; discover everything by scanning |
| `--detect holes\|zeros\|both\|none` | `both` | what may be skipped |
| `--mode skip\|zero` | `skip` | what happens to the skipped regions |
| `--seek BYTES` | `0` | byte offset on the destination to write the image at |
| `--wipe` | off | clear the whole destination first, including past the image |
| `--decompress auto\|none\|gzip` | `auto` | transparent decompression |
| `--no-verify` | off | skip the per-range checksums from the map |
| `--no-sync` | off | do not `fsync` before exiting |
| `--force` | off | write to a block device the kernel says is busy |
| `--bs BYTES` | `8M` | size of each read and each write — `dd`'s `bs=` |
| `--sync-every BYTES` | `16M` | flush the destination this often; `0` disables |
| `--queue-depth N` | `4` | batches in flight between reader and writer |

`--detect holes` reproduces upstream `bmaptool` behaviour exactly, if you want a
like-for-like comparison.

### `create`

| flag | default | meaning |
|---|---|---|
| `-o, --output FILE` | `<IMAGE>.bmap` | where to write (`-` for stdout) |
| `--detect holes\|zeros\|both\|none` | `both` | what counts as skippable |
| `--checksum sha1\|sha256\|sha512\|none` | `sha256` | per-range digests |
| `--decompress auto\|none\|gzip` | `auto` | map the decompressed image |
| `--block-size BYTES` | file system's preferred | map granularity |
| `--bs BYTES` | `8M` | size of each read |

`--detect holes --checksum none` needs no reads at all: the map falls straight
out of `SEEK_HOLE`.

## Writing at an offset

`--seek` puts the image somewhere other than byte zero of the destination —
`dd`'s `seek=`, except in bytes rather than blocks, so `--seek 8K` means 8192.

```bash
# A bootloader where the SoC's ROM expects to find it
thindd copy --seek 8K u-boot-sunxi-with-spl.bin /dev/sdb

# Refresh the system image inside a larger hand-built layout, leaving the data
# partition that follows it alone
thindd copy --seek 32M --mode zero system.img /dev/sdb
```

A non-zero offset makes the copy a **partial update**, and everything follows
from that:

* a regular-file destination is extended if it is too short, and **never
  truncated** — whatever sits after the image is left as it was;
* the capacity check accounts for the offset, and says so when it fails:
  *image is 256.0 MiB written at offset 300.0 MiB, needing 556.0 MiB, but
  destination only holds 512.0 MiB*;
* `--mode zero` zeroes the gaps **within the image's extent**, which now starts
  at the offset — it still does not reach outside it;
* the bmap is unaffected. It describes the image, not where the image is going,
  so the same map works at any offset.

`--wipe` still means the whole device, so combining it with `--seek` clears the
card and then writes the image at the offset — which is exactly what you want
when laying out a fresh card by hand, and a mistake in every other case.

## Compressed images

`--decompress auto` (the default) classifies the input by its **magic bytes**,
not its name, so a gzip stream is recognised whether it is called `.gz`, `.img`,
or arrives on standard input. `--decompress none` forces the raw reading of a
file that happens to look compressed; `--decompress gzip` forces the decoder
onto a header-less stream. Multi-member streams — `pigz`, `cat a.gz b.gz`,
rsyncable gzip — are handled.

Two consequences worth knowing:

* A compressed stream cannot be rewound, so **the whole image is inflated** even
  for the parts that turn out to be skippable — those bytes have to exist before
  we can tell they are zero. The saving is entirely on the write side, which is
  where the time goes on real media anyway.
* Because there is no seeking, hole detection does not apply to a compressed
  image; the zero scan does all the work. Nothing is lost — a compressed file
  has no holes to find.

Map lookup follows the compression suffix: `thindd copy core-image.wic.gz` looks
for `core-image.wic.gz.bmap` and then `core-image.wic.bmap`, and `create` on a
compressed image writes the latter, since the map describes the *decompressed*
image either way.

Decoding uses [`zlib-rs`](https://crates.io/crates/zlib-rs), the Rust rewrite of
zlib — no C, no `*-sys`. It is behind the `gzip` feature of `thindd-core`, on by
default; `--no-default-features` drops both the code and the dependency, and the
tool then reports a compressed image as an error instead of writing garbage to a
device.

## `skip` versus `zero` — the one decision that matters

The bmap contract, inherited from upstream, is *"the mapped blocks are
written"*. Nothing is promised about the rest. On a blank device, or on a fresh
file (which ends up sparse, so it reads back as zero), that is exactly right and
costs nothing.

On a device that **already holds data**, `--mode skip` leaves the old bytes
in the gaps. That is upstream behaviour and remains the default, but it is
rarely what you want when reflashing:

```bash
thindd copy --mode zero core-image.wic /dev/sdb
```

`zero` asks the kernel to zero the gaps with `fallocate`: `FALLOC_FL_PUNCH_HOLE`
on a regular file (no I/O and no disk space at all) or `FALLOC_FL_ZERO_RANGE` on
a block device, which most SSD/eMMC/SD controllers execute internally rather
than by writing zeroes over the bus. Writing zero pages by hand is only the
fallback for hardware — or platforms — that support neither.

### What is actually left behind

Measured against a 256 MiB loop device pre-filled with random data, flashing a
64 MiB image that holds 16 MiB of payload:

| | gap inside the image | past the end of the image |
|---|---|---|
| blank device, any mode | zero | zero |
| dirty device, `--mode skip` | **old bytes survive** | old bytes survive |
| dirty device, `--mode zero` | zeroed | **old bytes survive** |
| dirty device, `--wipe` | zeroed | zeroed |

Upstream `bmaptool` has only the first behaviour and no option to change it: its
`copy()` writes the mapped batches and nothing else.

Two things follow.

**Leftovers inside the image are usually harmless but not always.** They land in
what the new file system considers free space, so nothing reads them back as
file content and the device boots fine. They matter when the device leaves your
control — old keys and logs are recoverable with ordinary forensics — and when
you verify a flash by reading the device back and comparing it to the image,
which will not match.

**No `--mode` setting reaches beyond the image.** A bmap describes the image, and the
image says nothing about the space after it. If the device previously held a
different layout, a stale GPT backup header or an old file-system superblock
survives out there and can confuse `blkid`, udev or a bootloader into finding a
partition that no longer exists. That is what `--wipe` is for:

```bash
thindd copy --wipe core-image.wic /dev/sdb
```

It clears the whole device before copying — one `fallocate(ZERO_RANGE)` over the
lot, which a controller implementing write-zeroes or discard executes internally.
On a 512 MiB loop device that costs about 20 ms on top of the copy.

### Which to use

`--mode zero` and `--wipe` are not alternatives to each other. They answer
different questions: *make the image's own extent match the image, leave the
rest alone* versus *make the whole device clean*.

| | |
|---|---|
| blank or new media | the default — fastest, nothing to clean |
| clean image area, but keep whatever is on the device beyond it | `--mode zero` |
| whole device clean — new layout, stale GPT backup header, device changing hands | `--wipe` |

Two things separate them.

**`--wipe` destroys everything on the device, not just the image's extent.** If
the card carries a user-data partition after the system image — a very ordinary
arrangement — `--wipe` takes it with it and `--mode zero` does not.

**Their cost scales with different things.** `--mode zero` is bounded by the
image; `--wipe` by the device. Measured on an 8 GiB destination holding a
512 MiB image with 64 MiB of payload, counted at the block layer:

| | sectors written |
|---|---|
| default | 64 MiB |
| `--mode zero` | 512 MiB |
| `--wipe` | 8256 MiB |

On a controller that implements write-zeroes or discard all three take about the
same wall-clock time, because the zeroing never crosses the bus. On one that
does not, that column is real write time: on a 15 MB/s stick the same three runs
are roughly 5 s, 35 s and 9 minutes. The bigger the card relative to the image,
the wider that gap.

`--mode zero` is **not** the default, and deliberately so. It is nearly free
only where the destination implements write-zeroes or discard. Where it does not
— plain USB mass storage is the usual case — `fallocate` fails and the fallback
writes the zeroes for real: for a 2 GiB image holding 316 MiB that is 1.7 GiB of
zeroes, turning a 20-second flash into a two-minute one on a 15 MB/s stick. A
default whose cost swings between nothing and "as slow as `dd`" depending on the
hardware is the wrong default for a tool whose point is predictable speed. Ask
for it when you want it.

## Safety

* Block devices are opened `O_EXCL`. The kernel refuses that while the device or
  any of its partitions is mounted or otherwise claimed, so you cannot overwrite
  your running root file system by fumbling a device name. `--force` opts out.
* The destination is size-checked against the image before anything is written.
* Per-range checksums from the bmap are verified **while** copying, so a corrupt
  image or a mismatched bmap aborts rather than producing a broken device.
* After the copy, the number of mapped blocks read is checked against what the
  bmap claims — a bmap that belongs to a different image is caught even if every
  individual checksum happened to pass.
* On a block device the destination is flushed every 16 MiB by default, so
  interrupting a flash to a slow USB stick does not leave you waiting minutes
  inside `close()`.

## Platform support

| | Linux | macOS |
|---|---|---|
| copy, create, info, gzip, checksums | yes | yes |
| hole detection (`SEEK_HOLE`/`SEEK_DATA`) | yes | the call works, but see below |
| `O_EXCL` guard on block devices | yes | yes |
| `--mode zero` without writing zeroes | `fallocate` | falls back to explicit writes |
| I/O scheduler / writeback tuning | `sysfs` | nothing to tune |
| page-cache hints | `posix_fadvise` | left to the kernel's heuristics |

Everything platform-specific sits behind `cfg`, and CI builds, clippy-checks and
runs the whole test suite on both. Beyond that, on Linux:

* [`tests/blockdev/`](tests/blockdev/) drives loop devices in a privileged
  container — the `O_EXCL` guard, the capacity check, sysfs tuning, `fallocate`
  zeroing — and confirms at the block layer that only the mapped sectors are
  written;
* [`tests/vm/`](tests/vm/) boots a QEMU guest with its own virtio disk to cover
  the one case neither of those can reach: writing to a **partition**, where the
  I/O knobs live on the parent disk and have to be walked up to.

**On macOS none of that is covered** — the macOS binaries are built and
unit-tested, not field-tested.

Three macOS specifics to keep in mind:

* `/dev/rdiskN` is the one to use — it is the unbuffered path and much the
  faster of the two — after `diskutil unmountDisk /dev/diskN`. It is a
  *character* device rather than a block device, which `thindd` accounts for:
  the size comes from `lseek`, so the capacity check, `--wipe` and the final
  sync all work on it. One caveat remains: a raw device requires every write to
  be a multiple of its block size, and `thindd` does not pad the final partial
  block, so an image whose size is not a sector multiple can fail on its last
  write. Disk images essentially always are; if yours is not, use `/dev/diskN`.
* `--mode zero` and `--wipe` are honest but slow there: without `fallocate`
  they write the zeroes for real, so a whole-device `--wipe` costs as much as
  writing the whole device. `thindd` says so before it starts.
* `--detect holes` on its own can map an image 100%. `SEEK_HOLE` is implemented,
  but APFS commonly stores a file written the ordinary way with no holes in it,
  so there is nothing for it to report — CI sees exactly this. The default
  `--detect both` is unaffected: the zero scan finds those regions by content,
  which is the whole point of this tool.

Windows is not supported and not planned; the whole design rests on Unix
positional writes and sparse-file semantics.

## Format compatibility

The bmap 2.0 XML format is implemented for reading and writing, and 1.x files
are read as well. This is verified against upstream, not assumed:

* a map produced here is parsed by upstream `bmaptool`, its self-checksum and
  every range checksum verify, and the resulting image is byte-identical;
* a map produced by upstream `BmapCreate` is parsed here and copies correctly.

`thindd info` will describe either.

## How it works

```
 reader thread                          writer thread
 ─────────────                          ─────────────
 take buffer from pool  ──┐
 read one batch           │  bounded
 classify zero / data     │  channel  ──▶  pwrite the data spans
 hash for verification    │                fallocate the zero spans
 send batch             ──┘                return buffer to the pool
```

Reading and writing overlap, because the source (NVMe or page cache) and the
destination (SD card) usually differ by an order of magnitude in speed. Buffers
are recycled through a second channel, so copying a 32 GiB image allocates the
same fixed 32 MiB as copying a 32 MiB one.

Zero detection is the hot loop — every byte of the image passes through it. It
compares slices against a static zero page rather than iterating bytes, because
slice equality on `[u8]` lowers to `memcmp`, which is vectorised in libc and
bails out on the first differing byte. A batch that is uniformly zero, the
common case, settles in a single call. It runs at memory bandwidth and needs no
`unsafe`.

### Crates

| crate | contents |
|---|---|
| [`thindd-core`](crates/thindd-core) | format, hole/zero detection, decompression, copy engine — usable as a library |
| [`thindd`](crates/thindd) | the CLI |

Library example:

```rust
use thindd_core::{
    Destination, ImageSource,
    copy::{self, CopyOptions},
    create::{self, CreateOptions},
    progress::NoProgress,
};
use std::path::Path;

fn flash() -> Result<(), thindd_core::Error> {
    let image = Path::new("core-image.wic");
    let bmap = create::create(image, &CreateOptions::default(), &NoProgress)?;
    bmap.write_to(Path::new("core-image.wic.bmap"))?;

    let dest = Destination::open(Path::new("/dev/sdb"), false)?;
    let stats = copy::copy(
        ImageSource::open(image)?,
        &dest,
        Some(&bmap),
        &CopyOptions::default(),
        &NoProgress,
    )?;
    println!("skipped {:.1}%", stats.elided_percent());
    Ok(())
}
```

## Not implemented

Deliberately out of scope for now; upstream has them and they are additive:

* `.xz` / `.zst` / `.bz2` decompression — `.gz` is supported; pipe the others
  through their decompressor and read from `-`;
* reading images directly from a URL;
* GPG signature verification of bmap files;
* the `psplash` progress pipe.

## Development

```bash
just ci     # fmt + clippy + nextest + doctests + cargo-deny + cargo-shear
```

Two suites need more than `cargo test` can give them, and both run in a
privileged container against devices they create themselves:

* [`tests/blockdev/`](tests/blockdev/) — loop devices: `O_EXCL`, capacity check,
  sysfs tuning, `fallocate` zeroing, and a block-layer count proving only the
  mapped sectors are written;
* [`tests/vm/`](tests/vm/) — a QEMU guest with its own virtio disk, for writing
  to a partition.

Releases are cut by pushing a tag:

```bash
git tag -a v0.1.0 -m "v0.1.0" && git push origin v0.1.0
```

The workspace follows the PMA-Rust baseline: edition 2024,
`#![forbid(unsafe_code)]` in every crate, deny-warnings policy in
`[workspace.lints]` rather than on the CI command line, no `unwrap`/`expect`/
`panic!` outside tests.

## Licensing

GPL-2.0-only. `thindd` is an independent implementation, but the bmap file
format it reads and writes — and the semantics it has to match to stay
compatible — come from the Yocto Project's `bmaptool`
(Copyright (c) 2012-2014 Intel, Inc., GPLv2). The licence is kept the same so
the two can be used together without friction.
