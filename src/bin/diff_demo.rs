//! A no-connection demo of the first-class `Diff` render (ADR-0008).
//!
//! `cargo run --bin diff-demo` seeds a Screen with several `TranscriptItem::Diff`
//! items - a Rust edit, a created JS file with a multi-line JSDoc block, a JSON
//! file, and a capped diff - then renders it through the REAL frame path
//! (`ui::components::render`, the same entry each live frame uses) so the marker
//! glyphs, the added/removed background tint, and the two-pass syntect
//! highlighting are all faithful. No Agent, no Session, no IO.
//!
//! This is a driver, not a test, and (like `ui.rs`) is untested by design: it
//! owns the real terminal (raw mode + alternate screen via `ratatui::init`,
//! which also installs a panic hook that restores the terminal), maps a few
//! keys, and restores cleanly on exit.
//!
//! Keys: `q`/`Esc` quit; `j`/`Down` and `k`/`Up` scroll a line; `PageDown`/
//! `PageUp` (or space / `b`) scroll a page; `o` toggles the Ctrl-O tool fold;
//! `t` toggles a comparison against the light theme.

use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;

use suspenders::ui::components::{self, Anim, ConnectionFacts, RenderCache};
use suspenders::ui::screen::{Key, Screen};
use suspenders::ui::theme::{self, Theme};
use suspenders::ui::viewport::{Viewport, WHEEL_LINES};

/// How long a frame waits for input before repainting. There is no Agent to
/// wake for, so this only bounds how promptly a resize/redraw settles.
const POLL: Duration = Duration::from_millis(250);

fn main() -> anyhow::Result<()> {
    // Optional theme arg so Vinnie can compare: `diff-demo light` starts light;
    // `t` toggles at runtime. Anything else (or nothing) is the dark default -
    // the theme that carries the added_bg/removed_bg diff tints.
    let start_light = std::env::args().nth(1).as_deref() == Some("light");

    // Raw mode + the alternate screen, and a panic hook that restores the
    // terminal so a panic never wrecks Vinnie's shell (ratatui::init installs
    // both; ratatui::restore undoes them).
    let mut terminal = ratatui::init();
    let result = Demo::new(start_light).run(&mut terminal);
    ratatui::restore();
    result
}

/// The demo's whole mutable state, so the render and the key handler each take
/// one `&mut self` instead of a fistful of threaded arguments.
struct Demo {
    screen: Screen,
    viewport: Viewport,
    cache: RenderCache,
    conn: ConnectionFacts,
    light: bool,
}

impl Demo {
    fn new(light: bool) -> Self {
        Demo {
            screen: Screen::demo_diffs(),
            viewport: Viewport::new(),
            cache: RenderCache::new(),
            conn: ConnectionFacts {
                base_url: "demo://no-connection".into(),
                model: "diff-demo".into(),
            },
            light,
        }
    }

    /// The loop: draw one frame, read a key, apply it, repeat until quit.
    /// Blocking input (no async runtime) - there is nothing to await but keys.
    fn run<B: Backend>(mut self, terminal: &mut Terminal<B>) -> anyhow::Result<()> {
        loop {
            let geometry = self.draw(terminal)?;
            if !event::poll(POLL)? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match self.handle(key, geometry) {
                (next, Flow::Continue) => self = next,
                (_, Flow::Quit) => return Ok(()),
            }
        }
    }

    /// One frame through the REAL render path, returning its measured geometry
    /// `(total_lines, height)` - fed to the pure Viewport for the scroll actions
    /// that run between draws (as `ui.rs` does).
    fn draw<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> anyhow::Result<(usize, usize)> {
        let theme = active_theme(self.light);
        let mut geometry = (0usize, 0usize);
        terminal.draw(|frame| {
            geometry = components::render(
                frame,
                &self.screen,
                self.conn.view(),
                Anim::default(),
                &self.viewport,
                &mut self.cache,
                theme,
            );
        })?;
        Ok(geometry)
    }

    /// Applies one key against the last frame's geometry, returning the next
    /// state and whether to keep looping. Takes `self` by value so the fold key
    /// can hand the Screen to `handle_key` (which consumes it) without a swap
    /// dance. A Theme/fold toggle invalidates the render cache (it keys on
    /// both), so both rebuild it.
    fn handle(mut self, key: KeyEvent, (total, height): (usize, usize)) -> (Self, Flow) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return (self, Flow::Quit),
            KeyCode::Char('j') | KeyCode::Down => {
                self.viewport.scroll_down(WHEEL_LINES, total, height)
            }
            KeyCode::Char('k') | KeyCode::Up => self.viewport.scroll_up(WHEEL_LINES, total, height),
            KeyCode::PageDown | KeyCode::Char(' ') => self.viewport.page_down(total, height),
            KeyCode::PageUp | KeyCode::Char('b') => self.viewport.page_up(total, height),
            // `o` mirrors the app's Ctrl-O tool toggle: it collapses each Diff to
            // its one-line title. The fold produces no Effect we must act on here
            // (no Agent to drive), so the returned effects are dropped.
            KeyCode::Char('o') => {
                let (screen, _effects) = self.screen.handle_key(Key::ToggleTools);
                self.screen = screen;
                self.cache = RenderCache::new();
            }
            KeyCode::Char('t') => {
                self.light = !self.light;
                self.cache = RenderCache::new();
            }
            _ => {}
        }
        (self, Flow::Continue)
    }
}

/// Whether the loop keeps running after a key.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

/// The active Theme: dark by default (its slots carry the diff tints), light on
/// toggle. Both built-ins are total and infallible, so no error path exists.
fn active_theme(light: bool) -> &'static Theme {
    if light { theme::light() } else { theme::dark() }
}
