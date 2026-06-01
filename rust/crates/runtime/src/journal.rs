//! Append-only daily memory journal — the raw stream the autonomous "dream"
//! distillation later consolidates into durable memories.
//!
//! Layout: `<config_home>/logs/YYYY/MM/YYYY-MM-DD.md`, one file per UTC day.
//! Append-only by design: live memory is never rewritten in place, so a crash
//! mid-write can at worst lose the last entry, never corrupt prior history.
//! `<config_home>` resolves from `CLAW_CONFIG_HOME`, else `~/.claw` (same
//! contract as the credentials store).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

/// A civil date (UTC) used to address a day's journal file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl JournalDate {
    /// Today's date in UTC, derived from the system clock.
    #[must_use]
    pub fn today_utc() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self::from_unix_timestamp(secs)
    }

    /// Convert a Unix timestamp (seconds) to a UTC civil date using Howard
    /// Hinnant's `civil_from_days` algorithm — avoids pulling in a date crate
    /// for what is a well-known, branch-free conversion.
    #[must_use]
    pub fn from_unix_timestamp(secs: u64) -> Self {
        let days = (secs / 86_400) as i64;
        // Shift epoch to 0000-03-01 so leap-day handling is uniform.
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097; // [0, 146096]
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
        let mp = (5 * doy + 2) / 153; // [0, 11]
        let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
        let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
        let year = if m <= 2 { y + 1 } else { y };
        Self {
            year: year as i32,
            month: m as u32,
            day: d as u32,
        }
    }
}

pub(crate) fn config_home_dir() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("CLAW_CONFIG_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HOME is not set (set USERPROFILE/HOME, or CLAW_CONFIG_HOME to point at the config directory)",
            )
        })?;
    Ok(PathBuf::from(home).join(".claw"))
}

/// Directory that holds all daily journal files.
pub fn journal_root() -> io::Result<PathBuf> {
    Ok(config_home_dir()?.join("logs"))
}

/// Absolute path of the journal file for a given day.
pub fn journal_path_for(date: JournalDate) -> io::Result<PathBuf> {
    Ok(journal_root()?
        .join(format!("{:04}", date.year))
        .join(format!("{:02}", date.month))
        .join(format!(
            "{:04}-{:02}-{:02}.md",
            date.year, date.month, date.day
        )))
}

/// Append a timestamped entry to a specific day's journal file. `time_label`
/// is the intra-day marker (e.g. `14:03:22Z`); kept as a parameter so callers
/// remain testable without a clock.
pub fn append_entry_on(date: JournalDate, time_label: &str, text: &str) -> io::Result<()> {
    let path = journal_path_for(date)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    write!(file, "## {time_label}\n\n{}\n\n", text.trim_end())
}

/// Append an entry to today's (UTC) journal with a `HH:MM:SSZ` marker.
pub fn append_entry(text: &str) -> io::Result<()> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let time_of_day = secs % 86_400;
    let time_label = format!(
        "{:02}:{:02}:{:02}Z",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    );
    append_entry_on(JournalDate::from_unix_timestamp(secs), &time_label, text)
}

/// Read a day's full journal content, or `None` if that day has no file.
pub fn read_day(date: JournalDate) -> io::Result<Option<String>> {
    match fs::read_to_string(journal_path_for(date)?) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// List every day that has a journal file, ascending. Returns an empty vec if
/// the journal root does not exist yet.
pub fn list_journal_days() -> io::Result<Vec<JournalDate>> {
    let root = journal_root()?;
    let mut days = Vec::new();
    let year_dirs = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(days),
        Err(error) => return Err(error),
    };
    for year_entry in year_dirs.flatten() {
        let Some(year) = year_entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(month_dirs) = fs::read_dir(year_entry.path()) else {
            continue;
        };
        for month_entry in month_dirs.flatten() {
            let Ok(day_files) = fs::read_dir(month_entry.path()) else {
                continue;
            };
            for day_file in day_files.flatten() {
                if let Some(date) = parse_day_file_name(year, &day_file.file_name()) {
                    days.push(date);
                }
            }
        }
    }
    days.sort_by_key(|date| (date.year, date.month, date.day));
    Ok(days)
}

fn parse_day_file_name(year: i32, file_name: &std::ffi::OsStr) -> Option<JournalDate> {
    let stem = file_name.to_str()?.strip_suffix(".md")?;
    let mut parts = stem.split('-');
    let y = parts.next()?.parse::<i32>().ok()?;
    let m = parts.next()?.parse::<u32>().ok()?;
    let d = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || y != year || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(JournalDate {
        year: y,
        month: m,
        day: d,
    })
}

#[cfg(test)]
mod tests {
    use super::{append_entry_on, journal_path_for, list_journal_days, read_day, JournalDate};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "runtime-journal-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    #[test]
    fn civil_date_from_unix_timestamp_matches_known_vectors() {
        // Epoch.
        assert_eq!(
            JournalDate::from_unix_timestamp(0),
            JournalDate {
                year: 1970,
                month: 1,
                day: 1
            }
        );
        // 2024-02-29 12:00:00Z (leap day) = 1709208000.
        assert_eq!(
            JournalDate::from_unix_timestamp(1_709_208_000),
            JournalDate {
                year: 2024,
                month: 2,
                day: 29
            }
        );
        // 2026-06-01 00:00:00Z = 1780272000.
        assert_eq!(
            JournalDate::from_unix_timestamp(1_780_272_000),
            JournalDate {
                year: 2026,
                month: 6,
                day: 1
            }
        );
    }

    #[test]
    fn path_layout_is_year_month_day() {
        let _guard = crate::test_env_lock();
        let home = temp_config_home();
        std::env::set_var("CLAW_CONFIG_HOME", &home);
        let path = journal_path_for(JournalDate {
            year: 2026,
            month: 6,
            day: 1,
        })
        .expect("path");
        assert!(path.ends_with("logs/2026/06/2026-06-01.md"), "got {path:?}");
        std::env::remove_var("CLAW_CONFIG_HOME");
    }

    #[test]
    fn append_accumulates_and_read_lists_round_trip() {
        let _guard = crate::test_env_lock();
        let home = temp_config_home();
        std::env::set_var("CLAW_CONFIG_HOME", &home);

        let date = JournalDate {
            year: 2026,
            month: 6,
            day: 1,
        };
        assert_eq!(read_day(date).expect("read missing"), None);

        append_entry_on(date, "09:00:00Z", "first observation").expect("append 1");
        append_entry_on(date, "10:30:00Z", "second observation").expect("append 2");

        let contents = read_day(date).expect("read").expect("present");
        assert!(contents.contains("## 09:00:00Z"));
        assert!(contents.contains("first observation"));
        assert!(contents.contains("## 10:30:00Z"));
        assert!(
            contents.contains("second observation"),
            "append must accumulate, not truncate: {contents}"
        );

        // A second, earlier day so ordering is observable.
        append_entry_on(
            JournalDate {
                year: 2026,
                month: 5,
                day: 31,
            },
            "23:59:00Z",
            "yesterday",
        )
        .expect("append prior day");

        let days = list_journal_days().expect("list days");
        assert_eq!(
            days,
            vec![
                JournalDate {
                    year: 2026,
                    month: 5,
                    day: 31
                },
                JournalDate {
                    year: 2026,
                    month: 6,
                    day: 1
                },
            ]
        );

        std::env::remove_var("CLAW_CONFIG_HOME");
        std::fs::remove_dir_all(home).ok();
    }
}
