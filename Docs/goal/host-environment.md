# Goal: rootless host environment snapshot

Status: active
Source: user request for remote environment information, the reviewed plan, and
“делай копию плана в goal и итеративно реализовать и коммит билд, без пуша”.
Last updated: 2026-09-28

## Objective

Ship a bounded, rootless `host_environment` MCP tool with graceful Linux
fallbacks, a session-scoped snapshot cache, and a stable model-visible prefix;
verify the implementation, build the release binary, and commit without pushing.

## Execution Directive

Complete the frozen Required Outcomes using the listed Change Envelope and
Primary Evidence. Work on the smallest unresolved outcome. Do not add requirements
from reviews, tests, tools, speculative risks, or optional source text. Finish when
every required outcome is resolved and affected constraints remain satisfied.

## Frozen Contract

### Required Outcomes

- R1: The explicit read-only `host_environment({refresh:false})` tool reports all
  requested fields as one normalized JSON object; missing or malformed values
  become `null`, not crashes or invented defaults.
  - Source: original rootless/fallback request; corrected plan, sections 1 and 3.
  - Acceptance: strict optional boolean `refresh`; fixed nullable fields; one
    compact text JSON and identical `structuredContent`; independent fallbacks.
  - Primary evidence: parser/source tests and real Debian, Fish, BusyBox smoke.
  - Status: verified
  - Evidence: parser/strict-argument/JSON tests pass; real Debian/Fish SSH and
    genuine Alpine/BusyBox probe smoke pass, including missing/unreadable sources.
- R2: Collection never initiates `su`/`sudo`, including cold connect/reconnect,
  and preserves existing callers' configured automatic elevation behavior.
  - Source: rootless request; corrected plan, section 2.
  - Acceptance: ordinary exec without PTY on the shared route; deferred legacy
    auto-elevation once per route when the probe established transport first.
  - Primary evidence: configured-elevation cold/reconnect and legacy-call tests.
  - Status: verified
  - Evidence: cold and invalidated/reconnected probes with configured su/sudo
    leave invocation counters empty; legacy exec then initializes su exactly once
    and executes with UID 0; a later probe still reports UID 1000.
- R3: Bounded collection preserves completed fields on metadata timeout/flood;
  cancellation and detected transport errors do not publish a fresh snapshot;
  metadata failures do not break the healthy SSH session or MCP process.
  - Source: fallback request; corrected plan, section 4.
  - Acceptance: one absolute 3-second budget after transport establishment,
    including gate/slot waits and cleanup; cancellation from connect onward;
    byte caps before accumulation/parsing and complete framed records only.
  - Primary evidence: hang/flood/partial/cancellation tests followed by a command,
    and MCP tool-error followed by ping/tools-list.
  - Status: verified
  - Evidence: real SSH hang/stdout-flood/stderr-flood/partial, gate/slot budget,
    cancellation and next-command tests pass; stalled TCP establishment is
    cancellable and releases connection ownership; stdio tool error preserves ping/list.
- R4: Cache belongs to the actual route generation; misses coalesce; refresh
  fully replaces values, including new nulls; reconnect invalidates lazily;
  stale producers cannot publish into a new route.
  - Source: corrected plan, section 5.
  - Acceptance: one collector gate and snapshot; atomic generation/cache access;
    no session mutex during collection; failed refresh retains old cache only as
    old cache, never as a successful refreshed result; shutdown is terminal.
  - Primary evidence: cache/refresh/cancellation and deterministic generation-race tests.
  - Status: verified
  - Evidence: eight concurrent misses collect once; refresh replaces CPU with
    null; cancelled refresh preserves prior cache; reconnect recollects; controlled
    in-flight producer cannot publish after route removal; shutdown is terminal.
- R5: Instructions and tool definitions/order are byte-stable for fixed config
  through collection, refresh, reconnect and errors; snapshots are tool results
  only, deterministic and free of volatile cache/monitoring metadata.
  - Source: “важно не забыть про кеш friendly, шо бы kv кеш не ломать при работе”;
    corrected plan, section 6.
  - Acceptance: static appended tool and usage note, no list-changed push or
    dynamic instructions; documented append-only delivery and client/provider limits.
  - Primary evidence: MCP surface/result stability and measured wire-budget tests.
  - Status: verified
  - Evidence: real stdio cached/refresh snapshots preserve JSON/text bytes and
    definitions; both sudo configurations retain exact definitions after errors;
    instructions unchanged across collect/refresh/reconnect/elevation. Wire surface
    is 3513/3520 bytes. README documents append-only/client/provider limits.
- R6: Save this plan, pass relevant gates, produce a release build and local
  conventional commit(s), without pushing or committing generated binaries.
  - Source: latest user implementation/build/commit request; repository commit style.
  - Acceptance: successful fmt, clippy, all-features tests/check and release build;
    goal complete with current evidence; intended source/docs/tests committed.
  - Primary evidence: gate output, git diff/status and local commit log.
  - Status: in_progress
  - Evidence: plan saved; fmt, clippy, all-features tests/check and release build
    passed. Native binary reports `ssh-mcp 5.0.1`. Local commit remains the final action.

### Constraints

- Follow KISS/YAGNI/Pareto, existing Rust types, error model and MCP behavior.
- Rootless means no elevation above the SSH login user; root login may report UID 0.
- Snapshot describes the probe's namespaces/rootfs and running POSIX `sh`, not
  the physical host, login shell or elevated persistent shell.
- SSH establishment uses its existing timeout/retry policy, outside the metadata
  budget. Closing the probe channel is best-effort, not a descendant-kill guarantee.
- No push; preserve unrelated work; no secrets or generated artifacts in commits.

### Non-goals

No startup collection, background refresh, TTL/disk/global cache, subscriptions,
delta/ETag protocol, XML renderer, outputSchema, additional SSH session, new
dependency, generic scheduler/transport redesign, full cgroup quota calculation,
legacy distro release database, or new CI job. Provider cache hits and client
history rewriting/compaction are outside the server's control.

## Copy of the Corrected Plan

1. Add one static read-only tool at the end of the existing tool list, with strict
   `refresh` validation (default false), fixed fields and `CallToolResult::structured`.
   Add only a fixed usage note to existing instructions.
2. Separate transport establishment from best-effort automatic `su` initialization.
   Probe uses the same manager and an ordinary non-PTY channel; legacy callers
   initialize deferred automatic elevation once per actual route generation.
3. Use minimal independent sources (all unknowns are JSON null):

   | Field | Source / fallback |
   | --- | --- |
   | hostname | `uname -n`, `/proc/sys/kernel/hostname` |
   | os | `uname -s`, `/proc/sys/kernel/ostype` |
   | distribution | `/etc/os-release`; `/usr/lib/os-release` only if primary absent; parse PRETTY_NAME, NAME/version, ID in Rust, never source/eval |
   | kernel_release | `uname -r`, `/proc/sys/kernel/osrelease` |
   | machine_architecture | `uname -m` (kernel-reported, not proven physical ISA) |
   | process_architecture / pointer_width | bounded ELF header of `/proc/$$/exe`; small supported Linux ABI allowlist |
   | available_cpu_parallelism | positive `nproc` estimate after clearing OMP_NUM_THREADS / OMP_THREAD_LIMIT |
   | effective_uid / effective_gid | `id -u` / `id -g`; effective (second) ID in `/proc/$$/status` |
   | running_as_root | derived from known UID; null if UID unknown |
   | shell_executable | `readlink /proc/$$/exe`, not `$SHELL` or `command -v sh` |

4. Run a fixed POSIX non-login probe with framed records and explicit completion
   status; cap raw stdout/stderr and individual values/files independently of
   max_output_tokens. Preserve valid completed records if later collection fails
   locally. One absolute metadata budget includes gate/slot/open/exec/read/cleanup.
   Cancellation applies from outset; real detected transport/auth failure is an
   ordinary tool error, while optional metadata failures are channel-local.
5. Store one snapshot on the route and use one collection mutex. Recheck false
   misses under the gate; serialize true refreshes; replace the entire snapshot.
   Capture generation with the opened channel and check it atomically with
   publication under the route lock. Never lock the route throughout collection.
   Reconnect invalidates lazily; cancellation/failed transport cannot erase or
   falsely refresh old cache. Shutdown prevents late publication.
6. Keep discovery/instructions/tool schemas and order immutable for fixed
   version/config. Return only full deterministic snapshots, not timestamps,
   generation IDs, cache-hit flags or monitoring data. New results semantically
   supersede older results without replacing historical messages. No snapshot
   repetition in shell responses; no tools/list_changed for environment changes.
7. Keep production changes focused on `ssh/environment.rs`, the thin
   `handlers/host_environment.rs`, transport/cache lifecycle in `connection.rs`,
   tool registration/module wiring and argument validation. Change `command.rs`
   only if needed for the narrow transport seam; no general cache framework.
8. Verify parser/fallback/unknown/ELF behavior; timeout/flood/partial/cancellation;
   cold/reconnect rootless collection and legacy elevation; cache concurrency,
   refresh and stale-publication race; static MCP/KV surface and JSON compatibility.
   Reuse Debian/Fish fixtures, add genuine BusyBox smoke, mostly synthetic failure
   tests, and measure the necessary extension of the existing 3200-byte tool budget.

## Change Envelope

- Target: environment snapshot tool, directly affected SSH/MCP lifecycle and tests.
- Expected paths: `src/ssh/{environment,connection,command,mod}.rs`,
  `src/server.rs`, `src/server/{tools,args,testing}.rs`,
  `src/server/handlers/{host_environment,mod}.rs`, directly relevant tests and
  fixtures, `README.md`, `AGENTS.md`, this goal.
- Allowed: focused source/tests/documentation changes and existing local Docker
  fixtures. Forbidden: binaries/logs/secrets, dependency/framework expansion,
  unrelated fixes, release/version bump and push.

## Current Checkpoint

- Closes: R6.
- Smallest next action: stage only reviewed source/docs/tests, create the local feature
  commit, record its evidence and close this goal in a documentation commit. Do not push.
- Expected evidence: local commit log and clean worktree, with all gates already green.
- Replan if: existing transport/elevation behavior exposes a concrete regression.

## Current State

- Resolved: R1–R5 verified, implementation and public usage documentation complete.
- Last relevant evidence: `cargo fmt --all -- --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test --all-features --verbose` (308 passed, 4 pre-existing ignored),
  `cargo check --all-features --verbose`, `cargo build --release --all-features`,
  and `target/release/ssh-mcp --version` all pass. All 80 Docker integration tests pass.
- Blocker: none.
- Next: local feature commit, goal closure record and documentation commit.

## Material Decisions

- 2026-09-28: Use existing `Docs/goal/` convention. “коммит билд” means commit
  verified implementation, not ignored compiled binaries; no push.
- 2026-09-28: Freeze the audited explicit-tool plan; unknown fields stay null,
  CPU is explicitly an estimate, no incidental auto-elevation on probe connect.

## Checkpoint History

- 2026-09-28: R6 started: saved contract before implementation; tooling available.
- 2026-09-28: R1–R5 first checkpoint: six parser/source tests, two argument/JSON
  tests and the updated static tool budget pass. Compiler required an explicit
  internal error type; fixed without changing error semantics or dependencies.
- 2026-09-28: R1–R4 runtime checkpoint: seven SSH/stdio scenarios pass, including
  source fallbacks, rootless cold/reconnect, deferred elevation, cache/race and bounded
  failures. Fixture readiness now checks the SSH greeting (TCP accept was too early
  for Fish); legacy UID assertion accounts for existing PTY terminal prefixes.
  Clippy identified ambiguous NUL-plus-digit test literals; disambiguated with hex.
- 2026-09-28: R5 verified: successful real stdio structured snapshots/cache/refresh
  leave definitions unchanged; unavailable-host errors preserve MCP ping/list in
  both sudo configurations. All-target/all-feature clippy passes without allowances.
- 2026-09-28: R6 validation checkpoint: mandatory full gates pass, including
  all 80 Docker integrations. Release binary built at `target/release/ssh-mcp`
  and version smoke passed. Diff/whitespace reviewed; no dependencies, release
  version changes, generated binaries, actual credentials or unrelated files added.
- 2026-09-28: Closure check identified a poll-time cancellation window between
  the outer select and cache publication. Recheck the token under the route lock
  before commit; a deterministic lock/poll test passes. All mandatory gates and
  release build repeated successfully after this final code change (308 passed,
  including all 80 Docker integrations; no failed tests or new ignored tests).

## Completion

- Resolved outcomes: not yet complete.
- Commands and artifacts: not yet complete.
- Constraint and diff-scope check: pending.
- Final status: active.
