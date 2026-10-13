# Contributing

Thank you for considering contributing to this project!

## Development Setup

1. Install [Rust](https://rustup.rs/) (the toolchain is managed by [`rust-toolchain.toml`](rust-toolchain.toml)).
2. Clone the repository and run `cargo build`.

## Before Submitting a PR

Please make sure the repository's mandatory checks pass locally. The initial
implementation intentionally has no CI workflow:

```sh
cargo fmt --all
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## Commit Messages

Use clear, descriptive commit messages. Consider following the [Conventional Commits](https://www.conventionalcommits.org/) specification.
