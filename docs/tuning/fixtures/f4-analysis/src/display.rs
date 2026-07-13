//! Presentation: turns tasks into the text the user sees.
//!
//! This module owns the display ordering of tasks: listings are sorted by
//! priority (highest first) and, within a priority, by creation order
//! (lowest id first). Nothing else in the program sorts tasks — the store
//! keeps them in insertion order.

use crate::task::Task;

/// Sorts tasks for display: priority descending, then id ascending.
pub fn sort_for_display(tasks: &mut [Task]) {
    tasks.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
}

/// One-line rendering of a task, e.g. `#3 [open] p7 write the report`.
pub fn format_task(task: &Task) -> String {
    format!(
        "#{} [{}] p{} {}",
        task.id,
        task.status.as_str(),
        task.priority,
        task.title
    )
}

/// Renders the full listing (one task per line, display-sorted). An empty
/// database renders as a friendly placeholder instead of nothing.
pub fn render_list(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "no tasks\n".to_string();
    }
    let mut sorted = tasks.to_vec();
    sort_for_display(&mut sorted);
    let mut out = String::new();
    for task in &sorted {
        out.push_str(&format_task(task));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Status;

    #[test]
    fn sorts_by_priority_then_creation_order() {
        let mut tasks = vec![
            Task::new(1, "low", 1),
            Task::new(2, "high", 9),
            Task::new(3, "also high", 9),
            Task::new(4, "mid", 5),
        ];
        sort_for_display(&mut tasks);
        let ids: Vec<u64> = tasks.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![2, 3, 4, 1]);
    }

    #[test]
    fn formats_a_task_on_one_line() {
        let mut task = Task::new(3, "write the report", 7);
        assert_eq!(format_task(&task), "#3 [open] p7 write the report");
        task.status = Status::Done;
        assert_eq!(format_task(&task), "#3 [done] p7 write the report");
    }
}
