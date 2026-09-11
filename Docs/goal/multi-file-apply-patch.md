# Goal: Multi-file SSH patches in OpenCode

Status: active
Source: user-approved RECON plan; 2026-09-11 instruction to implement, build, commit, then test after restarting OpenCode
Last updated: 2026-09-11

## Objective

Apply multiple Add/Update/Delete sections in one `apply_patch` or
`sudo_apply_patch` call, without a file-count or hunk-count cap.

## Execution Directive

Complete the frozen Required Outcomes using the listed Change Envelope and
Primary Evidence. Work on the smallest unresolved outcome. Do not add
requirements from reviews, tests, tools, speculative risks, or optional source
text. Finish when every required outcome is resolved and affected constraints
remain satisfied.

## Frozen Contract

- R1: Implement the approved multi-file behavior.
  - Source: approved RECON plan, including removal of the proposed file-count cap.
  - Acceptance: parse all sections, preflight all snapshots/plans/size checks before
    target writes, commit sequentially, stop on failure and report per-file outcomes.
  - Primary evidence: focused parser/response tests and container checks in R3.
  - Status: in_progress
  - Evidence: implementation and local tests pass; remote behavior awaits R3.
- R2: Build and commit the implementation.
  - Source: “пиши код, билд коммит”.
  - Acceptance: release binary built and intended changes committed locally.
  - Primary evidence: `cargo build --release --locked`, Git commit.
  - Status: in_progress
  - Evidence: `cargo build --release --locked` passed; binary is
    `target/release/ssh-mcp`. Commit is the next action; use Git history as evidence.
- R3: Verify through OpenCode on the test container after restart.
  - Source: “потом после перезахода в опенкод протестируем на тестовом контейнере”.
  - Acceptance: refreshed schema; multi-file success, preflight no-write failure,
    partial commit/stop, and explicit sudo batch verified against remote contents.
  - Primary evidence: MCP calls after user restarts OpenCode.
  - Status: pending

Constraints: keep `{patch:string}`, tool names, exact matching, absolute paths,
duplicate-path rejection, 1 MiB per source/result file, SHA conflict checks,
per-file staging/locks, and explicit sudo gates. Single-file JSON stays compatible.
SSH commit loss is `unknown`, not proof of no write; never automatically retry.

Non-goals: batch atomicity/rollback, rename, parallel commits, background jobs,
dependencies, persistent state, OpenCode plugins/config changes, push/release.

## Change Envelope

Parser and handler: `src/patch.rs`, `src/server/handlers/apply_patch.rs`.
Direct consumers: tool schemas/parameter docs and existing parser, schema, and
Docker tests. Update README, crate docs and stale AGENTS parser description.
Reuse `file_edit_common.rs` unchanged. No new public API or dependencies.

## Current Checkpoint

Review the final diff and commit R2. Then wait for the requested OpenCode restart
before R3. Do not run container checks in the implementation session.

## Current State

Implemented one-envelope parsing with duplicate-path rejection and no count cap;
all-file preflight, ordered commits, per-file statuses, and legacy single-file JSON.
Syntax errors use the simple error response before file outcomes exist.
The remote snapshot/staging/locking code and dependencies are unchanged.

Local evidence (2026-09-11):
- `cargo fmt --all -- --check` passed.
- `cargo test --lib --locked patch`: 9 passed.
- `cargo test --lib --locked server::tools::tests`: 5 passed.
- `cargo clippy --all-targets --all-features --locked -- -D warnings` passed,
  including compilation of the extended Docker tests.
- `cargo build --release --locked` passed.

The first schema-budget check failed at 3628 bytes; shortening only patch tool
descriptions restored the existing 3200-byte gate. No gate was relaxed.

Docker/MCP tests have NOT run for this change. The existing
`test_mcp_tools_with_docker` now covers mixed Add/Update/Delete/no-op success,
second-file preflight failure with no writes, permission-denied partial commit
with stop/no elevation, and password-based sudo batching. After restart, verify
the live MCP tool descriptions advertise multi-file support, then exercise these
scenarios through `ssh-mcp-test-environment` in an isolated temporary directory.
Check remote contents, not just JSON; inspect unknown outcomes before retrying.
