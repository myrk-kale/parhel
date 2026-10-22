## What and why

<!-- What changes, and the problem it solves. Link the issue. -->

## Checklist

- [ ] Tests written first (IDs added to `docs/test-catalog.md`)
- [ ] `cargo test --features failpoints` passes
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --check`
- [ ] Man pages regenerated if the CLI changed (`PARHEL_UPDATE_MAN=1 cargo test --test man`)
- [ ] `CHANGELOG.md` updated under `[Unreleased]`
