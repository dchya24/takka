# Contributing to takka

Thanks for your interest in contributing! 🎉

## Getting Started

1. Fork the repository and clone it.
2. Ensure you have a recent stable Rust toolchain (`rustup update stable`).
3. Run the checks locally before opening a PR:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo audit   # if installed
```

The CI pipeline runs exactly these checks, so passing locally means a green PR.

## Project Layout

- `takka-core/` — library crate with the agent core (memory, tools, safety, model providers)
- `takka-cli/` — binary crate (`takka`) providing the interactive CLI

## Code Style

- Follow `rustfmt` (run `cargo fmt`).
- No clippy warnings (`-D warnings`).
- Keep changes minimal and focused; prefer small, reviewable PRs.
- Add tests for new functionality; bug fixes should include a regression test.

## Commit Conventions

Use clear, imperative commit messages (e.g. `fix: make search test CWD-independent`,
`feat: add GitHub Actions CI`).

## Reporting Bugs

Open an issue with:

- What you expected vs. what happened
- Steps to reproduce
- Your environment (OS, Rust version, `cargo --version`)

## Security

See [SECURITY.md](SECURITY.md) for how to report vulnerabilities privately.
