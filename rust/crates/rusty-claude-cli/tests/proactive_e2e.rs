//! End-to-end tests for claw-code's *shipped* proactive/background ("Kairos")
//! surface, driven through the real compiled `claw` binary against a mock
//! Anthropic server (the harness proven by `dream_e2e` / `mock_parity_harness`).
//!
//! Scope is deliberately limited to behavior that ships TODAY. Tier-1 source
//! review established the ground truth these tests encode:
//!
//! * Background *execution* is exposed through the `bash` tool's
//!   `run_in_background` flag (`runtime::execute_bash` spawns a detached child
//!   and returns `backgroundTaskId`). This is the real subprocess + teardown
//!   surface.
//! * `TaskCreate` is registry bookkeeping only — it returns a task with status
//!   `created` and does NOT spawn a subprocess or run the prompt. The
//!   worker/lane execution engine (queued→running→done) is not built.
//! * `SendUserMessage`/`Brief` accepts `status: "proactive"` as valid input, but
//!   the shipped tool treats `normal | proactive` identically and the result
//!   omits the status — there is no proactive *routing* yet (that belongs to the
//!   unbuilt notification/autonomous layer). So we assert acceptance, not
//!   routing.
//!
//! These tests must NOT imply autonomous scheduling or idle triggers.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::Value;

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "claw-proactive-e2e-{label}-{}-{nanos}",
        std::process::id()
    ))
}

/// Spawn the real binary in print mode against the mock, routing to `scenario`
/// via the `PARITY_SCENARIO:` token, and return the parsed `--output-format=json`
/// envelope. Uses the API-key auth path (no OAuth needed) and a fully isolated,
/// cleared environment so the host cannot contaminate the run.
fn run_scenario(root: &Path, base_url: &str, scenario: &str, allowed_tools: &str) -> Value {
    let config_home = root.join("config-home");
    let home = root.join("home");
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&home).expect("home");

    let prompt = format!("{SCENARIO_PREFIX}{scenario}");
    let output = Command::new(env!("CARGO_BIN_EXE_claw"))
        .current_dir(root)
        .env_clear()
        .env("ANTHROPIC_API_KEY", "test-proactive-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("CLAW_CONFIG_HOME", &config_home)
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--model",
            "sonnet",
            "--permission-mode",
            "danger-full-access",
            "--output-format=json",
            "--allowedTools",
            allowed_tools,
        ])
        .arg(&prompt)
        .output()
        .expect("claw should launch");

    assert!(
        output.status.success(),
        "claw should exit 0 for scenario {scenario}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_slice =
        &stdout[stdout.find('{').expect("json start")..=stdout.rfind('}').expect("json end")];
    serde_json::from_str(json_slice)
        .unwrap_or_else(|error| panic!("{scenario} JSON should parse: {error}\n{stdout}"))
}

/// Locate a tool use by name rather than by fragile array position (the JSON
/// transcript may carry more than one event in other scenarios).
fn tool_use_by_name<'a>(response: &'a Value, name: &str) -> &'a Value {
    response["tool_uses"]
        .as_array()
        .expect("tool_uses array")
        .iter()
        .find(|use_| use_["name"] == Value::String(name.to_string()))
        .unwrap_or_else(|| panic!("expected a {name} tool use in {response}"))
}

/// The envelope stores a tool use's `input` as the verbatim JSON string the
/// model sent (see `collect_tool_uses` / `ContentBlock::ToolUse`), so parse it
/// before inspecting fields.
fn tool_input_json(tool_use: &Value) -> Value {
    let raw = tool_use["input"].as_str().expect("tool use input string");
    serde_json::from_str(raw)
        .unwrap_or_else(|error| panic!("tool use input should be JSON: {error}\n{raw}"))
}

/// The single tool result for these one-tool scenarios, parsed from its string
/// `output` payload. `is_error` must be false.
fn sole_tool_result_payload(response: &Value) -> Value {
    let result = &response["tool_results"][0];
    assert_eq!(
        result["is_error"],
        Value::Bool(false),
        "tool result must not be an error: {response}"
    );
    let output = result["output"]
        .as_str()
        .expect("tool result output string");
    serde_json::from_str(output)
        .unwrap_or_else(|error| panic!("tool result payload should be JSON: {error}\n{output}"))
}

/// Bounded teardown poll. Not a behavioral timing assertion — it only waits for
/// process reaping/marker flush to settle so the test is robust on slow runners.
fn wait_until<F: Fn() -> bool>(condition: F) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

fn process_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn bash_run_in_background_launches_a_detached_subprocess_and_leaves_no_orphan() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();

    let root = unique_temp_dir("bash-bg");
    fs::create_dir_all(&root).expect("root");

    let response = run_scenario(&root, &base_url, "bash_background_roundtrip", "bash");

    // Two model turns: tool_use(bash) -> tool_result -> final text.
    assert_eq!(response["iterations"], Value::from(2));

    let bash_use = tool_use_by_name(&response, "bash");
    assert_eq!(
        tool_input_json(bash_use)["run_in_background"],
        Value::Bool(true)
    );

    // Shipped contract for a backgrounded bash command.
    let payload = sole_tool_result_payload(&response);
    let pid = payload["backgroundTaskId"]
        .as_str()
        .expect("backgroundTaskId must be present for a backgrounded command");
    assert!(!pid.is_empty(), "backgroundTaskId must be non-empty");
    assert_eq!(
        payload["noOutputExpected"],
        Value::Bool(true),
        "a backgrounded command reports noOutputExpected"
    );

    // Deterministic proof the subprocess actually ran: it wrote a marker file in
    // the binary's cwd and exited. `sh -lc` runs the command in `root`.
    let marker = root.join("kairos_bg_marker.txt");
    assert!(
        wait_until(|| marker.exists()),
        "background subprocess should have written its marker file"
    );

    // Teardown: the cheap, self-terminating command must leave no orphan.
    assert!(
        wait_until(|| !process_alive(pid)),
        "background subprocess (pid {pid}) should not linger as an orphan"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn task_create_registers_a_task_without_executing_it() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();

    let root = unique_temp_dir("task-create");
    fs::create_dir_all(&root).expect("root");

    let response = run_scenario(&root, &base_url, "task_create_lifecycle", "TaskCreate");

    assert_eq!(response["iterations"], Value::from(2));
    let _ = tool_use_by_name(&response, "TaskCreate");

    // Registry bookkeeping only: a fresh task is `created`. There is no worker
    // engine, so no queued->running->done transition and no execution output.
    let payload = sole_tool_result_payload(&response);
    assert_eq!(payload["status"], Value::String("created".to_string()));
    assert!(
        payload["task_id"].as_str().is_some_and(|id| !id.is_empty()),
        "TaskCreate must return a non-empty task_id: {payload}"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn send_user_message_accepts_proactive_status() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();

    let root = unique_temp_dir("brief-proactive");
    fs::create_dir_all(&root).expect("root");

    let response = run_scenario(
        &root,
        &base_url,
        "brief_proactive_accepted",
        "SendUserMessage",
    );

    assert_eq!(response["iterations"], Value::from(2));

    // The model emitted a SendUserMessage with status=proactive and the binary
    // accepted + dispatched it. We assert acceptance, NOT routing: the shipped
    // tool treats normal|proactive identically and the result omits the status,
    // so there is no observable proactive-vs-normal difference to assert here.
    let brief_use = tool_use_by_name(&response, "SendUserMessage");
    assert_eq!(
        tool_input_json(brief_use)["status"],
        Value::String("proactive".to_string())
    );

    let payload = sole_tool_result_payload(&response);
    assert_eq!(
        payload["message"],
        Value::String("kairos proactive note".to_string()),
        "Brief echoes the message back regardless of status"
    );

    fs::remove_dir_all(&root).ok();
}
