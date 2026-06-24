# Contributing to Trillian

Thanks for your interest in improving Trillian! Contributions of all kinds are
welcome — bug reports, fixes, performance work, documentation, and new SPARQL
features.

By participating, you agree to abide by our
[Code of Conduct](CODE_OF_CONDUCT.md).

## How contributions are accepted (governance)

Trillian is maintained by **42grad GmbH**. Anyone may open a pull request; the
maintainers review every change and **decide what gets merged**. We may decline
or request changes to a contribution — for example if it doesn't fit the
project's scope or direction, isn't sufficiently tested, or would compromise
correctness, performance, or maintainability. A rejection is never personal; it
keeps the project coherent.

To avoid wasted effort on larger work, **open an issue first** and agree on the
approach before writing a big PR. Small, focused PRs are easiest to review and
merge.

## Branching model and releases

Two long-lived branches, both protected — no direct pushes; changes land only
through reviewed pull requests, and only maintainers can merge:

- **`master`** — always releasable. Every release is a tagged commit here.
- **`develop`** — integration branch where reviewed work accumulates between
  releases.

The flow:

1. Anyone may fork or branch and open PRs. Create a `feature/<short-name>`
   branch off `develop`.
2. Open a PR **into `develop`**. CI (fmt + clippy + tests) must pass and a
   maintainer must approve and merge it.
3. To cut a release, a maintainer opens a PR **from `develop` into `master`**
   that bumps the version (see below).
4. After it merges, a maintainer tags the release on `master`:
   ```bash
   git checkout master && git pull
   git tag v1.2.3 && git push origin v1.2.3
   ```
   The tag triggers the Release workflow, which builds the binaries and
   publishes a GitHub Release with downloadable archives (Linux + macOS).

Direct pushes to `master` and `develop` are disabled — everything goes through
PRs reviewed and merged by a maintainer.

### Versioning

Trillian follows [Semantic Versioning](https://semver.org/). The `version` in
`Cargo.toml` is the source of truth. In the `develop` → `master` release PR:

1. Bump `version` in `Cargo.toml`.
2. Move the `## [Unreleased]` entries in [CHANGELOG.md](CHANGELOG.md) under a new
   `## [x.y.z] - YYYY-MM-DD` heading.
3. Tag the merged commit `vx.y.z` (matching the `Cargo.toml` version).

## Reporting bugs and requesting features

- **Bugs** and **feature requests** go through GitHub Issues — please use the
  templates. A good bug report includes a minimal SPARQL query and dataset that
  reproduces the problem.
- Feature requests are welcome as proposals; the maintainers prioritize them
  against the project's roadmap and may keep, defer, or decline them.

## Security

**Do not report security issues in public issues or PRs.** See
[SECURITY.md](SECURITY.md) for the private disclosure process.

## Developer Certificate of Origin (DCO)

Trillian uses the [Developer Certificate of Origin](https://developercertificate.org/)
instead of a CLA. It is a lightweight statement that you wrote the contribution
or otherwise have the right to submit it under the project's license.

Certify it by signing off your commits:

```bash
git commit -s -m "your message"
```

This appends a line to the commit message:

```
Signed-off-by: Your Name <you@example.com>
```

By signing off, you agree to the DCO (full text at the link above). Pull
requests whose commits are not signed off cannot be merged.

## Ground rules

- **License.** Contributions are made under the project's [Apache-2.0](LICENSE)
  license.
- **CI must be green.** Every change must pass:
  ```bash
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test
  ```
- **Tests.** New behaviour comes with tests; bug fixes come with a regression
  test that fails before the fix.
- **Comments and docs in English.**
- **Commit messages** explain the *why*, not just the *what*.

## Getting started

```bash
cargo build --release        # builds the `trillian` and `server` binaries
cargo test                   # runs the unit/integration suite
```

The toolchain is pinned in `rust-toolchain.toml`, so rustup uses the right
version automatically.

### Pre-commit hook (recommended)

Enable the bundled hook once to run the full pipeline (fmt + clippy + tests)
before every commit — the same checks CI runs:

```bash
git config core.hooksPath .githooks
```

Bypass it in an emergency with `git commit --no-verify`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for an overview of the storage layout,
the query engine, and the on-disk snapshot format.
