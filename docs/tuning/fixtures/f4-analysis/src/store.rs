//! Persistence: reads and writes the task database file.
//!
//! The format is deliberately simple — one task per line, four
//! tab-separated fields:
//!
//! ```text
//! <id>\t<priority>\t<status>\t<title>
//! ```
//!
//! Malformed lines are skipped on load rather than aborting, so a partially
//! corrupted file loses only the bad rows.

use std::fs;
use std::io;
use std::path::Path;

use crate::task::{Status, Task};

/// Serializes one task as a single database line (no trailing newline).
pub fn serialize_task(task: &Task) -> String {
    format!(
        "{}\t{}\t{}\t{}",
        task.id,
        task.priority,
        task.status.as_str(),
        task.title
    )
}

/// Parses one database line back into a [`Task`]. Returns `None` if the
/// line does not have exactly four fields or a field fails to parse.
pub fn parse_task(line: &str) -> Option<Task> {
    let mut fields = line.splitn(4, '\t');
    let id = fields.next()?.parse::<u64>().ok()?;
    let priority = fields.next()?.parse::<u8>().ok()?;
    let status = Status::parse(fields.next()?)?;
    let title = fields.next()?.to_string();
    Some(Task {
        id,
        title,
        priority,
        status,
    })
}

/// Loads every well-formed task from the database file. A missing file is
/// treated as an empty database.
pub fn load(path: &str) -> io::Result<Vec<Task>> {
    if !Path::new(path).exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
    Ok(contents.lines().filter_map(parse_task).collect())
}

/// Writes the full task list back to the database file, replacing its
/// previous contents.
pub fn save(path: &str, tasks: &[Task]) -> io::Result<()> {
    let mut out = String::new();
    for task in tasks {
        out.push_str(&serialize_task(task));
        out.push('\n');
    }
    fs::write(path, out)
}

/// The next id to assign: one past the highest id ever stored.
pub fn next_id(tasks: &[Task]) -> u64 {
    tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_then_parse_roundtrips() {
        let task = Task::new(42, "buy milk", 5);
        let line = serialize_task(&task);
        assert_eq!(parse_task(&line), Some(task));
    }

    #[test]
    fn parse_rejects_malformed_lines() {
        assert_eq!(parse_task(""), None);
        assert_eq!(parse_task("not\ta\ttask"), None);
        assert_eq!(parse_task("1\t5\tsnoozed\tunknown status"), None);
        assert_eq!(parse_task("1\t5\topen"), None); // missing title field
    }
}
