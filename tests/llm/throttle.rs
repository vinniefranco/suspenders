use super::*;

#[test]
fn monotonic_ms_never_runs_backwards() {
    let a = monotonic_ms();
    let b = monotonic_ms();
    assert!(b >= a);
}

#[test]
fn the_first_tick_always_emits() {
    let mut t = Throttle::new(33);
    assert_eq!(t.tick(1_000), Decision::Emit);
}

#[test]
fn ticks_inside_interval_skip_boundary_emits() {
    let mut t = Throttle::new(33);
    assert_eq!(t.tick(1_000), Decision::Emit);
    assert_eq!(t.tick(1_010), Decision::Skip);
    assert_eq!(t.tick(1_032), Decision::Skip);
    assert_eq!(t.tick(1_033), Decision::Emit);
}

#[test]
fn a_skip_does_not_re_arm_the_interval() {
    let mut t = Throttle::new(33);
    assert_eq!(t.tick(1_000), Decision::Emit);
    assert_eq!(t.tick(1_020), Decision::Skip);
    // Still measured from the last EMIT at 1_000, not the skip at 1_020.
    assert_eq!(t.tick(1_033), Decision::Emit);
}

#[test]
fn each_emit_re_arms_the_interval() {
    let mut t = Throttle::new(33);
    assert_eq!(t.tick(1_000), Decision::Emit);
    assert_eq!(t.tick(1_040), Decision::Emit);
    assert_eq!(t.tick(1_050), Decision::Skip);
}
