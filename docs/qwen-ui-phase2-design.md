# Phase 2 design — committed history rendering (qwen v0.16.0 chrome)

Reskin every settled Transcript item to qwen's exact chrome. All rendering change
lives in `components.rs` (ADR-0019); the pure core stays untouched. Ground truth:
`docs/qwen-ui-reference.md` §2/§7 + qwen source at
`/home/vinnie/Sandbox/qwen-code-v0.16.0/packages/cli/src/ui/`. Preserve the Phase 1
committed==pending identity: BOTH `render_committed_slice` and the pending body must
call the SAME new `grouped_rows()` so a box looks identical committed vs pending.

## Decisions

1. **Retire the lane/gutter/marker-plane** (ADR-0040, already superseded by ADR-0046
   — no new ADR, just execute + fix stale comments). Delete `LANE_GUTTER`,
   `GutterKind`, `lane_gutters`, `RowGutter`(+`glyph`), `row_gutter_for`, `GutterCtx`,
   `LaneStyles`, `gutter_rect`, `gutter_cell_widget`, `paint_gutter`, the gutter fields
   on `PendingStack`/`PendingRow`, and the `content_x`/gutter loops. Content becomes
   full-width minus a 2-col left + 2-col right margin (qwen `HistoryItemDisplay`
   `marginLeft:2/marginRight:2`), plus a blank separator row between items
   (`marginTop:1`, 0 for continuation types), added at assembly (not cached).

2. **Per-item prefix columns** built in `message_lines` as a hanging indent (prefix
   glyph on row 0, body continues via existing `indented_lines`). Widths/glyphs/colors
   (verify each against source, cite file:line):
   - User `>` U+003E, prefix+text `text.accent` (col width 2)
   - Assistant `✦` U+2726 `text.accent`, body full markdown (NEW — no marker today)
   - Thinking `✦` U+2726 `text.secondary` (grey), body markdown grey; continuation
     indents, no glyph. (Retire the `✦ thought:` one-line collapse.)
   - Info/notification `●` U+25CF `text.primary`; Success `✓` U+2713 `status.success`;
     Warning `△` U+25B3 `status.warning`; Error `✕` U+2715 `status.error` (+`(hint)`
     `text.secondary`); Retry `↻` U+21BB `text.secondary`. Drop the italic plane
     treatment — qwen uses prefix+color, no italic.
   - `Tone` mapping: Info/Plain/Aid→`●` primary; Housekeeping→`●` secondary;
     Constrain→`△` warning; Steering→never committed (non-terminal), stays a
     `text.secondary` pending line removed on delivery. Retry/Success as typed items
     is a Phase 7 item (Screen currently funnels them through `info(text)`).

3. **Tool-group box at RENDER time (option B; NEW ADR).** No core change. A maximal
   contiguous run of `ToolCall`/`ToolResult`/tool-`Diff` items is one qwen tool_group.
   (A mid-batch `Info`/`Marker` — extension error, standing approval — splits the run
   into two boxes with the line between them; that is accepted, see ADR-0047.)
   `grouped_rows()` folds consecutive tool items into ONE boxed
   `PendingRow`; everything else passes through as its cached lines. `render_tool_group`
   draws a `borderStyle:"round"` box (`╭╮╰╯─│`) width=inner: borderColor precedence
   shell/`run_command`→`ui.symbol`, else pending→`status.warning`, else
   `border.default`; `gap:1` between tools; per-tool row = 3-wide status marker +
   bold name + space + dim desc `truncate-end`; result body (diff/todo/text) indented
   3, inside the border. Every boxed row padded to exactly `width` via `push_cols`
   (rigidity — the HIGH risk; golden right-border test). Boxes are uncached (cheap);
   Diff syntect stays cached per-item.

4. **Status markers** 0.16.0 ASCII (`constants.ts` TOOL_STATUS, width 3): SUCCESS `✓`
   U+2713 (success), PENDING `o` (success), EXECUTING `⊷` U+22B7, CONFIRMING `?`
   (warning/shell:symbol), CANCELED `-` (warning, bold), ERROR `x` U+0078 (error,
   bold). Map: `ToolResult{is_error:false}`→`✓`; `{is_error:true}`→`x` bold (retire the
   current `✗`); pending `ToolCall`→`⊷` (pending region). CONFIRMING/CANCELED/PENDING
   reserved for Phase 4.

5. **Committed TodoWrite**: retire `plan.rs`'s `[ ]/[~]/[x]` → circle glyphs `○` U+25CB
   / `◐` U+25D0 / `●` U+25CF; 3-wide gutter; in_progress `status.success` else
   `text.primary`; completed strikethrough (color/strikethrough live in components.rs,
   glyph in plan.rs as plain &str). If wiring the structured list into the box is more
   than the glyph swap, defer the in-box structured render to Phase 3 (sticky box);
   Phase 2 does the glyph+color swap regardless.

6. **Diff**: keep suspenders' richer syntect+tint (ADR-0008 win); match qwen structure:
   ADD a line-number gutter (render-side `@@`-header parse, no core change; qwen does
   this) tinted with the diff bg; change inter-hunk separator to `═` U+2550 ×width;
   strip common per-hunk indentation; reword overflow to `... first/last N lines
   hidden ...`. Diff renders INSIDE the tool box now. Line numbers are the one real data
   gap (MEDIUM risk) — parse from header at render.

7. **Colour role→slot** (no hex; Phase 7 does hexes). Map: `text.secondary`→`muted`,
   `text.link`→`link`, `status.success`→`added`(green), `status.error`→`error`,
   `background.diff.*`→`added_bg`/`removed_bg`, `border.default`/`ui.symbol`→`muted`,
   `border.focused`→`popup_border`, `text.accent`→`prompt_gutter` (cyan today; qwen
   purple — Phase 7). Centralize behind helpers (`accent_style`/`secondary_style`/
   `success_style`/`warning_style`/`error_style`/`symbol_style`/`border_style`) so
   Phase 7 is one line per role. PHASE 7 GAPS (no clean slot): `text.primary`
   (foreground), `status.warning` (warning fg), a distinct `success` fg — stopgaps now.

## Preserve
- committed==pending identity: both paths call `grouped_rows()`; add a byte-identity
  test over a transcript containing a tool group.
- RenderCache per-item for measurement + cached Diff syntect; box wrapper at assembly.
- ADR-0019: all ratatui in components.rs; plan.rs glyph stays plain &str; grouping fold
  reads `&[TranscriptItem]` pure values. ADR-0029 measure==draw: every box/diff/prefix
  line width-capped so `wrapped_count` == drawn rows.

## Tests
Golden prefix+first-span style per item type; grouping-fold boundary unit test; box
golden + right-border rigidity; committed==pending identity over a group; status-marker
mapping; plan.rs circle-glyph test; diff structure (line-number gutter, `═` separator,
syntect preserved).

## Risks (ranked)
1. Box rigidity in flat Lines (HIGH) — pad every row to width via push_cols + golden test.
2. Diff line numbers data gap (MEDIUM) — render-side `@@` parse, avoid core change.
3. Structured todos not reaching view (MEDIUM) — glyph swap now, structured in-box render
   deferable to Phase 3.
4. Colour role gaps (LOW, Phase 7).
5. Uncached box cost (LOW) — small; promote to group-aware cache only if profiling demands.

## Checklist (green between steps; land 2+3 together to avoid double-prefix)
1. Centralize color helpers (no behavior change).
2+3. Add per-item prefix columns AND delete the lane/gutter/spine together; full-width
   content + 2-col margins + `marginTop:1` separator; rewrite ADR-0040 comments→ADR-0046.
4. Introduce `grouped_rows()`; wire BOTH render paths to it; identity test.
5. Implement `render_tool_group` box (borders, markers, name+desc, gap); golden+rigidity.
6. Move Diff inside the box + line-number gutter + `═` separator; keep syntect.
7. Retire plan.rs glyphs → circles + color/strikethrough helper; render todo result
   through it (or defer structured in-box render to Phase 3).
8. Status-message reskin for typed cases; flag untyped for Phase 7.
9. Full identity + golden sweep; rewrite retired-ADR comments.

## ADR to write
NEW ADR "Tool groups are grouped at render time, not modeled in the core" — record
option B over A: a contiguous tool run is a group; grouping is a pure view fold; core
stays group-free. A mid-batch Info/Marker legitimately splits the box (ADR-0047).
