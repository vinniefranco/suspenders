# Suspenders TUI redesign — full mockup spec

Working tuning surface for the UX/IA overhaul. Every interaction is drawn here
so we can argue about concrete screens before touching code. Nothing here is
committed to yet; captions call out the open decisions.

Terminal frame in every scene is ~66 columns. Real colors are noted in prose
since ASCII can't carry them.

## Visual grammar (the decisions locked so far)

- **Your voice sits at the margin.** A prompt is `› ` flush-left, bold in the
  prompt-gutter color. That is the only thing at column 0 with no spine.
- **The agent's turn is one lane.** Everything the agent produces in a turn —
  thinking, tool machinery, the answer — hangs off a single dim vertical spine
  `│` at column 0. A turn reads as one object; your prompts break the spine.
  (This needs turn-boundary awareness in the renderer — an ADR, see below.)
- **Two planes inside the lane.** Foreground = the answer prose (full color) and
  errors (red+bold). Background = tool machinery (`⋯`, dim) and thinking (`✦`,
  dim italic). Motion and the answer are the only things that pull the eye.
- **Live reasoning is content, not a metric.** A running turn streams the last
  few reasoning lines as a rolling tail under an animated `✦ Thinking` header.
  The braille spinner animates *here*, at the brain, not in the status bar.
- **Detail on demand.** Settled thinking collapses to a one-liner (Ctrl-T
  expands). Tool machinery / diffs collapse to one line (Ctrl-O expands).

Legend for these mockups: `⠋` = animated braille spinner (motion). Dim rows are
the background plane. `▓░` = token pressure gauge fill.

---

## 1. Cold start — idle, empty

```
                                                                  
  suspenders · ~/projects/parser                                  
                                                                  
  Describe a task. The agent reads, greps, edits, and runs        
  commands until the work settles.                                
                                                                  
                                                                  
                                                                  
                                                                  
──────────────────────────────────────────────────────────────────
 IDLE   localhost:8888   qwen2.5-coder          ✦▸  ⋯▸   Bot
──────────────────────────────────────────────────────────────────
 › Describe a task…
```

Caption: empty-state hint in muted text, centered vertically-ish at the top so
the composer isn't floating alone. Status bar idle: no spinner anywhere. The
`✦▸ ⋯▸` are the thinking/tools collapse-state indicators (▸ = collapsed).
Placeholder `Describe a task…` is dim inside the composer.

---

## 2. Composing — a multi-line draft

```
  suspenders · ~/projects/parser                                  
                                                                  
  Describe a task. The agent reads, greps, edits, and runs        
  commands until the work settles.                                
                                                                  
                                                                  
──────────────────────────────────────────────────────────────────
 IDLE   localhost:8888   qwen2.5-coder          ✦▸  ⋯▸   Bot
──────────────────────────────────────────────────────────────────
 › the tokenizer and parser are tangled in one fn —
   split the token loop into its own function and
   keep parser doing recursive descent only▏
```

Caption: composer grows upward with the draft; continuation rows align under the
`› `. Cursor `▏` at the end. The transcript viewport shrinks by exactly the rows
the composer takes (current behavior, kept).

---

## 3. Turn running — the hero shot (live reasoning + tools)

```
 › split the token loop into its own function and keep
   parser doing recursive descent only
 │ ⋯ read_file  parser.rs · 812 lines
 │ ⋯ grep  "fn parse"  · 3 hits
 │
 │ ✦ Thinking ⠋  1.2k tok
 │   parse_expr calls next_token directly, so the loop
 │   is inlined in three places; I'll lift it into a
 │   Tokenizer::next and have the parser pull from it▍
──────────────────────────────────────────────────────────────────
 ● RUNNING   localhost:8888   qwen2.5-coder     ✦▾  ⋯▾   6.1k/32k ▓░  Bot
──────────────────────────────────────────────────────────────────
 › ▏
```

Caption: THE fix. The reasoning streams as a rolling tail (last ~3–6 lines) with
the spinner `⠋` animating at the header, right above the status bar — next to
where your cursor sits. Older reasoning lines scroll up off the tail. The status
bar mode block is now a static `● RUNNING` (dot, no motion). The composer stays
live so you can steer without the thinking moving away from you.

Decision A (RESOLVED): short tail — header + last ~3 reasoning lines.
Decision B (RESOLVED): reasoning rows indent 2 cols under the `✦ Thinking`
header (a sub-block off the spine), not flush at the tool indent.

---

## 4. Turn running — answer streaming, thinking collapsed above

```
 › split the token loop into its own function …
 │ ⋯ read_file  parser.rs · 812 lines
 │ ⋯ grep  "fn parse"  · 3 hits
 │ ⋯ edit_file  parser.rs · +18 −6
 │ ✦ thought: lift the inlined loop into Tokenizer::next
 │ Split the token loop into `Tokenizer::next`. The parser
 │ now pulls tokens through it and only does recursive
 │ descent. `parse_expr`/`parse_term`/`parse_factor` lost
 │ their inline `next_token` calls.▏
──────────────────────────────────────────────────────────────────
 ● RUNNING   localhost:8888   qwen2.5-coder     ✦▾  ⋯▾   7.4k/32k ▓░  Bot
──────────────────────────────────────────────────────────────────
 › ▏
```

Caption: the moment thinking finishes and the answer begins. Thinking has
collapsed to `✦ thought: …` (one dim italic line) and the answer streams in
full-color prose directly beneath — one continuous thread, no jump. Inline code
spans (`Tokenizer::next`) keep the code color.

---

## 5. Settled turn — at rest

```
 › split the token loop into its own function …
 │ ✦ thought: lift the inlined loop into Tokenizer::next
 │ ⋯ read_file  parser.rs · 812 lines
 │ ⋯ edit_file  parser.rs · +18 −6
 │ ⋯ run_command  cargo test · ✓ 44 passed
 │ Split the token loop into `Tokenizer::next`. The parser
 │ now pulls tokens through it and only does recursive
 │ descent. Tests pass.

 › ▏
```

Caption: settled. Machinery receded to dim one-liners, thinking a single line,
answer in the foreground, one blank line before your next prompt. This is the
"scannable" resting state: eye runs down the spine, sees think → do → reply.

Decision C (RESOLVED): grouped order kept — thinking-first, then machinery, then
answer (matches `Streaming::end`; `materialize` unchanged). Thinking stays in one
place; the causal think→act interleaving is not preserved on settle.

---

## 6. Ctrl-T — thinking expanded

```
 › split the token loop into its own function …
 │ ✦ Thinking
 │   parse_expr calls next_token directly, so the loop is
 │   inlined in three places; I'll lift it into a
 │   Tokenizer::next and have the parser pull from it.
 │   Need to check parse_factor doesn't peek ahead — it
 │   does, so Tokenizer needs a peek() too.
 │ ⋯ read_file  parser.rs · 812 lines
 │ Split the token loop into `Tokenizer::next` …

 › ▏
```

Caption: Ctrl-T expands every settled thinking block in place (full dim italic
text under the `✦ Thinking` header). Toggle is global, matches today.

---

## 7. Ctrl-O — tool machinery + diff expanded

```
 › split the token loop into its own function …
 │ ✦ thought: lift the inlined loop into Tokenizer::next
 │ ⋯ read_file  parser.rs · 812 lines
 │ ⋯ edit_file  parser.rs · +18 −6
 │   @@ parser.rs  fn parse_expr @@
 │     fn parse_expr(&mut self) -> Expr {
 │   -     let t = next_token(self.src, &mut self.pos);
 │   -     while t.kind == Plus {
 │   +     while self.tok.peek() == Plus {
 │   +         self.tok.next();
 │           ...
 │ ⋯ run_command  cargo test · ✓ 44 passed
 │ Split the token loop into `Tokenizer::next` …

 › ▏
```

Caption: Ctrl-O expands machinery. A write shows its diff (the diff Plugin's
Block via Presentment) with added/removed/context colors, indented under the
spine. Read/grep stay one-liners (nothing to expand).

Decision D (RESOLVED, follows E): diffs stay minimal — the `@@ file fn @@` hunk
header plus added/removed/context semantic colors, no box, no line-number gutter.
Color carries it, same as fenced code.

---

## 8. Code block inside an answer — scannability

```
 › show me the new next()
 │ Here's the extracted tokenizer:
 │
 │    impl Tokenizer<'_> {
 │        fn next(&mut self) -> Token {
 │            let t = scan(self.src, &mut self.pos);
 │            self.last = t.kind;
 │            t
 │        }
 │    }
 │
 │ `peek()` is the same without advancing `pos`.

 › ▏
```

Caption: fenced code in assistant markdown renders bare — syntect highlighting on
a slightly inset block, one blank line above and below, no box or gutter. Color
carries it (decision E, RESOLVED: minimal). Diffs (scene 7) keep their
added/removed semantic colors but drop the box/line-number chrome to match.

---

## 9. run_command approval modal

```
 › run the tests
 │ ✦ thought: verify the split didn't break parsing
 │ ⋯ read_file  parser.rs · 812 lines
 ┌─ run_command ────────────────────────────────────┐
 │  cargo test                                       │
 │                                                   │
 │  [a] approve   [A] always   [d] deny   Esc cancel │
 └───────────────────────────────────────────────────┘
──────────────────────────────────────────────────────────────────
 ● RUNNING   localhost:8888   qwen2.5-coder     ✦▾  ⋯▾   5.2k/32k ▓░  Bot
──────────────────────────────────────────────────────────────────
 › ▏
```

Caption: the only true modal (ADR-0034). Centered, takes keys from the composer.
`always` = the standing-approval path (ADR-0005). Command shown verbatim.

---

## 10. Slash command menu overlay

```
 › split the token loop …
 │ Split the token loop into `Tokenizer::next` …

 ┌──────────────────────────────────────────────────┐
 │ /model     switch the active model                │
 │ /theme     switch the color theme                 │
 │ /compact   compact the conversation now           │
 │ /resume    resume an earlier session              │
 └──────────────────────────────────────────────────┘
 › /m▏
```

Caption: typing `/` opens the menu above the composer; it filters as you type
(`/m` → /model). A Composer overlay, not a modal — draft filters it, backspace
re-opens, Esc empties. Matches ADR-0032.

---

## 11. Model selector overlay (/model)

```
 ┌─ model ──────────────────────────────────────────┐
 │   qwen2.5-coder-7b        local · ready           │
 │ › qwen2.5-coder-14b       local · ready           │
 │   llama-3.1-8b            local · ready           │
 │   ─ needs key ─────────────────────────────────   │
 │   claude-opus-4           set ANTHROPIC_API_KEY   │
 └──────────────────────────────────────────────────┘
 › /model ▏
```

Caption: the selector (ADR-0033 + your signpost work). `›` cursor, greyed
credential hints as non-selectable cursor stops. Group headers (`─ needs key ─`)
are skipped by the cursor.

---

## 12. Error — a failed tool in a turn

```
 › make it build
 │ ✦ thought: apply the rename then check it compiles
 │ ⋯ edit_file  parser.rs · +3 −1
 │ ⚙ run_command  cargo build  ✗ exit 1
 │   error[E0599]: no method `peek` on `Tokenizer`
 │ Missing `peek()`. Adding it now.
 │ ⋯ edit_file  tokenizer.rs · +5 0
 │ ⋯ run_command  cargo build · ✓
 │ Fixed — `peek()` was missing. Builds clean.

 › ▏
```

Caption: errors are the one machinery item that stays in the foreground —
red+bold, `⚙` gutter, always a `✗` marker (colorblind-safe, ADR two-planes).
The failing detail line rides directly under it.

---

## 13. Steering while a turn runs

```
 › split the token loop …
 │ ⋯ read_file  parser.rs · 812 lines
 │ ✦ Thinking ⠙  0.8k tok
 │   lifting the loop into Tokenizer::next; parse_factor
 │   peeks so I'll add peek() too▍
──────────────────────────────────────────────────────────────────
 ● RUNNING   localhost:8888   qwen2.5-coder     ✦▾  ⋯▾   6.1k/32k ▓░  Bot
──────────────────────────────────────────────────────────────────
 › also keep the old fn as a deprecated shim▏
```

Caption: you type into the composer while the turn runs. Steering doesn't
interrupt — it joins the Conversation after the in-flight response/tools finish
(CONTEXT.md: Steering). On submit it drops a `↳ queued: …` marker into the lane
(decision F, RESOLVED: visible), which clears when the steering actually lands.
The thinking tail keeps animating; your draft sits calmly below it.

```
 │ ✦ Thinking ⠙  0.8k tok
 │   …parse_factor peeks so I'll add peek()▍
 │ ↳ queued: also keep the old fn as a deprecated shim
```

---

## 14. Context pressure + compaction

```
 › (long session) …
 │ ⋯ …edited, evicted, compacted…
 │ ⟨ compacted 41 earlier messages → summary ⟩
 │ ⋯ read_file  parser.rs · 812 lines
 │ Continuing from the summary: the split is done, now
 │ wiring peek().

 › ▏
──────────────────────────────────────────────────────────────────
 ● RUNNING   localhost:8888   qwen2.5-coder     ✦▾  ⋯▾   29.8k/32k ▓▓▓▓ 22% dead  Bot
──────────────────────────────────────────────────────────────────
```

Caption: every harness action leaves a distinct-glyph trace in the lane
(decision G, RESOLVED: all visible). Compaction = `⟨ compacted N → summary ⟩`,
eviction = `✂ evicted N stale tool results`, plus the Governor interventions —
`⚑ plan refreshed`, `⊘ tools narrowed to run_command`, `↺ recovery turn` — each
its own glyph. Distinct glyphs, all in the muted marker color so they read as
one "harness voice" plane, legible but not shouting. The token gauge still
carries the quantitative story (`▓▓▓▓ 22% dead`).

```
 │ ✂ evicted 3 stale tool results
 │ ⚑ plan refreshed
 │ ⟨ compacted 41 messages → summary ⟩
 │ ⊘ tools narrowed to run_command (endgame)
```

Decision I (RESOLVED): markers stay INLINE in chronological order — warmth never
groups or reorders them, it only tints the line. Three tints: housekeeping
(eviction, compaction, cap-cuts) = neutral gray; a Governor that AIDS the model
(nudge, plan refresh, recovery) = warm amber (not red); a Governor that
CONSTRAINS it (tool-narrow, turn-close) = cool blue (not green). Steering
(`↳ queued`) wears the prompt color — the user's voice, never the harness.

Mechanism: every marker is already a `TranscriptItem::Info` (steering, eviction,
compaction) or a `*Nudge` event. The redesign gives them a semantic `tone`
(Housekeeping | Aid | Constrain | Steering | Plain) so the adapter tints without
sniffing text. Growing the item vocabulary at ADR-0008's deliberate chokepoint;
enrolls in the prefix-or-bump property test (ADR-0034).

Proposed glyphs (tunable): `✂` evict · `⟨ ⟩` compact · `»` nudge · `⚑` plan
refresh · `⊘` tools narrowed · `▪` turn closed · `↺` recovery · `↳` steering.

---

## 15. Status bar reference

```
Idle:      ○   localhost:8888   qwen2.5-coder         ✦▸  ⋯▸   Bot
Running:   ●   localhost:8888   qwen2.5-coder         ✦▾  ⋯▾   7.4k/32k ▓░  Bot
Elevated:  ●   …   ✦▾  ⋯▾   22.1k/32k ▓▓▓░ (yellow)  42%
Critical:  ●   …   ✦▾  ⋯▾   30.9k/32k ▓▓▓▓ (red)  Bot
Priced:    ○   …   qwen2.5-coder   ✦▸  ⋯▸   12.3k/32k ▓░   $0.04   Bot
```

Caption: powerline segments (colored triangle separators in the real thing).
Left = mode-dot · connection · model. Right = thinking-toggle · tools-toggle ·
token gauge (pressure-colored) · cost (only when >0) · scroll position. Decision
H (RESOLVED): the mode block collapses to a single dot — `●` running (pulsing
color), `○` idle. No spinner, no word; motion lives at the brain, events live in
the lane. (The `IDLE`/`● RUNNING` blocks shown in earlier scenes read as this dot
in the final design.)
```

---

## Implementation plan (post distinguished-engineer review)

Amended after three read-only architecture reviews. See ADR-0040.

**Stage 0 — Theme slots.** `theme.rs`, `theme/active.rs`, `themes/*.toml`. Add
`marker_housekeeping`, `marker_aid`, `marker_constrain`, `lane_spine`,
`thinking_header`. Steering reuses `prompt_gutter`. Sparse TOML (ADR-0038).
Independent — parallel with Stage 1.

**Stage 1 — Marker tone (ATOMIC cross-layer).** `event.rs`, `src/ui/screen.rs`,
`src/ui/transcript.rs`, Session-Log decode. New `TranscriptItem::Marker { text,
tone }` and `enum Tone { Housekeeping, Aid, Constrain, Steering, Plain }`. Tone
stamped on the marker-bearing Events at the firing site; Screen copies onto the
Marker item; adapter never classifies (ADR-0026). Re-point the Steering removal
anchor from `Anchor::Info` to match `Marker` (keep `pending_steering_line` the
sole author). Keep `plugin_failure` as `Info` on the Presentment-bypass path.
Default tone on Session-Log decode so old logs resume. Enroll the new/changed
verbs in the prefix-or-bump property test; markers append (`bumps == 2` holds).
One commit — all layers green together.

**Stage 2 — Live tail + brain animation + status dot.** `components.rs`. Swap the
`🧠 thinking… (~N tokens)` block (`render_viewport` ~L454-467) for `✦ Thinking ⠋`
header + last ~3 reasoning rows, built in the SAME uncached live block; take the
tail from the whole `streaming_thinking()` (do not window in the store). Route the
`spinner` param into `render_viewport`; drop it from `render_status_bar`. Status
bar mode → `●`/`○` dot: edit `StatusSegment::cells` and `paint` keeping equal
column counts.

**Stage 3 — The lane (reserved gutter).** `components.rs` (+`viewport.rs` if
needed). Reserve a fixed 2-col left gutter off `text_area` (mirror the scrollbar
gutter reservation); measure `wrapped_count` at the reduced width. Compute lane
membership in the `render_viewport` assembly loop by a single in-order pass over
`transcript().items()` (a `User` opens a lane; everything until the next `User`
hangs off it; pre-first-`User` is spineless). Draw `│`/`›`/blank per VISUAL row so
soft-wrapped continuations keep the spine. Never touch the RenderCache key or
`message_lines` lane state.

**Stage 4a — Bare code blocks.** `components.rs` `markdown_lines`. Additive inset
padding + blank line above/below; syntect stays. No box/gutter.

**Stage 4b — Minimal diffs (PURE PLUGIN).** `plugins/diff/display.rs` + its tests.
Drop the line-number `pad` column; keep the `@@` hunk header and added/removed/
context semantic colors. Different file from the components chain.

**Stage 4c — Marker tinting.** `components.rs` render arm for `Marker`, tinting by
Stage-1 tone using Stage-0 slots. No `Color` in the store.

Order: {0 ∥ 1} → 2 → 3 → 4a → 4c on `components.rs` (serial); 4b parallel (own
file). Every stage gated by `cargo nextest run` + `cargo clippy`.
