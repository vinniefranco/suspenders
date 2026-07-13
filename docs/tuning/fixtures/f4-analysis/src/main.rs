//! tasktrack: a small command-line task tracker.
//!
//! Tasks are persisted to a plain-text database file (`tasks.db` in the
//! current directory, or the path in the `TASKTRACK_DB` environment
//! variable). See `store` for the on-disk format.

mod commands;
mod display;
mod store;
mod task;

use std::env;
use std::process;

fn db_path() -> String {
    env::var("TASKTRACK_DB").unwrap_or_else(|_| "tasks.db".to_string())
}

fn usage() -> ! {
    eprintln!("usage: tasktrack <command> [args]");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  add <title> [priority 0-9]   add a new task (default priority 1)");
    eprintln!("  list                         show all tasks");
    eprintln!("  done <id>                    mark a task as done");
    eprintln!("  remove <id>                  delete a task");
    process::exit(2);
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let db = db_path();

    let result = match args.first().map(String::as_str) {
        Some("add") => {
            let title = args.get(1).cloned().unwrap_or_else(|| usage());
            let priority = match args.get(2) {
                Some(p) => p.parse::<u8>().ok().filter(|p| *p <= 9).unwrap_or_else(|| usage()),
                None => 1,
            };
            commands::add(&db, &title, priority)
        }
        Some("list") => commands::list(&db),
        Some("done") => {
            let id = parse_id(args.get(1));
            commands::done(&db, id)
        }
        Some("remove") => {
            let id = parse_id(args.get(1));
            commands::remove(&db, id)
        }
        _ => usage(),
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        process::exit(1);
    }
}

fn parse_id(arg: Option<&String>) -> u64 {
    match arg.and_then(|a| a.parse::<u64>().ok()) {
        Some(id) => id,
        None => usage(),
    }
}
