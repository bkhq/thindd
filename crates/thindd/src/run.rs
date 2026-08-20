//! Subcommand implementations.

use crate::{
    cli::{Cli, Command, CopyArgs, CreateArgs, InfoArgs},
    output,
    progress::BarProgress,
};
use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};
use thindd_core::{
    Bmap, Compression, Destination, ImageSource, ZeroMode,
    bmap::{bmap_candidates, default_bmap_path, human_size},
    copy::{self, CopyOptions},
    create::{self, CreateOptions},
    dest::DestKind,
    sysfs::BdevTuning,
};

/// Route the parsed command line to the right handler.
pub(crate) fn dispatch(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Copy(args) => cmd_copy(cli, args),
        Command::Create(args) => cmd_create(cli, args),
        Command::Info(args) => cmd_info(args),
    }
}

fn show_progress(cli: &Cli) -> bool {
    !cli.no_progress && !cli.quiet && std::io::IsTerminal::is_terminal(&std::io::stderr())
}

fn cmd_copy(cli: &Cli, args: &CopyArgs) -> Result<()> {
    let source = open_source(&args.image, args.decompress.into())?;
    let bmap = resolve_bmap(args)?;

    let dest = Destination::open(&args.dest, args.force)
        .with_context(|| format!("cannot open destination '{}'", args.dest.display()))?;

    // Tune the block device for bulk sequential writes; the guard puts the
    // original settings back when it drops, including on error.
    let _tuning = (dest.kind() == DestKind::BlockDevice).then(|| {
        let tuning = BdevTuning::apply(dest.rdev());
        if !tuning.is_complete() && !cli.quiet {
            output::warn(
                "could not tune the block device (needs root); the copy will still work, \
                 but the system may be less responsive while it runs",
            );
        }
        tuning
    });

    let opts = CopyOptions {
        detect: args.detect.into(),
        zero_mode: args.mode.into(),
        verify: !args.no_verify,
        sync: !args.no_sync,
        block_size: args.block_size,
        batch_bytes: usize::try_from(args.bs).unwrap_or(thindd_core::DEFAULT_BATCH_BYTES),
        queue_depth: args.queue_depth.max(1),
        sync_watermark: (args.sync_every > 0).then_some(args.sync_every),
        wipe: args.wipe,
        zap: args.zap.then_some(thindd_core::dest::ZAP_SPAN),
        dest_offset: args.seek,
    };

    announce_copy(cli, args, bmap.as_ref(), &source, &dest, &opts);

    let progress = BarProgress::new(show_progress(cli));
    let stats = copy::copy(source, &dest, bmap.as_ref(), &opts, &progress).with_context(|| {
        format!("copying '{}' to '{}'", args.image.display(), args.dest.display())
    })?;

    if !cli.quiet {
        output::report_copy(&stats);
    }

    if args.verify {
        if !cli.quiet {
            output::note("reading the destination back to compare it against the image");
        }
        let source = open_source(&args.image, args.decompress.into())?;
        let progress = BarProgress::new(show_progress(cli));
        let outcome = copy::verify(source, &dest, opts.dest_offset, opts.batch_bytes, &progress)
            .with_context(|| format!("verifying '{}'", args.dest.display()))?;
        match outcome.first_mismatch {
            None => {
                if !cli.quiet {
                    output::note(&format!(
                        "verified: {} read back identical to the image",
                        human_size(outcome.bytes_compared)
                    ));
                }
            }
            Some(at) => {
                bail!(
                    "verification failed: '{}' differs from the image at byte {at} ({}){}",
                    args.dest.display(),
                    human_size(at),
                    if opts.zero_mode == ZeroMode::Skip && !opts.wipe {
                        "\n  the default --mode skip leaves the destination's previous contents \
                         wherever the image is zero; use --mode zero, or --zap/--wipe, if the \
                         device is to hold nothing but this image"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    Ok(())
}

fn cmd_create(cli: &Cli, args: &CreateArgs) -> Result<()> {
    let from_stdin = args.image == Path::new("-");
    if from_stdin && args.output.is_none() {
        bail!("reading the image from standard input needs an explicit -o/--output");
    }

    let opts = CreateOptions {
        block_size: args.block_size,
        checksum: args.checksum.into(),
        detect: args.detect.into(),
        batch_bytes: usize::try_from(args.bs).unwrap_or(thindd_core::DEFAULT_BATCH_BYTES),
        decompress: args.decompress.into(),
    };

    let source = open_source(&args.image, opts.decompress)?;
    if !cli.quiet && source.compression() != Compression::None {
        output::note(&format!(
            "image is {}-compressed; the map will describe the decompressed image",
            source.compression()
        ));
    }

    let progress = BarProgress::new(show_progress(cli));
    let bmap = create::create_from(source, &opts, &progress)
        .with_context(|| format!("creating a bmap for '{}'", args.image.display()))?;

    let output_path = args.output.clone().unwrap_or_else(|| default_output_path(&args.image));
    if output_path == Path::new("-") {
        output::stdout_write(&bmap.render()).context("writing the bmap to stdout")?;
    } else {
        bmap.write_to(&output_path)
            .with_context(|| format!("writing '{}'", output_path.display()))?;
        if !cli.quiet {
            output::note(&format!("wrote {}", output_path.display()));
        }
    }

    if !cli.quiet {
        output::note(&format!(
            "{} of {} needs copying ({:.1}%), in {} range(s)",
            human_size(bmap.mapped_bytes()),
            human_size(bmap.image_size),
            bmap.mapped_percent(),
            bmap.ranges.len()
        ));
    }
    Ok(())
}

/// Where `create` writes when `-o` is not given.
///
/// For a compressed image this drops the compression suffix, because the map
/// describes the decompressed image: `core.wic.gz` → `core.wic.bmap`. That name
/// is also the one `copy` looks for when handed either file.
fn default_output_path(image: &Path) -> PathBuf {
    bmap_candidates(image).pop().unwrap_or_else(|| default_bmap_path(image))
}

fn cmd_info(args: &InfoArgs) -> Result<()> {
    let bmap = Bmap::from_file(&args.bmap)
        .with_context(|| format!("reading '{}'", args.bmap.display()))?;
    output::describe_bmap(&bmap, args.ranges);
    Ok(())
}

/// Open the image, honouring `-` for standard input and decompressing when
/// asked to.
fn open_source(path: &Path, decompress: thindd_core::DecompressMode) -> Result<ImageSource> {
    if path == Path::new("-") {
        return ImageSource::from_reader_auto(Box::new(std::io::stdin()), "-", decompress)
            .context("cannot read the image from standard input");
    }
    ImageSource::open_auto(path, decompress)
        .with_context(|| format!("cannot open image '{}'", path.display()))
}

/// Work out which bmap file, if any, to use.
fn resolve_bmap(args: &CopyArgs) -> Result<Option<Bmap>> {
    if args.no_bmap {
        return Ok(None);
    }
    if let Some(explicit) = &args.bmap {
        let bmap = Bmap::from_file(explicit)
            .with_context(|| format!("reading bmap file '{}'", explicit.display()))?;
        return Ok(Some(bmap));
    }
    // `image.wic.gz` should find `image.wic.bmap` as readily as
    // `image.wic.gz.bmap` — the map describes the decompressed image either way.
    for guess in bmap_candidates(&args.image) {
        if guess.is_file() {
            let bmap = Bmap::from_file(&guess)
                .with_context(|| format!("reading bmap file '{}'", guess.display()))?;
            tracing::debug!(path = %guess.display(), "using discovered bmap file");
            return Ok(Some(bmap));
        }
    }
    Ok(None)
}

/// Tell the user what is about to happen, before the progress bar takes over.
fn announce_copy(
    cli: &Cli,
    args: &CopyArgs,
    bmap: Option<&Bmap>,
    source: &ImageSource,
    dest: &Destination,
    opts: &CopyOptions,
) {
    if cli.quiet {
        return;
    }
    match bmap {
        Some(b) => output::note(&format!(
            "using a bmap: {} of {} mapped ({:.1}%)",
            human_size(b.mapped_size()),
            human_size(b.image_size),
            b.mapped_percent()
        )),
        None => output::note(&format!(
            "no bmap file for '{}'; discovering what to copy while reading (detect={})",
            args.image.display(),
            opts.detect
        )),
    }
    if source.compression() != Compression::None {
        output::note(&format!(
            "image is {}-compressed; decoding it as we go",
            source.compression()
        ));
    }
    if let Some(size) = source.size() {
        output::note(&format!("image size {}", human_size(size)));
    }
    // Any destination with a capacity is a device, whatever the kernel calls
    // it: a Linux block device or a macOS raw disk.
    if dest.capacity().is_some() && opts.zero_mode == ZeroMode::Skip && !opts.wipe {
        output::note(
            "unmapped areas are left untouched, so anything the device held before survives \
             between the image's data; pass --mode zero to clear them, or --wipe to clear \
             the whole device including the space past the end of the image",
        );
    }
    if opts.dest_offset > 0 {
        output::note(&format!(
            "writing at offset {} ({} bytes) on {}",
            human_size(opts.dest_offset),
            opts.dest_offset,
            args.dest.display()
        ));
    }
    if let Some(span) = opts.zap {
        match dest.capacity() {
            Some(capacity) => output::note(&format!(
                "clearing {} at each end of {} (total {}), where the partition table and its \
                 backup live",
                human_size(span),
                dest.path().display(),
                human_size(span.saturating_mul(2).min(capacity))
            )),
            None => output::note(&format!(
                "{} has no ends to clear; the copy replaces it outright",
                dest.path().display()
            )),
        }
    }
    if opts.wipe {
        match dest.capacity() {
            Some(capacity) if dest.has_fast_zero() => output::note(&format!(
                "clearing all {} of {} first",
                human_size(capacity),
                dest.path().display()
            )),
            Some(capacity) => output::note(&format!(
                "clearing all {} of {} first by writing zeroes — this platform has no \
                 in-kernel zeroing, so expect it to take as long as writing the whole device",
                human_size(capacity),
                dest.path().display()
            )),
            // wipe() refuses this case; say why before it does.
            None => output::warn(&format!(
                "{} reports no size, so there is nothing to clear",
                dest.path().display()
            )),
        }
    }
}
