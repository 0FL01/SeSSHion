# Goal: cold-start SSH readiness and idle recovery

Status: complete
Source: user-approved audited plan; implement now, commit and standalone build later.
Last updated: 2026-10-04

## Objective
The CLI refuses MCP startup unless the remote SSH route authenticates. After that
success it keeps MCP alive through SSH outages and reconnects without tool calls
or a harness restart.

## Execution Directive
Complete the frozen Required Outcomes using the listed Change Envelope and
Primary Evidence. Work on the smallest unresolved outcome. Do not add
requirements from reviews, tests, tools, speculative risks, or optional source
text. Finish when every required outcome is resolved and affected constraints
remain satisfied. Stop substantive work at a proven external blocker or approved
budget boundary; record the exact evidence and smallest unlock.

## Frozen Contract

### Required Outcomes
- R1: Cold startup rejects inaccessible SSH, stalled handshake and rejected credentials.
  - Source: user requests Failed on initial connection, including unavailable remote.
  - Acceptance: no successful MCP response; bounded failure exit before serving.
  - Primary evidence: binary stdio lifecycle tests, legacy and modern opening RPCs.
   - Status: verified
   - Evidence: lifecycle tests reject unavailable SSH, silent TCP, wrong password,
     strict unknown host key and jump authentication before any MCP response.
- R2: A fresh launch succeeds once the same remote becomes available.
  - Source: user requests Connected after remote restoration and harness re-entry.
  - Acceptance: successful MCP startup and tool call from a new process.
  - Primary evidence: lifecycle fresh-launch regression.
   - Status: verified
   - Evidence: lifecycle regression relaunches against the same restored endpoint;
     legacy initialize and modern opening tools/list succeed, followed by shell.
- R3: Warm outages keep MCP alive and recover SSH while idle.
  - Source: user requests remaining Connected during outage and automatic reconnect.
  - Acceptance: same process/stdio survives outage; a new authenticated SSH session
    appears before any SSH-triggering call; subsequent tool execution succeeds.
  - Primary evidence: real stdio outage/recovery test with remote-side session evidence.
   - Status: verified
   - Evidence: sshd and its sessions are killed for 16s; MCP ping/list survive;
     authentication count grows 1 -> 2 before a new tool call, which then succeeds.
- R4: Preserve affected safety and lifecycle contracts.
  - Source: approved audited plan steps 2–4.
  - Acceptance: old-generation failures/channels cannot reset a new route; metadata
    remains rootless/best-effort/frozen; reconnect does not replay operations;
    shutdown cancels recovery; transport acquisition waits are bounded.
  - Primary evidence: focused manager regressions, lifecycle and environment tests.
   - Status: verified
   - Evidence: manager unit tests cover stale generation/SU/health, owner races,
     blackholed channel opening, total acquisition budgets and shutdown. Environment
     regressions retain rootless/frozen metadata and deferred elevation. The warm
     test observes a side effect, cuts SSH before its terminal outcome, receives
     an error and proves the side effect remains single after idle recovery.
     Shutdown produces no subsequent authentication. Auth/jump/job/timeout/Fish and
     exec-raw transfer regressions pass.
- R5: Document delivered cold/warm behavior and verify the implementation.
  - Source: user requests a goal copy and implementation of the audited plan.
  - Acceptance: README matches behavior; relevant tests, fmt and clippy succeed.
  - Primary evidence: recorded commands and reviewed diff.
   - Status: verified
   - Evidence: README and SDK notes updated; fmt, all-targets/all-features clippy,
     206 unit tests, 23 non-Docker integration tests and 49 Docker tests pass.

### Constraints
- Lazy public constructors and best-effort `with_startup_environment` remain compatible.
- Reuse the configured target/jump auth and host-key policies without weakening them.
- Keep tools, schemas, result formats, job semantics and frozen instructions stable.
- Background recovery is transport-only; existing deferred elevation remains.
- No commit or standalone/release build in this session; verification may compile tests.

### Non-goals
- Harness-specific UI integration or a guaranteed UI label.
- New modes, CLI flags, dependencies, state machine, retry framework or status tools.
- Global cleanup of command fallbacks or unrelated transfer/job behavior.

## Change Envelope
- Target: CLI startup, shared SSH connection lifecycle and its direct consumers.
- Expected paths: `src/main.rs`, `src/ssh/connection.rs`, necessary generation-aware
  callers in `src/ssh/command.rs` and `src/server/exec.rs`, existing
  lifecycle/reconnect/environment tests, README, `Docs/rmcp-sdk.md`, this goal and
  a supersession note in the historical `Docs/goal/host-environment.md` contract.
- Allowed: one tracked background task, narrow access to the existing rootless
  connection path, bounded waits and existing-generation bookkeeping.
- Forbidden: new manager/service/store/framework, eager library construction,
  runtime metadata refresh, automatic replay of user operations.

## Approved Plan
1. Gate CLI startup before `serve_with_ct`: bounded rootless SSH readiness, then
   optional environment snapshot; all exits share cancellation and cleanup.
2. Make the existing manager safe for a concurrent recovery consumer: connect-owner
   recheck, generation-aware health/invalidation/su-channel restoration, bounded waits.
3. Run one cancellable background task after cold readiness. Reuse finite retry
   bursts with pauses; continue until shutdown without closing MCP or elevating.
4. Adapt existing lifecycle assumptions and prove cold failure, fresh success,
   idle recovery, stale-generation safety and unchanged metadata/job contracts.
5. Update README and run targeted verification; do not commit/build release yet.

## Current Checkpoint
- Closes: R1–R5.
- Result: closure passed; no remaining implementation checkpoint.

## Current State
- Resolved: cold gate, idle recovery, manager ownership/budgets and stdio regressions.
- Last relevant evidence: final implementation passed the checks listed in Completion.
- Blocker: none.
- Next: none. Commit and standalone/release build remain deferred as requested.

## Material Decisions
- 2026-10-04: cold means a new CLI process, not every MCP request. Successful
  route authentication is the cold/warm boundary; a later network race is warm.
- 2026-10-04: startup failure is a nonzero process exit before MCP serving, not a
  custom initialize/discover error. UI wording is controlled by the harness.
- 2026-10-04: readiness budget 10s; optional metadata 3s; rootless acquisition and
  channel-open budgets 30s each; recovery pauses 5s between finite retry bursts.
- 2026-10-04: background wrapper pre-exec is a direct generation-aware consumer:
  its old unconditional reconnect could discard a supervisor's replacement route.

## Checkpoint History
- 2026-10-04: contract frozen from the approved audit; no implementation yet.
- 2026-10-04: implementation passed unit/lifecycle and targeted SSH regressions;
  idle recovery witnessed sshd authentication count 1 -> 2 before tool execution.
- 2026-10-04: generation-safe background wrapper retry and interrupted-operation
  no-replay assertion passed; docs updated; final relevant checks and closure green.

## Completion
- Resolved outcomes: R1–R5 verified.
- Commands and artifacts (all passed):
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test --lib --bin ssh-mcp` — 206 passed.
  - `cargo test --test integration_test --test logging_test --test compact_response_test`
    — 23 passed; 3 pre-existing real-SSH placeholder tests remain ignored.
  - `cargo test --test docker_integration_test docker_integration::lifecycle_tests:: -- --nocapture`
    — 17 passed, including cold/warm and interrupted-operation witnesses.
  - Separate Docker suite commands of the form
    `cargo test --test docker_integration_test docker_integration::<module>::`:
    `host_environment_tests` (4), `auth_tests` (6), `jump_tests` (2),
    `reconnect_tests` (1), `exec_raw_tests` (2), `timeout_tests` (7),
    `check_process_tests` (8), `fish_tests` (2).
  - `git diff --check` — no whitespace errors.
- Constraint and diff-scope check: existing lazy/best-effort APIs, tool surface,
  host-key policy, job semantics and frozen instructions retained. One rootless
  tracked task, existing generations and no new dependencies/flags/framework.
  No commit or standalone/release build performed. Harness UI label not promised.
- Final status: complete.
