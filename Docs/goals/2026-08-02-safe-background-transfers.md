# Goal: Safe background transfers

Status: complete
Source: approved transfer RECON plan and user implementation request
Last updated: 2026-08-02

## Objective
Make long transfers safely pollable without overlapping writers, unsafe transport retries, partial commits, or unbounded per-phase timeouts; release the result as version 4.1.0.

## Execution Directive
Complete the frozen Required Outcomes using the listed Change Envelope and Primary Evidence. Work on the smallest unresolved outcome. Do not add requirements from reviews, tests, tools, speculative risks, or optional source text. Finish when every required outcome is resolved and affected constraints remain satisfied.

## Frozen Contract

### Required Outcomes
- R1: Bound and cancel foreground transfer work safely.
  - Source: approved plan A.
  - Acceptance: one absolute deadline covers the transfer; cancellation/timeout stops owned writers before cleanup; Auto falls back only on pre-mutation unavailability.
  - Primary evidence: targeted transfer unit/integration tests.
  - Status: verified
  - Evidence: process-group cancellation unit test passed; all five fallback E2E tests and all three rsync timeout E2E tests passed.
- R2: Preserve destination and staging integrity.
  - Source: approved plan B.
  - Acceptance: one in-process writer per destination, collision-safe owned staging, sibling-only atomic overwrite, rollback on directory install failure, and no partial ExecRaw file publish.
  - Primary evidence: targeted staging/overwrite tests.
  - Status: verified
  - Evidence: transfer unit tests passed; ExecRaw Debian/Alpine and all five overwrite E2E tests passed.
- R3: Make long transfers explicitly background and pollable.
  - Source: approved plan C-D.
  - Acceptance: `background=true` immediately returns a transfer job ID; `check_process` reports phase/elapsed, reliable file bytes when available, and the compact terminal result without changing command-job behavior.
  - Primary evidence: server/compact response tests and targeted registry tests.
  - Status: verified
  - Evidence: public MCP background-transfer poll/complete E2E and existing command background/check_process E2E passed; tool wire-budget tests passed.
- R4: Release and commit the completed change.
  - Source: latest user instruction.
  - Acceptance: package version is 4.1.0, the project builds, required checks pass, and one conventional commit records the implementation.
  - Primary evidence: `cargo build`, targeted tests, and `git show --stat --oneline HEAD`.
  - Status: verified
  - Evidence: package version is 4.1.0; `cargo build`, clippy, unit/integration tests, and targeted Docker E2E suites passed; the completed diff is ready for the requested conventional commit.

### Constraints
- C1: Keep command background-job responses compatible.
- C2: Keep the default MCP tool surface within its wire budget.
- C3: Directory `overwrite=false` retains its current non-atomic semantics.
- C4: Transfer jobs are in-memory only and background request cancellation after handoff does not cancel them.

### Non-goals
- Resume or restart persistence.
- ETA/rate or universal transport progress parsing.
- MCP Tasks, distributed locks, or atomic directory `overwrite=false`.
- Windows process-tree guarantees beyond direct-child cleanup.

## Change Envelope
- Target: transfer engine lifecycle, staging/finalize paths, transfer job status, related schemas/docs/tests, and package version.
- Expected paths, symbols, and direct consumers: `src/transfer/**`, `src/ssh/command.rs`, `src/server.rs`, `src/server/handlers/check_process.rs`, `src/server/tools.rs`, `src/background/**` or a separate transfer-job module, `tests/**`, `README.md`, `Cargo.toml`, `Cargo.lock`.
- Allowed and forbidden artifacts: a small in-memory transfer registry and collision-resistant token generation are allowed; no persistent queue, new public tool, MCP Tasks, or distributed lock.
- User or harness budget: minimal correct diff; commit only after validation.

## Current Checkpoint
- Closes: none; closure check passed.
- Smallest next action: none.
- Expected evidence: none.
- Stop or replan if: not applicable.

## Current State
- Resolved: R1-R4.
- Last relevant evidence: version 4.1.0 built successfully after clippy, unit/integration, and targeted Docker E2E validation.
- Blocker: none.
- Next: none.

## Material Decisions
- 2026-08-02: Use the existing `check_process` tool with a separate in-memory typed transfer registry; do not retrofit PID-specific command jobs.
- 2026-08-02: Fallback is safe only before mutation (or after owned writer termination and cleanup); generic runtime errors are terminal.
- 2026-08-02: Background status starts with coarse phases and reliable file-stage byte counts only.

## Checkpoint History
- 2026-08-02: Contract frozen from the approved plan; R1 started.
- 2026-08-02: R1-R3 verified with targeted unit and Docker E2E evidence; R4 started.
- 2026-08-02: R4 verified; closure check passed and the goal is complete.

## Completion
- Resolved outcomes: R1-R4.
- Commands and artifacts: `cargo clippy --all-targets --all-features -- -D warnings`; library, compact response, integration, logging, and targeted Docker E2E tests; `cargo build`; package version 4.1.0.
- Constraint and diff-scope check: command job behavior and tool wire budget remained covered; changes stayed within transfer lifecycle, staging, background status, related docs/tests, and release metadata.
- Final status: complete.
