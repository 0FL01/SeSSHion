# Goal: automatic RouterOS detection and verified delivery

Status: active
Source: approved automatic-detection plan; user: «никаких ... --remote-platform=routeros ... автоматически понимать с чем работаем», then «делай копию плана в цель и итеративно реализовать и коммит пуш билд ... задача закрыта когда github action после пуша кооректно завершился»; KISS/YAGNI/Pareto.
Last updated: 2026-10-17

## Objective

The same SSH connection configuration (host, port, user, credentials) automatically selects the existing RouterOS or POSIX backend before publishing MCP tools. Deliver the verified implementation with successful GitHub Actions for the pushed SHA.

## Execution Directive

Complete the frozen Required Outcomes using the listed Change Envelope and Primary Evidence. Work on the smallest unresolved outcome. Do not add requirements from reviews, tests, tools, speculative risks, or optional source text. Finish when every required outcome is resolved and affected constraints remain satisfied.

## Frozen Contract

### Required Outcomes

- R1: Select compatibility automatically on the actual SSH transport.
  - Source: approved plan, identification helper and SSH integration sections.
  - Acceptance: exchange the configured client identification exactly once, read and replay bounded server identification/preamble/KEX bytes on the same stream, match the actual software token ROSSSH, choose keepalive policy before russh connect_stream. No second connection or diagnostic EXEC. Freeze target backend only after host-key validation/authentication. Reconnect rejects backend changes without publishing another contract; jump and target are classified independently.
  - Primary evidence: identification adapter tests, connection tests and real MCP fake-peer transcript.
  - Status: verified
  - Evidence: 9 identification adapter tests and 22 connection tests passed; parent reviewed bounded replay/duplicate-write suppression and authenticated publication. Direct/jump classification, authentication/host-key refusal without freezing, reconnect mismatch/cleanup and zero diagnostic EXEC are covered. Actual flagless MCP selected RouterOS and executed the reported command.
- R2: Remove manual profile configuration and publish a coherent automatically selected MCP surface.
  - Source: approved plan, config and MCP sections.
  - Acceptance: remove CLI/env/Config remote_platform; RouterOS exposes only native shell, POSIX retains its tool surface; startup probes only after backend selection, no POSIX/raw/SFTP probes on RouterOS. Lazy construction stays lazy; initialize/discover/list/call entry points share bounded first resolution, and offline get_info never invents POSIX. Incompatible nonempty RouterOS su/sudo configuration fails before elevation/tool publication; exact-empty normalization retained.
  - Primary evidence: config/server unit tests and legacy/modern real MCP subprocess tests without profile flags/env.
  - Status: verified
  - Evidence: `cargo test --all-features --test routeros_test -- --nocapture`: 7 passed (legacy and modern flagless discovery/raw execution, old env ignored, POSIX six/four tools, changed backend no EXEC, RouterOS elevation rejected, all four lazy-library opening requests). Server/config focused unit selections also passed. CLI and Config selector removed, existing literals mechanically updated.
- R3: Preserve affected execution, health, security and concurrency contracts.
  - Source: approved plan, invariants and reconnect sections.
  - Acceptance: reuse existing native/POSIX executors; preserve no replay, owned OPEN/channel cleanup, output/deadline limits and truthful partial failures. RouterOS target automatic keepalive/active ping disabled; POSIX and POSIX jump settings unchanged. Readiness remains 10 seconds and optional POSIX startup probes 3 seconds; warm discovery/schema stays available/frozen. Host-key/auth policy and explicit-only transfer transport unchanged.
  - Primary evidence: focused tests and existing library/Docker gates.
  - Status: verified
  - Evidence: full `cargo test --all-features --verbose` passed: 396 tests, including 273 library, 88 Docker, 7 real MCP/lazy-library cases; 4 pre-existing ignored. Existing native ownership tests retained. Actual MCP completed parallel/long commands, retained partial timeout output and remained usable after 35 seconds idle. No runtime production changes to the native executor, no Cargo/lock/workflow changes.
- R4: Document, build and verify the actual connected MCP with no profile argument.
  - Source: approved plan, tests and real activation sections.
  - Acceptance: README explains auto-selection/limits; fmt, clippy, check, full tests and local release build succeed. Actual connected ssh-mikrotik-router uses the new binary without platform flags and executes the reported read-only command, expected error, timeout/parallel follow-up and >30-second idle. No direct SSH from local shell for follow-up device validation.
  - Primary evidence: command results, model-visible MCP tool calls, effective client configuration/process args.
  - Status: verified
  - Evidence: fmt, all-target/all-feature clippy with -D warnings, cargo check, full tests and cargo build --release passed. Supported location-scoped OpenCode API activated the known workspace release binary with unchanged host/port/user/key arguments and no profile selector; process arguments confirmed. Model-visible MCP returned SSH_MCP_AUTODETECT_OK plus /system resource print, expected exit 1, partial timeout, parallel success, long-command start/end and SSH_MCP_AFTER_IDLE_OK after 35 seconds idle on RouterOS 7.15.3. README updated. No direct SSH or router writes in follow-up; user project config unchanged.
- R5: Commit, push, build, and observe successful exact-SHA GitHub Actions.
  - Source: latest explicit user finish line.
  - Acceptance: conventional commits with body; ordinary push to origin/main; CI and Release for the delivered head SHA completes successfully including platform build/release jobs. Stop only after this evidence and closure check.
  - Primary evidence: git head/remote and exact-SHA GitHub Actions URL/conclusion/jobs.
  - Status: in_progress
  - Evidence: prior manual-profile implementation 88397dbf04f643263dadfe7fc0536d69e7ede5f4 passed CI run https://github.com/0FL01/SeSSHion/actions/runs/39025593602; that does not verify this automatic-detection revision.

### Constraints

- KISS/YAGNI/Pareto: one small identification helper/adapter, existing SSH manager and executors; no platform registry, generic framework, extra connection, diagnostic EXEC, new dependency, fork, image or CI job.
- Match ROSSSH as the software token, not a substring in comments; other valid IDs retain the legacy POSIX compatibility backend, not a claim that their OS is Linux. Malformed/absent/timed-out identification fails connection, never a POSIX fallback. Preserve russh's accepted 2.0/1.99, LF/CRLF and bounded preamble behavior.
- Identification is compatibility evidence, not trust. Preserve host-key verification/authentication. Pending classification must not be published as an authenticated route or trigger elevation.
- Keep POSIX parser leniency, names/order/schemas, disable-sudo behavior, error model and explicit transfer transport/no fallback guarantees.
- Keep native one-line validation, output limits/UTF-8, truthful status/errors, no replay/jobs/PIDs/POSIX wrappers, cancellation/late OPEN ownership and captured-generation-only retirement.
- SSH zero does not prove configuration success; timeout/close does not guarantee remote termination or rollback.
- Preserve common logging/spool initialization. Device operations read-only; no router config changes, files, upgrades or disruptive commands.
- No blanket include-ignored gate; existing placeholders remain ignored. Preserve user's uncommitted opencode.jsonc entry (already has no platform flag).

### Non-goals

- Universal OS/CLI detection (Windows/Cisco/JunOS/etc), hidden-banner RouterOS detection, native hardware inventory, background/file/patch/REST support, encoding conversion, broad test-fixture cleanup.
- Proactive RouterOS idle-blackhole detection, changing crypto/auth or the native executor's production behavior.

## Change Envelope

- src/ssh/identification.rs (new adapter/classifier/tests), src/ssh/{mod,connection}.rs and necessary existing inline native fixture adjustments.
- src/config.rs, src/server.rs and minimal src/main.rs wiring; directly affected config literals, CLI tests and tests/routeros_test.rs; README; this goal.
- Runtime: update/reconnect only the existing authorized ssh-mikrotik-router MCP via supported OpenCode APIs; preserve other configuration and do not restart the whole service.
- Install verified release binary at the actual configured executable if permitted; otherwise point only this MCP at the verified workspace binary. No sudo/package-wide changes required.
- No Cargo/lock/workflow edits expected. Main baseline 88397db; ordinary fast-forward commits/push only.

## Copied Approved Plan

1. Read server identification on the same stream, sending client ID once. Replay all consumed bytes to russh and consume its duplicate initial ID write; delegate all later I/O. Bound lines/preamble and all waits inside connection deadlines; do not break servers waiting for the client ID.
2. Match software token ROSSSH, choose per-connection keepalive configuration before connect_stream, then authenticate and freeze target backend. Do not use jump identity as target identity. Reject a different backend on reconnect; never replay commands or change MCP schema in place.
3. Remove Args/Config remote_platform and SSH_MCP_REMOTE_PLATFORM selection; backend enum becomes an internal detection result. Validate incompatible RouterOS elevation after identification/auth, before publishing tools/elevation; exact-empty semantics unchanged.
4. Replace config branches with detected backend. Keep 10-second readiness then 3-second optional POSIX metadata/preflight; skip those probes for RouterOS. Lazy constructors plus one bounded async resolution guard for initialize/discover/list/call; synchronous get_info before resolution is neutral. Warm discovery uses the frozen result without requiring a new connection.
5. Update existing fake peer/MCP tests: no flags/env, one connection and zero startup EXEC for RouterOS, native raw command, correct POSIX six/four tools, comment false-positive exclusion, direct/jump, event/timeout/output/no-replay regressions, reconnect stability/mismatch. Test adapter fragmentation, preamble, LF/CRLF, server-waits-client, readahead and duplicate-write suppression.
6. README ordinary no-profile usage, observed backend log and identification limits. Build, activate actual MCP, test read-only success/error/timeout/parallel/idle via the tool. Run existing gates, commit/push, and observe full exact-SHA CI success.

## Current Checkpoint

- Closes: R5.
- Smallest next action: commit/push the verified revision, then observe the full exact-SHA CI and Release workflow.
- Expected evidence: pushed SHA matches the successful workflow, including platform build/release jobs.
- Stop or replan if: a concrete required outcome crosses the envelope; record minimal expansion before editing.

## Current State

- Resolved: R1–R4 verified; only current revision's commit/push and exact-SHA CI delivery remain.
- Last relevant evidence: all local gates and release build passed, 396 tests passed with 4 pre-existing ignored. Actual connected MCP ran the reported command and all read-only acceptance cases without profile flags. Known workspace release binary selected via runtime-only supported MCP replacement; user's opencode.jsonc changes preserved and not included in delivery.
- Blocker: none. Docker and local SSH transfer clients available.
- Next: exact-SHA R5 delivery.

## Material Decisions

- 2026-10-17: Latest user request supersedes the old explicit-profile R1 and manual-flag client fix R6. Update the nearest goal instead of creating a duplicate. Remove the selector, not move it into an environment variable.
- 2026-10-17: Approved auto-selection is intentionally two-backend compatibility detection; no claims of universal OS identification. Keep the established native executor unchanged.

## Checkpoint History

- 2026-10-17: 88397db delivered native RouterOS execution; local gates/live read-only acceptance and CI run 39025593602 succeeded. Actual client missing manual profile reproduced marker failure.
- 2026-10-17: User rejected manual profile requirement, approved automatic-detection plan and authorized implementation/commit/push/build. Freeze this replacement contract before code edits.
- 2026-10-17: R2 focused evidence passed: 7 actual MCP/lazy-library tests and server/config unit selections. Initial fixture failures were a missing String→Cow conversion, an incorrect no-SFTP-connection assumption for POSIX, and teardown before the legacy handshake barrier; corrected fixtures without weakening runtime behavior.
- 2026-10-17: R1 transport slice verified and reviewed. Full Docker run exposed one superseded assertion: an already-cancelled lazy server cannot publish a POSIX transfer hint before any target is identified. That case now asserts neutral pending instructions/no connection, then the original unknown-preflight POSIX hint after normal lazy authentication. No runtime workaround or suppressed check.
- 2026-10-17: R3–R4 verified: full gates/release build green (396 passed, 4 existing ignored). Activated the exact workspace release executable only for this location's router MCP through the supported API; no profile flags or project-config edits. Actual tool passed reported command, expected error, timeout/parallel, 7-second command and 35-second idle follow-up, read-only.

## Completion

- Resolved outcomes: pending.
- Commands and artifacts: pending.
- Constraint and diff-scope check: pending.
- Final status: active.
