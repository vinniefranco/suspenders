use super::*;

// --- applied_line (the three message branches) -------------------------

#[test]
fn a_persist_error_is_surfaced_and_the_swap_still_stands() {
    let err = Err(SessionError("disk full".into()));
    assert_eq!(
        applied_line("local/qwen/y", false, &err),
        "model → local/qwen/y (not saved: disk full)"
    );
    // The env-shadow branch never masks a persist error: the error wins.
    assert_eq!(
        applied_line("local/qwen/y", true, &err),
        "model → local/qwen/y (not saved: disk full)"
    );
}

#[test]
fn a_shadowing_env_warns_the_sticky_write_will_be_overridden() {
    assert_eq!(
        applied_line("local/qwen/y", true, &Ok(())),
        "model → local/qwen/y (SUSPENDERS_MODEL is set and will override this next launch)"
    );
}

#[test]
fn a_clean_persist_is_just_the_bare_line() {
    assert_eq!(
        applied_line("local/qwen/y", false, &Ok(())),
        "model → local/qwen/y"
    );
}

// --- model_rows (the multi-Provider selector rows, ADR-0037) ------------

use crate::view_model::RowRole;

fn listing(provider: &str, models: &[&str]) -> ProviderModels {
    ProviderModels {
        provider: provider.into(),
        models: models.iter().map(|m| m.to_string()).collect(),
        availability: Availability::Available,
    }
}

#[test]
fn rows_are_scoped_ids_under_a_header_per_provider_in_listing_order() {
    let rows = model_rows(
        &[
            listing("local", &["qwen/Qwen3.6-27B-MTP-GGUF"]),
            listing("anthropic", &["claude-fable-5", "claude-haiku-4-5"]),
        ],
        "local/qwen/Qwen3.6-27B-MTP-GGUF",
    );
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "local",
            "local/qwen/Qwen3.6-27B-MTP-GGUF",
            "anthropic",
            "anthropic/claude-fable-5",
            "anthropic/claude-haiku-4-5",
        ]
    );
    // Headers are unpickable; model rows are pickable and their values
    // ARE the scoped ids - a pick needs no re-scoping.
    let pickable: Vec<bool> = rows.iter().map(|r| r.pickable()).collect();
    assert_eq!(pickable, vec![false, true, false, true, true]);
    assert!(
        rows.iter()
            .filter(|r| r.pickable())
            .all(|r| r.value == r.label)
    );
}

#[test]
fn the_active_model_is_marked_current_by_its_scoped_id() {
    let rows = model_rows(
        &[
            listing("local", &["m"]),
            // The same bare id at ANOTHER Provider must not be marked.
            listing("other", &["m"]),
        ],
        "local/m",
    );
    assert_eq!(rows[1].hint.as_deref(), Some("(current)"));
    assert_eq!(rows[3].hint, None);
}

#[test]
fn an_unavailable_provider_shows_a_note_under_its_header() {
    // A down custom host no longer vanishes: its header stays, with a
    // note whose hint is derived from the boundary's availability fact -
    // right under the header, since nothing collapsed needs anchoring.
    let rows = model_rows(
        &[ProviderModels {
            provider: "local".into(),
            models: vec![],
            availability: Availability::Unreachable,
        }],
        "anthropic/claude-fable-5",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].label, "local");
    assert_eq!(rows[0].role, RowRole::Header);
    assert_eq!(rows[1].label, "  unavailable");
    assert_eq!(rows[1].role, RowRole::Note);
    assert_eq!(rows[1].hint.as_deref(), Some("unreachable"));
    assert!(rows.iter().all(|r| !r.pickable()), "nothing is pickable");
}

#[test]
fn an_empty_listing_shows_a_no_models_note() {
    let rows = model_rows(
        &[ProviderModels {
            provider: "local".into(),
            models: vec![],
            availability: Availability::NoModels,
        }],
        "anthropic/claude-fable-5",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].role, RowRole::Note);
    assert_eq!(rows[1].hint.as_deref(), Some("no models"));
}

#[test]
fn a_credential_less_builtin_lists_its_collapsed_catalog_then_the_note() {
    // A built-in whose environment key is unset appears greyed out: its
    // header, one collapsed row per Catalog model - scoped like a
    // pickable row, so a model-name filter reveals it - and LAST the
    // note whose hint names the key to export: the trailing note is the
    // cursor stop that anchors the popup window below the capped reveal.
    // No row of it is pickable.
    let rows = model_rows(
        &[ProviderModels {
            provider: "openrouter".into(),
            models: vec![],
            availability: Availability::MissingCredential {
                env: vec!["OPENROUTER_API_KEY".into()],
                catalog: vec!["qwen3.5-27b".into(), "kimi-k2".into()],
            },
        }],
        "anthropic/claude-fable-5",
    );
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].label, "openrouter");
    assert_eq!(rows[0].role, RowRole::Header);
    assert_eq!(rows[1].label, "openrouter/qwen3.5-27b");
    assert_eq!(rows[1].role, RowRole::Collapsed);
    assert_eq!(rows[2].label, "openrouter/kimi-k2");
    assert_eq!(rows[2].role, RowRole::Collapsed);
    assert_eq!(rows[3].label, "  unavailable");
    assert_eq!(rows[3].role, RowRole::Note);
    assert_eq!(rows[3].hint.as_deref(), Some("set OPENROUTER_API_KEY"));
    assert!(rows.iter().all(|r| !r.pickable()), "nothing is pickable");
}

#[test]
fn two_env_keys_read_as_either_or_in_the_hint() {
    let rows = model_rows(
        &[ProviderModels {
            provider: "vertex".into(),
            models: vec![],
            availability: Availability::MissingCredential {
                env: vec!["A_KEY".into(), "B_KEY".into()],
                catalog: vec![],
            },
        }],
        "anthropic/claude-fable-5",
    );
    assert_eq!(rows[1].hint.as_deref(), Some("set A_KEY or B_KEY"));
}

// --- pick (the pure pick policy) ----------------------------------------

#[test]
fn a_pick_is_the_scoped_row_value_itself() {
    assert_eq!(
        pick("local/old-model", "anthropic/claude-fable-5".into()),
        Some("anthropic/claude-fable-5".to_string())
    );
}

#[test]
fn re_selecting_the_current_model_is_no_pick() {
    assert_eq!(
        pick(
            "local/qwen/Qwen3.6-27B-MTP-GGUF",
            "local/qwen/Qwen3.6-27B-MTP-GGUF".into()
        ),
        None
    );
}
