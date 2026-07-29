# qwen-code v0.16.0 UI reference (extracted ground truth)

Phase 0 deliverable for the port (see `qwen-ui-port-plan.md`). Every value below is
read from the pinned source at `/home/vinnie/Sandbox/qwen-code-v0.16.0/packages/`
(git tag `v0.16.0`, commit `1b1f4867`), cross-checked against the shipped bundle.
Citations are `file:line` relative to `packages/`. Colour role → hex is in §7.

Default theme is **QwenDark**. Roles: `text.primary`=Foreground `#bfbdb6`,
`text.secondary`=Gray `#3D4149`, `text.accent`=AccentPurple `#D2A6FF`,
`text.link`=AccentBlue `#39BAE6`, `status.success`=AccentGreen `#AAD94C`,
`status.warning`=AccentYellow `#FFD700`, `status.error`=AccentRed `#F26D78`,
`border.default`=Gray, `border.focused`=AccentBlue, `ui.symbol`=Gray.

---

## 1. Rendering spine (Phase 1)

`cli/src/ui/components/MainContent.tsx` returns a Fragment of exactly two children:
a `<Static>` (committed history, printed once) and an `<OverflowProvider>` wrapping
a column Box (the pending/live region, redrawn every frame).

- **Static feed**: `[AppHeader, DebugModeNotification, Notifications, ...history]`
  mapped to `HistoryItemDisplay` with `isPending:false`, keyed by stable `h.id`;
  `availableTerminalHeight` = huge (`staticAreaMaxItemHeight = max(termH*4,100)`),
  gemini cap `MAX_GEMINI_MESSAGE_LINES = 65536` → committed content effectively
  never clamped (`MainContent.tsx:355-381`).
- **Pending feed**: `pendingHistoryItems` mapped to `HistoryItemDisplay` with
  `isPending:true`, `item.id` forced to `0`, keyed by array index; real
  `availableTerminalHeight` clamp applied when `constrainHeight`
  (`MainContent.tsx:382-406`). Then `<ShowMoreLines>` OUTSIDE the clamp.
- **History manager** = `useHistoryManager.ts`. `addItem` is the commit primitive:
  strictly **append-only**, monotonic id (`getNextMessageId` = baseTs + counter),
  dedups only identical consecutive `user` items. `updateItem` mutates by id;
  `truncateToItem` slices the tail. No reorder, no mid-insert.
- **Commit trigger** (invariant across `useGeminiStream.ts`): if a pending item
  exists, `addItem` it then `setPendingHistoryItem(null)`. Streaming assistant text
  commits at `findLastSafeSplitPoint(buffer)` markdown-safe boundaries — head
  commits, growing tail stays pending. **Tool groups (main conversation stream):**
  the pending group is a scheduler-derived memo `pendingToolCallGroupDisplay` over
  the tool scheduler's `toolCalls` (`useGeminiStream.ts:420-425`); it commits via
  `handleCompletedTools`→`addItem` once EVERY tool reaches a terminal state
  (`success`/`error`/`cancelled`) (`:2044-2063`). NOTE: the `splitIndex` mechanism
  (last group with an Executing/Confirming tool marks the live boundary) is the
  SUBAGENT/agent view only (`AgentChatContent.tsx:178-196`), NOT the main stream —
  do not model the main conversation on `splitIndex`.
- **Ink `<Static>` = the `insert_before` seam** (Ink library internals, accurate to
  Ink's model but not citable from qwen source): `Static` tracks a high-water index
  and renders only `items.slice(index)`; the renderer skips static subtrees in the
  live frame and writes NEW static output straight to scrollback above the live
  region (`renderInteractiveFrame`: `log.clear()` → write staticOutput → redraw
  live). Port: committed items → `Terminal::insert_before` keyed by a high-water
  mark; pending region → normal per-frame draw.
- **Overflow of a tall pending item**: `MaxSizedBox` (`MINIMUM_MAX_HEIGHT=2`) clamps
  to `maxHeight`, reserves 1 line for a marker, `overflowDirection:"top"` (keeps the
  bottom visible), registers its id with `OverflowProvider`. `ShowMoreLines` prints
  `Press ctrl-s to show more lines` when something overflowed and state is
  idle/waiting_for_confirmation. Ctrl-S flips `constrainHeight` off for one expanded
  view; any next key re-clamps.
- **refreshStatic** = `clearTerminal` + bump `historyRemountKey` (remount Static,
  replay). Triggered on view change and compact-merge shrink.

## 2. Committed history item rendering (Phase 2)

Dispatcher `HistoryItemDisplay.tsx`: outer Box `marginLeft:2, marginRight:2`,
`marginTop:1` except continuation types (`gemini_content`,`gemini_thought_content`)
get `marginTop:0`. `contentWidth = terminalWidth - 4`.

- **User** (`ConversationMessages` `UserMessage`): bare, prefix `>` U+003E, prefix+
  text both `text.accent`; fixed prefix column `stringWidth+1` then wrapping text.
- **User shell**: prefix `$` U+0024 `text.link`, text `text.primary`.
- **Assistant** (`AssistantMessage`): bare, prefix `✦` U+2726 `text.accent`, body
  full markdown. Continuation `gemini_content` reserves the marker column but omits
  the glyph.
- **Thought** (`ThinkMessage`): same `✦` U+2726 but `text.secondary` (grey) for BOTH
  glyph and markdown body. Continuation `ThinkMessageContent` indents, no glyph.
- **Status messages** (`StatusMessages.tsx`): fixed prefix column + wrapping inline-markdown
  body. Info/notification `●` U+25CF `text.primary`; success `✓` U+2713
  `status.success`; warning `△` U+25B3 `status.warning`; error `✕` U+2715
  `status.error`; retry `↻` U+21BB `text.secondary`.
- **Tool group box** (`ToolGroupMessage`): ONE rounded box (`borderStyle:"round"`)
  around all tools in the group, `gap:1` between tools. `borderColor` precedence
  (`ToolGroupMessage.tsx:325-330`): shell OR `isEmbeddedShellFocused` → `ui.symbol`
  (Gray) — wins over pending; else any tool pending → `status.warning`; else
  `border.default`. `borderDimColor` while pending.
- **Tool status marker** (`ToolStatusIndicator`, width `STATUS_INDICATOR_WIDTH=3`) —
  `constants.ts` `TOOL_STATUS`: SUCCESS `✓` U+2713, PENDING `o` U+006F, EXECUTING
  `⊷` U+22B7 (animated spinner), CONFIRMING `?` U+003F, CANCELED `-` U+002D, ERROR
  `x` U+0078. **These are the 0.16.0 ASCII glyphs — NOT main's `✗`.** Colours:
  pending/success `status.success`; confirming/canceled `status.warning` (shell:
  `ui.symbol`); error `status.error` bold; canceled bold.
- **Tool name+desc** (`ToolInfo`): `name` bold (`text.primary` high/med emphasis,
  `text.secondary` low) + single space + `description` (`text.secondary`); whole line
  `wrap:"truncate-end"` (never wraps, `…` U+2026 at edge); `strikethrough` if canceled.
  Emphasis: high if this tool is confirming, low if another tool is, else medium.
  `←` U+2190 trailing indicator when this tool is the active confirming one.
- **Tool result**: renders below the header INSIDE the same box, indented by 3,
  `marginTop:1`. Renderers by type: `todo`, `plan`, `task`(subagent), `diff`, else
  markdown/text/ansi. In compact mode results hidden unless `forceShowResult`.
- **Committed TodoWrite list** (`TodoDisplay`/`TodoItemRow`): `STATUS_ICONS` pending
  `○` U+25CB, in_progress `◐` U+25D0, completed `●` U+25CF; 3-wide glyph gutter then
  wrapping content; in_progress `AccentGreen`, else `Foreground`; completed
  strikethrough. (Distinct from the sticky "Current tasks" box, §3.)
- **Diff** (`DiffRenderer`): per line `Box` row = tinted line-number gutter (`text.
  secondary` fg, bg `background.diff.added/removed`) + content. Context: no tint,
  `colorizeLine` syntax highlight. Add/del: bg tint `background.diff.added`(`#AAD94C`)
  /`removed`(`#F26D78`), `+`/`-` prefix (`status.success`/`error`) + space +
  `colorizeLine` (highlight.js/lowlight) — **syntax-highlighted ON TOP of the tint**.
  Common indentation stripped per hunk. Hunk gap = `═` U+2550 × contentWidth.
  Overflow via `MaxSizedBox` → `... first/last N lines hidden ...`. `showLineNumbers`
  default true.

## 3. Pending region: spinner + footer + composer (Phase 3)

- **Sticky "Current tasks" box** (`StickyTodoList.tsx`; constants in
  `todoSnapshot.ts`) — VERIFIED: glyphs
  `STATUS_ICONS3` pending `○` U+25CB, in_progress `◐` U+25D0, completed `●` U+25CF;
  order `STICKY_TODO_STATUS_PRIORITY` {in_progress:0,pending:1,completed:2} stable by
  original index; number label = original index+1; cap
  `STICKY_TODO_MAX_VISIBLE_ITEMS=5`; overflow `"... and {{count}} more"`; in_progress
  `AccentGreen` else `Foreground`, completed strikethrough; box `borderStyle:"round"`,
  marginX 2, paddingX 1, header bold `"Current tasks"`.
- **LoadingIndicator** (`LoadingIndicator.tsx`): `primaryText = thought?.subject ||
  currentLoadingPhrase` (`:72`) in `text.accent`, truncate-end. Elapsed:
  `<60 → "${n}s"` else `formatDuration` → e.g. `1m 38s`. Tokens: ` · ↑ 1.2k tokens`
  where arrow `↑` U+2191 (not receiving) / `↓` U+2193 (receiving), sep `·` U+00B7,
  `formatTokenCount` (k/m). Cancel line: `({{time}}{{tokens}} · esc to cancel)` in
  `text.secondary`. Spinner glyph: ink `dots` (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`, 80ms) `text.primary`;
  tmux fallback `. .. ...` 750ms; waiting-confirmation static `⠏` U+280F. Layout
  `paddingLeft:2`; narrow (<80) stacks vertically.
- **Witty phrases** (`usePhraseCycler`): list in `locales/en.js`, index 0
  `"I'm Feeling Lucky"`; uniform-random pick every `PHRASE_CHANGE_INTERVAL_MS=15000`;
  waiting-confirmation → `"Waiting for user confirmation..."`; inactive → index 0.
  (Full 100+ list incl. "Never gonna give you up...", "Finding a suitable loading
  screen pun..." — see agent output / en.js.)
- **Footer** (`Footer.tsx`): row, `justifyContent:"space-between"`, `paddingX:2`,
  `gap:1`; narrow<80 stacks. Left: custom statusline lines, worktree `⎇ branch (slug)`
  U+2387 dim, then `leftBottomContent` priority ladder (Ctrl+C/D exit hints, Esc
  hints, `-- INSERT --`, shell indicator, `AutoAcceptIndicator`, else `? for
  shortcuts`). Right (joined by ` | ` `text.secondary`): sandbox `🔒 label`
  `status.success`, `Debug Mode` `status.warning`, `ContextUsageDisplay` `text.accent`,
  goal pill.
- **AutoAcceptIndicator** (`AutoAcceptIndicator.tsx`): `cycleText` = ` (shift + tab to
  cycle)` (win32: ` (tab to cycle)`) in `text.secondary`. Labels: plan→`plan mode`
  `status.success`; auto-edit→`auto-accept edits` `status.warning`; auto→`auto mode
  (classifier-evaluated)` `status.warning`; yolo→`YOLO mode` `status.error`;
  default→(nothing).
- **ContextUsageDisplay**: `percentage = promptTokenCount/contextWindowSize`; null if
  0; `formatPercentageUsed` = `>1 → ">100"` else `(p*100).toFixed(1)`; label
  `terminalWidth<100 ? "% used" : "% context used"` (leading `%` is in the label);
  over-limit `status.error` else `text.secondary`.
- **Composer** (`Composer.tsx`): stack = LoadingIndicator, QueuedMessageDisplay,
  InputPrompt, then SuggestionsDisplay | KeyboardShortcuts | Footer. Placeholder
  `  Type your message or @path/to/file` (two leading spaces). Prompt prefix `> `
  `text.accent` (`!` shell, `*` yolo). Input frame is NOT a full border: hand-drawn
  top dash rule `─`×cols with optional `─ sessionName ─` right label, and an ink
  bottom-only single border; `borderColor` = focused `border.focused` else
  `border.default`. Queue: up to 3 shown, `... (+N more)`, hint `Press ↑ to edit
  queued messages`.

## 4. Approval flow (Phase 4)

`ToolConfirmationMessage.tsx` renders INSIDE the tool group box below the confirming
tool. Layout: body (diff/command/plan/prompt) `marginBottom:1`; question line
`text.primary` truncate; `RadioButtonSelect` of options. Box turns `status.warning`
while pending.

Question + options (verbatim, in order) by type:
- **edit**: `Apply this change?` → `Yes, allow once`(proceed_once), `Yes, allow
  always`(proceed_always, if trusted), `Modify with external editor`(modify_with_editor,
  conditional), `No, suggest changes (esc)`(cancel).
- **exec**: `Allow execution of: '{{command}}'?` → `Yes, allow once`, `Always allow
  {{action}} in this project`(proceed_always_project), `Always allow {{action}} for
  this user`(proceed_always_user), `No, suggest changes (esc)`. When `action` is
  absent the always-options fall back to `Always allow in this project` / `Always
  allow for this user` (no `{{action}}`).
- **plan**: title(dynamic) → `Yes, restore previous mode ({{mode}})`(restore_previous),
  `Yes, and auto-accept edits`(proceed_always), `Yes, and manually approve edits`
  (proceed_once), `No, keep planning (esc)`(cancel).
- **info (web-fetch)**: `Do you want to proceed?` → once / always-project / always-user
  / suggest-changes.
- **mcp**: `Allow execution of MCP tool "{{tool}}" from server "{{server}}"?` → same 4.
- **compact transform** (NOT a peer confirmation type): the `compactMode` subagent
  inline banner trims ANY confirmation to a fixed 3-option set `Yes, allow once` /
  `Allow always` / `No`, and swaps the question to `Do you want to proceed?` only for
  `exec`/`mcp` (`ToolConfirmationMessage.tsx:453-481`).
- Outcome enum `ToolConfirmationOutcome`: proceed_once, proceed_always,
  proceed_always_project/user/server/tool, modify_with_editor, restore_previous, cancel.

`RadioButtonSelect`/`BaseSelectionList` (System A): selected marker `›` U+203A in a
2-wide gutter (`status.success`), else space; numbers right-aligned `1.`..`N.`
(`showNumbers`); nav ↑/↓/k/j/Ctrl+P/Ctrl+N wrap; Enter selects; digit quick-select
(`NUMBER_INPUT_TIMEOUT_MS=1000`); scroll arrows `▲`U+25B2/`▼`U+25BC; Esc/Ctrl+C →
cancel (handled in ToolConfirmationMessage). Modes cycle on **Shift+Tab** (win32:
Tab): `plan → default → auto-edit → auto → yolo → (wrap)`.

## 5. Menus / selectors (Phase 5) — TWO separate systems

- **System A** (`useSelectionList.ts` + `BaseSelectionList.tsx`) drives **model &
  theme dialogs**: numbered `›` rows (see §4), no free-text filter (only digit
  quick-select + arrows). `RadioButtonSelect` (label) / `DescriptiveRadioButtonSelect`
  (label+description). `maxItemsToShow` 10 (theme 12).
  - **Model dialog** (`ModelDialog.tsx`): `DescriptiveRadioButtonSelect showNumbers`,
    rounded box, title `Select Model`. Grouped by authType order (no header rows),
    per-row `[authType]` badge; detail pane on highlight (modality `·`, context
    window, base URL, API key). Does NOT live-switch on highlight — only on Enter.
    Footer `Enter to select, ↑↓ to navigate, Esc to close`.
  - **Theme dialog** (`ThemeDialog.tsx`): `RadioButtonSelect maxItemsToShow:12
    showScrollArrows`, two columns (list + live Preview pane). **Live-previews on
    highlight** (`applyTheme` on move); **Esc reverts** to pre-dialog theme; Tab
    toggles theme↔scope. `Auto` entry first.
- **System B** (`useCompletion.ts` nav + `useCommandCompletion.tsx` wiring +
  `useSlashCompletion.ts` fzf/ranking + `SuggestionsDisplay.tsx` view) drives the
  **`/` popup**: opens when `cursorRow===0 && isSlashCommand(line)`. **fzf fuzzy**
  (`fuzzy:'v2', case-insensitive`, in `useSlashCompletion.ts`) over name+
  altNames, ranked exact>prefix>segment-prefix>fuzzy then priority/recency; prefix
  fallback. NO `›` marker, NO numbers — active row is **colour only** (`text.accent`
  vs `text.secondary`). Two columns (command | description) in slash mode. Match
  substring rendered inverted. `←/→` collapse/expand long rows. Nav ↑/↓ wrap; Tab or
  Enter accepts (`handleAutocomplete`, inserts trailing space); Esc dismisses.
  `MAX_SUGGESTIONS_TO_SHOW=8`. Sub-command/argument completion via leaf
  `completion()`.

## 6. Thinking mechanic (Phase 6) — DEFINITIVE

**Thoughts PERSIST in committed history** (qwen 0.16.0 diverges from upstream here).
Types `gemini_thought` / `gemini_thought_content` (`types.ts:111-119`).
- Parse (`core/src/utils/thoughtUtils.ts` `parseThought`): subject = text between the
  first `**…**`; description = the rest. `ThoughtSummary = {subject, description}`.
- Spinner subject: `useGeminiStream.ts` `thought` state. Subject-only chunk →
  `setThought` immediately (replaces). Description chunks buffered/merged
  (`mergeThought`: subject last-wins, description accumulates).
- Persist (`handleThoughtEvent`): a description-bearing thought opens a pending
  `gemini_thought` and `addItem`s head chunks at safe split points. Committed to
  history at: first answer Content event (thought→answer transition), split overflow,
  and turn Finished.
- Render (`HistoryItemDisplay` → `ThinkMessage`): grey `✦` U+2726 + grey markdown
  body, `marginTop:1` head / continuation indented. Gated by `!compactMode`.
- Visibility: only `compactMode` (default false), toggled **Ctrl+O**
  (`TOGGLE_COMPACT_MODE`), hides thoughts + tool output together. **No Ctrl-T**
  (Ctrl+T = `TOGGLE_TOOL_DESCRIPTIONS`, unrelated). No per-thought expand/collapse.
- Transition: `thought` state cleared only on new prompt / cancel / error; spinner
  vanishes anyway when streaming goes Idle.

## 7. Colour roles → QwenDark hex (Phase 7 anchor)

Foreground `#bfbdb6` · Gray `#3D4149` · AccentPurple `#D2A6FF` · AccentBlue `#39BAE6`
· LightBlue `#59C2FF` · AccentGreen `#AAD94C` · AccentYellow `#FFD700` · AccentRed
`#F26D78` · DiffAdded bg `#AAD94C` · DiffRemoved bg `#F26D78`. (`semantic-colors.ts`,
`themes/`.)
