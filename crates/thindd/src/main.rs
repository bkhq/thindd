#![forbid(unsafe_code)]

//! `thindd` — copy an image, writing only the blocks that matter.
//!
//! See the crate-level docs of [`thindd_core`] for how the block map works and
//! why an image full of zeroes flashes so much faster than `dd` manages.

mod cli;
mod output;
mod progress;
mod run;

use clap::Parser as _;
use std::process::ExitCode;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    init_tracing(&cli);
    install_panic_hook();

    match run::dispatch(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            output::error(&err);
            ExitCode::FAILURE
        }
    }
}

/// Wire up `tracing`. Verbosity comes from the flags; `THINDD_LOG` overrides
/// it entirely for anyone who wants per-module filters.
fn init_tracing(cli: &cli::Cli) {
    let default = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };
    let filter = EnvFilter::try_from_env("THINDD_LOG")
        .unwrap_or_else(|_| EnvFilter::new(format!("thindd={default},thindd_core={default}")));

    // Colour only when a human is actually looking. Redirecting the log to a
    // file used to bake escape sequences into it, which is unpleasant to read
    // and breaks anything that greps the output.
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr())
        && std::env::var_os("NO_COLOR").is_none();

    let registry = tracing_subscriber::registry().with(filter);
    if cli.log_json {
        registry.with(fmt::layer().json().with_writer(std::io::stderr)).init();
    } else {
        registry
            .with(
                fmt::layer()
                    .without_time()
                    .with_target(false)
                    .with_ansi(ansi)
                    .with_writer(std::io::stderr),
            )
            .init();
    }
}

/// Turn a panic into a structured log line before the process dies, so a bug
/// report has something to quote beyond "it crashed".
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(
            panic = %info,
            backtrace = %std::backtrace::Backtrace::force_capture(),
            "thindd panicked — this is a bug, please report it"
        );
        previous(info);
    }));
}
