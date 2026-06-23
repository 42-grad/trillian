# Contributing to Trillian

Thanks for your interest in improving Trillian! Contributions of all kinds are
welcome — bug reports, fixes, performance work, documentation, and new SPARQL
features.

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

See [ARCHITECTURE.md](ARCHITECTURE.md) for an overview of the storage layout,
the query engine, and the on-disk snapshot format.
