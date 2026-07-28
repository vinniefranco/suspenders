//! The lull "waiting" scenes: the whimsical single-row ASCII that plays under
//! the running Turn's lane during a LULL - the Agent is Running but nothing is
//! streaming (waiting on the first token, or a tool executing). Distinct from
//! the `✦ Thinking` tail, which shows when reasoning IS streaming; this fills
//! the silence, with a live elapsed timer on its left.
//!
//! Pure data + selection, no ratatui (ADR-0019): a [`Scene`] is a frame list
//! and its pace, the [`SCENES`] registry holds them, and [`frame`] maps the
//! adapter's lull clock (`quiet_ticks`, `lull_seq`) to the glyph to draw. The
//! display layer (`ui::components`) owns indent/color/timer-placement; this
//! owns the motion and the content. Adding a scene is one [`SCENES`] entry -
//! extensible by construction.
//!
//! The scene is picked pseudo-randomly PER LULL (a hashed `lull_seq`), so a
//! fresh wait usually brings a fresh scene, yet the map is deterministic - the
//! codebase avoids `rand`/clock nondeterminism, and the pick must be testable.

/// One waiting animation: a single-row frame cycle plus its pace. Single-row is
/// a hard invariant - the row sits on its own line under the lane spine, so a
/// display-width wobble can never desync the spine (the class of bug the `🧠`
/// emoji width once caused). Keep frames ASCII (width-1 cells); the one kaomoji
/// scene is padded to a fixed display width BY HAND (its char counts differ
/// across frames because half/full-width glyphs mix - so do NOT assert equal
/// char length across frames; the single-row placement, not equal width, is
/// what keeps it safe).
pub struct Scene {
    /// The frames, cycled in order.
    pub frames: &'static [&'static str],
    /// Animation-ticks per frame: 1 advances every adapter tick (~100ms),
    /// higher slows the scene down. Treated as at least 1.
    pub ticks_per_frame: u64,
}

/// Ticks of unbroken quiet before ANY lull line shows - the settle so short
/// waits never flash a scene: only a wait past this point is "slow enough" to
/// deserve the whimsy. At ~100ms/tick this is ~5s (the timer therefore opens at
/// "5s"). The timer and the animation appear together, once this passes.
pub const SETTLE_TICKS: u64 = 50;

/// The animation frame to draw for a lull quiet for `quiet_ticks` ticks and
/// numbered `lull_seq` this session, or `None` during the settle window. The
/// scene is chosen by the hashed `lull_seq` (stable within one lull, varying
/// wait-to-wait); the frame advances with the elapsed quiet time.
pub fn frame(quiet_ticks: u64, lull_seq: u64) -> Option<&'static str> {
    if quiet_ticks < SETTLE_TICKS {
        return None;
    }
    let scene = &SCENES[(scramble(lull_seq) % SCENES.len() as u64) as usize];
    let elapsed = quiet_ticks - SETTLE_TICKS;
    let idx = (elapsed / scene.ticks_per_frame.max(1)) as usize % scene.frames.len();
    Some(scene.frames[idx])
}

/// A compact elapsed label for the lull timer: `"7s"` under a minute, `"2m 03s"`
/// under an hour, `"1h 04m"` beyond. Pure string formatting - the caller turns
/// `quiet_ticks` into seconds (it owns the tick cadence) and pads this to a
/// fixed field so the animation column never jitters.
pub fn format_elapsed(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// SplitMix64's finalizer: scrambles the lull counter into a well-spread value
/// so consecutive lulls jump around the registry instead of marching through
/// it. Deterministic (no `rand`, no clock) - the codebase avoids nondeterminism
/// and the tests need a fixed lull->scene map.
fn scramble(n: u64) -> u64 {
    let mut x = n.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The scene registry. Each entry is one waiting animation; add a scene by
/// adding an entry (the per-lull pick spreads over all of them). Frames are
/// single-row and, except the hand-padded kaomoji finale, pure ASCII.
const SCENES: &[Scene] = &[
    // A calm breathing dot.
    Scene {
        frames: &[" . ", " o ", " O ", " o "],
        ticks_per_frame: 3,
    },
    // A dot gliding across and back.
    Scene {
        frames: &[
            ".           ",
            " .          ",
            "  .         ",
            "   .        ",
            "    .       ",
            "     .      ",
            "      .     ",
            "       .    ",
            "        .   ",
            "         .  ",
            "          . ",
            "           .",
            "          . ",
            "         .  ",
            "        .   ",
            "       .    ",
            "      .     ",
            "     .      ",
            "    .       ",
            "   .        ",
            "  .         ",
            " .          ",
        ],
        ticks_per_frame: 1,
    },
    // An inchworm crawling across.
    Scene {
        frames: &[
            "~           ",
            "~~          ",
            "~~~         ",
            " ~~~        ",
            "  ~~~       ",
            "   ~~~      ",
            "    ~~~     ",
            "     ~~~    ",
            "      ~~~   ",
            "       ~~~  ",
            "        ~~~ ",
            "         ~~~",
            "          ~~",
            "           ~",
            "            ",
        ],
        ticks_per_frame: 2,
    },
    // A cup filling with coffee, then sipped.
    Scene {
        frames: &[
            "[    ]", "[.   ]", "[..  ]", "[... ]", "[....]", "[....]", "[... ]", "[..  ]",
            "[.   ]", "[    ]",
        ],
        ticks_per_frame: 2,
    },
    // Pac-man chomping across a row of dots.
    Scene {
        frames: &[
            "C . . . . .",
            "c . . . . .",
            "  C . . . .",
            "  c . . . .",
            "    C . . .",
            "    c . . .",
            "      C . .",
            "      c . .",
            "        C .",
            "        c .",
            "          C",
            "          c",
        ],
        ticks_per_frame: 2,
    },
    // The spellcast finale (kaomoji, hand-padded to a fixed display width).
    Scene {
        frames: &[
            "(ﾉ　 ヮ　 )ﾉ            ",
            "(ﾉ· ヮ· )ﾉ            ",
            "(ﾉ◕ヮ◕)ﾉ             ",
            "(ﾉ◕ヮ◕)ﾉ ·            ",
            "(ﾉ◕ヮ◕)ﾉ ☆            ",
            "(ﾉ◕ヮ◕)ﾉ ☆·           ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡           ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡·          ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡゛          ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡゛·         ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡゛:.        ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡゛:･        ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧          ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ ·        ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ ✦        ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ ✦·       ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ ✦✧       ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ ✦✧·      ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ ✦✧✦      ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ ✦✧·      ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ ✦·       ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ ·        ",
            "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧          ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡゛:･        ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡゛:.        ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡゛·         ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡゛          ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡·          ",
            "(ﾉ◕ヮ◕)ﾉ ☆彡           ",
            "(ﾉ◕ヮ◕)ﾉ ☆·           ",
            "(ﾉ◕ヮ◕)ﾉ ☆            ",
            "(ﾉ◕ヮ◕)ﾉ ·            ",
        ],
        ticks_per_frame: 2,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // The registry is the extension point: it must be non-empty (the modulo
    // pick in `frame` would divide by zero otherwise) and every scene must
    // carry at least one frame (an empty cycle would panic the frame index).
    #[test]
    fn scenes_is_non_empty_and_every_scene_has_frames() {
        assert!(!SCENES.is_empty());
        for scene in SCENES {
            assert!(!scene.frames.is_empty());
        }
    }

    // The settle window: nothing shows for the first SETTLE_TICKS of quiet, and
    // the animation appears the instant the window closes.
    #[test]
    fn frame_is_none_through_the_settle_window_and_some_at_its_close() {
        for q in 0..SETTLE_TICKS {
            assert_eq!(frame(q, 0), None, "quiet_ticks {q} is still settling");
        }
        assert!(frame(SETTLE_TICKS, 0).is_some());
    }

    // The frame advances with elapsed quiet time: index 0 at the settle close,
    // index 1 after one scene's `ticks_per_frame` more, and it wraps modulo the
    // frame count.
    #[test]
    fn the_frame_advances_by_ticks_per_frame_and_wraps() {
        let lull = 7u64;
        let scene = &SCENES[(scramble(lull) % SCENES.len() as u64) as usize];
        let per = scene.ticks_per_frame.max(1);

        // The settle close is frame 0.
        assert_eq!(frame(SETTLE_TICKS, lull), Some(scene.frames[0]));
        // `per` ticks later is frame 1 (scenes here all have >1 frame).
        assert_eq!(frame(SETTLE_TICKS + per, lull), Some(scene.frames[1]));
        // A full cycle later wraps back to frame 0.
        let cycle = per * scene.frames.len() as u64;
        assert_eq!(frame(SETTLE_TICKS + cycle, lull), Some(scene.frames[0]));
    }

    // The pick is stable WITHIN one lull: the same `lull_seq` at different quiet
    // depths always draws from the same scene (its first frame is the scene's
    // identity here).
    #[test]
    fn the_scene_pick_is_stable_within_a_lull() {
        for lull in 0..20u64 {
            let scene = &SCENES[(scramble(lull) % SCENES.len() as u64) as usize];
            let first = scene.frames[0];
            // At the settle close every scene shows its frame 0, so the drawn
            // glyph identifies the chosen scene across quiet depths.
            assert_eq!(frame(SETTLE_TICKS, lull), Some(first));
            let cycle = scene.ticks_per_frame.max(1) * scene.frames.len() as u64;
            assert_eq!(frame(SETTLE_TICKS + cycle, lull), Some(first));
        }
    }

    // The pick VARIES across lulls: iterating the lull counter must spread over
    // the registry, not march down one scene - proof the hash de-correlates
    // consecutive seeds.
    #[test]
    fn the_scene_pick_varies_across_lulls() {
        let picked: HashSet<usize> = (0..50u64)
            .map(|lull| (scramble(lull) % SCENES.len() as u64) as usize)
            .collect();
        assert!(
            picked.len() >= 3,
            "expected the hash to spread over >=3 scenes, got {}",
            picked.len()
        );
    }

    // The compact elapsed label at each boundary: bare seconds under a minute,
    // `Nm SSs` under an hour, `Nh MMm` beyond - the two-digit pads keep the
    // caller's fixed field from jittering.
    #[test]
    fn format_elapsed_spans_seconds_minutes_and_hours() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(7), "7s");
        assert_eq!(format_elapsed(83), "1m 23s");
        assert_eq!(format_elapsed(143), "2m 23s");
        assert_eq!(format_elapsed(3600), "1h 00m");
        assert_eq!(format_elapsed(3843), "1h 04m");
    }
}
