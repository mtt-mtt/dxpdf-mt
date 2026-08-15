# Release Gates

dxpdf-mt evaluates controlled business use and general DOCX compatibility as
separate product tracks. A result in one track does not silently waive the
requirements of the other.

## Defect severity

| Severity | Meaning | Examples |
|---|---|---|
| P0 | Incorrect or unusable business output | missing amount, missing text, corrupt PDF |
| P1 | Material layout failure | clipping, overlap, lost table header, misplaced signature area |
| P2 | Visible but usable fidelity difference | wrapping, spacing or font substitution |
| P3 | Cosmetic difference | decoration or small pixel-level variation |

P0 and P1 must be zero in the approved business corpus.

## Candidate identity

Every qualification run records:

- Git revision and worktree cleanliness;
- release binary size and SHA-256;
- Rust/Skia target and operating system;
- controlled font-pack identity and hashes;
- input corpus manifest and reference-renderer identity;
- render options, resource limits and concurrency;
- machine-readable per-document results and a human-readable decision.

Results without this identity are diagnostic evidence, not a release baseline.

## Engineering gate

The candidate must pass:

```text
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo build --no-default-features
cargo build --release
```

Rendering or font changes also require targeted before/after visual review.

## Controlled business gate

- all approved business fixtures convert successfully;
- no crash or timeout;
- P0 = 0 and P1 = 0;
- source semantic coverage meets the approved template contract;
- expected page structure is preserved for critical templates;
- representative repeated conversions are deterministic;
- isolated-process failure tests cover timeout, memory limit, crash and cleanup;
- performance is measured under the intended concurrency and font environment.

Rollout starts in shadow mode, then moves to an allowlist of templates with an
automatic fallback converter. It does not begin as a global replacement.

## General compatibility gate

The broad corpus gate additionally tracks conversion success, source semantic
coverage, physical and non-empty page structure, performance percentiles and
visual defect clusters across representative English and Chinese documents.
Known reference-PDF defects, revision markup and field-error output must be
classified instead of being hidden inside one aggregate character count.
