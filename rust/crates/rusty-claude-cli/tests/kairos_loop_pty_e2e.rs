//! End-to-end tests for the Kairos autonomous loop driving the *real* `claw`
//! REPL (E2E_TEST_PLAN B2 live wiring + B3 dream-on-start).
//!
//! `run_repl` is intentionally TTY-only (the binary refuses a headless REPL and
//! routes piped stdin to a one-shot prompt), so these allocate a pseudo-terminal
//! with `openpty`, hand the slave to the child as stdin/stdout/stderr, and read
//! the master. This is the only faithful way to exercise the interactive path —
//! including the rustyline external-printer sink the idle watcher uses.
//!
//! Timing is real but assertions are on externally visible behaviour (a unique
//! marker line; a memory file on disk), with padded waits and a hard kill
//! backstop so a stuck child can never hang the suite. The one-shot latch means
//! over-padding still yields exactly one emission.
#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use nix::pty::openpty;

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "claw-kairos-pty-e2e-{label}-{}-{nanos}",
        std::process::id()
    ))
}

fn make_dirs(label: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = unique_temp_dir(label);
    let config_home = root.join("config-home");
    let home = root.join("home");
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&home).expect("home");
    (root, config_home, home)
}

/// A live `claw` REPL running against a pty, with a background thread draining
/// the master into a shared buffer.
struct ReplPty {
    child: Child,
    master: OwnedFd,
    reader: Option<std::thread::JoinHandle<()>>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl ReplPty {
    fn spawn(config_home: &Path, home: &Path, kairos: bool, base_url: Option<&str>) -> Self {
        let pty = openpty(None, None).expect("openpty");
        let stdin_fd = pty.slave.try_clone().expect("clone slave");
        let stdout_fd = pty.slave.try_clone().expect("clone slave");
        let stderr_fd = pty.slave.try_clone().expect("clone slave");

        let mut command = Command::new(env!("CARGO_BIN_EXE_claw"));
        command
            .current_dir(config_home)
            .env_clear()
            .env("ANTHROPIC_API_KEY", "test-kairos-key")
            .env("CLAW_CONFIG_HOME", config_home)
            .env("HOME", home)
            .env("TERM", "xterm")
            .env("NO_COLOR", "1")
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::from(stdin_fd))
            .stdout(Stdio::from(stdout_fd))
            .stderr(Stdio::from(stderr_fd));
        if kairos {
            command
                .env("CLAW_KAIROS", "1")
                .env("CLAW_KAIROS_IDLE_SECS", "1");
        }
        if let Some(url) = base_url {
            command.env("ANTHROPIC_BASE_URL", url);
        }
        let child = command.spawn().expect("claw repl should launch");
        // Drop our slave handle so only the child holds it; then the child's
        // exit closes the pty and the reader sees EOF.
        drop(pty.slave);

        let output = Arc::new(Mutex::new(Vec::new()));
        let reader_master = pty.master.try_clone().expect("clone master for reader");
        let reader_buf = Arc::clone(&output);
        let reader = std::thread::spawn(move || {
            let mut file = std::fs::File::from(reader_master);
            let mut chunk = [0u8; 4096];
            loop {
                match file.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => reader_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        });

        Self {
            child,
            master: pty.master,
            reader: Some(reader),
            output,
        }
    }

    fn output_string(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Send Ctrl-D (EOF) so rustyline returns `Eof` and the REPL exits cleanly.
    /// Writes through a cloned master fd so the owned `master` stays open.
    fn send_eof(&mut self) {
        if let Ok(clone) = self.master.try_clone() {
            let mut file = std::fs::File::from(clone);
            let _ = file.write_all(&[0x04]);
            let _ = file.flush();
        }
    }

    /// Ensure the child exits; kill as a backstop so a stuck REPL never hangs.
    fn shutdown(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    path.exists()
}

// ---------------------------------------------------------------------------
// B2 — autonomous idle loop, live through the real REPL
// ---------------------------------------------------------------------------

#[test]
fn idle_loop_emits_exactly_one_proactive_brief_when_enabled() {
    let (root, config_home, home) = make_dirs("idle-on");
    // No journal seeded → the gated startup dream no-ops before any model call,
    // isolating the idle watcher (no network).
    let mut repl = ReplPty::spawn(&config_home, &home, true, None);
    std::thread::sleep(Duration::from_millis(2800));
    repl.send_eof();
    repl.shutdown();

    let stdout = repl.output_string();
    let count = stdout.matches(runtime::IDLE_BRIEF_NOTICE).count();
    assert_eq!(
        count, 1,
        "exactly one proactive Brief expected while idle, got {count}\noutput:\n{stdout}"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn idle_loop_is_silent_when_kairos_disabled() {
    let (root, config_home, home) = make_dirs("idle-off");
    let mut repl = ReplPty::spawn(&config_home, &home, false, None);
    std::thread::sleep(Duration::from_millis(2800));
    repl.send_eof();
    repl.shutdown();

    let stdout = repl.output_string();
    assert!(
        !stdout.contains(runtime::IDLE_BRIEF_NOTICE),
        "no proactive Brief expected with Kairos off\noutput:\n{stdout}"
    );

    fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// B3 — dream-on-start, live through the real REPL
// ---------------------------------------------------------------------------

fn seed_past_journal(config_home: &Path) {
    let journal_day = config_home.join("logs").join("2020").join("01");
    fs::create_dir_all(&journal_day).expect("journal dir");
    fs::write(
        journal_day.join("2020-01-01.md"),
        format!("## 09:00:00Z\n\nOn-start dream path {SCENARIO_PREFIX}dream_distill today.\n\n"),
    )
    .expect("seed journal");
}

#[test]
fn dream_on_start_runs_once_when_enabled_and_due() {
    let tokio_rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = tokio_rt
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();

    let (root, config_home, home) = make_dirs("dream-on");
    seed_past_journal(&config_home);

    let mut repl = ReplPty::spawn(&config_home, &home, true, Some(&base_url));
    let topic_file = config_home.join("memory").join("e2e-dream-verified.md");
    let appeared = wait_for(&topic_file, Duration::from_secs(8));
    repl.send_eof();
    repl.shutdown();

    assert!(
        appeared,
        "on-start dream should write the distilled memory within the window\noutput:\n{}",
        repl.output_string()
    );
    let contents = fs::read_to_string(&topic_file).expect("topic file readable");
    assert!(
        contents.contains("end-to-end path"),
        "distilled memory should carry the model body: {contents}"
    );
    assert!(
        config_home.join("last-dream").exists(),
        "last-dream marker should be advanced after a successful dream"
    );
    let captured = tokio_rt.block_on(server.captured_requests());
    let dream_requests: Vec<_> = captured
        .iter()
        .filter(|request| request.path == "/v1/messages")
        .collect();
    assert_eq!(dream_requests.len(), 1, "exactly one model round-trip");
    assert!(
        dream_requests[0].raw_body.contains("On-start dream path"),
        "the distillation prompt must embed the seeded journal entry"
    );

    fs::remove_dir_all(&root).ok();
}
