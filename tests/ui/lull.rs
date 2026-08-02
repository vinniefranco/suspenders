use super::*;
use std::collections::HashSet;

// The registry is the extension point: it must be non-empty (the modulo
// pick in `frame` would divide by zero otherwise) and every scene must
// carry at least one frame (an empty cycle would panic the frame index).
// Exercised through `frame` - the public SUT - so an empty registry or a
// scene with no frames would panic rather than returning a wrong value.
#[test]
fn frame_returns_some_for_every_scene_in_the_registry() {
    assert!(!SCENES.is_empty(), "registry must be non-empty");
    for lull in 0..SCENES.len() as u64 {
        let result = frame(SETTLE_TICKS, lull);
        assert!(
            result.is_some(),
            "frame returned None at lull {lull}, meaning a scene with no frames was picked"
        );
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
