//! Dream distillation — the autonomous consolidation pass that turns the raw
//! append-only [`crate::journal`] stream into durable, reusable memories.
//!
//! This module is deliberately **model-agnostic**: it knows how to read recent
//! journal days, build a bounded distillation prompt, parse the model's reply,
//! and write the distilled memories under `<config_home>/memory/`. The actual
//! model call is supplied by the caller as a `completer` closure. That keeps
//! `runtime` free of any dependency on the `api` crate (which itself depends on
//! `runtime`, so the reverse edge would be a cycle); the CLI — which depends on
//! both — wires the real subscription-auth `AnthropicClient` into the seam,
//! while tests inject a deterministic mock.
//!
//! Durability rules (see the dream subsystem design):
//! - per-day truncation is applied *before* the total cap so one runaway day
//!   cannot starve the others out of the prompt;
//! - the parser is tolerant: prose around the blocks is ignored, only complete
//!   `<<<MEMORY>>> … <<<END>>>` blocks with a non-empty `topic:` count, and a
//!   reply with zero valid blocks is a handled no-op (nothing is written);
//! - topic files and `MEMORY.md` are written atomically (temp + rename) so an
//!   interrupted dream can never leave a half-written memory;
//! - the last-dream marker is advanced *only* after every write succeeds, so a
//!   transient failure re-dreams the same days next time instead of skipping
//!   them permanently.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use crate::journal::{config_home_dir, list_journal_days, read_day, JournalDate};

/// A single durable memory distilled out of the raw journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistilledMemory {
    pub topic: String,
    pub body: String,
}

/// Tunable bounds for a dream pass.
#[derive(Debug, Clone, Copy)]
pub struct DreamOptions {
    /// Most-recent eligible journal days to include (older ones are dropped).
    pub max_days: usize,
    /// Hard cap on characters taken from any single day.
    pub max_chars_per_day: usize,
    /// Hard cap on the assembled prompt's journal payload.
    pub max_total_chars: usize,
}

impl Default for DreamOptions {
    fn default() -> Self {
        Self {
            max_days: 7,
            max_chars_per_day: 8_000,
            max_total_chars: 24_000,
        }
    }
}

/// Result summary of a dream pass, surfaced to the CLI for user-visible output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamOutcome {
    pub days_considered: usize,
    pub memories_written: usize,
    pub skipped_blocks: usize,
    pub files: Vec<PathBuf>,
    pub marker_advanced: bool,
    /// Set when the pass was a no-op (no new days, or no valid memories).
    pub note: Option<String>,
}

impl DreamOutcome {
    fn noop(days_considered: usize, note: &str) -> Self {
        Self {
            days_considered,
            memories_written: 0,
            skipped_blocks: 0,
            files: Vec::new(),
            marker_advanced: false,
            note: Some(note.to_string()),
        }
    }
}

const BLOCK_OPEN: &str = "<<<MEMORY>>>";
const BLOCK_CLOSE: &str = "<<<END>>>";

/// Directory holding distilled topic files.
pub fn memory_root() -> io::Result<PathBuf> {
    Ok(config_home_dir()?.join("memory"))
}

fn memory_index_path() -> io::Result<PathBuf> {
    Ok(config_home_dir()?.join("MEMORY.md"))
}

fn last_dream_marker_path() -> io::Result<PathBuf> {
    Ok(config_home_dir()?.join("last-dream"))
}

/// Read the last processed journal day, or `None` if no dream has run yet.
pub fn read_last_dream() -> io::Result<Option<JournalDate>> {
    let path = last_dream_marker_path()?;
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(parse_marker(contents.trim())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn parse_marker(text: &str) -> Option<JournalDate> {
    let mut parts = text.split('-');
    let y = parts.next()?.parse::<i32>().ok()?;
    let m = parts.next()?.parse::<u32>().ok()?;
    let d = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(JournalDate {
        year: y,
        month: m,
        day: d,
    })
}

fn format_marker(date: JournalDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year, date.month, date.day)
}

fn day_is_after(day: JournalDate, marker: JournalDate) -> bool {
    (day.year, day.month, day.day) > (marker.year, marker.month, marker.day)
}

/// Whether a dream is due when the agent starts up, given the last-dream marker
/// and today's date. See [`dream_on_start_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamOnStartDecision {
    /// No dream has yet processed a day on or after `today` — a dream is due.
    Run,
    /// A dream has already covered `today` (marker == today), or the marker is
    /// dated in the future (clock skew / hand-edited marker) — nothing to do.
    SkipUpToDate,
}

/// Pure dream-on-start scheduling decision (E2E_TEST_PLAN B3). Clock-free: the
/// caller supplies `today`, so the result is fully deterministic under test.
///
/// `Run` iff no dream has yet processed a day on or after `today`. This makes
/// same-day restarts idempotent: once a dream advances the marker to `today`,
/// every further start that day returns `SkipUpToDate`. A future-dated marker is
/// treated as up-to-date rather than re-dreaming the future. This is only a
/// cheap pre-flight guard — [`run_dream`] is independently idempotent (its
/// eligible set is the journal days strictly after the marker), so a spurious
/// `Run` still distills nothing when there is no new day.
#[must_use]
pub fn dream_on_start_decision(
    last_dream: Option<JournalDate>,
    today: JournalDate,
) -> DreamOnStartDecision {
    match last_dream {
        Some(marker) if !day_is_after(today, marker) => DreamOnStartDecision::SkipUpToDate,
        _ => DreamOnStartDecision::Run,
    }
}

/// Normalize a free-form topic into a filesystem-safe slug. Lowercased ASCII
/// alphanumerics, every other run collapsed to a single `-`. Centralised so the
/// in-run collision check and the on-disk filename always agree.
#[must_use]
pub fn slugify(topic: &str) -> String {
    let mut slug = String::with_capacity(topic.len());
    let mut prev_dash = false;
    for ch in topic.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Build the distillation prompt from the supplied journal days (already
/// filtered to the eligible set). Deterministic and bounded: each day is
/// truncated to `max_chars_per_day` first, then the whole payload to
/// `max_total_chars`, oldest content dropped first so the freshest survives.
#[must_use]
pub fn build_distill_prompt(days: &[(JournalDate, String)], opts: &DreamOptions) -> String {
    let mut sections: Vec<String> = Vec::new();
    for (date, contents) in days {
        let body = truncate_chars(contents.trim(), opts.max_chars_per_day);
        sections.push(format!(
            "### {:04}-{:02}-{:02}\n{}",
            date.year, date.month, date.day, body
        ));
    }
    // Keep the freshest days when the total cap bites: drop whole oldest
    // sections until the payload fits.
    let mut payload = sections.join("\n\n");
    while payload.chars().count() > opts.max_total_chars && sections.len() > 1 {
        sections.remove(0);
        payload = sections.join("\n\n");
    }
    payload = truncate_chars(&payload, opts.max_total_chars);

    format!(
        "You are the dream/distillation process of an autonomous coding agent. \
Below are raw, append-only journal entries from recent days. Consolidate them \
into a small set of durable, reusable memories worth recalling in future \
sessions — decisions, conventions, gotchas, architecture facts. Skip transient \
noise and anything that will not matter tomorrow.\n\n\
For EACH memory, emit exactly one block in this format and nothing else:\n\
{BLOCK_OPEN}\n\
topic: <a short title, a few words>\n\
<1-5 sentences: the durable fact and why it matters>\n\
{BLOCK_CLOSE}\n\n\
Do not emit any text outside these blocks. If nothing is worth keeping, emit no \
blocks.\n\n\
--- JOURNAL ---\n{payload}\n--- END JOURNAL ---\n"
    )
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Parsed result: the valid memories plus how many malformed blocks were
/// skipped (surfaced so a degenerate model reply is diagnosable, not silent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    pub memories: Vec<DistilledMemory>,
    pub skipped: usize,
}

/// Tolerant parser: ignores prose around the blocks, accepts only complete
/// `<<<MEMORY>>> … <<<END>>>` blocks that carry a non-empty `topic:` line, and
/// preserves the body verbatim. An unterminated trailing open block is counted
/// as skipped.
#[must_use]
pub fn parse_distilled(model_output: &str) -> ParseResult {
    let mut memories = Vec::new();
    let mut skipped = 0usize;
    let mut rest = model_output;

    while let Some(open_at) = rest.find(BLOCK_OPEN) {
        let after_open = &rest[open_at + BLOCK_OPEN.len()..];
        let Some(close_at) = after_open.find(BLOCK_CLOSE) else {
            // Unterminated open block — nothing valid can follow.
            skipped += 1;
            break;
        };
        let block = &after_open[..close_at];
        rest = &after_open[close_at + BLOCK_CLOSE.len()..];

        match parse_block(block) {
            Some(memory) => memories.push(memory),
            None => skipped += 1,
        }
    }

    ParseResult { memories, skipped }
}

fn parse_block(block: &str) -> Option<DistilledMemory> {
    let mut topic: Option<String> = None;
    let mut body_lines: Vec<&str> = Vec::new();
    for line in block.lines() {
        if topic.is_none() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(value) = trimmed.strip_prefix("topic:") {
                let value = value.trim();
                if value.is_empty() {
                    return None;
                }
                topic = Some(value.to_string());
                continue;
            }
            // First non-empty line was not a topic — malformed block.
            return None;
        }
        body_lines.push(line);
    }
    let topic = topic?;
    let body = body_lines.join("\n").trim().to_string();
    if body.is_empty() {
        return None;
    }
    Some(DistilledMemory { topic, body })
}

/// Merge memories that slug-collide within a single run so a later block can
/// never silently overwrite an earlier one. Insertion order is preserved.
fn merge_by_slug(memories: Vec<DistilledMemory>) -> Vec<(String, DistilledMemory)> {
    let mut order: Vec<String> = Vec::new();
    let mut by_slug: BTreeMap<String, DistilledMemory> = BTreeMap::new();
    for memory in memories {
        let slug = slugify(&memory.topic);
        match by_slug.get_mut(&slug) {
            Some(existing) => {
                existing.body.push_str("\n\n");
                existing.body.push_str(&memory.body);
            }
            None => {
                order.push(slug.clone());
                by_slug.insert(slug, memory);
            }
        }
    }
    order
        .into_iter()
        .map(|slug| {
            let memory = by_slug.remove(&slug).expect("slug present");
            (slug, memory)
        })
        .collect()
}

fn atomic_write(path: &std::path::Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
    }
    fs::rename(&tmp, path)
}

/// Write distilled memories to disk. Each topic becomes
/// `<config_home>/memory/<slug>.md`; if the file already exists (a prior
/// dream), the new body is appended under a dated section so chronology is
/// preserved instead of being destructively overwritten. `MEMORY.md` gains one
/// idempotent pointer line per slug. Returns the paths written/updated.
pub fn write_distilled(
    as_of: JournalDate,
    memories: &[DistilledMemory],
) -> io::Result<Vec<PathBuf>> {
    let merged = merge_by_slug(memories.to_vec());
    let dir = memory_root()?;
    let date_header = format_marker(as_of);
    let mut written = Vec::new();

    for (slug, memory) in &merged {
        let path = dir.join(format!("{slug}.md"));
        let new_contents = match fs::read_to_string(&path) {
            Ok(existing) => format!(
                "{}\n\n## {} (update)\n\n{}\n",
                existing.trim_end(),
                date_header,
                memory.body.trim()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => format!(
                "# {}\n\n## {}\n\n{}\n",
                memory.topic,
                date_header,
                memory.body.trim()
            ),
            Err(error) => return Err(error),
        };
        atomic_write(&path, &new_contents)?;
        written.push(path);
    }

    update_memory_index(&merged)?;
    Ok(written)
}

fn update_memory_index(merged: &[(String, DistilledMemory)]) -> io::Result<()> {
    let index_path = memory_index_path()?;
    let mut contents = match fs::read_to_string(&index_path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            "# MEMORY index\n\nDistilled memories. One line per topic.\n\n".to_string()
        }
        Err(error) => return Err(error),
    };
    let mut changed = false;
    for (slug, memory) in merged {
        let pointer = format!("- [{}](memory/{}.md)", memory.topic, slug);
        let link = format!("(memory/{slug}.md)");
        // Idempotent: skip if a pointer to this slug already exists.
        if contents.lines().any(|line| line.contains(&link)) {
            continue;
        }
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(&pointer);
        contents.push('\n');
        changed = true;
    }
    if changed {
        atomic_write(&index_path, &contents)?;
    }
    Ok(())
}

/// Run one dream pass as of `as_of` (UTC day). Reads journal days strictly
/// after the last-dream marker, builds the prompt, invokes `completer`, parses
/// the reply, writes the distilled memories, and advances the marker — but only
/// when at least one valid memory was written. `completer` maps prompt text to
/// model reply text; it is the sole model seam.
pub fn run_dream<F>(
    as_of: JournalDate,
    opts: &DreamOptions,
    completer: F,
) -> io::Result<DreamOutcome>
where
    F: FnOnce(&str) -> io::Result<String>,
{
    let marker = read_last_dream()?;
    let mut eligible: Vec<JournalDate> = list_journal_days()?
        .into_iter()
        .filter(|day| !day_is_after(*day, as_of)) // never dream the future
        .filter(|day| marker.is_none_or(|m| day_is_after(*day, m)))
        .collect();
    if eligible.is_empty() {
        return Ok(DreamOutcome::noop(0, "no new journal days to distill"));
    }
    // Keep the most recent `max_days`.
    if eligible.len() > opts.max_days {
        let cut = eligible.len() - opts.max_days;
        eligible.drain(..cut);
    }
    let latest = *eligible.last().expect("non-empty");
    let days_considered = eligible.len();

    let mut loaded: Vec<(JournalDate, String)> = Vec::new();
    for day in eligible {
        if let Some(contents) = read_day(day)? {
            loaded.push((day, contents));
        }
    }
    if loaded.is_empty() {
        return Ok(DreamOutcome::noop(
            days_considered,
            "journal days were empty",
        ));
    }

    let prompt = build_distill_prompt(&loaded, opts);
    let reply = completer(&prompt)?;
    let ParseResult { memories, skipped } = parse_distilled(&reply);

    if memories.is_empty() {
        // Do NOT advance the marker: nothing durable was produced, so these
        // days remain eligible for a future, better dream.
        return Ok(DreamOutcome {
            days_considered,
            memories_written: 0,
            skipped_blocks: skipped,
            files: Vec::new(),
            marker_advanced: false,
            note: Some("model returned no valid memories".to_string()),
        });
    }

    let files = write_distilled(as_of, &memories)?;
    atomic_write(&last_dream_marker_path()?, &format_marker(latest))?;

    Ok(DreamOutcome {
        days_considered,
        memories_written: memories.len(),
        skipped_blocks: skipped,
        files,
        marker_advanced: true,
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_distill_prompt, parse_distilled, read_last_dream, run_dream, slugify, DreamOptions,
    };
    use crate::journal::{append_entry_on, JournalDate};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "runtime-dream-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    fn day(year: i32, month: u32, day: u32) -> JournalDate {
        JournalDate { year, month, day }
    }

    #[test]
    fn slugify_normalizes_and_handles_empty() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  multi   space "), "multi-space");
        assert_eq!(slugify("***"), "untitled");
    }

    #[test]
    fn parse_distilled_is_tolerant_and_counts_skips() {
        let output = "\
preamble prose that should be ignored\n\
<<<MEMORY>>>\n\
topic: Build command\n\
Run scripts/fmt.sh from root, not cargo fmt.\n\
<<<END>>>\n\
some noise\n\
<<<MEMORY>>>\n\
no topic line here\n\
<<<END>>>\n\
<<<MEMORY>>>\n\
topic: Auth\n\
Subscription uses claw login OAuth.\n\
<<<END>>>\n";
        let result = parse_distilled(output);
        assert_eq!(result.memories.len(), 2, "two valid blocks");
        assert_eq!(result.skipped, 1, "the topic-less block is skipped");
        assert_eq!(result.memories[0].topic, "Build command");
        assert!(result.memories[1].body.contains("claw login"));
    }

    #[test]
    fn parse_distilled_empty_on_garbage() {
        let result = parse_distilled("the model just chatted with no blocks");
        assert!(result.memories.is_empty());
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn build_distill_prompt_caps_total_and_keeps_freshest() {
        let opts = DreamOptions {
            max_days: 7,
            max_chars_per_day: 50,
            max_total_chars: 80,
        };
        let days = vec![
            (day(2026, 5, 30), "old ".repeat(40)),
            (day(2026, 5, 31), "new fresh content".to_string()),
        ];
        let prompt = build_distill_prompt(&days, &opts);
        // Freshest day survives; oldest section is dropped under the total cap.
        assert!(prompt.contains("new fresh content"), "prompt: {prompt}");
        assert!(prompt.contains("--- JOURNAL ---"));
    }

    #[test]
    fn run_dream_writes_memories_and_advances_marker() {
        let _guard = crate::test_env_lock();
        let home = temp_config_home();
        std::env::set_var("CLAW_CONFIG_HOME", &home);

        append_entry_on(day(2026, 6, 1), "09:00:00Z", "decided to use closure seam")
            .expect("journal");

        let canned = "\
<<<MEMORY>>>\n\
topic: Dream seam\n\
runtime stays model-agnostic; CLI injects the AnthropicClient completer.\n\
<<<END>>>\n";
        let outcome = run_dream(day(2026, 6, 1), &DreamOptions::default(), |prompt| {
            assert!(prompt.contains("closure seam"), "journal fed into prompt");
            Ok(canned.to_string())
        })
        .expect("dream");

        assert_eq!(outcome.memories_written, 1);
        assert!(outcome.marker_advanced);
        assert_eq!(outcome.files.len(), 1);

        let topic_file = std::fs::read_to_string(home.join("memory/dream-seam.md")).expect("file");
        assert!(topic_file.contains("model-agnostic"));
        let index = std::fs::read_to_string(home.join("MEMORY.md")).expect("index");
        assert!(index.contains("(memory/dream-seam.md)"));
        assert_eq!(
            read_last_dream().expect("marker"),
            Some(day(2026, 6, 1)),
            "marker advances to latest day"
        );

        std::env::remove_var("CLAW_CONFIG_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn run_dream_no_valid_memories_does_not_advance_marker() {
        let _guard = crate::test_env_lock();
        let home = temp_config_home();
        std::env::set_var("CLAW_CONFIG_HOME", &home);

        append_entry_on(day(2026, 6, 1), "09:00:00Z", "some raw entry").expect("journal");

        let outcome = run_dream(day(2026, 6, 1), &DreamOptions::default(), |_prompt| {
            Ok("model produced only chatter, no blocks".to_string())
        })
        .expect("dream");

        assert_eq!(outcome.memories_written, 0);
        assert!(!outcome.marker_advanced, "marker must not advance on no-op");
        assert_eq!(
            read_last_dream().expect("marker"),
            None,
            "days remain eligible for a future dream"
        );

        std::env::remove_var("CLAW_CONFIG_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn run_dream_no_new_days_is_noop() {
        let _guard = crate::test_env_lock();
        let home = temp_config_home();
        std::env::set_var("CLAW_CONFIG_HOME", &home);

        // No journal at all.
        let mut called = false;
        let outcome = run_dream(day(2026, 6, 1), &DreamOptions::default(), |_prompt| {
            called = true;
            Ok(String::new())
        })
        .expect("dream");
        assert_eq!(outcome.memories_written, 0);
        assert!(outcome.note.is_some());
        assert!(!called, "completer must not be called with no journal days");

        std::env::remove_var("CLAW_CONFIG_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn second_dream_appends_section_and_keeps_one_pointer() {
        let _guard = crate::test_env_lock();
        let home = temp_config_home();
        std::env::set_var("CLAW_CONFIG_HOME", &home);

        let canned = "\
<<<MEMORY>>>\n\
topic: Dream seam\n\
runtime stays model-agnostic; CLI injects the completer.\n\
<<<END>>>\n";

        append_entry_on(day(2026, 6, 1), "09:00:00Z", "entry one").expect("journal 1");
        run_dream(day(2026, 6, 1), &DreamOptions::default(), |_p| {
            Ok(canned.to_string())
        })
        .expect("dream 1");

        // A new day with the SAME distilled topic must append, not overwrite,
        // and must not add a second pointer for the same slug.
        append_entry_on(day(2026, 6, 2), "09:00:00Z", "entry two").expect("journal 2");
        let outcome = run_dream(day(2026, 6, 2), &DreamOptions::default(), |_p| {
            Ok(canned.to_string())
        })
        .expect("dream 2");
        assert!(outcome.marker_advanced);

        let topic_file = std::fs::read_to_string(home.join("memory/dream-seam.md")).expect("file");
        assert!(
            topic_file.contains("## 2026-06-01") && topic_file.contains("## 2026-06-02 (update)"),
            "both dated sections present: {topic_file}"
        );
        let index = std::fs::read_to_string(home.join("MEMORY.md")).expect("index");
        let pointer_count = index
            .lines()
            .filter(|line| line.contains("(memory/dream-seam.md)"))
            .count();
        assert_eq!(pointer_count, 1, "pointer is idempotent across dreams");

        std::env::remove_var("CLAW_CONFIG_HOME");
        std::fs::remove_dir_all(&home).ok();
    }
}
