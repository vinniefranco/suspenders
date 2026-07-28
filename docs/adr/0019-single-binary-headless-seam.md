# Single binary with a --headless flag; the UI is confined to one module

We ship one binary. The terminal UI (ratatui, ADR-0001) is walled into a single `ui` module, and `--headless` selects a stdout event-subscriber runner instead of launching the UI. Everything below the UI - the Agent, Run, Tools, LLM, and Session - is UI-free and event-driven, so tests and the headless runner drive the Agent purely through Commands plus the Event broadcast and never touch a TTY.

Rationale: the headless path is the same path the tests use, so "runs headless" and "is testable" are the same property. Keeping ratatui out of the core is an invariant - no `use ratatui` outside the `ui` module - worth a CI/clippy guard.

Considered and rejected:

- **A Cargo workspace splitting core/tui/drive crates.** Compile-time enforcement of the seam, but heavier than a single-author binary warrants.
- **Feature-gated UI in one crate.** Draws the seam by feature flags rather than a clear module boundary.

Consequence: the pure-Screen-core / thin-adapter split (ADR-0001) is a module boundary, not a crate boundary.
