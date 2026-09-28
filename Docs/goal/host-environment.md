# Goal: rootless startup environment in MCP instructions

Status: active
Source: user rejected the environment tool, approved the startup-instructions
plan after recon, then instructed “Утверждаю реализовать и коммит билд”.
Last updated: 2026-09-28

## Objective

Remove the `host_environment` MCP tool and deliver the same bounded, rootless
environment snapshot automatically in immutable init/discovery instructions.
Verify the change, build the release binary and commit locally without pushing.

## Execution Directive

Complete the frozen Required Outcomes using the listed Change Envelope and
Primary Evidence. Work on the smallest unresolved outcome. Do not add requirements
from reviews, tests, tools, speculative risks or optional source text. Finish when
every required outcome is resolved and affected constraints remain satisfied.

## Frozen Contract

### Required Outcomes

- R1: Remove the environment tool and deliver all 12 nullable snapshot fields in
  the existing MCP `instructions` via both `initialize` and `server/discover`.
  - Source: approved plan, steps 1 and 3.
  - Acceptance: previous 6 tools / 4 with disable-sudo, original order and 3200-byte
    tool budget; no environment schema/handler/arguments/wrapper; compact JSON data
    plus a short semantic note, preserving the existing user-facing error policy.
  - Primary evidence: real stdio init/discovery and exact surface tests.
  - Status: verified
  - Evidence: library surface is 3192 bytes within 3200; real stdio tests verify
    initialize/discover JSON, absence of the environment tool and 6/4 definitions.
- R2: Startup collection never initiates su/sudo, including cold establishment;
  subsequent ordinary commands preserve configured automatic elevation.
  - Source: original rootless request; approved plan, steps 2 and 5.
  - Acceptance: same shared manager, ordinary non-PTY POSIX probe; retain deferred
    legacy auto-su and generation safety; root login may honestly report UID 0.
  - Primary evidence: configured-su/sudo startup followed by a legacy command.
  - Status: verified
  - Evidence: controlled su/sudo fixture records no startup elevation; the next
    ordinary command initializes su once and runs as UID 0.
- R3: One 3-second bootstrap deadline includes SSH connect/auth/retries and probe
  waits/read/cleanup; missing, malformed and unreadable sources degrade to null.
  - Source: original fallback request; approved plan, step 2.
  - Acceptance: bounded bytes and completed-record partials; transport failure or
    establishment timeout yields an unknown snapshot without failing MCP startup;
    lifecycle cancellation/signals interrupt bootstrap; healthy SSH survives
    optional metadata timeout/flood and remains usable by the next command.
  - Primary evidence: stalled-connect, source/partial/flood/cancellation and stdio
    startup-recovery/lifecycle tests; existing Debian/Fish and real BusyBox smoke.
  - Status: verified
  - Evidence: 9 probe tests, 7 targeted startup tests and all 11 lifecycle tests
    pass, including 3-second stalled connect, hang/flood/partial/null, signals,
    Fish, and real auth rejection followed by command recovery with frozen nulls.
- R4: Collect once in CLI startup and freeze the final instructions before MCP
  serving. No SSH-session snapshot cache, refresh argument or collector gate remains.
  - Source: approved plan, steps 2, 4 and 5.
  - Acceptance: repeated bootstrap preparation does not recollect; init/discovery
    only read prepared instructions. Commands/reconnect do not change them; new
    process startup recollects. Initially unknown fields stay unknown for this run.
  - Primary evidence: controlled probe counter and byte-stability after reconnect.
  - Status: verified
  - Evidence: probe counter remains 1 after repeated preparation, commands and
    reconnect; instructions are identical. A fresh server startup recollects.
- R5: Preserve KV-friendly steady-state instructions and static tool definitions;
  document startup snapshot semantics and client/provider cache boundaries.
  - Source: user's KV-cache requirement; approved plan, steps 3, 4 and 6.
  - Acceptance: deterministic compact JSON with no timestamps, elapsed time,
    cache-hit or generation IDs; values labeled data, SSH-user probe context, CPU
    estimate and potentially stale; README/AGENTS/this goal describe the new UX.
  - Primary evidence: deterministic rendering and real init/discovery byte tests.
  - Status: verified
  - Evidence: prepared JSON and real discovery bytes remain stable; README/AGENTS
    now document automatic startup data, frozen freshness and cache boundaries.
- R6: Pass fmt, strict clippy, all-features tests/check and release build; record
  current evidence and intended source/docs/test commits, without push or binaries.
  - Source: user approval/build/commit request; previous no-push constraint and
    repository commit style.
  - Primary evidence: actual gate outputs, release version smoke, git diff/status/log.
  - Status: in_progress
  - Evidence: fmt check, strict all-target/all-feature clippy, full tests/check,
    release build and version smoke passed. Commit/durable closure remain.

### Constraints

- KISS/YAGNI/Pareto; reuse the existing probe, parsers, byte caps and rootless seam.
- Fixed fields: hostname, os, distribution, kernel_release, machine_architecture,
  process_architecture, pointer_width, available_cpu_parallelism, effective_uid,
  effective_gid, running_as_root, shell_executable. Unknown UID means unknown root.
- Describe probe namespaces/rootfs and non-login sh, not necessarily the physical
  host, account login shell or elevated commands. CPU is a best-effort nproc estimate.
- Parse os-release as data, never source/eval; use /usr only when /etc is absent.
- Preserve ordinary SSH timeout/auth/host-key policies; only optional bootstrap is
  capped at 3 seconds. Channel close is best-effort, not guaranteed descendant kill.
- Keep lazy library construction; the CLI prepares instructions before serving.
- Preserve existing user edits to docker-compose.yml and opencode.jsonc.
- No push, actual secrets, generated artifacts, dependencies or version bump.

### Non-goals

No replacement tool/resource/prompt, automatic refresh, TTL/disk cache, background
collector, provider cache settings, added CLI configuration, client configuration
edits, general lifecycle redesign, new dependencies or CI job.

## Copy of the Approved Plan

1. Remove tool definition/schema, dispatch, arguments, handler, wrappers and the
   usage note. Restore the original 6/4 tools, order and 3200-byte budget.
2. After constructing the server, install lifecycle cancellation/signals, collect
   once with one 3-second total deadline including connect/auth, then serve MCP.
   Unknown/partial metadata does not fail startup; cancellation stops startup work.
3. Combine the existing instructions with one short snapshot note and compact JSON;
   store the completed instructions string. Both init and discovery read it.
4. Freeze the string for the process lifetime, including SSH reconnect. Recollect
   only on a new process start; no timestamp/volatile metadata or prefix rewriting.
5. Retain HostEnvironment, probe, bounds and parsers; remove refresh/session cache,
   collector gate/cache publication and obsolete tests. Retain transport-only
   connection and deferred auto-su needed by startup and subsequent legacy callers.
6. Adapt init/discovery/surface, rootless, fallback, partial/flood/cancellation,
   lifetime-stability and Debian/Fish/BusyBox tests. Update docs/goal, run gates,
   build release and make local conventional commits without pushing.

## Change Envelope

- Target: bootstrap environment delivery and directly affected SSH/MCP/tests/docs.
- Paths: src/main.rs, src/server.rs, src/server/{args,tools,testing}.rs,
  src/server/handlers/{host_environment,mod}.rs, src/ssh/{environment,connection}.rs,
  directly affected tests, README.md, AGENTS.md, this goal.
- Allowed: focused production/test/documentation edits using existing fixtures.
  Forbidden: modifying/staging existing client/Compose changes, generated binaries,
  new dependency/framework/configuration surface, unrelated fixes, release bump, push.

## Current Checkpoint

- Closes: R6.
- Smallest next action: stage and commit the 14 reviewed implementation/docs/test
  files, then record the actual commit and close the goal.
- Expected evidence: only approved files are committed; the two existing user
  configuration changes remain unchanged and unstaged; no push occurs.
- Replan if: hooks alter the verified source or reject the intended commit.

## Current State

- Resolved: R1–R5; tool/cache removed, bounded startup preparation and immutable
  instructions installed; documentation updated.
- Last relevant evidence: all required gates passed; 305 tests passed (197 library,
  15 compact-response, 79 Docker, 5 integration, 3 logging, 6 doctests), 4 previous
  ignored tests unchanged. Native release version smoke reports ssh-mcp 5.0.1.
- Blocker: none.
- Next: local implementation commit and durable closure commit, without push.

## Material Decisions

- 2026-09-28: User rejected explicit-tool UX after commits 8238745/c330440.
  Their tool/refresh/cache criteria are superseded by the approved startup plan;
  preserve the useful probe/rootless implementation and previous git history.
- 2026-09-28: Snapshot remains frozen across SSH reconnect and may be stale or
  unknown until process restart. This is the approved KV-friendly freshness tradeoff.
- 2026-09-28: “коммит билд” means commit verified source/docs/tests; the ignored
  native binary stays local and no push is performed.

## Checkpoint History

- 2026-09-28: Saved revised approved contract before editing production code.
- 2026-09-28: Replaced tool/cache with startup preparation. Targeted probe, rootless,
  init/discovery, lifetime-stability, partial/fallback and signal evidence is green;
  surface restored to 3192 bytes / 6 tools (4 without sudo). No client/Compose edits.
- 2026-09-28: Auth-recovery fixture initially inspected Docker exec before completion
  (exit_code=None); awaiting stdout EOF fixed the fixture. All 11 lifecycle tests
  and strict clippy then passed; startup nulls stay frozen after successful recovery.
- 2026-09-28: Full fmt/clippy/tests/check/release build succeeded. Diff review found
  only the 14 approved files; client/Compose diff hash matches the initial state.

## Completion

- Resolved outcomes: not yet complete.
- Commands: `cargo fmt --all -- --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test --all-features --verbose`, `cargo check --all-features --verbose`,
  `cargo build --release --all-features`, `target/release/ssh-mcp --version` passed.
- Artifact: `target/release/ssh-mcp`, version 5.0.1; ignored binary stays local.
- Constraint and diff-scope check: approved envelope only; no dependency/version
  changes, actual secrets or artifacts; original client/Compose edits untouched.
- Final status: active.
