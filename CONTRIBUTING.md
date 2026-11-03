# Contributing to Whoopz

Before you build a feature, read [the vision](docs/vision.md) (including what Whoopz will never
do) and [the roadmap](docs/roadmap.md). When unsure, open an issue first, so no one spends time
on a pull request that doesn't fit.

Questions and chat: [Discord](https://discord.gg/RWuSdBhhFM). Bugs and feature requests: GitHub issues.

## Building

You need [rustup](https://rustup.rs), which picks up `rust-toolchain.toml`. Nothing else:
Whoopz talks to the kernel directly, without libseccomp.

```sh
cargo build
cargo run --bin whoops
```

The interactive tests use bash, and zsh and fish when they're installed (they're skipped
otherwise). To run them in every shell and in older versions, use the shell lab (needs Docker):
`scripts/shell-lab.sh`. CI runs it on every push.

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

## Releasing

1. Make sure everything for the release is under `## [Unreleased]` in `CHANGELOG.md`.
2. On an up-to-date `main`, run `scripts/release.sh 0.1.0`. It sets the version in
   `Cargo.toml`, moves the `[Unreleased]` entries under `## [0.1.0] - <today>`, updates the
   compare links, commits `chore: release 0.1.0` and tags `v0.1.0`. Nothing is pushed yet.
3. Look it over, then `git push && git push origin v0.1.0`.

Pushing the tag runs `.github/workflows/release.yml`. It checks the tag matches `Cargo.toml` and
the changelog has the version, runs the tests, builds static binaries for x86_64 and ARM64,
checks the x86_64 one on four distributions, and publishes a GitHub release with those
binaries, `SHA256SUMS`, and the version's changelog section as notes. A version with a dash
(`0.2.0-beta.1`) becomes a pre-release.

To try the whole build without publishing, run the workflow by hand: **Actions → release →
Run workflow**. The binaries are attached to the run.
