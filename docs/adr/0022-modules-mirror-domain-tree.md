# Modules mirror the domain decomposition one-to-one

The module tree mirrors the domain's decomposition 1:1 with snake_case names — for example `turn/loop_`, `conversation`, `tools/edit_file`, `plugins/diff`, `turn/nudges`, `turn/endgame`, `turn/settlement` — and each unit of behaviour has one colocated test module.

Rationale: a one-module-per-concept layout keeps coverage checkable concept by concept and gives "is concept X implemented and tested?" a yes/no answer. The trailing-underscore names (`loop_`) exist only to dodge Rust keywords and are deliberate, not typos.

Considered and rejected:

- **Merging small modules into larger idiomatic groupings up front.** A cleaner final shape, but it fuzzes the concept↔module↔test mapping during the build.

Consequence: refactoring toward coarser boundaries is deferred until the suite is green.
