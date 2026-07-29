# Verification friction is solved at the Approval layer, not the Tool layer

The system prompt's Verify step steers the model into run_command after edits, so verification commands recur many times per Session - each behind an Approval modal. Answering the same modal for the same command is friction that trains users to hit 'y' reflexively, which erodes the gate it exists to provide.

We resolve it with Standing Approval: the modal's third answer, approve-always, records the exact command string in the Agent's set of approved commands; later run_command Tool Calls with an identical string are auto-approved for the rest of the Session (marked as an auto-approval, shown in the Transcript, never modal-blocked). Matching is string equality only - no prefix, glob, or whitespace normalization. `cargo test` covers `cargo test` and nothing else; `cargo  test` is a different command.

Considered and rejected:

- **An approval-exempt test_runner Tool** (auto-detects the project's test framework). Moves the decision from the user to the Agent, and the exemption is a hole: a `test` alias in the project's build config runs arbitrary code without any gate. It is also a second code-execution path to maintain, and small models handle plain test invocations through run_command fine.
- **Prefix or glob matching** (`cargo test*` covers `cargo test --seed 0 && rm -rf /`). Every widening rule is a place where the model can compose an unapproved command out of an approved stem. String equality is the only rule with no such seam.
- **Persisting Standing Approvals across Sessions.** A Session-scoped grant matches the Session-scoped Conversation; persistence adds a config surface and a stale-trust problem (the project's test alias may have changed since the grant).

Consequence: users approve each distinct command string once per Session, including trivially different variants. That repetition is the accepted cost of a matching rule with no widening seam.
