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
`bar_bg`, the status-bar segment pairs, ...), not a base16-style abstract
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
