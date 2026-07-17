# Goal: Replace text edit tools with `apply_patch`

Status: active
Source: user-approved plan in `plan.md` and user clarification to support create, edit, and delete
Last updated: 2026-07-17

## Objective

Ship a breaking `3.0.0` MCP tool contract in which one public `apply_patch`
tool creates, edits, or deletes one remote text file per call, replacing
`write-file`, `replace-in-file`, and read tickets.

## Execution Directive

Complete the frozen Required Outcomes using the listed Change Envelope and
Primary Evidence. Work on the smallest unresolved outcome. Do not add
requirements from reviews, tests, tools, speculative risks, or optional source
text. Finish when every required outcome is resolved and affected constraints
remain satisfied.

## Frozen Contract

### Required Outcomes

- R1: Expose one text-edit tool.
  - Source: User instruction to replace the write/create tools with one
    `apply_patch`, followed by approval of the reviewed plan.
  - Acceptance: `list_tools` exposes canonical `apply_patch`; `write-file` and
    `replace-in-file` are absent and are not retained as aliases. Transfer tools
    remain unchanged.
  - Primary evidence: Public tool-list and dispatch unit tests, then
    `cargo test --lib`.
  - Status: pending
  - Evidence:

- R2: Parse and plan deterministic one-file patches.
  - Source: Approved plan and the user's clarification that the tool must
    create, edit, and delete files.
  - Acceptance: The strict `*** Begin Patch` / `*** End Patch` envelope accepts
    exactly one absolute-path `*** Add File`, `*** Update File`, or
    `*** Delete File` section. Update supports multiple exact hunks and rejects
    missing or ambiguous context without fuzzy matching.
  - Primary evidence: Parser and planner unit tests via `cargo test --lib patch`.
  - Status: pending
  - Evidence:

- R3: Apply create, edit, and delete safely on the current remote runtime path.
  - Source: Approved plan, including `dry_run`, SHA conflict detection, and the
    user's create/edit/delete clarification.
  - Acceptance: Add requires a missing destination, Update and Delete require
    an existing UTF-8 file, `dry_run` makes no remote change, and an optional
    `expected_sha256` mismatch fails. Commit rechecks the loaded snapshot under
    the existing same-path lock so a concurrent change is not overwritten or
    deleted. Successful writes retain atomic final rename behavior.
  - Primary evidence: Relevant cases in
    `cargo test --test docker_integration_test`.
  - Status: pending
  - Evidence:

- R4: Remove ticket and MCP-handler coupling from the edit path.
  - Source: Approved plan requirement that handlers use typed snapshot and
    commit results rather than invoking handlers and parsing `CallToolResult`.
  - Acceptance: `apply_patch` consumes typed remote snapshots and commit
    results; internal edit code does not parse public MCP JSON. `read_ticket`,
    `TicketSigner`, and their dedicated dependencies are removed. `read-file`
    reports `content_sha256` for returned content and `file_sha256` only for a
    full-file read.
  - Primary evidence: Source diff plus read/tool contract tests via
    `cargo test --lib` and the read-file cases in
    `cargo test --test docker_integration_test`.
  - Status: pending
  - Evidence:

- R5: Publish the approved breaking contract in repository metadata and docs.
  - Source: Approved plan.
  - Acceptance: The crate version is `3.0.0`; ticket-only dependencies and
    legacy edit modules are gone; `README.md`, crate docs, and `AGENTS.md`
    describe the resulting tool and module layout.
  - Primary evidence: Final source/documentation diff and `cargo test`.
  - Status: pending
  - Evidence:

- R6: Pass the approved validation gates.
  - Source: Acceptance criteria in the approved plan.
  - Acceptance: Formatting, the Rust test suite, and relevant Docker read/edit
    integration tests pass after the final implementation diff.
  - Primary evidence: `cargo fmt --check`, `cargo test`, and
    `cargo test --test docker_integration_test`.
  - Status: pending
  - Evidence:

### Constraints

- C1: One patch may target exactly one absolute remote path.
- C2: Remote text and resulting content remain UTF-8 and limited to the current
  1 MiB edit limit; the destination parent directory must already exist.
- C3: Keep the current same-path lock, snapshot SHA conflict behavior, remote
  staging cleanup, and atomic final rename for Add and Update.
- C4: Expected domain failures are structured tool errors; malformed MCP
  parameters remain MCP errors.
- C5: The public response identifies the path, operation, dry-run state,
  changed state, previous/new SHA where applicable, and resulting byte count.

### Non-goals

- Multi-file patches or batch atomicity.
- `Move File` or rename operations.
- Configured edit roots, a new authorization layer, or path-policy redesign.
- New mode/ownership preservation or symlink semantics.
- Retry frameworks, persistent state, outcome ledgers, or background workers.
- Changes to transfer tool behavior.
- Hidden compatibility aliases for removed edit tools.

## Change Envelope

- Target: The public text-file edit API and its directly used read, planning,
  remote snapshot, commit, response, test, and documentation paths.
- Expected paths, symbols, and direct consumers:
  - `Cargo.toml`, `Cargo.lock`, `README.md`, `AGENTS.md`, and `src/lib.rs`.
  - `src/tools/mod.rs`, `src/server.rs`, and `src/server/tools.rs` for params,
    schemas, tool listing, routing, and response documentation.
  - A minimal patch parser/planner module and
    `src/server/handlers/apply_patch.rs`.
  - `src/server/handlers/read_file.rs` and
    `src/server/handlers/file_edit_common.rs` for typed snapshot/commit flow.
  - Handler/module exports and `src/server/testing.rs`.
  - Removal of `write_file.rs`, `replace_in_file.rs`, `src/ticket.rs`, and their
    parameters, routes, tests, and dependencies.
  - Direct unit and Docker integration tests for tool contracts and retained
    edit/read behavior.
- Allowed artifacts: Rust implementation, directly relevant tests, MCP schemas
  and docs, dependency cleanup, and the required version bump.
- Forbidden artifacts: New dependencies unless the frozen outcomes are proven
  impossible without one; services, workers, queues, persistent stores,
  generic remote-filesystem frameworks, auth layers, or compatibility shims.
- User or harness budget: No numeric file or diff budget was supplied. Stay
  within the paths and artifact categories above; record a concrete blocker
  before expanding them.

## Current Checkpoint

- Closes: R2.
- Smallest next action: Implement the one-file Add/Update/Delete AST, strict
  parser, exact planner, and their unit tests without wiring remote I/O yet.
- Expected evidence: `cargo test --lib patch` passes for valid operations,
  malformed envelopes, unsupported/multiple sections, non-absolute paths, and
  missing or ambiguous Update context.
- Stop or replan if: The approved envelope cannot represent one of the three
  required operations deterministically without expanding the public contract.

## Current State

- Resolved: The finish line and scope were approved; Delete was explicitly
  added alongside Add and Update.
- Last relevant evidence: Repository inspection confirmed the current public
  tools, ticket coupling, typed-boundary gap, lock/CAS transaction, 1 MiB limit,
  and existing unit/Docker test surfaces.
- Blocker: None.
- Next: Execute the R2 checkpoint.

## Material Decisions

- 2026-07-17: Use canonical `apply_patch` only and remove both legacy text-edit
  tools rather than retaining aliases.
- 2026-07-17: Support Add, Update, and Delete for one absolute path; exclude
  Move and multi-file patches.
- 2026-07-17: Preserve the existing absolute-path contract instead of adding
  configured edit roots.
- 2026-07-17: Keep the patch implementation local and typed; do not introduce a
  generic remote-filesystem abstraction.

## Checkpoint History

- 2026-07-17: Contract frozen after repository/`plan.md` review and explicit
  user approval. No implementation changes made. Next: R2 parser/planner.

## Completion

- Resolved outcomes: Not complete.
- Commands and artifacts: Not complete.
- Constraint and diff-scope check: Not complete.
- Final status: active.
