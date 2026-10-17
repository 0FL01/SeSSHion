# Goal: native RouterOS commands and successful delivery

Status: active
Source: user-approved RouterOS audit/plan and request to implement, commit, push, build, and finish only after GitHub Actions succeeds.
Last updated: 2026-10-17

## Objective

Ship an explicit, command-only RouterOS profile without changing the default POSIX contract. Verify it locally and on the authorized router, push the implementation, and observe successful GitHub Actions for the pushed commit.

## Execution Directive

Complete the frozen Required Outcomes using the listed Change Envelope and Primary Evidence. Work on the smallest unresolved outcome. Do not add requirements from reviews, tests, tools, speculative risks, or optional source text. Finish when every required outcome is resolved and affected constraints remain satisfied.

## Frozen Contract

### Required Outcomes

- R1: Explicit profile, coherent MCP discovery, validation, and startup.
  - Source: corrected plan steps 1–3.
  - Acceptance: `--remote-platform posix|routeros` / `SSH_MCP_REMOTE_PLATFORM`, default POSIX; RouterOS exposes only native `shell(command, timeout_ms?)`; unsupported arguments/tools rejected before jobs/logs; no optional POSIX/raw/SFTP probes; profile-aware frozen instructions and fallback; incompatible nonempty elevation rejected at CLI and constructor boundaries.
  - Primary evidence: config/schema/parser tests and real MCP subprocess tests against a fake SSH peer.
  - Status: verified
  - Evidence: 2026-10-17 config/schema/parser/constructor unit tests and `cargo test --all-features --test routeros_test -- --nocapture`: 3 passed, including key-authenticated legacy/modern startup, strict runtime/schema gating, no probes/jobs and isolated env/CLI precedence.
- R2: Native, bounded, non-replaying execution with truthful failures and owned cleanup.
  - Source: corrected plan steps 4–5 and pending-OPEN correction.
  - Acceptance: raw one-line EXEC, no PTY/POSIX wrappers/jobs; deadline starts before slot; bounded byte-prefix output, partial output retained; close/status/event handling; timeout/cancellation never replay EXEC; late OPEN closes without EXEC; exhausted OPEN budget retires only its captured generation; normal cancellation preserves sibling channels; existing text result model with uncertainty/truncation diagnostics.
  - Primary evidence: deterministic fake-peer execution tests and read-only MCP smoke.
  - Status: verified
  - Evidence: 2026-10-17 reviewed native owner/event-loop implementation; 15 native executor tests and 3 actual MCP subprocess tests passed. Release binary on the authorized RouterOS returned normal/nonzero output, preserved partial output on a 1-second timeout, served a parallel command and follow-up without jobs, and completed a 7-second command across recovery ticks.
- R3: RouterOS-compatible target health policy without POSIX/jump regression.
  - Source: audit's confirmed `SSH_MSG_UNIMPLEMENTED` incompatibility and corrected plan step 3.
  - Acceptance: RouterOS target sends neither active OpenSSH health pings nor automatic OpenSSH keepalives; closed routes still recover and real channel opens are bounded; POSIX defaults and jump-host keepalives remain unchanged; passive idle-health limitation documented.
  - Primary evidence: policy/route tests, fake-peer transcript, live MCP idle/long-command smoke.
  - Status: verified
  - Evidence: 2026-10-17 connection policy/passive route/jump closure tests passed. Release binary on RouterOS with explicit JSON application logging produced 678 records, exactly one successful SSH connection and zero route invalidations while serving capped output, a 7-second command and a command after 35 seconds idle. Target keepalives/pings disabled, jump/default POSIX configuration retained by tests; passive idle limitation documented.
- R4: Documented, tested, locally built implementation preserving affected contracts.
  - Source: corrected plan steps 6–7; user asks for build.
  - Acceptance: README documents RouterOS usage/limits; focused tests, formatting, clippy, cargo check, full suite including existing Docker regressions, and release build succeed; authorized real device works through the new MCP binary.
  - Primary evidence: command results and read-only live observations.
  - Status: verified
  - Evidence: 2026-10-17 `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo check --all-features --verbose`, `cargo test --all-features --verbose` (375 passed, 4 pre-existing ignored; 88 Docker tests), `cargo build --release` all passed. README documents native usage/limits. New release binary authenticated with strict host-key checking and passed read-only live version/error/literal/block/timeout/parallel/long-command/35-second-idle checks on RouterOS 7.15.3; post-idle call completed in 9 ms. No router configuration changes.
- R5: Commit, push, and successful GitHub Actions for the delivered SHA.
  - Source: user: «итеративно реализовать и коммит пуш билд … задача закрыта когда github action после пуша кооректно завершился».
  - Acceptance: changes committed with repository commit style and pushed to origin; `CI and Release` completes successfully for the exact delivered head SHA, including main-branch build/release jobs.
  - Primary evidence: git SHA/remote and GitHub Actions run URL, head SHA, conclusion and jobs.
  - Status: in_progress
  - Evidence: local implementation and acceptance complete; commit/push and exact-SHA workflow observation next.

### Constraints

- Keep POSIX parser leniency, tool names/order/schemas, sudo flag behavior, error model, host-key/authentication policy, reconnect generations and transfer guarantees unchanged.
- Transfers still require explicit transport; no fallback or automatic selection.
- Reject RouterOS empty commands, NUL, internal CR/LF; never flatten multiline commands or infer errors from output keywords.
- SSH status zero does not verify the intended configuration effect. Closing a channel does not guarantee remote termination/rollback.
- Deadline covers all request waits; up to 100 ms best-effort close applies to an owned channel, not all pending-OPEN cleanup. An unresolved OPEN exhausting the existing acquisition budget may retire its generation and interrupt siblings.
- Preserve configured output-limit semantics, including explicitly unlimited settings; decode bounded bytes safely and drain after truncation.
- Keep common startup logging/spool initialization; no per-command RouterOS jobs/logs. Normalize exactly empty elevation passwords, never trim real passwords.
- Device access is read-only: no router configuration changes, upgrades, files, or disruptive commands.
- No blanket `--include-ignored`: existing ignored placeholders are not runnable acceptance tests.

### Non-goals

- Auto-detection, capability registry, REST/API, PTY/interactive/Safe Mode, background jobs, RouterOS transfer/patch support.
- New public result envelope, crypto downgrades, dependency upgrades/forks, new dependencies/images/CI jobs, shared test framework, broad fixture cleanup, automatic encoding conversion.
- Proactive idle-blackhole detection for RouterOS or removal of common spool initialization.

## Change Envelope

- Target: application config, profile-aware server dispatch/instructions/schema/parser, one native executor and local failure formatter, narrow target health policy, direct tests and README.
- Expected paths: `src/config.rs`, `src/server.rs`, `src/server/{args,tools}.rs`, `src/server/handlers/{mod,routeros}.rs`, `src/ssh/{mod,connection,routeros}.rs`, `tests/routeros_test.rs`, existing Config/Args test literals, README, this goal.
- If SSH config must carry internal policy, preserve existing public construction/default behavior; no public remote-platform enum throughout transport.
- Tests: one small fake peer/MCP harness, existing unit and Docker suites; mechanical POSIX additions to existing literals, no fixture redesign.
- No Cargo/lockfile/workflow edits expected; expand only for a concrete blocker to R1–R5 or a regression introduced by this diff.
- Delivery: current clean `main`, origin `0FL01/SeSSHion`; ordinary fast-forward commits/push, no force push.

## Copied Implementation Plan

1. Add explicit POSIX(default)/RouterOS config; validate elevation before constructor side effects, preserve exact-empty normalization.
2. RouterOS advertises only `shell` with command and positive optional timeout; strict unknown fields (including background false/log_path); schema/parser agree on null; reject hidden tools before work; preserve unknown-name errors and the full POSIX surface.
3. Preserve initial transport readiness/recovery cadence, skip optional probes, freeze native instructions including fallback. Disable target OpenSSH ping/keepalive for RouterOS only; retain jump keepalives and document passive health.
4. Native slot → connection → channel → raw EXEC → collection → close. One line only. One owner handles channel/open lifecycle, deadline, cancellation, bounded bytes, late OPEN cleanup and generation-scoped unresolved-OPEN recovery. No command replay or POSIX/background machinery. One event loop records status/output, closes on Close without requiring EOF, handles signals/incomplete reads as errors.
5. Reuse CommandOutput/text results; small local failure holds partial output/cause/EXEC-queued state. Failures stay errors even after status zero; report not-started versus may-have-run, no automatic retry, no guaranteed remote termination, and neutral truncation notices.
6. Test config and environment isolation, discovery/strict parsing/hidden tools, frozen reconnect behavior; exact raw payload and no probes/PTY/subsystem; event/error orders, limits/UTF-8/partial output, deadline/cancel/late OPEN/no replay/siblings; target-versus-jump health policy. Reuse patterns, not a new test framework.
7. README usage/limitations; new MCP binary read-only smoke (success/error/timeout/parallel/idle >30 seconds/long command across recovery ticks); existing CI gates and local release build; commit/push, observe exact-SHA GitHub Actions success.

## Primary Device Evidence (pre-implementation)

- Authorized RouterOS 7.15.3 stable, RB951Ui-2HnD, mipsbe; OpenSSH and Paramiko key-file authentication work without agent or crypto downgrade.
- Native single-line commands/blocks work; ordinary errors return status 1 on stdout. Simple LF-separated commands work but a multiline brace block printed `missing closing brace` with status 0: keep one-line scope.
- Observed Success → Data → ExitStatus → Close, no EOF. Sequential/parallel channels work, closing a delayed command's channel leaves the shared route usable.
- Both idle and running-command `keepalive@openssh.com` requests receive SSH_MSG_UNIMPLEMENTED while the route remains usable. russh 0.61.2 ignores that reply; active pings hang and automatic response entries can accumulate.
- Literal non-ASCII text in a direct `:put` was filtered by this firmware; do not promise remote Unicode semantics or add automatic conversion. Locally preserve valid UTF-8 output safely.
- No router configuration was modified. This is not yet proof of the new russh/MCP execution path.

## Current Checkpoint

- Closes: R5.
- Smallest next action: commit/push the verified implementation and wait for exact-SHA GitHub Actions.
- Expected evidence: pushed commit's CI and Release workflow completes successfully, including platform build/release jobs.
- Stop or replan if: a concrete required outcome cannot be achieved inside the envelope; record evidence and smallest expansion first.

## Current State

- Resolved: R1–R4 verified; baseline `a2d233326a65927b1ed4b16e6e85b5dcc0798bad`.
- Last relevant evidence: all local CI gates, full Docker suite and release build passed; read-only real-device MCP smoke passed. Explicit JSON application logging confirmed one connection, zero invalidations (678 records) across long command and idle. Stderr remains text by existing logger design; only actual JSON file records were used for this count.
- Blocker: none.
- Next: exact-SHA R5 delivery.

## Material Decisions

- 2026-10-17: Apply the corrected audited plan, with one-line scope and existing text response. User now explicitly authorizes implementation and commit/push/build; CI success is mandatory.

## Checkpoint History

- 2026-10-17: Freeze contract and copy plan before code changes; no new dependencies or CI changes planned.
- 2026-10-17: R1 verified through strict config/parser/schema and actual stdio MCP fake-peer tests; integrated library tests, clippy and cargo check green. Continue full-suite/build and real-device verification without expanding scope.
- 2026-10-17: R2/R4 verified: reviewed single-owner cleanup and typed OPEN timeout behavior; all local gates (375 passing tests) and release build succeeded; actual release binary passed read-only RouterOS acceptance including timeout/parallel, long command and 35-second idle. Complete concrete R3 log evidence, then commit/push and observe CI.
- 2026-10-17: R3 verified with nonempty structured application logs: 678 records, one connection, no invalidations across long native command/35-second idle; real device also confirmed output capping. Only delivery/CI remains.

## Completion

- Resolved outcomes: pending.
- Commands and artifacts: pending.
- Constraint and diff-scope check: pending.
- Final status: active.
