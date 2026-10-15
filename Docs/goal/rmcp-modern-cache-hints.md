# Goal: valid MCP discovery through the upstream rmcp fix

Status: blocked
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
  - Status: verified
  - Evidence: user performed the root installation; `cmp` confirms artifact
    equality, root:root 0755 and release SHA-256. Claude 2.1.300 with temporary
    v2/auto negotiated 2026-07-28 (protocolEra modern) on all three servers;
    structured init events contain all six tools for each. A later read found
    both global MCP env overrides absent without an agent settings edit. Fresh
    Claude without temporary settings selected v2 by default and negotiated
    2026-07-28 on all three; structured events confirm exactly one `hostname`
    MCP shell call/result per target, no tool errors and no permission denials.
- R4: Preserve OpenCode 2 compatibility and smoke an existing integration if present.
  - Source: user's explicit OpenCode 2 compatibility request and audited plan.
  - Acceptance: no OpenCode-specific code/config changes; one existing integration
    obtains its expected tools and executes MCP `shell` `hostname` after an
    address-specific child reconnect. Do not create an integration if none exists.
  - Primary evidence: protocol source review, plus one existing-integration smoke.
  - Status: blocked
  - Evidence: audit reviewed OpenCode 2.0.29 commit 4397ee44d038a5f17bf6ac161431673d93e563b5;
    legacy initialize and modern 0/private hints are compatible. Running service
    version is confirmed by `/api/info` as 2.0.29. User approved the idle
    ssh-n-de2 reconnect in Location `/home/stfu/ai/trash-can/53`; it returned
    connected, and its new child executable inode matches the installed artifact.
    The subsequent read-only `opencode run --format json` smoke stopped before
    tools with `provider.auth`, HTTP 403, `forbidden: access denied`. No tool
    names/count or hostname result was fabricated. Smallest unlock: a working
    provider for this existing OpenCode session's single MCP smoke. Provider
    configuration is outside this fix and was not changed.
    Failed smoke session: ses_ec1334024ffe9a05CuSmyYybkl; its structured error is
    retained in the private diagnostic directory as `opencode-smoke.jsonl`.
- R5: Commit and push the implementation and goal state.
  - Source: latest user instruction: implement iteratively, commit and push.
  - Acceptance: conventional commits with concrete bodies are published to origin/main.
  - Primary evidence: git commit and successful push/remote revision verification.
  - Status: verified
  - Evidence: implementation commit 4bdb717b1de009cd0d0515a7ed8c368d2c9fda81
    pushed to origin/main; `git ls-remote --heads origin main` returned the same
    revision. Goal checkpoint updates are committed/pushed with delivery.

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
- Closes: R4 only; R1–R3 and R5 are verified.
- Smallest next action after external unlock: run the single existing OpenCode
  ssh-n-de2 catalog/hostname smoke with a working provider.
- Expected evidence: actual catalog and MCP shell result, not a model assertion.
- Stop: the current provider rejects the request with HTTP 403 before tool use.
  No approved source-code action can unblock it. Do not modify credentials,
  routing or unrelated client configuration as part of this SDK fix.

## Current State
- Resolved: R1–R3 implementation, all planned gates, installation and real Claude
  modern tools/hostname checks; R5 implementation commit/push.
- Last relevant evidence: the five-file diff matches the plan; no unrelated lock
  changes; no additional runtime edits needed after the successful checks.
- Blocker: R4 model request is rejected by the OpenCode provider with HTTP 403.
  Root installation is resolved. No broad client/service restart was performed.
- Rollback artifact: `/home/stfu/.cache/opencode-tmp/opencode/ssh-mcp-rmcp-3.5.1.veQtwt/ssh-mcp.previous`;
  SHA-256: 45b815ebea167cb67afcefc15d1e0d83eb1bb5c4b5c58353dec688e8fc0442b2.
- Diagnostics: private files in the rollback directory, including
  `claude-modern.jsonl`, `claude-modern.debug.log`, `claude-final.jsonl`,
  `claude-final.debug.log` and `opencode-smoke.jsonl`; none are committed.
- Next: R4 after provider access is restored. No remaining source-code edit or
  repeat build/test pass is required for the unchanged implementation.

## Material Decisions
- 2026-10-15: preserve SDK-owned normalization; no application compatibility path.
- 2026-10-15: use the repository's existing `Docs/goal/` convention; no duplicate plan file.

## Checkpoint History
- 2026-10-15: froze the audited plan before implementation; no runtime checks yet.
- 2026-10-15: R1–R2 verified in one fmt/Clippy/full-test/release pass. Reviewed
  the implementation diff once; exactly the approved changes. R3 installation
  is blocked on root privileges; preserved the previous artifact for rollback.
- 2026-10-15: R5 implementation published as 4bdb717 to origin/main; matching
  remote revision confirmed. Remaining rollout needs external root installation.
- 2026-10-15: user installed the artifact and approved one idle OpenCode reconnect.
  Artifact equality and real modern Claude catalogs verified; no global MCP env
  overrides remain. OpenCode reconnect uses the new binary, but model auth 403
  prevents the requested tool smoke; no provider changes attempted.
- 2026-10-15: R3 verified in a fresh default-runtime Claude process: all three
  negotiated modern 2026-07-28, exposed six tools each and returned MCP hostname
  results without errors. Closure remains blocked only on R4's external provider.

## Completion
- Resolved outcomes: R1, R2, R3 and implementation delivery R5.
- Commands/artifacts: all four Approved Plan step 2 commands passed; installed
  release matches its SHA-256; both Claude structured runs confirm modern catalogs,
  and the default-runtime run confirms all three MCP hostname calls.
- Constraint/diff check: only the five approved implementation files and this
  goal changed; no list_tools/dispatch/schema/SSH changes, new tests, client config
  edits, runtime protocol pins or provider workarounds were introduced.
- Final status: blocked solely on R4's OpenCode provider auth; not complete.
