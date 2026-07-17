# Goal: Simplify `apply_patch` state and contract

Status: complete
Source: user-approved simplification plan after the OpenCode comparison
Last updated: 2026-07-17

## Objective

Keep the one-file exact Add/Update/Delete behavior while reducing the public
tool to `{patch}`, hiding concurrency state inside one call, and storing local
temporary state under `/tmp/ssh-mcp`.

## Execution Directive

Complete the frozen Required Outcomes using the listed Change Envelope and
Primary Evidence. Work on the smallest unresolved outcome. Do not add
requirements from reviews, tests, tools, speculative risks, or optional source
text. Finish when every required outcome is resolved and affected constraints
remain satisfied.

## Frozen Contract

### Required Outcomes

- R1: Expose only the patch text as public edit input.
  - Source: User-approved plan to remove explicit SHA, dry-run, and timeout
    arguments.
  - Acceptance: `apply_patch` accepts exactly `{patch}` and rejects unknown
    fields. Success is `{ok,path,operation}`; errors expose no hashes.
  - Primary evidence: Parameter/tool tests and source diff.
  - Status: verified
  - Evidence: 2026-07-17 parameter/schema tests passed in `cargo test`; the
    public struct and MCP schema contain only `patch`, and Docker verified the
    compact success response.

- R2: Keep concurrency protection internal to one call.
  - Source: Approved plan to copy OpenCode ergonomics without copying unsafe
    direct-write behavior.
  - Acceptance: The handler reads its own snapshot and commit still checks that
    hidden baseline under the remote same-path lock. No prior `read-file` or
    persistent cross-call state is required.
  - Primary evidence: Focused Docker Add/Update/Delete and injected-conflict
    cases.
  - Status: verified
  - Evidence: 2026-07-17 Docker Add/Update/Delete and injected concurrent
    mutation cases passed; the conflict response contains no hash state.

- R3: Store ephemeral local edit state in the server spool.
  - Source: User instruction to keep state in `/tmp` on the host.
  - Acceptance: Both snapshot and result staging use the existing
    `/tmp/ssh-mcp` spool and are cleaned after each call; edit runtime no longer
    depends on transfer `local_root`.
  - Primary evidence: Source diff and Docker edit cases.
  - Status: verified
  - Evidence: 2026-07-17 source diff uses `spooler.base_dir()` for both
    `apply-patch-read-*` and `apply-patch-write-*`; focused Docker edits passed.

- R4: Remove superseded code and tests.
  - Source: User instruction to remove dead code/tests and not cover deleted
    behavior.
  - Acceptance: Read responses have no SHA fields; dry-run/diff, explicit SHA,
    detailed hash responses, obsolete fault helpers/markers, and `similar` are
    removed with their dedicated tests.
  - Primary evidence: Source/dependency grep and final test gate.
  - Status: verified
  - Evidence: 2026-07-17 removed public/hash/dry-run branches, markers, fault
    helpers, integration cases, and `similar`; final tests passed.

- R5: Pass proportional validation.
  - Source: User instruction to complete the work and use Pareto tests.
  - Acceptance: Formatting, Rust tests, and relevant Docker runtime tests pass.
  - Primary evidence: `cargo fmt --check`, `cargo test`, Docker test command,
    and final `git status`/commit.
  - Status: verified
  - Evidence: 2026-07-17 `cargo fmt --check`, focused Docker MCP runtime, and
    final `cargo test` passed: 180 library tests, 62 Docker integration tests,
    remaining integration suites, and 6 doctests.

### Constraints

- One patch targets one absolute path and supports Add, Update, or Delete only.
- Exact unique matching, UTF-8, the 1 MiB limit, parent-exists rule, remote
  staging, hidden snapshot check, same-path lock, and atomic Add/Update finalize
  remain.
- Tests stay focused on current observable behavior; removed behavior receives
  no replacement coverage.

### Non-goals

- Persistent state across calls, sessions, or restarts.
- Fuzzy matching, Move, multi-file patches, formatter/LSP integration.
- Transfer behavior changes, edit roots/auth, metadata, or symlink redesign.

## Change Envelope

- Public apply/read schemas, handlers, typed edit runtime, direct tests, docs,
  dependency metadata, and this goal state.
- Allowed: deletion and simplification of superseded implementation and tests.
- Forbidden: new dependencies, persistent stores, services, workers, retries,
  auth layers, compatibility aliases, and unrelated cleanup.

## Current Checkpoint

- Closes: R1-R5.
- Smallest next action: None; closure check passed.
- Expected evidence: Recorded above and in Completion.
- Stop or replan if: Not applicable; objective is complete.

## Current State

- Resolved: R1-R5. Public edit input is only `patch`; temporary state is local
  to each call under the spool; obsolete public fields and code are removed.
- Last relevant evidence: Final `cargo test` passed after implementation and
  test cleanup.
- Blocker: None.
- Next: None.

## Material Decisions

- 2026-07-17: “State in `/tmp`” means ephemeral per-call snapshot/result files,
  not a persistent cache of prior reads.
- 2026-07-17: Retain hidden SHA comparison and remote lock, but remove all
  caller-visible hashes and preconditions.
- 2026-07-17: Remove dry-run/diff and its dependency rather than preserving
  tests for deleted behavior.

## Completion

- Resolved outcomes: R1-R5 verified.
- Commands and artifacts: `cargo fmt --check`; focused Docker MCP tool test;
  final `cargo test`; implementation, schemas, focused tests, docs, and
  dependency cleanup.
- Constraint and diff-scope check: One exact absolute-path Add/Update/Delete
  patch remains; hidden per-call CAS and lock remain; no persistent state,
  public hashes, dry-run, diff dependency, transfer behavior change, or new
  infrastructure was added.
- Final status: complete.
