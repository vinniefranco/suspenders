//! The Voice (CONTEXT.md): every Suspenders-voiced string the model reads -
//! the system prompt, the compaction prompt, and the markers that enter the
//! Conversation (malformed input, tool-result/error answers, run-close). The
//! boundary is voice, not arity: wording may be parameterized, but Suspenders
//! authors it.
//!
//! Wording is the highest-leverage tuning surface for small local models;
//! owning it in one module means swapping wordings model-by-model touches one
//! file. Strings a tool produces about its own execution (path-bearing errors)
//! stay in that tool - only Suspenders' own authored text lives here.
//!
//! Everything returned here either enters the Conversation or is the system
//! prompt. Markers are bracketed (`[...]`) so a small model can tell the Voice
//! from tool output.
//!
//! The module is three cohesive parts, one submodule each: the fixed
//! [`marker`]s (the [`Marker`] enum and the parameterized cap markers), the two
//! large [`prompt`]s (system + compaction), and the Compaction summary framing
//! ([`compaction`]). Their public API is re-exported here so `crate::voice::X`
//! stays the single import path.

pub mod compaction;
pub mod marker;
pub mod prompt;

pub use compaction::{
    FileOps, compaction_facts, compaction_prompt, serialize_for_compaction, summary_block,
};
pub use marker::{
    Marker, malformed_input, omitted_middle, please_continue, truncated_file, truncated_output,
};
pub use prompt::system_prompt;

#[cfg(test)]
#[path = "../tests/voice.rs"]
mod tests;
