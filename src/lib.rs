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
pub(crate) mod glob_match;
pub mod hooks;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod notebook;
pub mod plan;
pub mod run;
pub mod session;
pub mod shell_safety;
pub mod skills;
pub mod stop_reason;
pub mod subagents;
pub mod text;
pub mod tool;
/// The Tool Registry lives under the `tool` module (co-located with the Tool
/// contract and the Capability Context it is part of, so the three
/// mutually-recursive types form one acyclic module node). Re-exported at the
/// crate root under its historical name.
pub use tool::registry as tool_registry;
pub mod tools;
pub mod ui;
pub mod view_model;
pub mod voice;
pub mod walk;

pub mod app;

#[cfg(test)]
#[path = "../tests/test_support.rs"]
pub mod test_support;
