//! Command-shape helpers (ported from qwen `shell.ts` / `shell-utils.ts`). Used by
//! validation and the background branch to reason about the command text before it
//! runs, with leading shell wrappers stripped so `bash -c '...'` cannot hide the
//! pattern. Two cohesive faces: [`operators`] (shell operators + wrapper parsing)
//! and [`sleep`] (blocking-`sleep` detection).

mod operators;
mod sleep;

pub(super) use operators::{
    has_top_level_git_commit, has_top_level_trailing_background_operator, strip_shell_wrapper,
    strip_trailing_background_amp,
};
pub(super) use sleep::detect_blocked_sleep_pattern;
