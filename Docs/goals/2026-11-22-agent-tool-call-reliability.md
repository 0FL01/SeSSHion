# Goal: Reduce agent tool-call failures

Status: complete
Source: user-approved RECON, tool-hints diff review, and delivery request (2026-11-22)
Last updated: 2026-11-22

## Objective

Reduce reproducible invocation failures in SeSSHion without replaying commands or weakening transfer boundaries; deliver and publish 5.3.1.

## Execution Directive

The plan is GPS, not rails: when evidence disproves an assumption or a step is blocked, reflect, record why, and take the smallest alternative route to the same outcome. The finish line and constraints are fixed unless the user changes them. Do not promote adjacent findings into requirements. Stop when verified or when a proven external blocker leaves no actionable path.

## Frozen Contract

| ID | Required outcome and source | Acceptance / primary evidence | Status |
| --- | --- | --- | --- |
| R1 | Reviewed, concise tool guidance (user review request; RECON P0/P1) | Safe replay preserves JS escapes with the documented pattern; `transfer.local_path`, jobs, and truncation offer actionable hints; tool catalog stays within 3200 bytes. | verified |
| R2 | Bash-first shell policy with portable fallback (RECON P1) | On SSH, auto chooses non-login Bash when available; otherwise POSIX sh; explicit `sh`/`bash` work as documented. Same payload policy for foreground/background/sudo; no retries after possible dispatch, no extra SSH round trip, no effect on internal scripts or RouterOS. | verified |
| R3 | Reject malformed `background` (RECON P1) | Raw MCP call with a non-boolean value fails before any payload executes; boolean and omitted behavior remain compatible. | verified |
| R4 | Commit, push, build, and verify against the test container (latest user request) | Pushed commit/build is identifiable; release binary against test sshd passes minimal reproductions, timeout handoff, transfer boundary, and relevant existing checks. | verified |
| R5 | Increment Cargo patch version and publish to crates.io (latest user request) | Cargo manifest/lock are 5.3.1 and crates.io lists `ssh-mcp-rs` 5.3.1. | verified |

Constraints: show proposed hint changes before applying; keep payload escaping intact, `local_root` containment and explicit transfer transport unchanged, and retain PID/log/exit and timeout-handoff semantics. Do not replay commands from the private export, access the production `mi50` target, or include private inputs/credentials in the repository.

## Plan and Change Envelope

1. Apply the shown compact hints, with one shared Code Mode note. Use a safe JS/MCP replay and a direct raw MCP call as control; large scripts use transferred files or correctly formed patch strings. No global OpenCode config change.
2. Implement `auto`/`sh`/`bash` for `shell` and `sudo_shell` only, selecting once before interpreting payload. Preserve existing sudo authorization and job wrapper, and test Debian/dash+Bash and BusyBox without Bash. Canary the new behavior on the test container, retaining `sh` as rollback. Do not change startup probing, transfer dispatch, patching, transport retry, or RouterOS.
3. Validate boolean `background` and preserve job result/error semantics. Verify lossless escaping, malformed arguments, once-only execution, privilege, PID/log/exit, timeout handoff and transfer path policy. Use the existing regression suites and focused additions for the new behavior.
4. Bump Cargo patch version/lock, commit and push, check build, verify the release binary against the test container and publish the same version to crates.io. Live production rollout is outside this task.

Expected files: `src/server{.rs,/args.rs,/exec.rs,/tools.rs}`, CLI/server construction, a small payload selector, affected hint text in `src/ssh/command.rs`, README configuration, focused tests, `Cargo.toml`/`Cargo.lock`. No new dependencies or workflows.

## Checkpoint Notes

- Baseline: 114 MCP calls, 3 MCP errors (2 JS transformations, 1 out-of-root transfer); 4 more JS parse errors before dispatch. Separately, literal `{a,b}` caused a terminal background failure on POSIX sh. Debian test sshd reproduced shell/escaping/transfer boundaries and silent foreground execution on invalid `background`. One unresolved poll in the export is not evidence of failure.
- Replay adjustment: waiting for SSH readiness alone did not fix the restart fixture because Docker reassigned its ephemeral port. Preserve the port, wait for the SSH banner, then send the payload once. No production retry change.

## Evidence and Completion

- Local checks: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, 275 library tests and 26 targeted integration tests (3 fixture-dependent tests ignored), `cargo build --locked --release` (5.3.1). Worst default catalog: 3091/3200 bytes (previously 3172).
- Release binary, 12 safe MCP checks: Debian with Bash/dash, real Alpine BusyBox ash in a disposable Debian sshd fixture with Bash removed, both sudo wrapper variants, PID/logs, timeout handoff, no replay after `exit 127`, raw escape round trip, malformed boolean with no side effect, transfer containment and checksum, and reconnect.
- The Code Mode check verifies the documented JavaScript pattern through real MCP; it is not a live comparison of model error rates.
- Implementation commit `fce02a61e3c9f24b4c1ffb8e044ce7034a73e631` pushed to `origin/main`; [GitHub CI run](https://github.com/0FL01/SeSSHion/actions/runs/45018569573) triggered. The local release build and Cargo archive verification are the delivery build evidence.
- `cargo publish --locked` verified and uploaded [ssh-mcp-rs 5.3.1](https://crates.io/crates/ssh-mcp-rs/5.3.1). `cargo info ssh-mcp-rs@5.3.1 --registry crates-io` downloaded the public archive; its `.cargo_vcs_info.json` matches the pushed implementation commit. Cargo warned about three existing yanked transitive versions, without blocking publication.
- Closure: R1–R5 verified. No production change, new dependencies, workflow changes, or weakened transfer policy.
