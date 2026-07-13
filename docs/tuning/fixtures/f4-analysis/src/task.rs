//! Core task types: [`Task`] and its [`Status`].

/// Lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Open,
    Done,
}

impl Status {
    /// Stable string form used in the database file and in listings.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Done => "done",
        }
    }

    /// Inverse of [`Status::as_str`]. Returns `None` for unknown input.
    pub fn parse(s: &str) -> Option<Status> {
        match s {
            "open" => Some(Status::Open),
            "done" => Some(Status::Done),
            _ => None,
        }
    }
}

/// A single tracked task.
///
/// `id` is unique within one database file and never reused while the file
/// exists; ids also encode creation order (higher id = created later).
/// `priority` runs from 0 (lowest) to 9 (highest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: u64,
    pub title: String,
    pub priority: u8,
    pub status: Status,
}

impl Task {
    /// Creates a new open task.
    pub fn new(id: u64, title: &str, priority: u8) -> Task {
        Task {
            id,
            // Tabs and newlines would corrupt the line-oriented db format.
            title: title.replace(['\t', '\n'], " "),
            priority,
            status: Status::Open,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrips_through_strings() {
        for status in [Status::Open, Status::Done] {
            assert_eq!(Status::parse(status.as_str()), Some(status));
        }
        assert_eq!(Status::parse("bogus"), None);
    }

    #[test]
    fn new_task_is_open_and_sanitized() {
        let task = Task::new(3, "write\tthe report", 7);
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.title, "write the report");
        assert_eq!(task.priority, 7);
    }
}
