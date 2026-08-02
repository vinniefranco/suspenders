# Lifecycle interception is the Hook subsystem, not an extension pipeline

suspenders has no extension or middleware pipeline. Generic interception of the Tool Call lifecycle is the Hook subsystem (ADR-0066), the single seam an operator installs guards on.

Tool behaviors live in their Tools, not in a wrapper layer: diff rendering in the `edit_file` and `write_file` tools, todo shaping in the `todo_write` tool, and output shaping in the `run_shell_command` tool (the exit-code badge and the noise-run condensing of compile/test output). Conversation-level compaction is a separate concern owned by the compaction service (ADR-0012).

The fail-open-with-visibility principle (ADR-0018) governs Hook execution: a hook failure never fails the Run and is recorded visibly. Approval is the hard safety gate (ADR-0005).
