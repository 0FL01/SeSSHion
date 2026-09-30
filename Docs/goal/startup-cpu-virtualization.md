# Goal: CPU and virtualization in the startup snapshot

Status: complete
Source: approved CPU/container/VM audit plan; user: “реализовать и коммит билд”
Last updated: 2026-10-01

## Objective
Extend the existing immutable rootless startup JSON with CPU models and observed
container/machine-virtualization evidence, build and commit locally without push.
The completed `host-environment.md` objective remains historical and unchanged.

## Execution Directive
Complete the frozen Required Outcomes using the listed Change Envelope and Primary
Evidence. Work on the smallest unresolved outcome. Do not add requirements from
reviews, tests, tools, speculative risks, or optional source text. Finish when every
required outcome is resolved and affected constraints remain satisfied.

## Frozen Contract

- R1: Add `cpu_models:null|array` and `virtualization:{container:null|string,vm:null|string}`.
  - Source: approved audit plan, sections CPU and JSON contract.
  - Acceptance: 14 fixed fields; at most four unique sorted models, each trimmed,
    valid UTF-8, control-free and at most 256 bytes. Only `model name` identifies CPU.
  - Primary evidence: nearest environment parser and POSIX/BusyBox tests.
  - Status: verified
  - Evidence: `cargo test --lib ssh::environment -- --nocapture`: 12 passed,
    including builtin extraction without utilities, bounds, partial framing and real BusyBox.
- R2: Minimal positive-evidence container/VM classification without extra packages.
  - Source: approved source precedence and virtualization semantics.
  - Acceptance: named container declaration before Podman/Docker markers; generic
    OCI can use markers; direct Xen before narrow DMI families; KVM refines QEMU;
    exact CPU `hypervisor` token is generic evidence, not `vmx`/`svm`. `null` is
    inconclusive, `unknown` is positive unidentified evidence, never bare-metal proof.
  - Primary evidence: focused classification and builtin source-extraction tests.
  - Status: verified
  - Evidence: focused source-precedence/classification tests plus actual builtin
    marker/declaration/DMI extraction passed in the 12-test environment group.
- R3: Preserve rootless, bounded, frozen startup and unchanged tool discovery.
  - Source: approved integration and verification sections; previous startup contract.
  - Acceptance: new records before ELF; completed prefixes survive; shared three-second
    total deadline, 64 KiB stdout/4 KiB stderr; no elevation; six/four tools unchanged;
    init/discovery instructions remain byte-stable after commands/reconnect.
  - Primary evidence: existing SSH/stdio environment tests and tool wire-budget test.
  - Status: verified
  - Evidence: `cargo test --test docker_integration_test startup_environment -- --nocapture`:
    7 passed (rootless/frozen/reconnect, unavailable/auth fallback, init/discovery,
    partial/flood/cancellation, exotic sources, Fish); wire budget remains 3192/3200 bytes.
- R4: Release build and local conventional commit, no push.
  - Source: user implementation/build/commit request and repository gates.
  - Acceptance: fmt, clippy, all-feature tests/check and release build pass; intended
    source/tests/docs only committed; user configuration changes preserved and unstaged.
  - Primary evidence: gate output, release version, commit and final git status.
  - Status: verified
  - Evidence: fmt/clippy/all-feature tests/check/release build passed; 308 tests
    passed, 4 pre-existing ignored. `target/release/ssh-mcp --version`: 5.0.1.
    Source/tests/README committed as `5bacc19`; user configurations remain unstaged.

## Constraints and Non-goals
- Scan `/proc/cpuinfo` once with POSIX builtins: at most 128 lines and 64 KiB
  post-read processing budget, excluding a crossing line. This is not a hard remote
  allocation/input-byte cap; `read` consumes a whole line before checking length.
- Emit only bounded summaries, build JSON in Rust. No GPU/codecs, Device Tree,
  CPUID helper, cgroup taxonomy, complete nesting/guest-role detection or security proof.
- No new tools, dependencies, flags, caches, refresh, timestamps or version bump.
- Preserve user changes in `docker-compose.yml` and `opencode.jsonc`.

## Change Envelope
`src/ssh/environment.rs`, `src/server.rs`, nearest environment/stdio tests,
`README.md`, and this separate goal. No connection/startup/tool-schema redesign.

## Current Checkpoint
Closed: all required outcomes verified; no remaining implementation checkpoint.

## Current State
R1-R4 verified. New records precede ELF, startup/connection/tool-schema paths unchanged.
Historical goal and user configurations untouched. No push performed.

## Completion
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-features --verbose`: 200 library, 15 compact-response,
  79 Docker, 5 integration, 3 logging and 6 doctests passed; 4 existing ignored.
- `cargo check --all-features --verbose`
- `cargo build --release --all-features`; artifact: `target/release/ssh-mcp`.
- Source commit: `5bacc19 feat(environment): report startup CPU and virtualization`.
- Closure: diff stays inside the envelope; no dependencies, tool definitions,
  connection/startup behavior or version changed; no push or generated binary commit.
- User configuration diff hash remains `8ea384b984ef99983680ad3dd18c58d7ccc2dd54`.
