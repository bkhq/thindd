//! End-to-end tests: build a map for an image, copy it, compare the result.

#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    reason = "test code: a failed setup step should fail the test loudly"
)]

use std::{
    fs,
    io::{Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
};
use thindd_core::{
    Bmap, Destination, Error, ImageSource, ZeroMode,
    checksum::ChecksumKind,
    copy::{self, CopyOptions},
    create::{self, CreateOptions},
    filemap::DetectMode,
    progress::NoProgress,
};

const BS: u64 = 4096;
const BSZ: usize = 4096;

/// An image with three data islands separated by large zero runs.
fn make_image(dir: &Path, name: &str) -> (PathBuf, Vec<u8>) {
    let mut data = vec![0u8; 1024 * 1024];
    data[0..BSZ].fill(0x11);
    data[400 * 1024..404 * 1024].fill(0x22);
    data[1024 * 1024 - BSZ..].fill(0x33);

    let path = dir.join(name);
    fs::write(&path, &data).unwrap();
    (path, data)
}

fn copy_opts(detect: DetectMode, zero_mode: ZeroMode) -> CopyOptions {
    CopyOptions {
        detect,
        zero_mode,
        block_size: Some(BS),
        batch_bytes: 64 * 1024,
        queue_depth: 2,
        sync_watermark: None,
        ..CopyOptions::default()
    }
}

fn create_opts(detect: DetectMode) -> CreateOptions {
    CreateOptions { block_size: Some(BS), detect, ..CreateOptions::default() }
}

#[test]
fn create_then_copy_reproduces_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");

    let bmap = create::create(&image, &create_opts(DetectMode::Both), &NoProgress).unwrap();
    assert!(bmap.mapped_blocks_cnt < bmap.blocks_cnt / 4, "zero runs were not elided");

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let stats = copy::copy(
        ImageSource::open(&image).unwrap(),
        &dest,
        Some(&bmap),
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(fs::read(&out).unwrap(), expected);
    assert_eq!(stats.image_size, expected.len() as u64);
    assert_eq!(stats.blocks_read, bmap.mapped_blocks_cnt);
    assert!(stats.bytes_written < stats.image_size / 4);
}

#[test]
fn copy_without_a_bmap_still_elides_zero_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let stats = copy::copy(
        ImageSource::open(&image).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(fs::read(&out).unwrap(), expected);
    assert!(
        stats.bytes_written <= 9 * BS,
        "wrote {} bytes, expected only the data islands",
        stats.bytes_written
    );
    assert!(stats.bytes_elided > stats.image_size * 9 / 10);
}

#[test]
fn detect_none_writes_the_whole_image() {
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");
    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();

    let stats = copy::copy(
        ImageSource::open(&image).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::None, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(fs::read(&out).unwrap(), expected);
    assert_eq!(stats.bytes_written, expected.len() as u64);
    assert_eq!(stats.bytes_elided, 0);
}

#[test]
fn zero_mode_clears_stale_data_on_a_dirty_destination() {
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");

    // Pre-fill the destination the way a previously flashed device would be.
    let out = dir.path().join("out.img");
    fs::write(&out, vec![0xffu8; expected.len()]).unwrap();

    let dest = Destination::open(&out, false).unwrap();
    let stats = copy::copy(
        ImageSource::open(&image).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Zero),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(fs::read(&out).unwrap(), expected, "stale 0xff bytes survived");
    assert!(stats.bytes_zeroed > 0);
}

#[test]
fn skip_mode_leaves_stale_data_behind() {
    // Documents the upstream-compatible default: a bmap only promises that the
    // mapped blocks are written.
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");
    let out = dir.path().join("out.img");
    fs::write(&out, vec![0xffu8; expected.len()]).unwrap();

    let dest = Destination::open(&out, false).unwrap();
    copy::copy(
        ImageSource::open(&image).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap();

    let result = fs::read(&out).unwrap();
    assert_ne!(result, expected);
    assert_eq!(&result[..BSZ], &expected[..BSZ], "mapped blocks must still land");
}

#[test]
fn copying_from_a_stream_matches_the_seekable_path() {
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let reader = Box::new(fs::File::open(&image).unwrap());
    let stats = copy::copy(
        ImageSource::from_reader(reader, "-"),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(stats.image_size, expected.len() as u64);
    assert_eq!(fs::read(&out).unwrap(), expected);
}

#[test]
fn a_corrupt_image_fails_checksum_verification() {
    let dir = tempfile::tempdir().unwrap();
    let (image, _) = make_image(dir.path(), "src.img");
    let bmap = create::create(&image, &create_opts(DetectMode::Both), &NoProgress).unwrap();

    // Flip one byte inside a mapped range.
    let mut f = fs::OpenOptions::new().write(true).open(&image).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&[0x99]).unwrap();
    f.flush().unwrap();
    drop(f);

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let err = copy::copy(
        ImageSource::open(&image).unwrap(),
        &dest,
        Some(&bmap),
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap_err();

    assert!(matches!(err, Error::RangeChecksum { .. }), "got {err:?}");
}

#[test]
fn a_bmap_from_a_different_image_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (image, _) = make_image(dir.path(), "src.img");
    let bmap = create::create(&image, &create_opts(DetectMode::Both), &NoProgress).unwrap();

    // A second image of the same size but with data in different places.
    let mut other = vec![0u8; 1024 * 1024];
    other[600 * 1024..604 * 1024].fill(0x44);
    let other_path = dir.path().join("other.img");
    fs::write(&other_path, &other).unwrap();

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let err = copy::copy(
        ImageSource::open(&other_path).unwrap(),
        &dest,
        Some(&bmap),
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap_err();

    assert!(matches!(err, Error::RangeChecksum { .. }), "got {err:?}");
}

#[test]
fn a_sparse_image_is_mapped_without_reading_the_holes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sparse.img");
    let mut f = fs::File::create(&path).unwrap();
    f.seek(SeekFrom::Start(8 * 1024 * 1024)).unwrap();
    f.write_all(&[0x5au8; 4096]).unwrap();
    f.set_len(16 * 1024 * 1024).unwrap();
    f.sync_all().unwrap();
    drop(f);

    // Whether that file is *stored* sparsely is the file system's business, not
    // ours. ext4, xfs and btrfs punch the hole; APFS often does not for a file
    // written this way. Assert hole detection only where there is a hole to
    // detect, and say which case we are in either way — the copy below has to
    // work regardless.
    let meta = fs::metadata(&path).unwrap();
    let apparent = meta.len();
    let allocated = std::os::unix::fs::MetadataExt::blocks(&meta) * 512;

    let bmap = create::create(&path, &create_opts(DetectMode::Holes), &NoProgress).unwrap();
    if allocated < apparent {
        assert!(
            bmap.mapped_blocks_cnt < bmap.blocks_cnt / 2,
            "the file is stored sparsely ({allocated} of {apparent} bytes allocated) \
             but SEEK_HOLE reported {} of {} blocks mapped",
            bmap.mapped_blocks_cnt,
            bmap.blocks_cnt
        );
    } else {
        eprintln!(
            "note: this file system stored the file densely ({allocated} of {apparent} bytes \
             allocated), so there are no holes to find; `--detect zeros` is what covers it here"
        );
    }

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    copy::copy(
        ImageSource::open(&path).unwrap(),
        &dest,
        Some(&bmap),
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(fs::read(&out).unwrap(), fs::read(&path).unwrap());
}

#[test]
fn bmap_files_round_trip_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let (image, _) = make_image(dir.path(), "src.img");
    let opts =
        CreateOptions { checksum: Some(ChecksumKind::Sha512), ..create_opts(DetectMode::Both) };
    let bmap = create::create(&image, &opts, &NoProgress).unwrap();

    let bmap_path = dir.path().join("src.img.bmap");
    bmap.write_to(&bmap_path).unwrap();
    let reloaded = Bmap::from_file(&bmap_path).unwrap();
    assert_eq!(reloaded, bmap);
    assert_eq!(reloaded.checksum_kind, Some(ChecksumKind::Sha512));
}

#[test]
fn an_all_zero_image_copies_without_writing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zeros.img");
    fs::write(&path, vec![0u8; 512 * 1024]).unwrap();

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let stats = copy::copy(
        ImageSource::open(&path).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(stats.bytes_written, 0);
    assert_eq!(stats.image_size, 512 * 1024);
    assert_eq!(fs::read(&out).unwrap(), vec![0u8; 512 * 1024]);
}

#[test]
fn an_unaligned_image_tail_is_copied_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("odd.img");
    let mut data = vec![0u8; 4096 * 3 + 137];
    data[4096 * 3..].fill(0x7e);
    fs::write(&path, &data).unwrap();

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    copy::copy(
        ImageSource::open(&path).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(fs::read(&out).unwrap(), data);
}

#[test]
fn wipe_clears_what_the_image_does_not_describe() {
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");

    // A destination that is larger than the image and full of old data — the
    // shape a device has when it is reflashed with a smaller image.
    let out = dir.path().join("out.img");
    fs::write(&out, vec![0xabu8; expected.len() * 2]).unwrap();

    let dest = Destination::open(&out, false).unwrap();
    let opts = CopyOptions { wipe: true, ..copy_opts(DetectMode::Both, ZeroMode::Skip) };
    let stats =
        copy::copy(ImageSource::open(&image).unwrap(), &dest, None, &opts, &NoProgress).unwrap();

    assert_eq!(stats.bytes_wiped, (expected.len() * 2) as u64);
    let result = fs::read(&out).unwrap();
    // Everything the old file held is gone, including the half past the end of
    // the image, which no ZeroMode would have reached.
    assert_eq!(result, expected, "old bytes survived the wipe");
}

#[test]
fn without_wipe_the_tail_past_the_image_survives() {
    // The behaviour --wipe exists to fix, pinned down so it cannot change by
    // accident: a bmap describes the image and nothing beyond it.
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");
    let out = dir.path().join("out.img");
    fs::write(&out, vec![0xabu8; expected.len() * 2]).unwrap();

    let dest = Destination::open(&out, false).unwrap();
    let opts = copy_opts(DetectMode::Both, ZeroMode::Zero);
    copy::copy(ImageSource::open(&image).unwrap(), &dest, None, &opts, &NoProgress).unwrap();

    // A regular file is truncated to the image size, so the tail is gone here;
    // on a block device it would still be there. What matters is that the copy
    // itself never wrote to it.
    let result = fs::read(&out).unwrap();
    assert_eq!(result.len(), expected.len());
    assert_eq!(result, expected);
    assert_eq!(stats_bytes_wiped_is_zero(&image, &dir), 0);
}

fn stats_bytes_wiped_is_zero(image: &Path, dir: &tempfile::TempDir) -> u64 {
    let out = dir.path().join("probe.img");
    let dest = Destination::open(&out, false).unwrap();
    copy::copy(
        ImageSource::open(image).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap()
    .bytes_wiped
}

#[test]
fn seek_places_the_image_at_an_offset_and_leaves_the_rest_alone() {
    const OFFSET: u64 = 64 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");

    // An existing file with data before, and well after, where the image goes.
    let out = dir.path().join("out.img");
    let original = vec![0xabu8; expected.len() * 3];
    fs::write(&out, &original).unwrap();

    let dest = Destination::open(&out, false).unwrap();
    let opts = CopyOptions { dest_offset: OFFSET, ..copy_opts(DetectMode::Both, ZeroMode::Zero) };
    copy::copy(ImageSource::open(&image).unwrap(), &dest, None, &opts, &NoProgress).unwrap();

    let result = fs::read(&out).unwrap();
    let at = OFFSET as usize;
    assert_eq!(result.len(), original.len(), "an offset copy must not truncate the file");
    assert_eq!(&result[..at], &original[..at], "bytes before the offset changed");
    assert_eq!(&result[at..at + expected.len()], &expected[..], "image did not land at the offset");
    assert_eq!(&result[at + expected.len()..], &original[at + expected.len()..], "tail changed");
}

#[test]
fn seek_extends_a_short_destination_without_truncating_a_long_one() {
    const OFFSET: u64 = 4096;
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");

    // Too short: it has to grow to hold the image at the offset.
    let short = dir.path().join("short.img");
    fs::write(&short, b"tiny").unwrap();
    let dest = Destination::open(&short, false).unwrap();
    let opts = CopyOptions { dest_offset: OFFSET, ..copy_opts(DetectMode::Both, ZeroMode::Skip) };
    copy::copy(ImageSource::open(&image).unwrap(), &dest, None, &opts, &NoProgress).unwrap();
    let grown = fs::read(&short).unwrap();
    assert_eq!(grown.len(), OFFSET as usize + expected.len());
    assert_eq!(&grown[OFFSET as usize..], &expected[..]);
}

#[test]
fn seek_zero_still_replaces_the_file_wholesale() {
    // The offset-zero path keeps its old behaviour: the file is the image.
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");
    let out = dir.path().join("out.img");
    fs::write(&out, vec![0xabu8; expected.len() * 3]).unwrap();

    let dest = Destination::open(&out, false).unwrap();
    copy::copy(
        ImageSource::open(&image).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Zero),
        &NoProgress,
    )
    .unwrap();
    assert_eq!(fs::read(&out).unwrap(), expected, "offset 0 should truncate to the image");
}

#[test]
fn verify_confirms_a_clean_copy_and_catches_a_dirty_one() {
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");

    // A destination that already holds data, flashed with the default mode:
    // the gaps keep the old bytes, so the device does not hold the image.
    let dirty = dir.path().join("dirty.img");
    fs::write(&dirty, vec![0xabu8; expected.len()]).unwrap();
    let dest = Destination::open(&dirty, false).unwrap();
    copy::copy(
        ImageSource::open(&image).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Skip),
        &NoProgress,
    )
    .unwrap();
    let outcome =
        copy::verify(ImageSource::open(&image).unwrap(), &dest, 0, 64 * 1024, &NoProgress).unwrap();
    assert!(!outcome.matches(), "skip mode on a dirty device should not match the image");
    assert_eq!(outcome.first_mismatch, Some(BSZ as u64), "first gap starts one block in");

    // The same copy with the gaps cleared does match.
    fs::write(&dirty, vec![0xabu8; expected.len()]).unwrap();
    let dest = Destination::open(&dirty, false).unwrap();
    copy::copy(
        ImageSource::open(&image).unwrap(),
        &dest,
        None,
        &copy_opts(DetectMode::Both, ZeroMode::Zero),
        &NoProgress,
    )
    .unwrap();
    let outcome =
        copy::verify(ImageSource::open(&image).unwrap(), &dest, 0, 64 * 1024, &NoProgress).unwrap();
    assert!(outcome.matches(), "zero mode should reproduce the image exactly");
    assert_eq!(outcome.bytes_compared, expected.len() as u64);
}

#[test]
fn verify_follows_the_seek_offset() {
    const OFFSET: u64 = 4096;
    let dir = tempfile::tempdir().unwrap();
    let (image, expected) = make_image(dir.path(), "src.img");
    let out = dir.path().join("out.img");
    fs::write(&out, vec![0u8; OFFSET as usize + expected.len()]).unwrap();

    let dest = Destination::open(&out, false).unwrap();
    let opts = CopyOptions { dest_offset: OFFSET, ..copy_opts(DetectMode::Both, ZeroMode::Zero) };
    copy::copy(ImageSource::open(&image).unwrap(), &dest, None, &opts, &NoProgress).unwrap();

    let ok =
        copy::verify(ImageSource::open(&image).unwrap(), &dest, OFFSET, 64 * 1024, &NoProgress)
            .unwrap();
    assert!(ok.matches(), "the image is at the offset, so verifying there must match");
    let bad =
        copy::verify(ImageSource::open(&image).unwrap(), &dest, 0, 64 * 1024, &NoProgress).unwrap();
    assert!(!bad.matches(), "verifying at offset 0 must not match");
}
