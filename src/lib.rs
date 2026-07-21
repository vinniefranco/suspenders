//! suspenders - a terminal coding agent for small local models.
//!
//! Library crate declaring the full module tree (ported 1:1 from baud/lib).

pub mod agent;
pub mod approvals;
pub mod compaction;
pub mod content;
pub mod context_files;
pub mod conversation;
pub mod event;
pub mod llm;
pub mod plan;
pub mod plugin;
pub mod plugins;
pub mod scout;
pub mod session;
pub mod tool;
pub mod tools;
pub mod turn;
pub mod ui;
pub mod voice;

pub mod app;

#[cfg(test)]
pub mod test_support;
