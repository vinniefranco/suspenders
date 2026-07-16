# web_fetch is Approval-gated and reuses Standing Approval on the exact URL

web_fetch is the first tool that reaches outside the Project Root: doc lookups need the network, and the confinement story every other tool lives under (`with_path`, root escape refusal) does not apply to a URL. The permission model is therefore the design, not a detail.

Every fetch passes the Approval gate, showing the user the full URL. The gate is the same one run_command uses, and Standing Approval (ADR-0005) applies unchanged: approve-always records the exact URL string, and only a later Tool Call with an identical string is auto-approved. `https://docs.rs/tokio` covers `https://docs.rs/tokio` and nothing else - not the domain, not the path prefix, not a different query string. Matching is string equality only, the same no-widening-seam argument as ADR-0005: every widening rule is a place where the model can compose an unapproved fetch out of an approved stem.

Considered and rejected:

- **Free read access** ("it's only a GET"). A GET's URL is a write channel: anything the model has read can be exfiltrated in a query string to a server the model chose. Showing the full URL in the modal is exactly the review that closes it.
- **Domain allowlists** (approve `docs.rs` once, fetch anything under it). A config surface to maintain, and a widening seam: path and query still carry arbitrary data to an approved host. Same class of hole ADR-0005 rejected for command prefixes.

Consequences: users approve each distinct URL once per Session, including trivially different variants - the accepted ADR-0005 cost. HTML converts to readable text (html2text, plain-text config) before it enters the Conversation; other `text/*` and JSON pass through raw, anything else is an error result. The download is guarded at 2 MB (marked `[fetch cut at 2MB]`) so a huge body never streams into memory; the Result Cap (ADR-0013) shapes what the Conversation actually keeps.
