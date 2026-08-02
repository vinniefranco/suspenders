use super::*;

const T0: Millis = 0;

fn list(len: usize) -> SelectionList {
    SelectionList::new(len)
}

#[test]
fn new_starts_at_row_zero() {
    let l = list(3);
    assert_eq!(l.active(), 0);
    assert_eq!(l.len(), 3);
    assert!(!l.is_empty());
}

#[test]
fn down_advances_and_wraps() {
    let mut l = list(3);
    assert_eq!(l.handle(SelectionKey::Down, T0), SelectionOutcome::Moved);
    assert_eq!(l.active(), 1);
    l.handle(SelectionKey::Down, T0);
    assert_eq!(l.active(), 2);
    // Wrap from the last row back to the first.
    assert_eq!(l.handle(SelectionKey::Down, T0), SelectionOutcome::Moved);
    assert_eq!(l.active(), 0);
}

#[test]
fn up_wraps_to_the_last_row() {
    let mut l = list(3);
    assert_eq!(l.handle(SelectionKey::Up, T0), SelectionOutcome::Moved);
    assert_eq!(l.active(), 2);
}

#[test]
fn enter_selects_the_active_row() {
    let mut l = list(3);
    l.handle(SelectionKey::Down, T0);
    assert_eq!(
        l.handle(SelectionKey::Enter, T0),
        SelectionOutcome::Selected(1)
    );
}

#[test]
fn escape_cancels() {
    let mut l = list(3);
    assert_eq!(
        l.handle(SelectionKey::Escape, T0),
        SelectionOutcome::Cancelled
    );
}

// With <= 9 rows every single digit already overshoots on the '0'
// extension, so a digit selects immediately (the approval radio's case).
#[test]
fn a_digit_selects_immediately_when_it_cannot_be_a_prefix() {
    let mut l = list(3);
    assert_eq!(
        l.handle(SelectionKey::Digit(2), T0),
        SelectionOutcome::Selected(1)
    );
    assert_eq!(l.active(), 1);
}

#[test]
fn digit_one_selects_the_first_row() {
    let mut l = list(3);
    assert_eq!(
        l.handle(SelectionKey::Digit(1), T0),
        SelectionOutcome::Selected(0)
    );
}

// In a long list a leading digit that COULD prefix a two-digit row buffers
// (moves) and waits; a following digit resolves it.
#[test]
fn a_prefix_digit_buffers_then_a_second_digit_selects() {
    let mut l = list(12);
    // '1' could prefix 10/11/12, so it buffers and moves to row 0.
    assert_eq!(
        l.handle(SelectionKey::Digit(1), T0),
        SelectionOutcome::Moved
    );
    assert_eq!(l.active(), 0);
    // '2' → "12" → row 11, which cannot be extended: select now.
    assert_eq!(
        l.handle(SelectionKey::Digit(2), T0),
        SelectionOutcome::Selected(11)
    );
}

// A buffered digit auto-selects once the host tick reaches the deadline.
#[test]
fn a_buffered_digit_expires_into_a_selection() {
    let mut l = list(12);
    // '1' buffers (could prefix 10-12), active row 0.
    assert_eq!(
        l.handle(SelectionKey::Digit(1), T0),
        SelectionOutcome::Moved
    );
    // Before the deadline: nothing.
    assert_eq!(
        l.expire(NUMBER_INPUT_TIMEOUT_MS - 1),
        SelectionOutcome::Ignored
    );
    // At the deadline: the buffered target (row 0) selects.
    assert_eq!(
        l.expire(NUMBER_INPUT_TIMEOUT_MS),
        SelectionOutcome::Selected(0)
    );
    // Buffer cleared: a second expire does nothing.
    assert_eq!(
        l.expire(NUMBER_INPUT_TIMEOUT_MS + 10),
        SelectionOutcome::Ignored
    );
}

#[test]
fn an_out_of_range_digit_is_ignored_and_clears_the_buffer() {
    let mut l = list(3);
    assert_eq!(
        l.handle(SelectionKey::Digit(9), T0),
        SelectionOutcome::Ignored
    );
    assert_eq!(l.active(), 0, "an out-of-range digit does not move");
}

#[test]
fn a_single_zero_is_invalid_and_buffers_without_selecting() {
    let mut l = list(3);
    assert_eq!(
        l.handle(SelectionKey::Digit(0), T0),
        SelectionOutcome::Ignored
    );
    // A following '1' → "01" → 1 → row 0, selects (cannot extend past 3).
    assert_eq!(
        l.handle(SelectionKey::Digit(1), T0),
        SelectionOutcome::Selected(0)
    );
}

#[test]
fn a_non_digit_key_clears_a_pending_digit_buffer() {
    let mut l = list(12);
    l.handle(SelectionKey::Digit(1), T0); // buffers
    // A non-digit key clears the buffer; the timeout no longer fires.
    assert_eq!(l.handle(SelectionKey::Down, T0), SelectionOutcome::Moved);
    assert_eq!(
        l.expire(NUMBER_INPUT_TIMEOUT_MS + 10),
        SelectionOutcome::Ignored
    );
}

#[test]
fn expire_with_no_buffer_is_ignored() {
    let mut l = list(3);
    assert_eq!(l.expire(9_999), SelectionOutcome::Ignored);
}

// --- disabled mask (Phase-5 dialogs, qwen findNextValidIndex) -----------

#[test]
fn with_active_snaps_the_initial_off_a_disabled_row_walking_down() {
    // Row 0 disabled (a Provider header): the initial active snaps to the
    // first navigable row below it.
    let l = SelectionList::with_active(vec![true, false, false], 0);
    assert_eq!(l.active(), 1);
    // An out-of-range initial clamps to 0, then snaps off the disabled row.
    let l = SelectionList::with_active(vec![true, false], 99);
    assert_eq!(l.active(), 1);
    // A navigable initial is kept as-is.
    let l = SelectionList::with_active(vec![true, false, false], 2);
    assert_eq!(l.active(), 2);
}

#[test]
fn navigation_skips_disabled_rows_in_both_directions_and_wraps() {
    // [header, model, header, model, model]: nav lands only on the models.
    let mut l = SelectionList::with_active(vec![true, false, true, false, false], 0);
    assert_eq!(l.active(), 1, "snapped off the header");
    assert_eq!(l.handle(SelectionKey::Down, T0), SelectionOutcome::Moved);
    assert_eq!(l.active(), 3, "skipped the second header");
    l.handle(SelectionKey::Down, T0);
    assert_eq!(l.active(), 4);
    // Down from the last navigable wraps back to the first, skipping headers.
    l.handle(SelectionKey::Down, T0);
    assert_eq!(l.active(), 1);
    // Up wraps the other way, skipping the trailing/leading headers.
    l.handle(SelectionKey::Up, T0);
    assert_eq!(l.active(), 4);
}

#[test]
fn a_single_navigable_row_never_moves() {
    let mut l = SelectionList::with_active(vec![true, false, true], 0);
    assert_eq!(l.active(), 1);
    assert_eq!(
        l.handle(SelectionKey::Down, T0),
        SelectionOutcome::Ignored,
        "the only navigable row does not move"
    );
    assert_eq!(l.handle(SelectionKey::Up, T0), SelectionOutcome::Ignored);
    assert_eq!(l.active(), 1);
}

#[test]
fn enter_refuses_a_disabled_active_row() {
    // An all-disabled list keeps `initial`; Enter there is a no-op.
    let mut l = SelectionList::with_active(vec![true, true], 0);
    assert_eq!(l.active(), 0);
    assert_eq!(l.handle(SelectionKey::Enter, T0), SelectionOutcome::Ignored);
}

#[test]
fn a_digit_refuses_a_disabled_target() {
    // Row 0 disabled (header): digit '1' targets it and is ignored.
    let mut l = SelectionList::with_active(vec![true, false, false], 0);
    assert_eq!(
        l.handle(SelectionKey::Digit(1), T0),
        SelectionOutcome::Ignored
    );
    assert_eq!(l.active(), 1, "the digit did not move onto the header");
    // Digit '2' targets the navigable row 1 and selects it (short list).
    assert_eq!(
        l.handle(SelectionKey::Digit(2), T0),
        SelectionOutcome::Selected(1)
    );
}
