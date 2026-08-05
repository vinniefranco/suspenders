# Themes: user-authored sparse TOML over the semantic slots

ADR-0008 pushed every color decision into one presentation-boundary module that
maps the semantic display vocabulary onto terminal styles; ADR-0032 anticipated
`/theme`. This ADR makes that mapping user-replaceable. A Theme is a TOML file:
built-ins embedded in the binary, user themes in
`$XDG_CONFIG_HOME/suspenders/themes/*.toml`, filename as identity. Custom
themes ship day one - the file format is the feature, not a later add.

## Decision

**Keys are the semantic slots themselves, sparse.** A theme's `[colors]` table
names the app's own slots (~20: `added`, `removed`, `heading`, `code_block_bg`,
`popup_border`, `prompt_gutter`, ...), not a base16-style abstract
palette. Unstated keys fall back to the built-in default, so a three-line theme
is valid. Base16 roles were rejected: portable, but a user cannot retune one
slot without moving a role everywhere it is used, and the mapping from role to
slot becomes its own hidden vocabulary.

**Colors only.** Bold, italic, and underline are meaning (Emphasis, Muted,
Link) and stay app semantics per ADR-0008. A theme cannot make emphasis
invisible or links unmarked; the schema stays one value type.

**A slot value is `#rrggbb` or an ANSI-16 name.** The ANSI form exists for one
load-bearing reason: today's palette is mostly named ANSI colors, so Suspenders
currently inherits the user's terminal palette. The built-in default keeps its
ANSI names and stays byte-identical and terminal-respecting; re-expressing it
as hex would silently change appearance for anyone with a customized terminal.
Custom themes will typically use hex (truecolor).

**One `syntax` key for code blocks.** Optional, naming a bundled syntect theme
(default `base16-ocean.dark`), so a light theme can pick light syntax colors.
Deriving syntax colors from the ~20 UI slots was rejected (a design project
with mediocre output); loading user `.tmTheme` files is deliberately left for
later.

**Strict per-file, resilient app.** Any error - unknown key, unparsable color,
bad TOML - rejects the whole file; `/theme` lists it unselectable with its
reason. Typos surface instead of silently no-oping (the lenient alternative
reads as "themes are broken"). A missing or broken configured theme at launch
falls back to the built-in default with a visible notice - never a crash, never
a launch block.

**Built-ins: `dark` and `light`.** `dark` is today's palette and the fallback
floor, so it must be total over the slots; `light` exists to prove the schema
covers both polarities before users hit the gaps. Ports of popular palettes
(dracula, nord, catppuccin) are user files, not maintenance burdens.

**Selection: `/theme` with live preview.** The selector follows `/model`
(ADR-0032/0033): moving the highlight repaints the whole screen in that theme,
Enter keeps it and persists a sparse `theme` key to `config.json`, Escape
reverts exactly to the previous theme. `SUSPENDERS_THEME` shadows the file with
the same next-launch warning as the model key.

## Consequences

- `config.json` gains a `theme` key (amends ADR-0031's key set); `/theme`
  shares `/model`'s sanctioned create-if-absent exception.
- The presentation boundary's mapping functions read from an active Theme value
  instead of hardcoding colors; the background constants (`CODE_BG`, `BAR_BG`,
  `SEGMENT_DARK_BG`) become slots.
- The theme TOML schema is a user-facing contract: adding a slot is routine
  (sparse themes keep working), renaming or removing one is a break - and the
  strict parser makes it a loud one.

## Amendment (ADR-0051): the `/theme` DIALOG is filter-less System A

`/theme` is now a System-A numbered `›` DIALOG (`ui::selection::SelectionList`),
qwen-faithful: no free-text filter (few themes), so the dialog is `Frozen` - it
swallows editing chars, and the normalized `/theme ` draft never grows past the
trailing space. Navigation is arrows (wrapping, skipping disabled rows) + digit
quick-select. A broken user file is a DISABLED note (its reason dim inline):
reachable by the cursor, refused by Enter. qwen's theme/scope Tab toggle is
DROPPED (suspenders is single-scope).

**The live-preview firewall holds.** `preview_name` and its four tests are
unchanged; only its INPUT repoints from `Selector::highlight` to the dialog's
active row (`Composer::selector_highlight`). Moving the highlight still previews
that theme live, Enter keeps + persists it, Escape reverts - the revert still
falls out of the per-frame derivation, not a new state machine.

## Amendment (ADR-0053): four qwen roles added, powerline slots removed

The flat-footer port carved four qwen semantic roles that used to BORROW a
neighbouring slot into slots of their own, entering as designed HEX (QwenDark
hues, not legacy ANSI): `foreground` (`text.primary` `#bfbdb6`, was the terminal
default), `accent` (`text.accent` `#D2A6FF`, was the cyan `prompt_gutter`),
`success` (`status.success` `#AAD94C`, was the diff `added` green), and `warning`
(`status.warning` `#FFD700`, was the warm amber `marker_aid`). All four are stated
in BOTH tomls - `dark.toml` (the total fallback floor) as the QwenDark hexes and
`light.toml` as light-polarity counterparts (`#24292f`/`#8839ef`/`#1a7f37`/
`#9a6700`). The totality/drift tests and a new roles-parse-to-their-hexes test
pin them, so a drift in either toml is caught at the slot boundary.

The powerline colour slots (`bar_bg`, `segment_muted_bg`, and the
`segment_*`/`pressure_*` family) are GONE: the flat footer reads none of them,
so they were removed outright from the schema and both tomls (ADR-0053). This is
a deliberate, one-time schema shrink accompanying the powerline's deletion - the
total-floor contract holds over the REMAINING slots, and `dark.toml`/`light.toml`
still state every one of them, pinned by the totality/drift tests. Adding the
four qwen roles was the routine growth this ADR anticipates; dropping the dead
powerline slots keeps the schema honest rather than carrying colours nothing can
ever paint.
