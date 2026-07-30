# Sessions persist to a linear JSONL log

A Session's Conversation must survive a crash. The workload is an append-only event journal: written strictly in order, read once at Resume by folding from the top, tiny in volume.

We persist one JSONL file per Session: a header line carrying the Session's fixed facts, then one line per Conversation event. The Agent appends at the points it already touches state - submit, checkpoint, settlement. Each line is written and flushed immediately (a plain append-only file via the standard filesystem API), with serde_json as the line codec. A torn last line (crash mid-write) is detected and dropped; the log is linear - no parent pointers, no branching, no leaf marker.

Considered and rejected:

- **An embedded key-value store (sled/redb).** A KV table, not a log: ordering must be faked with sequence keys, and the store's own on-disk format is opaque. It brings a dependency, a schema, and a compaction/recovery path to defend a workload that a flat file already handles.
- **A binary or otherwise opaque log format.** Compact and fast, but rejected for opacity: our thesis is tuning small-model behavior by inspecting what the model saw, and a session file you can read, grep, and diff is a working tool. JSONL matches the ecosystem convention and its crash mode (truncated last line) is even more self-evidently recoverable than a binary repair pass.
- **A full session tree** (parent pointers, leaf entries, branching, config-change entries). Our Session facts are immutable after launch, which removes the config-entry payoff; branching is a large TUI surface deferred until wanted.

Cost accepted: a small codec between the internal content-block representation and JSON - half of which exists already in the wire conversion performed on every request.

Consequences: Thinking is never in the log (it never enters the Conversation), so a resumed Transcript rebuilds from the Conversation alone. A log ending mid-Run settles that Run as failed on Resume, per the existing rule that a crash settles as a failure.

## Amendment (ADR-0059): ToolResult content is a block list

A logged `ContentBlock::ToolResult`'s `content` is now a `Vec<ResultBlock>`
(ADR-0059), not a `String`. The log line is still self-evident JSONL: the
common case is a single `{"type":"text","text":...}` block, and the tagged
`ResultBlock` enum round-trips through serde like every other content block.
Media (`{"type":"image",...}` / `{"type":"document",...}`) is logged verbatim
when a tool produced it, so a resumed Transcript rebuilds from the Conversation
alone as before - the log stays greppable and diffable.
