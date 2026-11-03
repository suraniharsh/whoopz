# Contributing to Whoopz

Before you build a feature, read [the vision](docs/vision.md). When unsure, open an issue first,
so no one spends time on a pull request that doesn't fit.

## Building

You need [rustup](https://rustup.rs), which picks up `rust-toolchain.toml`. Nothing else:
Whoopz talks to the kernel directly, without libseccomp.

```sh
cargo build
cargo run --bin whoops
```

The interactive tests use bash, and zsh and fish when they're installed (they're skipped
otherwise).

Release builds are static, so one file runs on any distribution:

```sh
cargo build --release --target x86_64-unknown-linux-musl
scripts/portable-check.sh target/x86_64-unknown-linux-musl/release/whoops   # needs Docker
```

## Adding a shell

Whoopz needs a small hook per shell to know where commands start and end. The protocol, the
rules learned the hard way, and a step-by-step guide are in [docs/hooks.md](docs/hooks.md).

## Testing safely

Whoopz sits in the middle of every file delete. **Only try it in a throwaway folder or a VM**,
never in your home folder.

Every bug fix comes with a test that reproduces it. Integration tests should run real commands
against files in a temporary folder and check what `whoops` brings back.

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

CI runs the same three checks.

## Pull requests

Open them against `main`. Commit messages follow
[Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`, …).
Mention user-facing changes under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md).

## AI tools

Use whatever tools you like, but you must understand, review and test what you submit. Issues,
pull request descriptions and replies must be in your own words.

## License

Whoopz is [MIT](LICENSE) licensed, and your contributions are licensed under the same terms.
