# Goal: valid MCP discovery through the upstream rmcp fix

Status: active
Source: user-approved audited plan; copy the plan into a goal, implement iteratively, commit and push.
Last updated: 2026-10-15

## Objective
Upgrade rmcp without application compatibility code so modern `tools/list` is
valid, legacy clients remain compatible, and the deployed SSH MCP servers work
without Claude's global legacy override.

## Execution Directive
Complete the frozen Required Outcomes using the listed Change Envelope and
Primary Evidence. Work on the smallest unresolved outcome. Do not add
requirements from reviews, tests, tools, speculative risks, or optional source
text. Finish when every required outcome is resolved and affected constraints
remain satisfied. Stop substantive work at a proven external blocker or approved
budget boundary; record the exact evidence and smallest unlock.

## Frozen Contract

### Required Outcomes
- R1: Fix modern discovery while preserving legacy and the existing tool surface.
  - Source: approved plan, implementation batch.
  - Acceptance: rmcp and rmcp-macros resolve to 3.5.1; modern `tools/list` contains
    `resultType: complete`, `ttlMs: 0`, `cacheScope: private`; legacy omits all
    three. Existing tools, schemas, errors and frozen instructions remain intact.
  - Primary evidence: the two existing stdio lifecycle tests, within the full suite.
  - Status: verified
  - Evidence: rmcp and rmcp-macros alone updated to 3.5.1. Both modified stdio
    tests passed in the full suite, including 6/4-tool and frozen-state regressions.
- R2: Verify the change and produce the deployable release artifact.
  - Source: approved plan, verification and SDK documentation.
  - Acceptance: SDK notes match 3.5.1; fmt, all-targets/all-features Clippy and the
    existing full test suite pass; the locked release binary builds.
  - Primary evidence: the four commands in Approved Plan, step 2.
  - Status: verified
  - Evidence: all four commands in step 2 passed. Tests: 230 unit, 88 Docker,
    22 other integration and 6 doc tests passed; 4 pre-existing tests ignored.
    Release SHA-256: 44e201764cc34620f11698837b3ef80b166be92337cdb9d5a8e3bc5d6948d83b.
- R3: Deploy and remove the Claude workaround after actual modern discovery.
  - Source: approved plan, address-specific rollout.
  - Acceptance: `/usr/bin/ssh-mcp` matches the verified release artifact; a fresh
    Claude v2/auto process negotiates 2026-07-28 and obtains the expected catalog
    from ssh-beta4, ssh-beta5 and ssh-kube2. Only the global
    `env.MCP_PROTOCOL_NEGOTIATION` key is removed; a fresh process without
    overrides can execute `hostname` through each server's MCP `shell`.
  - Primary evidence: artifact comparison and actual Claude discovery/tool results.
  - Status: blocked
  - Evidence: `/usr/bin/ssh-mcp` is root:root 0755; `sudo -n true` reports
    `sudo: a password is required`. No NOPASSWD install permission exists.
    Old binary preserved outside the repo; no deployment, client reconnect or
    Claude settings change performed. Smallest unlock: user installs the verified
    artifact atomically with root privileges, then the client smoke can continue.
- R4: Preserve OpenCode 2 compatibility and smoke an existing integration if present.
  - Source: user's explicit OpenCode 2 compatibility request and audited plan.
  - Acceptance: no OpenCode-specific code/config changes; one existing integration
    obtains its expected tools and executes MCP `shell` `hostname` after an
    address-specific child reconnect. Do not create an integration if none exists.
  - Primary evidence: protocol source review, plus one existing-integration smoke.
  - Status: in_progress
  - Evidence: audit reviewed OpenCode 2.0.29 commit 4397ee44d038a5f17bf6ac161431673d93e563b5;
    legacy initialize and modern 0/private hints are compatible. Running service
    version is confirmed by `/api/info` as 2.0.29. Existing SSH integrations are
    connected in Location `/home/stfu/ai/trash-can/53`; their children execute the
    old host `/usr/bin/ssh-mcp`. Runtime upgrade smoke awaits R3 installation and
    confirmation that the selected integration can be reconnected without active work.
- R5: Commit and push the implementation and goal state.
  - Source: latest user instruction: implement iteratively, commit and push.
  - Acceptance: conventional commits with concrete bodies are published to origin/main.
  - Primary evidence: git commit and successful push/remote revision verification.
  - Status: pending
  - Evidence: starting revision 0f80647; clean main tracking origin/main.

### Constraints
- KISS/YAGNI/Pareto: five implementation files plus this explicitly requested goal.
- Add only five assertions to two existing tests; no new tests, helpers or matrices.
- Preserve features/defaults, tool schemas/order, error model, startup instructions,
  SSH readiness/recovery and background job semantics.
- No blanket dependency update, direct macros dependency or application cache hints.
- Do not change unrelated client configuration or restart all shared services.
- Do not reconnect important active work; preserve the old binary for rollback.
- No secrets, credentials or sensitive runtime dumps in this goal or commits.

### Non-goals
- Claude-specific compatibility, custom negotiation, real caching, SDK Tasks,
  MRTR, subscriptions, HTTP transport, SSH/firewall/routing fixes and cleanup.

## Change Envelope
- `Cargo.toml`, `Cargo.lock`: exact rmcp 3.5.1 and necessary related lock updates.
- `src/server.rs`: `get_info` alias/construction and initialize fallback only.
- `tests/docker_integration/lifecycle_tests.rs`: two existing test bodies only.
- `Docs/rmcp-sdk.md`: version, lifecycle distinction and automatic cache hints.
- This goal records the approved plan, completed checkpoints and exact blockers.
- Operational paths: `/usr/bin/ssh-mcp`, only the specified key in Claude's user
  settings, and address-specific existing client connections. No new integration.

## Approved Plan
1. One related implementation batch:
   - Pin rmcp to `=3.5.1`, retain features/defaults; address lock resolution to
     rmcp/rmcp-macros 3.5.1 without unrelated updates.
   - In `get_info`, use `ServerConfig` and `LATEST_WITH_INITIALIZE`; preserve
     identity/capabilities/instructions. Do not change `list_tools` or hooks.
   - Modern test: add assertions for 0/private; legacy test: assert absence of
     resultType/ttlMs/cacheScope. Preserve existing 6/4-tool and frozen-state tests.
   - Update the short SDK integration notes.
2. Run existing gates once, then build the deployment artifact:
   ```sh
   cargo fmt --all -- --check
   cargo clippy --locked --all-targets --all-features -- -D warnings
   cargo test --locked --all-features --verbose
   cargo build --release --locked
   ```
   Docker and its existing fixtures are prerequisites. No separate successful
   targeted runs before the same full suite; use targeted runs only to diagnose
   an actual failure.
3. Address-specific rollout and delivery:
   - Preserve the old binary; atomically replace `/usr/bin/ssh-mcp`, retain correct
     owner/mode and compare its contents to the built artifact.
   - Reconnect only the relevant idle MCP children. A new OpenCode TUI is not a
     backend child restart; verify the artifact in its actual execution environment.
   - In `/home/stfu/livemaster_work/beta4-toolkit`, use temporary Claude settings:
     ```sh
     claude --settings '{"env":{"MCP_SDK_GENERATION":"v2","MCP_PROTOCOL_NEGOTIATION":"auto"},"enabledMcpjsonServers":["ssh-beta4","ssh-beta5","ssh-kube2"]}' mcp list
     ```
     This is preliminary health only. In a fresh process with those settings,
     establish actual 2026-07-28 negotiation and real catalogs for all three.
   - Smoke one existing OpenCode 2 integration without changing its protocol/config.
   - Only after Claude succeeds, delete the single global legacy key; in a fresh
     process without temporary overrides, run `hostname` through all three MCP
     shell tools, not direct SSH.
   - Stop rollout on failure and retain/restore the previous working binary as
     necessary. Global settings and the shared binary affect other projects;
     do not expand this into a mass reconnect or client matrix.
   - Record actual evidence/blockers, commit and push the resulting work.

## Current Checkpoint
- Closes: R3–R4; R5 remains independently deliverable.
- Smallest next action: obtain the root installation of the verified artifact,
  compare it, then run the already approved client smoke; commit/push the repo work.
- Expected evidence: installed artifact equality and real client catalogs/tool calls.
- Stop or replan if: root installation or a safe address-specific reconnect is
  unavailable. Do not remove Claude's legacy override before actual modern success.

## Current State
- Resolved: R1–R2 implementation and all planned build/test gates.
- Last relevant evidence: the five-file diff matches the plan; no unrelated lock
  changes; no additional runtime edits needed after the successful checks.
- Blocker: root installation requires interactive user privileges. Global legacy
  remains intact and no existing MCP child has been stopped.
- Rollback artifact: `/home/stfu/.cache/opencode-tmp/opencode/ssh-mcp-rmcp-3.5.1.veQtwt/ssh-mcp.previous`;
  SHA-256: 45b815ebea167cb67afcefc15d1e0d83eb1bb5c4b5c58353dec688e8fc0442b2.
- Next: user-assisted atomic installation, R3–R4 smoke, and R5 commit/push.

## Material Decisions
- 2026-10-15: preserve SDK-owned normalization; no application compatibility path.
- 2026-10-15: use the repository's existing `Docs/goal/` convention; no duplicate plan file.

## Checkpoint History
- 2026-10-15: froze the audited plan before implementation; no runtime checks yet.
- 2026-10-15: R1–R2 verified in one fmt/Clippy/full-test/release pass. Reviewed
  the implementation diff once; exactly the approved changes. R3 installation
  is blocked on root privileges; preserved the previous artifact for rollback.

## Completion
- Final status: active; implementation is verified, runtime acceptance and delivery
  remain open. Do not mark complete based on source review or transport health alone.
