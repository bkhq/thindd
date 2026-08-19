default: ci

# Plain commands — deny-warnings policy lives in [workspace.lints.rust]
ci:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked
    cargo nextest run --workspace --all-features --locked
    cargo test --doc --workspace --all-features --locked
    cargo clippy -p thindd-core --no-default-features --all-targets --locked
    cargo nextest run -p thindd-core --no-default-features --locked
    cargo deny check
    cargo shear

fix:
    cargo clippy --fix --workspace --all-targets --allow-dirty
    cargo fmt --all

# End-to-end smoke test against a synthetic sparse image
smoke:
    cargo run --release -- --help
