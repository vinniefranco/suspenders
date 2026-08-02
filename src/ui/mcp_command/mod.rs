//! The `/mcp` management dialog - its POLICY pure and tested, its I/O in the
//! thin adapter (ADR-0065 Phase E, ADR-0001's pure-core/adapter split).
//!
//! `/mcp` is a navigation-stack WIZARD, not a flat list (ADR-0051 keeps System A
//! and B distinct): a faithful port of qwen v0.16.0's `MCPManagementDialog`. It
//! is a distinct Composer overlay ([`McpDialog`]), mutually exclusive with the
//! flat `CommandSelector`, because the two are different shapes - the selector is
//! one pickable list, this is a stack of heterogeneous steps (a grouped server
//! list, a key/value detail with a radio action list, a scrolling tool list, a
//! tool-schema detail, an OAuth progress log).
//!
//! Steps: SERVER_LIST -> SERVER_DETAIL -> {TOOL_LIST -> TOOL_DETAIL |
//! AUTHENTICATE}. Enter pushes a step (or fires an action); Escape pops one
//! (root Escape closes). The dialog reads the Phase A/C read model
//! ([`McpServerView`]) and NEVER touches the manager or the rmcp crate; the only
//! things crossing the impure seam are the Agent action calls and the OAuth
//! progress lines. Data loads async exactly like `/model`: opening emits an
//! effect that calls `Agent::mcp_views()` off-loop and posts an
//! [`Event::McpDialogReady`](crate::event::Event::McpDialogReady) the Composer folds (generation-tagged so a stale
//! fetch is dropped). After an action succeeds the adapter re-fetches views and
//! posts a fresh ready event, so the dialog reflects the change.
//!
//! Two deliberate divergences from a literal port, both to avoid dead code
//! (ADR-0065): qwen's `DISABLE_SCOPE_SELECT` step is unreachable in v0.16.0
//! (`handleDisable` auto-resolves the scope and never navigates to it), so it is
//! omitted - Enable/Disable dispatch `mcp_set_enabled` directly. Everything the
//! runtime actually reaches is ported faithfully.
//!
//! The module splits along its testability seams (ADR-0001): this file owns the
//! pure navigation fold ([`McpDialog`] + its stack and public folds) and the
//! step-view dispatch; the render is per-step in `server_list`/`server_detail`/
//! `tool_list`/`tool_detail`/`auth` over the shared `row` vocabulary; `run` owns
//! the impure Agent/async seam. The pure
//! render types and the async entry points are re-exported here so
//! `crate::ui::mcp_command::X` stays stable.

use crate::mcp::McpServerView;
use crate::ui::selection::{SelectionKey, SelectionList};

mod auth;
mod row;
mod run;
mod server_detail;
mod server_list;
mod tool_detail;
mod tool_list;

pub use row::{McpDialogView, McpRow, McpSpan, McpStyle};

pub(in crate::ui) use run::{act, open_effect, run};

use auth::{AuthLine, CopyState};

/// The command's registered name - what [`super::command::handled`] routes here,
/// minted once beside the module that owns the command.
pub(crate) const NAME: &str = "mcp";

/// The most tool rows the TOOL_LIST step shows at once before the scroll window
/// engages (qwen `VISIBLE_TOOLS_COUNT`): the list scrolls to keep the active row
/// in view and prints a `current/total` indicator beneath. Read by the tool-row
/// builders in `tool_list`.
const VISIBLE_TOOLS_COUNT: usize = 10;

/// A live SERVER_DETAIL action (qwen `ServerDetailStep`'s `ServerAction`), the
/// value a picked action row resolves to. Shown conditionally exactly as qwen
/// does (see `server_detail::detail_actions`); each maps to an Agent method the adapter
/// calls, re-fetching views on completion. `ViewTools` navigates (no Agent call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAction {
    /// Push the TOOL_LIST step (no Agent call).
    ViewTools,
    /// Push the AUTHENTICATE step (no Agent call - the flow runs there).
    Authenticate,
    /// `mcp_reconnect(name)` then re-fetch.
    Reconnect,
    /// `mcp_set_enabled(name, false)` then re-fetch.
    Disable,
    /// `mcp_set_enabled(name, true)` then re-fetch.
    Enable,
    /// `mcp_clear_auth(name)` then re-fetch.
    ClearAuth,
}

/// One frame of the navigation stack (qwen's `navigationStack` entries), each
/// carrying its OWN selection state so a pop restores the parent's cursor - a
/// deliberate divergence from qwen, which keeps one `selectedIndex` per step
/// component and rebuilds it on re-entry. Modelling the cursor per-frame keeps
/// the pure fold total (no re-derivation).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// The grouped server list (qwen SERVER_LIST). `list` navigates the FLAT
    /// server order (grouping is a render concern).
    ServerList { list: SelectionList },
    /// One server's key/value detail + its conditional action radio (qwen
    /// SERVER_DETAIL). `server` indexes [`McpDialog::servers`]; `list` navigates
    /// the live `server_detail::detail_actions`.
    ServerDetail { server: usize, list: SelectionList },
    /// One server's scrolling tool list (qwen TOOL_LIST). `list` navigates the
    /// server's tools.
    ToolList { server: usize, list: SelectionList },
    /// One tool's description + parameter schema (qwen TOOL_DETAIL). No
    /// selection - Escape is the only key.
    ToolDetail { server: usize, tool: usize },
    /// The OAuth flow's streamed progress log (qwen AUTHENTICATE). `progress`
    /// accumulates [`AuthLine`]s as
    /// [`Event::McpAuthProgress`](crate::event::Event::McpAuthProgress) arrives;
    /// `copy` tracks the OSC52 copy-URL feedback (qwen's `copyState`), driven by
    /// the `c` key once an auth URL is on screen.
    Authenticate {
        server: usize,
        progress: Vec<AuthLine>,
        copy: CopyState,
    },
}

/// The `/mcp` management dialog overlay (ADR-0065 Phase E): the fetched
/// [`McpServerView`]s, the navigation `stack` (never empty - the SERVER_LIST
/// root is the bottom frame), and the `generation` the opening
/// [`Effect::McpCommand`](crate::ui::screen::Effect::McpCommand) carried (the fill events echo it, so a stale fetch is
/// dropped exactly like the selector's). Held as `Option<McpDialog>` on the
/// Composer, mutually exclusive with the flat selector. Pure: no ratatui, no
/// async, no IO.
///
/// Only `PartialEq` (no `Eq`): the fetched [`McpServerView`]s carry a tool
/// `input_schema: serde_json::Value`, which is `PartialEq` but not `Eq`. The
/// step render surfaces ([`McpDialogView`]) ARE `Eq` (plain styled strings), so
/// the overlay view the adapter draws stays `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpDialog {
    /// The fetched read model, server-name-sorted (the adapter sorts). Empty
    /// until the first [`Event::McpDialogReady`](crate::event::Event::McpDialogReady) lands (the SERVER_LIST shows
    /// "Loading…" via [`McpDialog::is_loading`] until then).
    servers: Vec<McpServerView>,
    /// The navigation stack, root-first; the last frame is the active step.
    stack: Vec<Step>,
    /// The activation counter the opening effect carried, echoed by the fill
    /// events so a late fetch from a re-open can never land on this dialog.
    generation: u64,
    /// Whether the first views fetch is still outstanding (the SERVER_LIST shows
    /// "Loading…"). Cleared by the first ready fill.
    loading: bool,
}

impl McpDialog {
    /// Opens a fresh dialog on activation `generation`: an empty server list at
    /// the SERVER_LIST root, loading until the first fill. Mirrors the selector's
    /// `Loading` open (ADR-0033).
    pub fn open(generation: u64) -> Self {
        McpDialog {
            servers: Vec::new(),
            stack: vec![Step::ServerList {
                list: SelectionList::new(0),
            }],
            generation,
            loading: true,
        }
    }

    /// Whether the first views fetch is still outstanding (the SERVER_LIST shows
    /// "Loading…" until the first fill).
    fn is_loading(&self) -> bool {
        self.loading
    }

    /// The active step (the top of the stack). Never `None` - the root
    /// SERVER_LIST is always present - so this unwraps by construction.
    fn step(&self) -> &Step {
        self.stack.last().expect("stack never empties (root)")
    }

    fn step_mut(&mut self) -> &mut Step {
        self.stack.last_mut().expect("stack never empties (root)")
    }

    /// Folds a delivered views fetch into the SERVER_LIST (ADR-0065 Phase E):
    /// only for the activation that opened this dialog (the `generation` guard,
    /// mirroring the selector), else a no-op. Replaces the servers and clears the
    /// loading flag; the whole stack is reset to a fresh SERVER_LIST so an action
    /// that changed the set re-seeds a valid cursor. A stale fill is dropped.
    pub fn fill_ready(&mut self, generation: u64, servers: Vec<McpServerView>) {
        if generation != self.generation {
            return;
        }
        let list = SelectionList::new(servers.len());
        self.servers = servers;
        self.stack = vec![Step::ServerList { list }];
        self.loading = false;
    }

    /// Folds an OAuth progress line into an OPEN AUTHENTICATE step (ADR-0065
    /// Phase D/E, qwen's `OauthDisplayMessage`/`OauthAuthUrl`): appends the line
    /// when the active step is Authenticate for `server`, else a no-op (a stray
    /// progress line for a step the user already left is dropped). Returns
    /// whether it landed, so the Composer can consume it (fold semantics).
    pub fn fold_auth_progress(&mut self, server: &str, message: String, is_url: bool) -> bool {
        let names_match = matches!(
            self.step(),
            Step::Authenticate { server: idx, .. }
                if self.servers.get(*idx).is_some_and(|s| s.name == server)
        );
        if !names_match {
            return false;
        }
        if let Step::Authenticate { progress, .. } = self.step_mut() {
            progress.push(if is_url {
                AuthLine::Url(message)
            } else {
                AuthLine::Message(message)
            });
        }
        true
    }

    /// Sets the AUTHENTICATE step's OSC52 copy feedback (qwen's `copyState`) after
    /// the adapter attempted the write: `Copied` when it reached a TTY, else
    /// `Unsupported` (qwen's `ok ? 'copied' : 'unsupported'`). A no-op when the
    /// active step is not AUTHENTICATE (the user popped away before the report
    /// landed), so the fold stays total.
    pub fn fold_copy_result(&mut self, copied: bool) {
        if let Step::Authenticate { copy, .. } = self.step_mut() {
            *copy = if copied {
                CopyState::Copied
            } else {
                CopyState::Unsupported
            };
        }
    }

    /// Folds one navigation key (first refusal, ADR-0034): Up/Down move the
    /// active step's cursor; Enter pushes a step or fires an action; Escape pops
    /// (or, at the root, closes). Returns the pure [`McpFold`] the Composer acts
    /// on - the impure action dispatch lives in the adapter. A key the step
    /// ignores is [`McpFold::None`] (still consumed by the overlay).
    pub fn fold_key(&mut self, key: McpKey) -> McpFold {
        match key {
            McpKey::Up => self.nav(SelectionKey::Up),
            McpKey::Down => self.nav(SelectionKey::Down),
            McpKey::Enter => self.enter(),
            McpKey::Escape => self.escape(),
            McpKey::Copy => self.copy_url(),
        }
    }

    // `c` on the AUTHENTICATE step (qwen `AuthenticateStep`'s `c` keypress): when
    // an auth URL is on screen, request an OSC52 copy of it - the adapter writes
    // the escape and reports success back via [`McpDialog::fold_copy_result`].
    // Any other step, or no URL yet, is a swallowed no-op (qwen gates the copy on
    // `authUrl && authState === 'authenticating'`).
    fn copy_url(&mut self) -> McpFold {
        match self.step() {
            Step::Authenticate { progress, .. } => match auth::auth_url(progress) {
                Some(url) => McpFold::CopyUrl(url.to_string()),
                None => McpFold::None,
            },
            _ => McpFold::None,
        }
    }

    // Moves the active step's cursor (no-op for the cursor-less TOOL_DETAIL /
    // AUTHENTICATE steps).
    fn nav(&mut self, sel: SelectionKey) -> McpFold {
        if let Some(list) = self.active_list() {
            let _ = list.handle(sel, 0);
        }
        McpFold::None
    }

    // Enter on the active step: push the next step, or fire the picked SERVER_
    // DETAIL action. TOOL_DETAIL / AUTHENTICATE have no Enter target.
    fn enter(&mut self) -> McpFold {
        match self.step() {
            Step::ServerList { list } => self.select_server(list.active()),
            Step::ServerDetail { server, list } => self.pick_action(*server, list.active()),
            Step::ToolList { server, list } => self.select_tool(*server, list.active()),
            Step::ToolDetail { .. } | Step::Authenticate { .. } => McpFold::None,
        }
    }

    // Escape pops the stack; at the root (only the SERVER_LIST left) it closes
    // the whole dialog (qwen's root-Escape `onClose`).
    fn escape(&mut self) -> McpFold {
        if self.stack.len() <= 1 {
            return McpFold::Close;
        }
        self.stack.pop();
        McpFold::None
    }

    // Pushes SERVER_DETAIL for the highlighted server (qwen `handleSelectServer`).
    // A disabled active row (an empty list has none) or an out-of-range index is
    // a no-op.
    fn select_server(&mut self, index: usize) -> McpFold {
        if index >= self.servers.len() {
            return McpFold::None;
        }
        let actions = server_detail::detail_actions(&self.servers[index]);
        self.stack.push(Step::ServerDetail {
            server: index,
            list: SelectionList::new(actions.len()),
        });
        McpFold::None
    }

    // Fires the picked SERVER_DETAIL action for `server` (qwen's `onSelect`
    // switch): ViewTools/Authenticate navigate here; the rest are Agent calls the
    // Composer surfaces as an [`McpFold::Act`] for the adapter to run.
    fn pick_action(&mut self, server: usize, row: usize) -> McpFold {
        let Some(view) = self.servers.get(server) else {
            return McpFold::None;
        };
        let Some(action) = server_detail::detail_actions(view).get(row).copied() else {
            return McpFold::None;
        };
        match action {
            McpAction::ViewTools => {
                self.stack.push(Step::ToolList {
                    server,
                    list: SelectionList::new(view.tools.len()),
                });
                McpFold::None
            }
            McpAction::Authenticate => {
                self.stack.push(Step::Authenticate {
                    server,
                    progress: Vec::new(),
                    copy: CopyState::Idle,
                });
                // The adapter runs the flow off-loop and streams progress back
                // as McpAuthProgress; the step renders it.
                McpFold::Act(McpAction::Authenticate, view.name.clone())
            }
            other => McpFold::Act(other, view.name.clone()),
        }
    }

    // Pushes TOOL_DETAIL for the highlighted tool (qwen `handleSelectTool`). An
    // out-of-range index (an empty tool list) is a no-op.
    fn select_tool(&mut self, server: usize, index: usize) -> McpFold {
        let has_tool = self
            .servers
            .get(server)
            .is_some_and(|s| index < s.tools.len());
        if !has_tool {
            return McpFold::None;
        }
        self.stack.push(Step::ToolDetail {
            server,
            tool: index,
        });
        McpFold::None
    }

    // The active step's navigation list, if it has one (the two leaf steps do
    // not).
    fn active_list(&mut self) -> Option<&mut SelectionList> {
        match self.step_mut() {
            Step::ServerList { list }
            | Step::ServerDetail { list, .. }
            | Step::ToolList { list, .. } => Some(list),
            Step::ToolDetail { .. } | Step::Authenticate { .. } => None,
        }
    }

    /// The active step's render surface (header + content + footer). Pure; the
    /// adapter draws it in the bordered box. Dispatches each step to its per-step
    /// builder (`server_list`/`server_detail`/`tool_list`/`tool_detail`/`auth`);
    /// the two missing-selection guards are
    /// defensive (the navigation never pushes an out-of-range step).
    pub fn view(&self) -> McpDialogView {
        match self.step() {
            Step::ServerList { list } => {
                server_list::server_list_view(&self.servers, self.is_loading(), list.active())
            }
            Step::ServerDetail { server, list } => match self.servers.get(*server) {
                Some(view) => server_detail::server_detail_view(view, list.active()),
                None => row::missing_view("No server selected"),
            },
            Step::ToolList { server, list } => match self.servers.get(*server) {
                Some(view) => tool_list::tool_list_view(view, list.active()),
                None => row::missing_view("No server selected"),
            },
            Step::ToolDetail { server, tool } => {
                match self.servers.get(*server).and_then(|s| s.tools.get(*tool)) {
                    Some(view) => tool_detail::tool_detail_view(view),
                    None => row::missing_view("No tool selected"),
                }
            }
            Step::Authenticate {
                server,
                progress,
                copy,
            } => match self.servers.get(*server) {
                Some(view) => auth::authenticate_view(view, progress, *copy),
                None => row::missing_view("No server selected"),
            },
        }
    }
}

/// A key the `/mcp` dialog acts on (ADR-0019): the adapter maps a real key to
/// one of these. Navigation and Enter/Escape, plus the AUTHENTICATE step's
/// `Copy` (qwen's `c`-to-copy-the-auth-URL) - the dialog has no editable filter
/// (unlike the model selector), so other typed chars are not offered to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpKey {
    Up,
    Down,
    Enter,
    Escape,
    /// `c` on the AUTHENTICATE step: copy the shown auth URL to the clipboard via
    /// OSC52 (qwen `AuthenticateStep`'s `copyToClipboardViaOsc52`). A no-op on
    /// any other step, or when no URL is on screen yet.
    Copy,
}

/// What folding one [`McpKey`] produced (the Composer acts on it): a pure
/// navigation move (`None`), a request to CLOSE the dialog (root Escape), or an
/// ACTION to dispatch to the Agent - the server name rides along so the adapter
/// need not re-read the dialog.
#[must_use = "a dropped McpFold drops the dialog's decision (a close or an Agent action)"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpFold {
    /// The key moved the cursor or pushed/popped a navigation step - nothing for
    /// the adapter to do beyond a redraw.
    None,
    /// Close the dialog (root Escape). The Composer drops the overlay.
    Close,
    /// Dispatch an action to the Agent for the named server, then re-fetch views.
    /// `ViewTools`/`Authenticate` never reach here as a nav (they push a step);
    /// `Authenticate` reaches here as the "run the flow" request.
    Act(McpAction, String),
    /// Copy the AUTHENTICATE step's auth URL to the clipboard via OSC52 (qwen's
    /// `copyToClipboardViaOsc52`): the adapter writes `\x1b]52;c;<base64>\x07` to
    /// the terminal and reports whether it reached a TTY back through
    /// [`McpDialog::fold_copy_result`], which sets the copy-feedback hint. Carried
    /// as the raw URL string so the adapter need not re-read the dialog.
    CopyUrl(String),
}

#[cfg(test)]
#[path = "../../../tests/ui/mcp_command.rs"]
mod tests;
