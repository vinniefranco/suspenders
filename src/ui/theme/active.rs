//! The Active Theme - the adapter-owned Theme state the run loop draws from
//! (ADR-0038), Theme-domain state in the Active Model's mold (ADR-0033: the
//! Agent owns the Active Model, not `/model`): the launch resolution with its
//! dark fallback, the active resolved [`Theme`] a `/theme` pick swaps, and
//! the per-open previews cache behind the live preview. `/theme`'s POLICY
//! (row order, pick rules, message wording) stays in `ui::theme_command`;
//! this module only holds and swaps the state those decisions act on.
//! Ratatui-free like the rest of the theme domain (ADR-0019); the themes
//! directory is captured once at the launch edge, like the config path.

use std::collections::HashMap;
use std::path::PathBuf;

use super::{SparseTheme, Theme, ThemeError};

/// The launch notice a configured theme that cannot load falls back with
/// (ADR-0038): the name, the reason, and the fact that `dark` took over -
/// phrased like the context-file skip line, never a crash or a launch block.
fn fallback_notice(configured: &str, error: &ThemeError) -> String {
    format!("theme \"{configured}\" could not be used ({error}); using the built-in dark")
}

/// The adapter-owned Theme state the run loop draws from (ADR-0038): the
/// active resolved [`Theme`] (swappable - `/theme`'s Enter replaces it), and
/// the previews cache filled once per selector open so a highlight change
/// costs a map lookup, never a re-parse per frame.
pub struct ActiveTheme {
    active_name: String,
    active: Theme,
    themes_dir: PathBuf,
    /// Every theme the last `open` could resolve, by name: the built-ins plus
    /// each valid user file laid over the `dark` floor. Refilled on every
    /// open (a fresh directory read, like `/model`'s fetch), so previews
    /// track the files on disk as of the open. Previews ONLY - `apply`
    /// re-loads from disk, so Enter never applies a stale cache entry.
    previews: HashMap<String, Theme>,
}

impl ActiveTheme {
    /// Resolves the configured theme name for launch (ADR-0038): the named
    /// Theme when it loads, else the built-in `dark` plus the visible notice
    /// [`fallback_notice`] states - a missing or broken configured theme
    /// never fails launch. The fallback also becomes the ACTIVE name, so
    /// `/theme` marks `dark` "(current)" and re-picking it stays a no-op.
    pub fn launch(configured: &str, themes_dir: PathBuf) -> (Self, Option<String>) {
        let (active_name, active, notice) = match super::load(configured, &themes_dir) {
            Ok(loaded) => (configured.to_string(), loaded, None),
            Err(e) => (
                "dark".to_string(),
                super::dark().clone(),
                Some(fallback_notice(configured, &e)),
            ),
        };
        let selection = ActiveTheme {
            active_name,
            active,
            themes_dir,
            previews: HashMap::new(),
        };
        (selection, notice)
    }

    /// The active Theme - what every frame renders with outside a `/theme`
    /// preview, and what Escape reverts to.
    pub fn active(&self) -> &Theme {
        &self.active
    }

    /// The active Theme's name - what `/theme` marks "(current)" and treats
    /// as a no-op re-pick.
    pub(crate) fn active_name(&self) -> &str {
        &self.active_name
    }

    /// One selector open (ADR-0038): a FRESH read of the themes directory
    /// (like `/model` re-fetches), refilling the previews cache with every
    /// theme that resolves. Returns the discovery so the caller builds its
    /// rows from the same read.
    pub(crate) fn open(&mut self) -> Vec<(String, Result<SparseTheme, ThemeError>)> {
        let discovered = super::discover(&self.themes_dir);
        self.previews = resolve_previews(&discovered);
        discovered
    }

    /// The Theme this frame renders with (ADR-0038's live preview): the
    /// resolved Theme `preview` names while `/theme`'s selector highlights a
    /// pickable row (`ui::theme_command::preview_name` derives it), the
    /// active Theme otherwise - so closing the overlay (Escape included) IS
    /// the revert, with no bookkeeping to unwind.
    pub fn render_theme(&self, preview: Option<&str>) -> &Theme {
        preview
            .and_then(|name| self.previews.get(name))
            .unwrap_or(&self.active)
    }

    /// Makes `name` the active Theme by a FRESH load from disk (built-ins by
    /// the static [`super::built_in`] mapping inside [`super::load`]): Enter
    /// applies what is on disk NOW, never the open-time previews cache - a
    /// file that broke between the open and the pick surfaces its reason
    /// instead of applying stale colors under a name that no longer loads.
    pub(crate) fn apply(&mut self, name: &str) -> Result<(), ThemeError> {
        self.active = super::load(name, &self.themes_dir)?;
        self.active_name = name.to_string();
        Ok(())
    }
}

// Every theme `discovered` (plus the built-ins) that resolves, by name -
// resolution work done ONCE per open, so the per-frame preview is a lookup.
fn resolve_previews(
    discovered: &[(String, Result<SparseTheme, ThemeError>)],
) -> HashMap<String, Theme> {
    let mut previews: HashMap<String, Theme> = super::BUILT_INS
        .iter()
        .filter_map(|name| Some((name.to_string(), super::built_in(name)?.clone())))
        .collect();
    for (name, parsed) in discovered {
        if let Ok(sparse) = parsed {
            previews.insert(name.clone(), sparse.over(super::dark()));
        }
    }
    previews
}

#[cfg(test)]
#[path = "../../../tests/ui/theme/active.rs"]
mod tests;
