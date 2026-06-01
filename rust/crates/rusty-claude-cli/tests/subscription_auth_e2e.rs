//! End-to-end tests for the Claude **subscription** (OAuth) auth path — items
//! A0 and A1 of `E2E_TEST_PLAN.md`.
//!
//! These drive the real `claw` binary against the mock Anthropic server with a
//! saved OAuth credential (and no env API key), then assert the *captured wire
//! shape*. The mock records each request's headers + raw body, so we can verify
//! the subscription protocol without a live subscription:
//! - `authorization: Bearer <token>` (saved token is actually loaded + used),
//! - `anthropic-beta: …oauth-2025-04-20…`,
//! - **no** `x-api-key`,
//! - the request body's `system[0]` is the Claude Code identity block.
//!
//! The credential is written to disk in the exact `credentials.json` shape the
//! loader expects (no env mutation, so tests stay parallel-safe), with a
//! far-future expiry so the non-expired path is taken and no token-refresh
//! network call happens (refresh is covered separately by A3).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::{json, Value};

const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// 2100-01-01Z — far enough out that the saved token is never treated as
/// expired, so `resolve_startup_auth_source` uses it directly (no refresh).
const FAR_FUTURE_EXPIRY: u64 = 4_102_444_800;

struct Workspace {
    root: PathBuf,
    config_home: PathBuf,
    home: PathBuf,
}

fn workspace(label: &str) -> Workspace {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "claw-sub-e2e-{label}-{}-{nanos}",
        std::process::id()
    ));
    let config_home = root.join("config-home");
    let home = root.join("home");
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&home).expect("home");
    Workspace {
        root,
        config_home,
        home,
    }
}

/// Write a saved Claude-subscription credential in the on-disk format
/// `oauth.rs` reads: `{ "oauth": { accessToken, refreshToken, expiresAt, scopes } }`.
fn write_saved_oauth_token(config_home: &Path, access_token: &str) {
    let creds = json!({
        "oauth": {
            "accessToken": access_token,
            "refreshToken": null,
            "expiresAt": FAR_FUTURE_EXPIRY,
            "scopes": ["user:inference"],
        }
    });
    fs::write(
        config_home.join("credentials.json"),
        serde_json::to_vec_pretty(&creds).expect("creds json"),
    )
    .expect("write credentials.json");
}

/// Run `claw -p <scenario>` against the mock. `api_key` controls whether
/// `ANTHROPIC_API_KEY` is present (env auth wins over a saved OAuth token).
fn run_claw(ws: &Workspace, base_url: &str, scenario: &str, api_key: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_claw"));
    command
        .current_dir(&ws.root)
        .env_clear()
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("CLAW_CONFIG_HOME", &ws.config_home)
        .env("HOME", &ws.home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin");
    if let Some(key) = api_key {
        command.env("ANTHROPIC_API_KEY", key);
    }
    command
        .args([
            "--model",
            "sonnet",
            "--permission-mode",
            "read-only",
            "--output-format=json",
        ])
        .arg(format!("{SCENARIO_PREFIX}{scenario}"))
        .output()
        .expect("claw should launch")
}

/// Run with ONLY the saved OAuth credential available (no `ANTHROPIC_API_KEY`).
fn run_claw_oauth_only(ws: &Workspace, base_url: &str, scenario: &str) -> Output {
    run_claw(ws, base_url, scenario, None)
}

fn messages_of(
    captured: &[mock_anthropic_service::CapturedRequest],
) -> Vec<&mock_anthropic_service::CapturedRequest> {
    captured
        .iter()
        .filter(|r| r.path == "/v1/messages")
        .collect()
}

fn has_oauth_markers(req: &mock_anthropic_service::CapturedRequest) -> bool {
    let beta_oauth = req
        .headers
        .get("anthropic-beta")
        .is_some_and(|beta| beta.contains(OAUTH_BETA));
    let identity_in_body = req.raw_body.contains(IDENTITY);
    beta_oauth || identity_in_body
}

/// A non-expired saved token must be used directly: assert the binary never hit
/// an OAuth token endpoint (no refresh). The only paths a healthy turn produces
/// are `/v1/messages` and its `/count_tokens` preflight.
fn assert_no_token_refresh(captured: &[mock_anthropic_service::CapturedRequest]) {
    for request in captured {
        assert!(
            request.path == "/v1/messages" || request.path == "/v1/messages/count_tokens",
            "unexpected endpoint hit (token refresh?): {} {}",
            request.method,
            request.path
        );
    }
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "claw should exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A0 — saved-token-load spike (the linchpin). Proves the real binary loads a
/// saved OAuth credential from `CLAW_CONFIG_HOME` with no env auth and actually
/// sends it as a bearer token. Minimal on purpose: a setup failure here is easy
/// to diagnose before the fuller A1 protocol assertions.
#[test]
fn a0_saved_oauth_token_is_loaded_and_sent_as_bearer() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service");
    let base_url = server.base_url();

    let ws = workspace("a0");
    write_saved_oauth_token(&ws.config_home, "saved-sub-token-a0");

    let output = run_claw_oauth_only(&ws, &base_url, "streaming_text");
    assert_ok(&output);

    let captured = runtime.block_on(server.captured_requests());
    let messages: Vec<_> = captured
        .iter()
        .filter(|r| r.path == "/v1/messages")
        .collect();
    assert_eq!(messages.len(), 1, "exactly one /v1/messages request");
    assert_eq!(
        messages[0].method, "POST",
        "messages request must be a POST"
    );
    assert_eq!(
        messages[0].headers.get("authorization").map(String::as_str),
        Some("Bearer saved-sub-token-a0"),
        "the saved subscription token must be sent as the bearer; headers: {:?}",
        messages[0].headers
    );
    assert_no_token_refresh(&captured);

    fs::remove_dir_all(&ws.root).ok();
}

/// A1 — subscription protocol happy path. Asserts only stable protocol facts:
/// bearer token, the oauth beta header, absence of `x-api-key`, and the
/// injected Claude Code identity as the first `system` block.
#[test]
fn a1_subscription_request_carries_oauth_beta_and_identity() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service");
    let base_url = server.base_url();

    let ws = workspace("a1");
    write_saved_oauth_token(&ws.config_home, "saved-sub-token-a1");

    let output = run_claw_oauth_only(&ws, &base_url, "streaming_text");
    assert_ok(&output);

    let captured = runtime.block_on(server.captured_requests());
    let messages: Vec<_> = captured
        .iter()
        .filter(|r| r.path == "/v1/messages")
        .collect();
    assert_eq!(messages.len(), 1, "exactly one /v1/messages request");
    let req = messages[0];
    assert_eq!(req.method, "POST", "messages request must be a POST");
    assert_no_token_refresh(&captured);

    assert_eq!(
        req.headers.get("authorization").map(String::as_str),
        Some("Bearer saved-sub-token-a1"),
        "headers: {:?}",
        req.headers
    );
    assert!(
        req.headers
            .get("anthropic-beta")
            .is_some_and(|beta| beta.contains(OAUTH_BETA)),
        "anthropic-beta must advertise the oauth subscription beta; headers: {:?}",
        req.headers
    );
    assert!(
        !req.headers.contains_key("x-api-key"),
        "subscription traffic must NOT carry x-api-key; headers: {:?}",
        req.headers
    );

    // The OAuth path rewrites `system` into an array whose first block is the
    // Claude Code identity (required by the API for subscription tokens).
    let body: Value = serde_json::from_str(&req.raw_body).expect("request body is JSON");
    assert_eq!(
        body["system"][0]["text"].as_str(),
        Some(IDENTITY),
        "system[0] must be the Claude Code identity; body.system: {}",
        body["system"]
    );

    fs::remove_dir_all(&ws.root).ok();
}

/// A2 (api-key only) — with an env API key and no saved OAuth, the request uses
/// `x-api-key` and carries NONE of the subscription markers (beta/identity).
#[test]
fn a2_api_key_only_uses_x_api_key_without_oauth_markers() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service");
    let base_url = server.base_url();

    let ws = workspace("a2-apikey");
    // No saved OAuth credential written.
    let output = run_claw(&ws, &base_url, "streaming_text", Some("test-api-key-a2"));
    assert_ok(&output);

    let captured = runtime.block_on(server.captured_requests());
    let messages = messages_of(&captured);
    assert_eq!(messages.len(), 1, "exactly one /v1/messages request");
    let req = messages[0];
    assert_eq!(
        req.headers.get("x-api-key").map(String::as_str),
        Some("test-api-key-a2"),
        "api-key auth must send x-api-key; headers: {:?}",
        req.headers
    );
    assert!(
        !has_oauth_markers(req),
        "api-key traffic must NOT carry the oauth beta or identity; headers: {:?} body: {}",
        req.headers,
        req.raw_body
    );

    fs::remove_dir_all(&ws.root).ok();
}

/// A2 (both present) — env API key wins over a saved OAuth token, and the
/// subscription markers must NOT leak onto the api-key request.
#[test]
fn a2_api_key_wins_over_saved_oauth_and_no_marker_leak() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service");
    let base_url = server.base_url();

    let ws = workspace("a2-both");
    write_saved_oauth_token(&ws.config_home, "saved-but-ignored");
    let output = run_claw(&ws, &base_url, "streaming_text", Some("test-api-key-wins"));
    assert_ok(&output);

    let captured = runtime.block_on(server.captured_requests());
    let messages = messages_of(&captured);
    assert_eq!(messages.len(), 1, "exactly one /v1/messages request");
    let req = messages[0];
    assert_eq!(
        req.headers.get("x-api-key").map(String::as_str),
        Some("test-api-key-wins"),
        "env API key must win over a saved OAuth token; headers: {:?}",
        req.headers
    );
    assert_ne!(
        req.headers.get("authorization").map(String::as_str),
        Some("Bearer saved-but-ignored"),
        "the saved OAuth token must not be used when an API key is present"
    );
    assert!(
        !has_oauth_markers(req),
        "no oauth beta/identity may leak onto api-key traffic; headers: {:?} body: {}",
        req.headers,
        req.raw_body
    );

    fs::remove_dir_all(&ws.root).ok();
}

/// A2 (neither) — with no API key and no saved OAuth credential, the CLI fails
/// before sending any request rather than attempting subscription auth.
#[test]
fn a2_no_credentials_fails_without_sending_a_request() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service");
    let base_url = server.base_url();

    let ws = workspace("a2-none");
    // No API key, no saved credential.
    let output = run_claw(&ws, &base_url, "streaming_text", None);
    assert!(
        !output.status.success(),
        "with no credentials the CLI must fail; stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let captured = runtime.block_on(server.captured_requests());
    assert!(
        messages_of(&captured).is_empty(),
        "no model request should be sent without credentials; captured: {captured:?}"
    );

    fs::remove_dir_all(&ws.root).ok();
}

/// Write a settings.json that overrides the OAuth client so the refresh flow
/// targets the mock's token endpoint instead of the real console endpoint.
fn write_oauth_settings(config_home: &Path, base_url: &str, token_path: &str) {
    let settings = json!({
        "oauth": {
            "clientId": "test-client",
            "authorizeUrl": format!("{base_url}/oauth/authorize"),
            "tokenUrl": format!("{base_url}{token_path}"),
        }
    });
    fs::write(
        config_home.join("settings.json"),
        serde_json::to_vec_pretty(&settings).expect("settings json"),
    )
    .expect("write settings.json");
}

/// Write an EXPIRED saved OAuth credential (with a refresh token) so the startup
/// path performs a refresh before using it.
fn write_expired_oauth_token(config_home: &Path, access_token: &str, refresh_token: &str) {
    let creds = json!({
        "oauth": {
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": 1000,
            "scopes": ["user:inference"],
        }
    });
    fs::write(
        config_home.join("credentials.json"),
        serde_json::to_vec_pretty(&creds).expect("creds json"),
    )
    .expect("write credentials.json");
}

/// A3 (refresh success + persistence) — an expired saved token is refreshed at
/// the OAuth token endpoint, the new bearer is used for `/v1/messages`, and the
/// refreshed credential is persisted back to disk.
#[test]
fn a3_expired_token_is_refreshed_persisted_and_new_bearer_used() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service");
    let base_url = server.base_url();

    let ws = workspace("a3-ok");
    write_oauth_settings(&ws.config_home, &base_url, "/oauth/token");
    write_expired_oauth_token(&ws.config_home, "expired-access", "old-refresh");

    let output = run_claw_oauth_only(&ws, &base_url, "streaming_text");
    assert_ok(&output);

    let captured = runtime.block_on(server.captured_requests());
    assert!(
        captured.iter().any(|r| r.path == "/oauth/token"),
        "the expired token must trigger a refresh at the token endpoint; captured: {captured:?}"
    );
    let messages = messages_of(&captured);
    assert_eq!(messages.len(), 1, "exactly one /v1/messages request");
    assert_eq!(
        messages[0].headers.get("authorization").map(String::as_str),
        Some(
            format!(
                "Bearer {}",
                mock_anthropic_service::REFRESHED_OAUTH_ACCESS_TOKEN
            )
            .as_str()
        ),
        "messages must use the refreshed bearer; headers: {:?}",
        messages[0].headers
    );

    // Persistence: the refreshed token is written back to credentials.json.
    let creds: Value = serde_json::from_str(
        &fs::read_to_string(ws.config_home.join("credentials.json")).expect("read creds"),
    )
    .expect("creds json");
    assert_eq!(
        creds["oauth"]["accessToken"].as_str(),
        Some(mock_anthropic_service::REFRESHED_OAUTH_ACCESS_TOKEN),
        "the refreshed access token must be persisted; creds: {creds}"
    );

    fs::remove_dir_all(&ws.root).ok();
}

/// A3 (refresh failure) — a non-transient refresh failure aborts the run with a
/// clear error and no `/v1/messages` request is sent.
#[test]
fn a3_refresh_failure_aborts_without_sending_messages() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service");
    let base_url = server.base_url();

    let ws = workspace("a3-fail");
    write_oauth_settings(&ws.config_home, &base_url, "/oauth/token-fail");
    write_expired_oauth_token(&ws.config_home, "expired-access", "old-refresh");

    let output = run_claw_oauth_only(&ws, &base_url, "streaming_text");
    assert!(
        !output.status.success(),
        "a failed refresh must abort the run; stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let captured = runtime.block_on(server.captured_requests());
    assert!(
        captured.iter().any(|r| r.path == "/oauth/token-fail"),
        "the refresh must have been attempted; captured: {captured:?}"
    );
    assert!(
        messages_of(&captured).is_empty(),
        "no model request should be sent after a failed refresh; captured: {captured:?}"
    );

    fs::remove_dir_all(&ws.root).ok();
}
