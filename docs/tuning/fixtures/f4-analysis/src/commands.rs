//! Command implementations: each public function here backs one CLI verb.
//!
//! Every command follows the same load -> mutate -> save shape, going
//! through `store` for persistence and `display` for output.

use std::io;

use crate::display;
use crate::store;
use crate::task::{Status, Task};

/// `add <title> [priority]`: appends a new open task and prints it.
pub fn add(db: &str, title: &str, priority: u8) -> io::Result<()> {
    let mut tasks = store::load(db)?;
    let task = Task::new(store::next_id(&tasks), title, priority);
    println!("added: {}", display::format_task(&task));
    tasks.push(task);
    store::save(db, &tasks)
}

/// `list`: prints every task, highest priority first.
pub fn list(db: &str) -> io::Result<()> {
    let tasks = store::load(db)?;
    print!("{}", display::render_list(&tasks));
    Ok(())
}

/// `done <id>`: marks the matching task as done.
pub fn done(db: &str, id: u64) -> io::Result<()> {
    let mut tasks = store::load(db)?;
    let task = tasks
        .iter_mut()
        .find(|t| t.id == id)
        .ok_or_else(|| not_found(id))?;
    task.status = Status::Done;
    println!("done: {}", display::format_task(task));
    store::save(db, &tasks)
}

/// `remove <id>`: deletes the matching task.
pub fn remove(db: &str, id: u64) -> io::Result<()> {
    let mut tasks = store::load(db)?;
    let before = tasks.len();
    tasks.retain(|t| t.id != id);
    if tasks.len() == before {
        return Err(not_found(id));
    }
    println!("removed task {id}");
    store::save(db, &tasks)
}

fn not_found(id: u64) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no task with id {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;

    fn temp_db(name: &str) -> String {
        let path = env::temp_dir().join(format!("tasktrack-test-{}-{name}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn add_assigns_sequential_ids_and_persists() {
        let db = temp_db("add");
        add(&db, "first", 2).unwrap();
        add(&db, "second", 8).unwrap();
        let tasks = store::load(&db).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!((tasks[0].id, tasks[0].title.as_str()), (1, "first"));
        assert_eq!((tasks[1].id, tasks[1].title.as_str()), (2, "second"));
        fs::remove_file(&db).unwrap();
    }

    #[test]
    fn done_flips_status_and_remove_deletes() {
        let db = temp_db("done-remove");
        add(&db, "a", 1).unwrap();
        add(&db, "b", 1).unwrap();
        done(&db, 1).unwrap();
        let tasks = store::load(&db).unwrap();
        assert_eq!(tasks[0].status, Status::Done);
        assert_eq!(tasks[1].status, Status::Open);
        remove(&db, 1).unwrap();
        let tasks = store::load(&db).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 2);
        assert!(remove(&db, 99).is_err());
        fs::remove_file(&db).unwrap();
    }
}
