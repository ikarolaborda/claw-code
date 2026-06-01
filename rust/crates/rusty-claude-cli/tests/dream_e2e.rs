//! End-to-end test for `claw dream`.
//!
//! Unlike the `runtime::dream` unit tests (which inject a mock completer), this
//! drives the *real* compiled `claw` binary against a mock Anthropic server:
//! seed a journal day → run `claw dream` → assert the binary performed a live
//! non-streaming `/v1/messages` round-trip and wrote durable memory files under
//! the config home. This exercises the whole production path —
//! `run_dream_command` → `AnthropicClient::send_message(stream:false)` → HTTP →
//! `parse_distilled` → `write_distilled` — that the unit tests stub out.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::Value;

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "claw-dream-e2e-{label}-{}-{nanos}",
        std::process::id()
    ))
}

#[test]
fn claw_dream_distills_journal_into_memory_files_end_to_end() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();

    let root = unique_temp_dir("run");
    let config_home = root.join("config-home");
    let home = root.join("home");
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&home).expect("home");

    // Seed a journal day well in the past so it is always eligible (<= today,
    // no last-dream marker yet). The scenario token is embedded in the entry so
    // it rides along inside the distillation prompt and routes the mock to the
    // `dream_distill` reply — no change to the production prompt is needed.
    let journal_day = config_home.join("logs").join("2020").join("01");
    fs::create_dir_all(&journal_day).expect("journal dir");
    fs::write(
        journal_day.join("2020-01-01.md"),
        format!(
            "## 09:00:00Z\n\nWorked on the dream path {SCENARIO_PREFIX}dream_distill today.\n\n"
        ),
    )
    .expect("seed journal");

    let output = Command::new(env!("CARGO_BIN_EXE_claw"))
        .current_dir(&root)
        .env_clear()
        .env("ANTHROPIC_API_KEY", "test-dream-key")
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("CLAW_CONFIG_HOME", &config_home)
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args(["dream", "--output-format", "json"])
        .output()
        .expect("claw should launch");

    assert!(
        output.status.success(),
        "claw dream should exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_slice =
        &stdout[stdout.find('{').expect("json start")..=stdout.rfind('}').expect("json end")];
    let response: Value = serde_json::from_str(json_slice)
        .unwrap_or_else(|error| panic!("dream JSON should parse: {error}\n{stdout}"));
    assert_eq!(response["status"], Value::String("dreamed".to_string()));
    assert_eq!(response["memories_written"], Value::from(1));
    assert_eq!(response["marker_advanced"], Value::Bool(true));

    // The distilled memory landed on disk under the config home.
    let topic_file = config_home.join("memory").join("e2e-dream-verified.md");
    let topic_contents = fs::read_to_string(&topic_file)
        .unwrap_or_else(|error| panic!("topic file {topic_file:?} should exist: {error}"));
    assert!(
        topic_contents.contains("end-to-end path"),
        "topic file should carry the distilled body: {topic_contents}"
    );

    let index = fs::read_to_string(config_home.join("MEMORY.md")).expect("MEMORY.md should exist");
    assert!(
        index.contains("(memory/e2e-dream-verified.md)"),
        "MEMORY.md should index the new memory: {index}"
    );

    assert!(
        config_home.join("last-dream").exists(),
        "last-dream marker should be written"
    );

    // The binary really hit the model: one non-streaming /v1/messages request.
    let captured = runtime.block_on(server.captured_requests());
    let dream_requests: Vec<_> = captured
        .iter()
        .filter(|request| request.path == "/v1/messages")
        .collect();
    assert_eq!(dream_requests.len(), 1, "exactly one model round-trip");
    assert!(
        !dream_requests[0].stream,
        "claw dream must send stream:false"
    );
    assert_eq!(dream_requests[0].scenario, "dream_distill");
    // Prove the production prompt actually consumed the seeded journal (not just
    // that the endpoint was hit) — guards against a tautological pass if the
    // prompt ever stopped including journal content.
    assert!(
        dream_requests[0]
            .raw_body
            .contains("Worked on the dream path"),
        "the distillation prompt must embed the journal entry: {}",
        dream_requests[0].raw_body
    );

    fs::remove_dir_all(&root).ok();
}
