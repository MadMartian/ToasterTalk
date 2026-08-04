# Contributing to ToasterTalk

Thanks for your interest in contributing!

## Before you start

This project uses **Spec-Driven Development (SDD)**: the documentation under `sdd/` is the authoritative description of what the system does and how it must be developed.

- Start with `AGENTS.md` — it indexes the SDD documents by task.
- `sdd/POLICY.md` contains hard development rules (build pipeline, logging, architectural constraints). Changes that violate it will not be merged.
- `sdd/TDD.md` is the behavioural contract. New behaviour needs new rubrics; changed behaviour needs its rubrics updated in the same PR.

## Build pipeline

Every change must pass, in order, with **zero warnings**:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo build --release
cargo test --release
```

Changes touching session lifecycle, the inbox/receive slot, send-with-reply, the transport, or the TTL sweep must also run the manual suite in `toaster-talk-server/tests/MANUAL-TEST.md`.

## Pull requests

- Keep PRs scoped to one concern.
- Update the relevant `sdd/` documents in the same PR as the code they describe — stale docs are treated as defects.
- CI runs the pipeline above on Linux, macOS, and Windows; all three legs must be green.

## License

By contributing, you agree that your contributions are licensed under the [Apache License 2.0](LICENSE), per its Section 5.
