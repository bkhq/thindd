//! Transparent gzip handling, end to end.

#![cfg(feature = "gzip")]
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    reason = "test code: a failed setup step should fail the test loudly"
)]

use flate2::{Compression as Level, write::GzEncoder};
use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};
use thindd_core::{
    Compression, DecompressMode, Destination, Error, ImageSource,
    copy::{self, CopyOptions},
    create::{self, CreateOptions},
    filemap::DetectMode,
    progress::NoProgress,
};

const BS: u64 = 4096;

/// A 1 MiB image with three data islands and large zero runs between them.
fn payload() -> Vec<u8> {
    let mut data = vec![0u8; 1024 * 1024];
    data[0..4096].fill(0x11);
    data[400 * 1024..404 * 1024].fill(0x22);
    data[1024 * 1024 - 4096..].fill(0x33);
    data
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Level::default());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

/// Two gzip members concatenated, the way `pigz` and `cat a.gz b.gz` produce.
fn gzip_multi_member(bytes: &[u8]) -> Vec<u8> {
    let (head, tail) = bytes.split_at(bytes.len() / 2);
    let mut out = gzip(head);
    out.extend(gzip(tail));
    out
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn copy_opts() -> CopyOptions {
    CopyOptions {
        block_size: Some(BS),
        batch_bytes: 64 * 1024,
        queue_depth: 2,
        sync_watermark: None,
        ..CopyOptions::default()
    }
}

fn create_opts() -> CreateOptions {
    CreateOptions { block_size: Some(BS), ..CreateOptions::default() }
}

#[test]
fn a_gzipped_image_is_detected_and_copied() {
    let dir = tempfile::tempdir().unwrap();
    let expected = payload();
    let image = write(dir.path(), "core.wic.gz", &gzip(&expected));

    let source = ImageSource::open_auto(&image, DecompressMode::Auto).unwrap();
    assert_eq!(source.compression(), Compression::Gzip);
    assert_eq!(source.size(), None, "a decoded stream has no size up front");

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let stats = copy::copy(source, &dest, None, &copy_opts(), &NoProgress).unwrap();

    assert_eq!(fs::read(&out).unwrap(), expected);
    assert_eq!(stats.image_size, expected.len() as u64);
    // The zero runs are still elided: decompressing produces the zero bytes,
    // and we simply do not write them.
    assert!(stats.bytes_written <= 9 * BS, "wrote {} bytes", stats.bytes_written);
}

#[test]
fn a_multi_member_stream_is_decoded_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let expected = payload();
    let image = write(dir.path(), "core.wic.gz", &gzip_multi_member(&expected));

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    copy::copy(
        ImageSource::open_auto(&image, DecompressMode::Auto).unwrap(),
        &dest,
        None,
        &copy_opts(),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(fs::read(&out).unwrap(), expected);
}

#[test]
fn a_bmap_for_the_raw_image_drives_the_gzipped_one() {
    let dir = tempfile::tempdir().unwrap();
    let expected = payload();
    let raw = write(dir.path(), "core.wic", &expected);
    let bmap = create::create(&raw, &create_opts(), &NoProgress).unwrap();

    let image = write(dir.path(), "core.wic.gz", &gzip(&expected));
    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let stats = copy::copy(
        ImageSource::open_auto(&image, DecompressMode::Auto).unwrap(),
        &dest,
        Some(&bmap),
        &copy_opts(),
        &NoProgress,
    )
    .unwrap();

    assert_eq!(fs::read(&out).unwrap(), expected);
    // Range checksums were verified against the decoded stream.
    assert_eq!(stats.blocks_read, bmap.mapped_blocks_cnt);
}

#[test]
fn a_map_built_from_a_gzipped_image_matches_the_raw_one() {
    let dir = tempfile::tempdir().unwrap();
    let expected = payload();
    let raw = write(dir.path(), "core.wic", &expected);
    let gz = write(dir.path(), "core.wic.gz", &gzip(&expected));

    let from_raw = create::create(&raw, &create_opts(), &NoProgress).unwrap();
    let from_gz = create::create(&gz, &create_opts(), &NoProgress).unwrap();

    assert_eq!(from_gz, from_raw);
    assert_eq!(from_gz.render(), from_raw.render());
}

#[test]
fn a_gzipped_stream_on_stdin_works_too() {
    let dir = tempfile::tempdir().unwrap();
    let expected = payload();
    let compressed = gzip(&expected);

    let source = ImageSource::from_reader_auto(
        Box::new(std::io::Cursor::new(compressed)),
        "-",
        DecompressMode::Auto,
    )
    .unwrap();
    assert_eq!(source.compression(), Compression::Gzip);

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    copy::copy(source, &dest, None, &copy_opts(), &NoProgress).unwrap();
    assert_eq!(fs::read(&out).unwrap(), expected);
}

#[test]
fn decompress_never_treats_the_file_as_raw_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let compressed = gzip(&payload());
    let image = write(dir.path(), "core.wic.gz", &compressed);

    let source = ImageSource::open_auto(&image, DecompressMode::Never).unwrap();
    assert_eq!(source.compression(), Compression::None);
    assert_eq!(source.size(), Some(compressed.len() as u64));

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    copy::copy(source, &dest, None, &copy_opts(), &NoProgress).unwrap();
    assert_eq!(fs::read(&out).unwrap(), compressed);
}

#[test]
fn an_uncompressed_image_is_left_seekable_under_auto() {
    let dir = tempfile::tempdir().unwrap();
    let expected = payload();
    let image = write(dir.path(), "core.wic", &expected);

    let source = ImageSource::open_auto(&image, DecompressMode::Auto).unwrap();
    assert_eq!(source.compression(), Compression::None);
    assert_eq!(source.size(), Some(expected.len() as u64), "hole detection must stay available");
}

#[test]
fn a_truncated_gzip_stream_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let mut compressed = gzip(&payload());
    compressed.truncate(compressed.len() / 2);
    let image = write(dir.path(), "core.wic.gz", &compressed);

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let err = copy::copy(
        ImageSource::open_auto(&image, DecompressMode::Auto).unwrap(),
        &dest,
        None,
        &copy_opts(),
        &NoProgress,
    )
    .unwrap_err();

    assert!(matches!(err, Error::Io { .. }), "got {err:?}");
}

#[test]
fn detect_none_over_gzip_writes_every_decoded_byte() {
    let dir = tempfile::tempdir().unwrap();
    let expected = payload();
    let image = write(dir.path(), "core.wic.gz", &gzip(&expected));

    let out = dir.path().join("out.img");
    let dest = Destination::open(&out, false).unwrap();
    let opts = CopyOptions { detect: DetectMode::None, ..copy_opts() };
    let stats = copy::copy(
        ImageSource::open_auto(&image, DecompressMode::Auto).unwrap(),
        &dest,
        None,
        &opts,
        &NoProgress,
    )
    .unwrap();

    assert_eq!(fs::read(&out).unwrap(), expected);
    assert_eq!(stats.bytes_written, expected.len() as u64);
}
