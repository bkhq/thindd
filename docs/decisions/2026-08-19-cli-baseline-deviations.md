# ADR: PMA-Rust acceptance items that do not apply to this workspace

Status : Accepted
Date   : 2026-08-19
Sunset : reassess if this workspace ever grows a long-running service
Owner  : thindd maintainers

## Context

The PMA-Rust acceptance checklist is written for long-running network services.
This workspace ships a single short-lived CLI that reads a local image and
writes it to a local file or block device. Several checklist items have no
counterpart here, and pretending otherwise would add dependencies and ceremony
without buying any safety.

## Decision

The following checklist items are recorded as not applicable, with the reason:

| Item | Why it does not apply |
|---|---|
| rustls + `aws_lc_rs` provider installed in `main` | No TLS. No network I/O of any kind. `cargo deny bans` still rejects `openssl`/`native-tls` so this stays true. |
| `/healthz` + `/readyz` endpoints | No server. |
| Graceful shutdown via `axum::serve(...).with_graceful_shutdown` | No server, no async runtime. The copy engine is two OS threads joined by a scoped `thread::scope`; a failure on either side tears down the other through channel disconnection. |
| `secrecy::Secret<T>` for secrets | The process handles no credentials. |
| Layered config (defaults → file → env → CLI) | Every knob is a CLI flag. `THINDD_LOG` is the one env var, and it only selects a tracing filter. |
| Core dumps suppressed via `rlimit` | The process holds no secrets, so a core leaks nothing; a core from a flashing tool is a genuinely useful bug report. The structured panic hook (checklist item, implemented) is kept. |
| JSON logs by default in prod | This is an interactive tool; human-readable is the right default. `--log-json` switches formats for scripted use. |

The items that *do* apply are all implemented: edition 2024, declared MSRV,
`#![forbid(unsafe_code)]` in both crates, workspace-manifest deny-warnings
policy, typed per-crate errors with `thiserror` and `anyhow` only at the binary
entry point, the panic hook, the dual `release`/`dist` profiles, musl
`+crt-static` in `.cargo/config.toml`, and the full quality-gate set
(fmt, clippy, nextest, doctests, cargo-deny, cargo-shear, typos).

## Consequences

If this workspace ever grows a daemon (a flashing service on a production line,
say), this ADR must be revisited item by item rather than inherited.
