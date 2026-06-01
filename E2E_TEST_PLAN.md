# E2E Test Plan — Claude Subscription Auth & Proactive/Background Agent

Scope: end-to-end tests for (A) the Claude **subscription** auth path and (B) the
**proactive/background** ("Kairos") agent. Status is split deliberately:

- **A — subscription auth: IMPLEMENTED.** Testable end-to-end now.
  Surface: `AuthSource::OAuthBearer`, beta `oauth-2025-04-20`,
  `inject_oauth_subscription_system` (prepends the `You are Claude Code…`
  identity block), `resolve_startup_auth_source` (saved-token fallback, env
  wins), `save_oauth_credentials`, `claw login`/`logout`
  (`rust/crates/api/src/providers/anthropic.rs`, `runtime/src/{oauth,config}.rs`,
  `rusty-claude-cli/src/main.rs`).
- **B — proactive/background agent: PARTIAL.** The background-task *tools*
  (`create/status/list/stop/send/output`) and `Brief`/`SendUserMessage`
  `normal|proactive` routing exist (`rust/crates/tools/src/lib.rs:656,759-855,2400`).
  The autonomous **loop** (tick engine, SleepTool, on-start dream scheduler) is
  **not built** — only the auth-trace component shipped. So B is "test what
  exists now" + "test-first specs for the loop, built alongside the feature".

## Harness (already proven by `tests/dream_e2e.rs`)

Real binary + mock HTTP + temp config, Tier-2 fidelity:

- `Command::new(env!("CARGO_BIN_EXE_claw"))` — the real CLI.
- `mock-anthropic-service` via `ANTHROPIC_BASE_URL`. Its `CapturedRequest`
  exposes **`headers: HashMap`** and **`raw_body`** — this is what lets us assert
  the exact subscription wire shape *without a live subscription*.
- `CLAW_CONFIG_HOME=<temp>` isolates credentials/journal/markers.
- `.env_clear()` + explicit env so the host can't contaminate auth.

## Anti-flake controls (acceptance criteria for every test below)

- temp working dir + temp `CLAW_CONFIG_HOME`; full `.env_clear()` with only the
  vars the case needs (never inherit `ANTHROPIC_API_KEY` / `CLAW_AUTH_TRACE`).
- **virtual clock only — no real `sleep`** anywhere in loop/scheduler tests.
- deterministic mock responses; assert the **first** outbound request after start.
- explicit teardown: stop/await every background task; assert **no orphaned child
  process** remains.
- golden/transcript tests must freeze or normalize timestamps, IDs, and ordering.

---

## A — Claude subscription auth

Order matters: prove the load path, then precedence, then wire shape, then refresh.

### A0 — Saved-token-load spike (LINCHPIN, do first)
Prove the real binary loads a saved OAuth credential from temp `CLAW_CONFIG_HOME`
with **no env auth** and that the request path selects `AuthSource::OAuthBearer`.
- Arrange: `save_oauth_credentials(<fake token>)` into temp config (or write the
  `credentials.json` the loader expects); unset `ANTHROPIC_API_KEY`.
- Act: `claw -p hi` against the mock.
- Assert (minimal): exactly one `/v1/messages`; `authorization: Bearer <tok>`.
- If this path is awkward through the binary, add a tiny fixture helper — but do
  **not** start A1 until A0 is green. (Mitigates the plan's main risk.)

### A2 — Auth-source precedence matrix
One outbound request per case; assert which source wins:
| Case | env API key | saved OAuth | Expect |
|---|---|---|---|
| api-only | yes | no | `x-api-key` present; **no** oauth beta, **no** identity block |
| oauth-only | no | yes | `authorization: Bearer`; oauth beta + identity present; **no** `x-api-key` |
| both | yes | yes | **API key wins**; oauth beta + identity **absent** (no leak) |
| neither | no | no | CLI fails with the expected auth error; does **not** attempt subscription auth |

### A1 — Subscription protocol happy path
oauth-only config; assert only stable protocol facts:
- `authorization: Bearer <tok>`
- `anthropic-beta` contains `oauth-2025-04-20`
- **no** `x-api-key`
- `raw_body.system[0]` == `You are Claude Code, Anthropic's official CLI for Claude.`
Do **not** over-assert unrelated body structure.

### A3 — Token refresh (success + failure + persistence)
Point the OAuth token URL at the mock via `settings.oauth` override; seed an
expired access token + valid refresh token.
- **Success**: token endpoint is called → subsequent `/v1/messages` uses the
  **new** bearer; then assert whether the refreshed credential is persisted back
  under temp config (confirm intended behavior; document either way).
- **Failure**: refresh 4xx → clear user-visible failure, **no** silent fallback
  to a different auth source.

### A4 — Live subscription checklist (MANUAL, not CI)
Mocks cannot verify real-wire acceptance or **billing-credit attribution** — that
is the standing residual. Manual run with a real `claw login`:
1. confirm login state; 2. `CLAW_AUTH_TRACE=1 claw -p hi`;
3. confirm the trace reports `mode=oauth_subscription` + the beta header;
4. confirm the real endpoint **accepts** the request (200, request-id);
5. verify subscription/billing side-effects externally (credit vs metered).

---

## B — Proactive/background agent

### B1 — What exists today (automated now)
- **Background-task lifecycle**: mock emits a `tool_use` for
  `create_background_task` running a cheap shell command → assert subprocess
  started, status transitions `queued→running→done`, `get_background_task_output`
  returns it, `stop_background_task` works. Teardown: no orphaned child.
- **Brief proactive routing**: mock emits `Brief{importance:"proactive"}` →
  assert it routes to the proactive notification path (vs `normal`).
- Keep B1 strictly to shipped behavior — it must **not** imply autonomous
  scheduling/idle triggers.

### B2 — Autonomous loop (TEST-FIRST; build with the feature)
Prerequisite (build these seams first, or tests will be flaky/implementation-coupled):
**injected virtual clock**, deterministic **tick driver**, feature gate.
- **Idle → proactive**: under the virtual clock, after N idle ticks with no user
  input, exactly **one** proactive `Brief` is emitted (assert externally visible
  behavior, not internal tick counts).
- **Negative (recent activity suppresses)**: recent user interaction resets the
  idle threshold → **no** proactive Brief fires. (Real behavioral contract.)
- **Golden Kairos-OFF**: with the feature gate off, the REPL transcript is
  **byte-identical** to baseline (normalize timestamps/IDs). Bounds blast radius.

### B3 — Dream-on-start scheduler (TEST-FIRST; layer 5)
Prerequisite: injectable clock + marker store (`read_last_dream`/`last-dream`
already exist).
- last-dream = **yesterday** → dream runs **once** on start (in background).
- last-dream = **today** → dream does **not** run.
- **Same-day restart idempotence**: restart on the same virtual day → still runs
  at most once. (Common marker-scheduler gap.)

### B-gate — Feature-flag isolation
With Kairos/autonomous mode **off**: background-task tools may still work (already
shipped) but **no** autonomous loop side-effects occur. Be explicit about which
behaviors are gated so the golden-OFF test is unambiguous.

---

## Sequencing & strategy

1. A0 spike → A2 → A1 → A3 → (A4 manual, anytime).
2. B1 now (shipped behavior).
3. Build DI seams (clock/tick/marker/gate) → B2/B3 test-first alongside the loop.
4. Prefer real-binary+mock-HTTP for **auth/protocol**; use lower-level
   deterministic tests for **loop mechanics** once injection exists (full E2E for
   the loop is too broad initially — hybrid reduces flakiness).

## What each tier can and cannot prove
- **Automated E2E (Tier-2)**: wire shape, header/identity correctness, auth
  precedence, tool/scheduler behavior under a virtual clock.
- **Manual live (A4)**: real-endpoint acceptance + billing-credit attribution —
  the only residual mocks cannot close.
