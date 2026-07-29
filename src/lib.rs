//! suspenders - a terminal coding agent for small local models.
//!
//! Library crate declaring the full module tree (ported 1:1 from baud/lib).

pub mod agent;
pub mod approvals;
pub mod compaction;
pub mod content;
pub mod context_files;
pub mod conversation;
pub mod env_context;
pub mod event;
pub mod extensions;
pub mod llm;
pub mod middleware;
pub mod plan;
pub mod presenter;
pub mod run;
pub mod session;
pub mod tool;
pub mod tools;
pub mod ui;
pub mod view_model;
pub mod voice;

pub mod app;

#[cfg(test)]
pub mod test_support;
