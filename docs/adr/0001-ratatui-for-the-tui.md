# ratatui for the TUI

Use ratatui (immediate-mode) with crossterm for the terminal UI. The UI is split at a seam: a **pure Transcript core** holds every decision, and a **thin adapter** owns the terminal, the event streams, and the drawing.

The core follows The Elm Architecture. Functions `apply_event`, `handle_key`, `input_changed`, `submitted`, `steered`, and `agent_down` each take the current Transcript and return `(Transcript, Vec<Effect>)`, where an `Effect` is plain data — an Agent command, a scroll — never an action performed inline. This half of the crate is UI-free and fully unit-tested with no terminal attached.

The adapter runs a `tokio::select!` loop over crossterm's async `EventStream` and the Agent's broadcast events. It feeds each into the core, executes the returned effects, and renders the resulting Transcript. It carries no decisions of its own.

Immediate-mode rendering fits an event-driven redraw model: on every event the adapter re-derives the frame from the current Transcript. Keeping the decisions in a pure core rather than entangled with rendering is what makes the render logic testable and the UI swappable.

Considered and rejected: inline-stateful ratatui, with the transcript logic living directly in the event loop. Rejected because it entangles rendering with decisions and defeats unit-testing the Transcript.
