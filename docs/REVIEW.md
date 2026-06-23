# Code Review (Fable 5) — Findings and Disposition

Three independent review passes (correctness, security, spec-fit) ran over the
codebase. This is the consolidated list with each finding's status. Severities
are the reviewers'.

## Critical — fixed

| ID | Finding | Fix |
|----|---------|-----|
| C1 | The zero-isolation `mock` runtime could be selected in production via config/env with no guard, and reported `kvm_ok=true`. | `serve` now **refuses to start** with `runtime=mock` unless `WORKDIR_ALLOW_INSECURE_RUNTIME=1` is set, and logs a loud warning. (`app.rs`) |
| C2 | The Firecracker file API passed raw user paths to the guest agent — no workspace jail, so `PUT /files {"path":"/etc/passwd"}` could clobber guest files. | `jail_guest_path` confines `read_file`/`write_file`/`list_dir` to `/workspace`, rejecting `..`/absolute escapes. (`runtime/firecracker.rs`) |

## High — fixed

| ID | Finding | Fix |
|----|---------|-----|
| #1 | Admission/quota TOCTOU: concurrent creates each read a stale capacity snapshot and all pass, overcommitting the node. | An `admission` async mutex serializes quota+placement+reservation of the `creating` row; released before the (slow) boot. (`state.rs`, `service.rs`) |
| #2,#3 | Blind whole-record upserts + stale copies let concurrent/late writers resurrect deleted sandboxes and double-open billing intervals. | All lifecycle changes go through `store::cas_sandbox` (compare-and-set on state); `open_usage_if_none` refuses a second open interval. (`store.rs`, `service.rs`) |
| #4 | Failed `pause`/`delete` returned before closing usage, leaving an interval open forever (unbounded billing) and the sandbox stuck in `stopping`. | Billing is closed **before** the runtime call; pause failure transitions to `failed`, not stuck. (`service.rs`) |
| #5 | No crash/restart reconciliation: `creating`/`running` rows + open intervals survived a restart with no backing VM. | `store::reconcile_interrupted` runs at startup: marks interrupted sandboxes `failed` and closes their intervals. (`app.rs`, `store.rs`) |
| #6,#7 | Idle reaper killed sandboxes with live preview/VNC/PTY/exec traffic because `last_active_at` was only touched after exec. | Activity is touched on preview, PTY, and before+after exec, via a cheap `json_set` update that can't clobber state. (`api/preview.rs`, `api/pty.rs`, `api/sandboxes.rs`, `store.rs`) |
| H1 | Preview `?key=` API token was forwarded to the (untrusted) sandbox upstream and logged. | `strip_key_query` removes the token before forwarding; logs are redacted. (`api/preview.rs`) |
| H2 | Preview proxy dialed `127.0.0.1:<user-port>`, an SSRF gateway to the control plane and any loopback service; any sandbox could hit any port. | The proxy now only forwards ports the sandbox actually exposed and refuses the control-plane port. (`api/preview.rs`) |
| L2 | Preview returned distinguishable 404/409/401, leaking cross-org sandbox existence/state. | Authorization happens **before** existence is revealed; unauthorized callers get a uniform 404. (`api/preview.rs`) |

## Medium — fixed

| ID | Finding | Fix |
|----|---------|-----|
| #10 | A Firecracker boot that failed after the jailer spawn (e.g. agent timeout) leaked a live VM + jail dir. | Post-spawn work is wrapped; any error calls `kill_and_reclaim` (kill pid, drop record, rm jail/workspace). (`runtime/firecracker.rs`) |
| #11 | The published at-cost price divided a month of node cost by **all-time** delivered units, trending to zero. | `admin_overview` clips delivered units to the current calendar month. (`api/usage.rs`) |
| M3 | Secrets were written to a guest env file and could be captured in snapshots. | Secrets are applied **per-exec from host memory** (never written to a guest file); snapshots are **refused** while secrets are resident. (`runtime/*`, `service.rs`) |

## Deferred (documented, lower priority)

These are real but lower-severity or larger-scope; tracked for a follow-up.

| ID | Finding | Plan |
|----|---------|------|
| #8 | Credit enforcement is lazy (`spent_usd` only updates on interval close), so an out-of-credit org with a long-running sandbox isn't stopped. | Add a periodic spend-recompute pass that suspends over-balance orgs; compute projected balance at create time. |
| #9 | Hot-pool warm VMs are in-memory only; a restart orphans the previous warm VMs (leaked workspaces/jails). | Persist warm handles or sweep `warm_*`/jail dirs at startup. |
| H3 | Host-originated fetches (startup `ready.http`, preview) bypass the sandbox-scoped nftables metadata block. | Validate `ready.http`/proxy targets against metadata/link-local/RFC1918 at the app layer; add host output-chain rules. |
| H4 | No rate limiting on auth failures. | Add failed-auth rate limiting + audit, especially on the preview path. |
| M1 | systemd grants `CAP_SYS_ADMIN`+`CAP_DAC_OVERRIDE` to the whole control-plane process. | Split privileges: unprivileged API process + a minimal jailer/network helper. |
| M2 | nftables `input` chain is a no-op (`policy accept`), no IPv6 rules, and the installer's embedded copy diverges from the repo file. | `policy drop` + explicit accepts, IPv6 rules, single source of truth. |
| M4 | Custom-image builds materialize real Firecracker rootfs artifacts on Firecracker nodes, but the advertised capability scan/secret scrub is not yet an enforcing validation step. | Add enforced capability scanning, secret scrubbing, and `context_url`/`image_ref` SSRF validation before treating custom images as safe for untrusted tenants. |
| L1 | Workspace jail is lexical (symlink-unsafe for the dev runtime file API). | `canonicalize` the resolved parent / open with `O_NOFOLLOW`. |

## Verified positives

Cross-org ownership is enforced consistently (`load_owned`, image/secret org
scoping). API keys are stored only as SHA-256 hashes (shown once). Shell
interpolation of git URLs / env values is single-quote escaped. The new secret
store uses AES-256-GCM with the key kept out of the database.
