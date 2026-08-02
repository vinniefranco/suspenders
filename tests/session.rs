use super::*;
use crate::tools::shaping;

// Reads the config at `path` and overlays its SCALARS onto `cfg` (ADR-0031). An
// absent file is an empty overlay (Ok, base defaults, no file touched); any IO
// or parse error becomes a [`SessionError`] naming `path`. The MCP fields are
// NOT landed here - the scope-aware [`SessionConfig::compose`] merges those - so
// this stays a single-scope scalar overlay, exercised only through the tests
// (production composes through [`SessionConfig::compose`]).
fn load_file_overlay(cfg: &mut SessionConfig, path: &str) -> Result<(), SessionError> {
    if let Some(file) = read_file_config(path)? {
        file.apply(cfg);
    }
    Ok(())
}

// A valid explicit launch Model, so tests never depend on config for it.
fn test_model() -> Model {
    model_with_cap(1_000)
}

fn model_with_cap(max_tokens: u64) -> Model {
    Model::new(
        "local",
        "test-model",
        Api::AnthropicMessages,
        64_000,
        max_tokens,
    )
}

fn cfg() -> SessionConfig {
    SessionConfig::test_defaults()
}

// Sugar mirroring baud's `Session.new(root: "/tmp", ...)`.
fn opts() -> SessionOpts {
    SessionOpts {
        root: Some("/tmp".into()),
        ..Default::default()
    }
}

// ---- new/1 ----

#[test]
fn defaults_come_from_config() {
    let session = Session::build(opts(), &cfg()).unwrap();
    assert_eq!(session.llm_module, "Suspenders.FakeLLM");
    assert_eq!(session.context_budget, cfg().context_budget);
    assert_eq!(session.model.max_tokens, cfg().max_tokens);
    assert_eq!(session.extensions, Vec::<String>::new());
}

#[test]
fn the_launch_model_resolves_the_scoped_default_against_the_local_provider() {
    // Out-of-the-box behavior (ADR-0037): the default custom `local`
    // Provider carries today's default endpoint, and the default model is
    // scoped to it - splitting on the FIRST slash only.
    let session = Session::build(opts(), &cfg()).unwrap();
    assert_eq!(session.model.provider, "local");
    assert_eq!(session.model.id, "qwen/Qwen3.6-27B-MTP-GGUF");
    assert_eq!(session.model.api, Api::AnthropicMessages);
    assert_eq!(session.model.max_tokens, cfg().max_tokens);
    assert_eq!(session.temperature, cfg().temperature);

    let local = session.provider_of(&session.model).expect("local resolves");
    assert_eq!(local.base_url, "http://localhost:0/v1");
    assert_eq!(local.api, Api::AnthropicMessages);
    // The base config ships NO window for `local` (ADR-0037): the server
    // reports its real window at discovery and enrichment makes it
    // authoritative, so a shipped figure would only shadow it. The
    // sync-resolved Model still gets a window from the fallback figure.
    assert_eq!(local.context_window, None);
    assert_eq!(session.model.context_window, FALLBACK_WINDOW);
}

// ---- enrich_model_window (server window is authoritative, ADR-0037) ----

#[tokio::test]
async fn enrich_gives_a_custom_model_the_server_window_over_config() {
    // "Server wins, period": a custom Provider's Model takes the host's
    // reported window (meta.n_ctx), and the dependent output cap re-derives
    // against it.
    let session = Session::build(opts(), &cfg()).unwrap();
    let model = session.model.clone();
    let server_window = 145_664;
    let fake = crate::test_support::FakeLlm::script([]).with_models([Ok(vec![
        crate::llm::DiscoveredModel {
            id: model.id.clone(),
            context_window: Some(server_window),
        },
    ])]);

    let enriched = session.enrich_model_window(&fake, model).await;
    assert_eq!(enriched.context_window, server_window);
    assert_eq!(
        enriched.max_tokens,
        session.max_tokens.min(server_window / 2)
    );
}

#[tokio::test]
async fn enrich_keeps_the_resolved_window_when_the_host_reports_none() {
    // The host lists the Model but reports no n_ctx: the sync-resolved
    // window stands.
    let session = Session::build(opts(), &cfg()).unwrap();
    let model = session.model.clone();
    let resolved = model.context_window;
    let fake = crate::test_support::FakeLlm::script([]).with_models([Ok(vec![
        crate::llm::DiscoveredModel {
            id: model.id.clone(),
            context_window: None,
        },
    ])]);

    let enriched = session.enrich_model_window(&fake, model).await;
    assert_eq!(enriched.context_window, resolved);
}

#[tokio::test]
async fn enrich_keeps_the_resolved_window_when_discovery_fails() {
    // A down host is not fatal: a failed listing is data, not a panic (the
    // error algebra), so enrichment falls back to the resolved window.
    let session = Session::build(opts(), &cfg()).unwrap();
    let model = session.model.clone();
    let resolved = model.context_window;
    let fake = crate::test_support::FakeLlm::script([]).with_models([Err("host_down".to_string())]);

    let enriched = session.enrich_model_window(&fake, model).await;
    assert_eq!(enriched.context_window, resolved);
}

#[tokio::test]
async fn enrich_leaves_a_builtin_model_untouched() {
    // A Catalog-backed Model's window is authoritative; enrichment never
    // queries the host for it, so the scripted answer stays unconsumed.
    let session = Session::build(opts(), &cfg()).unwrap();
    let builtin = session.resolve_model("anthropic/claude-fable-5").unwrap();
    let cataloged = builtin.context_window;
    let fake = crate::test_support::FakeLlm::script([]).with_models([Ok(vec![
        crate::llm::DiscoveredModel {
            id: builtin.id.clone(),
            context_window: Some(1),
        },
    ])]);

    let enriched = session.enrich_model_window(&fake, builtin).await;
    assert_eq!(enriched.context_window, cataloged);
    assert_ne!(enriched.context_window, 1);
}

#[test]
fn the_provider_set_carries_customs_and_unshadowed_builtins() {
    let session = Session::build(opts(), &cfg()).unwrap();
    let ids: Vec<&str> = session.providers.iter().map(|p| p.id.as_str()).collect();
    assert!(ids.contains(&"local"));
    assert!(ids.contains(&"anthropic"));

    // A custom entry with a built-in's id shadows it (config wins).
    let mut config = cfg();
    config.providers.insert(
        "anthropic".to_string(),
        ProviderConfig {
            base_url: "http://proxy:9000/v1".into(),
            api: Api::AnthropicMessages,
            context_window: Some(100_000),
            token: Some("proxy-token".into()),
        },
    );
    let session = Session::build(opts(), &config).unwrap();
    let anthropic: Vec<_> = session
        .providers
        .iter()
        .filter(|p| p.id == "anthropic")
        .collect();
    assert_eq!(anthropic.len(), 1);
    assert_eq!(anthropic[0].base_url, "http://proxy:9000/v1");
    assert_eq!(anthropic[0].token, "proxy-token");
}

#[test]
fn an_unresolvable_launch_model_fails_launch_loudly() {
    // Unknown provider.
    let mut config = cfg();
    config.model = "nowhere/some-model".into();
    let err = Session::build(opts(), &config).unwrap_err();
    assert!(err.0.contains("nowhere"), "error was: {err}");

    // An unscoped id (no provider part) fails too.
    let mut config = cfg();
    config.model = "bare-model".into();
    let err = Session::build(opts(), &config).unwrap_err();
    assert!(err.0.contains("scoped"), "error was: {err}");
}

#[test]
fn resolve_model_synthesizes_from_the_session_knobs_for_unknown_models() {
    let session = Session::build(opts(), &cfg()).unwrap();
    let model = session.resolve_model("local/another-model").unwrap();
    assert_eq!(model.provider, "local");
    assert_eq!(model.id, "another-model");
    // The custom Provider's config window and the Session's output-cap knob.
    assert_eq!(model.context_window, 64_000);
    assert_eq!(model.max_tokens, session.max_tokens);

    assert!(session.resolve_model("nowhere/m").is_err());
}

#[test]
fn opts_override_config() {
    let o = SessionOpts {
        root: Some("/tmp".into()),
        llm_module: Some("SomeLLM".into()),
        extensions: Some(vec!["some_extension".into()]),
        context_budget: Some(5_000),
        compaction_slack: Some(0.1),
        compaction_keep: Some(0.4),
        run_limit: Some(3),
        command_timeout_ms: Some(1_000),
        model: Some(test_model()),
        ..Default::default()
    };
    let session = Session::build(o, &cfg()).unwrap();
    assert_eq!(session.llm_module, "SomeLLM");
    assert_eq!(session.extensions, vec!["some_extension".to_string()]);
    assert_eq!(session.context_budget, Some(5_000));
    assert_eq!(session.compaction_slack, 0.1);
    assert_eq!(session.compaction_keep, 0.4);
    assert_eq!(session.run_limit, 3);
    assert_eq!(session.command_timeout_ms, 1_000);
    assert_eq!(session.model.max_tokens, 1_000);
}

#[test]
fn compaction_keep_defaults_from_config() {
    let session = Session::build(opts(), &cfg()).unwrap();
    assert_eq!(session.compaction_keep, cfg().compaction_keep);
}

// ---- loop_stall_limit (the loop-detector knob) ----

#[test]
fn run_limit_defaults_generous_and_loop_stall_limit_defaults_small() {
    // The turn cap is sized for a real multi-step task under the ReAct loop
    // (qwen's ~100 session-turn ceiling); the loop-detector catches a stuck
    // model far sooner.
    let session = Session::build(opts(), &cfg()).unwrap();
    assert_eq!(session.run_limit, 100);
    assert_eq!(session.loop_stall_limit, 5);
}

#[test]
fn loop_stall_limit_opts_override_and_must_be_positive() {
    let session = Session::build(
        SessionOpts {
            loop_stall_limit: Some(3),
            model: Some(test_model()),
            ..opts()
        },
        &cfg(),
    )
    .unwrap();
    assert_eq!(session.loop_stall_limit, 3);

    let err = Session::build(
        SessionOpts {
            loop_stall_limit: Some(0),
            model: Some(test_model()),
            ..opts()
        },
        &cfg(),
    )
    .unwrap_err();
    assert!(err.0.contains(":loop_stall_limit"));
}

#[test]
fn env_loop_stall_limit_positive_integer() {
    assert_eq!(parse_loop_stall_limit("4").unwrap(), 4);
    assert_eq!(parse_loop_stall_limit(" 5 ").unwrap(), 5);
    assert!(
        parse_loop_stall_limit("0")
            .unwrap_err()
            .0
            .contains("SUSPENDERS_LOOP_STALL_LIMIT must be a positive integer")
    );
    assert!(parse_loop_stall_limit("nope").is_err());
}

#[test]
fn file_loop_stall_limit_overlays_onto_base() {
    let mut cfg = SessionConfig::test_defaults();
    FileConfig::parse(r#"{"loop_stall_limit": 7}"#)
        .unwrap()
        .apply(&mut cfg);
    assert_eq!(cfg.loop_stall_limit, 7);
    let session = Session::build(opts(), &cfg).unwrap();
    assert_eq!(session.loop_stall_limit, 7);
}

// ---- thinking_budget (the extended-thinking knob, qwen-code parity) ----

#[test]
fn thinking_budget_defaults_to_qwen_codes_32000() {
    assert_eq!(SessionConfig::base().thinking_budget, Some(32_000));
    let session = Session::build(opts(), &cfg()).unwrap();
    assert_eq!(session.thinking_budget, Some(32_000));
}

#[test]
fn env_thinking_budget_parses_and_disables_on_zero_or_empty() {
    assert_eq!(parse_thinking_budget("32000").unwrap(), Some(32_000));
    assert_eq!(parse_thinking_budget(" 16000 ").unwrap(), Some(16_000));
    // The disable convention: 0 or empty turns extended thinking off.
    assert_eq!(parse_thinking_budget("0").unwrap(), None);
    assert_eq!(parse_thinking_budget("").unwrap(), None);
    assert_eq!(parse_thinking_budget("   ").unwrap(), None);
    assert!(
        parse_thinking_budget("nope")
            .unwrap_err()
            .0
            .contains("SUSPENDERS_THINKING_BUDGET must be a non-negative integer")
    );
}

#[test]
fn file_thinking_budget_overlays_onto_base() {
    let mut cfg = SessionConfig::test_defaults();
    FileConfig::parse(r#"{"thinking_budget": 8000}"#)
        .unwrap()
        .apply(&mut cfg);
    assert_eq!(cfg.thinking_budget, Some(8_000));
    let session = Session::build(opts(), &cfg).unwrap();
    assert_eq!(session.thinking_budget, Some(8_000));
}

#[test]
fn thinking_budget_opts_override_config_and_can_disable() {
    let session = Session::build(
        SessionOpts {
            thinking_budget: Some(None),
            model: Some(test_model()),
            ..opts()
        },
        &cfg(),
    )
    .unwrap();
    assert_eq!(session.thinking_budget, None);
}

// ---- skip_next_speaker (the next-speaker-check knob, ADR-0043) ----

#[test]
fn skip_next_speaker_defaults_on_in_base_and_test_defaults() {
    // Both ship with the check skipped, matching qwen-code's
    // `skipNextSpeakerCheck` default (ADR-0043): a no-tool-call Pass finishes
    // the Run without a side-query. Tests that want the check opt back in
    // with `skip_next_speaker: Some(false)`.
    assert!(SessionConfig::base().skip_next_speaker);
    assert!(SessionConfig::test_defaults().skip_next_speaker);
}

// The silent-regression guard for the todo-render defect (ADR-0048): the
// shipped default MUST enlist the `todo` extension, else `todo_write` dumps
// raw JSON (the Presenter never runs). `base()` IS the shipped default (there
// is no `Default` impl; the app builds from `base()` overlaid by config).
#[test]
fn the_shipped_default_config_enlists_the_todo_extension() {
    assert!(
        SessionConfig::base()
            .extensions
            .contains(&"todo".to_string()),
        "the shipped default must register the todo extension (risk #5)"
    );
}

#[test]
fn skip_next_speaker_opts_override_config() {
    let session = Session::build(
        SessionOpts {
            skip_next_speaker: Some(false),
            model: Some(test_model()),
            ..opts()
        },
        &cfg(),
    )
    .unwrap();
    assert!(!session.skip_next_speaker);
}

#[test]
fn env_skip_next_speaker_parses_bool_forms() {
    assert!(parse_bool("true", "SUSPENDERS_SKIP_NEXT_SPEAKER").unwrap());
    assert!(parse_bool(" 1 ", "SUSPENDERS_SKIP_NEXT_SPEAKER").unwrap());
    assert!(!parse_bool("false", "SUSPENDERS_SKIP_NEXT_SPEAKER").unwrap());
    assert!(!parse_bool("0", "SUSPENDERS_SKIP_NEXT_SPEAKER").unwrap());
    assert!(
        parse_bool("nope", "SUSPENDERS_SKIP_NEXT_SPEAKER")
            .unwrap_err()
            .0
            .contains("SUSPENDERS_SKIP_NEXT_SPEAKER must be")
    );
}

#[test]
fn file_skip_next_speaker_overlays_onto_base() {
    let mut cfg = SessionConfig::test_defaults();
    FileConfig::parse(r#"{"skip_next_speaker": false}"#)
        .unwrap()
        .apply(&mut cfg);
    assert!(!cfg.skip_next_speaker);
    let session = Session::build(opts(), &cfg).unwrap();
    assert!(!session.skip_next_speaker);
}

// ---- the per-Model budget derivation (ADR-0037) ----

#[test]
fn the_context_budget_is_the_captured_models_window_capped_by_config() {
    // No cap: the Model's own window IS the budget.
    let session = Session::build(opts(), &cfg()).unwrap();
    assert_eq!(session.context_budget, None);
    assert_eq!(
        session.context_budget_for(&session.model),
        session.model.context_window
    );

    // A cap set: the effective budget is min(cap, window), per Model.
    let session = Session::build(
        SessionOpts {
            context_budget: Some(5_000),
            model: Some(test_model()),
            ..opts()
        },
        &cfg(),
    )
    .unwrap();
    assert_eq!(session.context_budget_for(&session.model), 5_000);
    let wide = Model::new("local", "wide", Api::AnthropicMessages, 1_000_000, 1_000);
    assert_eq!(session.context_budget_for(&wide), 5_000);
    let narrow = Model::new("local", "narrow", Api::AnthropicMessages, 3_000, 1_000);
    assert_eq!(session.context_budget_for(&narrow), 3_000);
}

#[test]
fn a_wide_window_catalog_model_validates_out_of_the_box() {
    // The Stage A sharp edge, fixed (ADR-0037): the launch validation runs
    // against the resolved Model's OWN figures, so the 1M-window /
    // 128K-output fable needs no config surgery.
    let mut config = cfg();
    config.model = "anthropic/claude-fable-5".into();
    let session = Session::build(opts(), &config).unwrap();
    assert_eq!(session.context_budget_for(&session.model), 1_000_000);
    assert_eq!(session.model.max_tokens, 128_000);
}

#[test]
fn reply_reserve_clamps_the_output_cap_to_half_the_budget() {
    // The reply reserve halves a wire cap that equals the budget so a live
    // window survives (ADR-0037). (The wire cap itself is halved earlier, at
    // model resolution; this is the second, budget-side clamp.)
    let session = Session::build(opts(), &cfg()).unwrap();
    let full = model_with_cap(64_000); // cap == the 64K window
    assert_eq!(session.reply_reserve_for(&full), 32_000);

    // A modest cap is left untouched - only degenerate caps clamp.
    let modest = model_with_cap(1_000);
    assert_eq!(session.reply_reserve_for(&modest), 1_000);
}

#[test]
fn validate_model_budget_accepts_a_cap_that_matches_the_window() {
    // An output cap that matches the window leaves no room past it, yet the
    // clamped reserve (reply_reserve_for) keeps a live window, so the pick
    // validates and lands as ordinary budget pressure.
    let session = Session::build(opts(), &cfg()).unwrap();
    let picked = model_with_cap(64_000); // cap == the 64K window
    assert_eq!(session.validate_model_budget(&picked), Ok(()));

    // The launch Model passes the same check.
    assert_eq!(session.validate_model_budget(&session.model), Ok(()));
}

#[test]
fn the_result_cap_derives_from_the_captured_models_figures() {
    let session = Session::build(
        SessionOpts {
            context_budget: Some(5_000),
            model: Some(test_model()),
            ..opts()
        },
        &cfg(),
    )
    .unwrap();
    let ctx = session.tool_ctx(&session.model, crate::tool::caps::Capabilities::for_test());
    assert_eq!(
        ctx.result_cap,
        shaping::cap_for(5_000, session.model.max_tokens)
    );
}

#[test]
fn out_of_range_values_raise() {
    let with = |o: SessionOpts| Session::build(o, &cfg());

    assert!(
        with(SessionOpts {
            context_budget: Some(0),
            model: Some(test_model()),
            ..opts()
        })
        .unwrap_err()
        .0
        .contains(":context_budget")
    );

    assert!(
        with(SessionOpts {
            compaction_slack: Some(1.0),
            model: Some(test_model()),
            ..opts()
        })
        .unwrap_err()
        .0
        .contains(":compaction_slack")
    );

    assert!(
        with(SessionOpts {
            run_limit: Some(0),
            model: Some(test_model()),
            ..opts()
        })
        .unwrap_err()
        .0
        .contains(":turn_limit")
    );

    assert!(
        with(SessionOpts {
            command_timeout_ms: Some(0),
            model: Some(test_model()),
            ..opts()
        })
        .unwrap_err()
        .0
        .contains(":command_timeout_ms")
    );

    assert!(
        with(SessionOpts {
            model: Some(model_with_cap(0)),
            ..opts()
        })
        .unwrap_err()
        .0
        .contains("max_tokens")
    );
}

#[test]
fn compaction_keep_must_be_strictly_inside_open_interval() {
    let with_keep = |k: f64| {
        Session::build(
            SessionOpts {
                compaction_keep: Some(k),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
    };
    assert!(with_keep(0.0).unwrap_err().0.contains(":compaction_keep"));
    assert!(with_keep(1.0).unwrap_err().0.contains(":compaction_keep"));
    // baud's `2` (an integer) case: any value >= 1.0 fails the same way.
    assert!(with_keep(2.0).unwrap_err().0.contains(":compaction_keep"));
}

#[test]
fn compaction_keep_amount_must_sit_below_trigger() {
    // live window = 10_000 - 1_000 = 9_000. trigger = 9_000 - 0.1*10_000 =
    // 8_000. 0.95 * 9_000 = 8_550 >= 8_000, so it must raise.
    let err = Session::build(
        SessionOpts {
            context_budget: Some(10_000),
            compaction_slack: Some(0.1),
            compaction_keep: Some(0.95),
            model: Some(model_with_cap(1_000)),
            ..opts()
        },
        &cfg(),
    )
    .unwrap_err();
    assert!(
        err.0.contains("Compaction Keep") || err.0.contains("below") || err.0.contains("fire high")
    );

    let session = Session::build(
        SessionOpts {
            context_budget: Some(10_000),
            compaction_slack: Some(0.1),
            compaction_keep: Some(0.5),
            model: Some(model_with_cap(1_000)),
            ..opts()
        },
        &cfg(),
    )
    .unwrap();
    assert_eq!(session.compaction_keep, 0.5);
}

#[test]
fn tool_call_style_defaults_to_auto_and_opts_override() {
    let session = Session::build(opts(), &cfg()).unwrap();
    assert_eq!(session.tool_call_style, ToolCallStyle::Auto);

    let session = Session::build(
        SessionOpts {
            tool_call_style: Some(ToolCallStyle::Structured),
            model: Some(test_model()),
            ..opts()
        },
        &cfg(),
    )
    .unwrap();
    assert_eq!(session.tool_call_style, ToolCallStyle::Structured);
}

#[test]
fn env_tool_call_style_names_the_three_arms_only() {
    assert_eq!(parse_tool_call_style("auto").unwrap(), ToolCallStyle::Auto);
    assert_eq!(
        parse_tool_call_style(" structured ").unwrap(),
        ToolCallStyle::Structured
    );
    assert_eq!(parse_tool_call_style("text").unwrap(), ToolCallStyle::Text);
    assert_eq!(
        parse_tool_call_style("nope").unwrap_err().0,
        "SUSPENDERS_TOOL_CALL_STYLE must be \"auto\", \"structured\", or \"text\", got: \"nope\""
    );
}

// ---- malformed_retry_budget ----

#[test]
fn malformed_retry_budget_defaults_to_3_and_opts_override_including_the_off_value() {
    let session = Session::build(opts(), &cfg()).unwrap();
    assert_eq!(session.malformed_retry_budget, 3);

    let with_budget = |n| {
        build_session(|o| SessionOpts {
            malformed_retry_budget: Some(n),
            ..o
        })
    };
    assert_eq!(with_budget(5).malformed_retry_budget, 5);
    // 0 is valid: it disables the in-band re-draw entirely.
    assert_eq!(with_budget(0).malformed_retry_budget, 0);
}

#[test]
fn env_malformed_retry_budget_is_a_non_negative_integer() {
    assert_eq!(
        parse_int("0", "SUSPENDERS_MALFORMED_RETRY_BUDGET").unwrap(),
        0
    );
    assert_eq!(
        parse_int("3", "SUSPENDERS_MALFORMED_RETRY_BUDGET").unwrap(),
        3
    );
    assert!(
        parse_int("-1", "SUSPENDERS_MALFORMED_RETRY_BUDGET")
            .unwrap_err()
            .0
            .contains("SUSPENDERS_MALFORMED_RETRY_BUDGET must be an integer")
    );
}

// ---- SUSPENDERS_* env parsing/validation ----

#[test]
fn env_positive_int_parses_and_rejects() {
    assert_eq!(parse_positive_int("8000").unwrap(), 8_000);
    assert_eq!(
        parse_positive_int("0").unwrap_err().0,
        "SUSPENDERS_MAX_TOKENS must be a positive integer, got: \"0\""
    );
    assert!(
        parse_positive_int("-5")
            .unwrap_err()
            .0
            .contains("must be a positive integer")
    );
    assert!(
        parse_positive_int("nope")
            .unwrap_err()
            .0
            .contains("must be a positive integer")
    );
}

#[test]
fn env_temperature_bounds() {
    assert_eq!(parse_temperature("0.0").unwrap(), 0.0);
    assert_eq!(parse_temperature("2.0").unwrap(), 2.0);
    assert!(
        parse_temperature("2.1")
            .unwrap_err()
            .0
            .contains("SUSPENDERS_TEMPERATURE must be a float in [0.0, 2.0]")
    );
    assert!(
        parse_temperature("-0.1")
            .unwrap_err()
            .0
            .contains("[0.0, 2.0]")
    );
    assert!(
        parse_temperature("hot")
            .unwrap_err()
            .0
            .contains("[0.0, 2.0]")
    );
}

#[test]
fn env_compaction_slack_left_closed() {
    assert_eq!(parse_compaction_slack("0.0").unwrap(), 0.0);
    assert!(
        parse_compaction_slack("1.0")
            .unwrap_err()
            .0
            .contains("SUSPENDERS_COMPACTION_SLACK must be a fraction in [0.0, 1.0)")
    );
}

#[test]
fn env_compaction_keep_open_interval() {
    assert_eq!(parse_compaction_keep("0.5").unwrap(), 0.5);
    assert!(
        parse_compaction_keep("0.0")
            .unwrap_err()
            .0
            .contains("SUSPENDERS_COMPACTION_KEEP must be a fraction in (0.0, 1.0)")
    );
    assert!(
        parse_compaction_keep("1.0")
            .unwrap_err()
            .0
            .contains("(0.0, 1.0)")
    );
}

#[test]
fn env_context_budget_integer() {
    assert_eq!(
        parse_int("64000", "SUSPENDERS_CONTEXT_BUDGET").unwrap(),
        64_000
    );
    assert!(
        parse_int("x", "SUSPENDERS_CONTEXT_BUDGET")
            .unwrap_err()
            .0
            .contains("SUSPENDERS_CONTEXT_BUDGET must be an integer")
    );
}

// ---- tool_ctx/1 ----

#[test]
fn tool_ctx_carries_root_result_cap_and_timeout() {
    let session = Session::build(
        SessionOpts {
            command_timeout_ms: Some(1_234),
            model: Some(test_model()),
            ..opts()
        },
        &cfg(),
    )
    .unwrap();
    let ctx = session.tool_ctx(&session.model, crate::tool::caps::Capabilities::for_test());
    assert_eq!(ctx.root, std::path::PathBuf::from("/tmp"));
    assert_eq!(
        ctx.result_cap,
        shaping::cap_for(
            session.context_budget_for(&session.model),
            session.model.max_tokens
        )
    );
    assert_eq!(ctx.command_timeout_ms, 1_234);
}

// ---- FileConfig (ADR-0031: user config file) ----

#[test]
fn file_config_parse_accepts_a_sparse_subset() {
    let fc = FileConfig::parse(r#"{"model": "custom/model", "max_tokens": 4096}"#).unwrap();
    assert_eq!(fc.model.as_deref(), Some("custom/model"));
    assert_eq!(fc.max_tokens, Some(4096));
    // Absent keys stay None.
    assert_eq!(fc.providers, None);
    assert_eq!(fc.temperature, None);
}

#[test]
fn file_config_parse_rejects_an_unknown_key() {
    // deny_unknown_fields: a misspelled/excluded key is a hard error. The
    // message is path-agnostic (the reader wraps it with the path) but still
    // names the offending key.
    let err = FileConfig::parse(r#"{"max_token": 4096}"#).unwrap_err();
    assert!(err.0.contains("max_token"));

    // An excluded field (never in the DTO) is rejected the same way.
    assert!(FileConfig::parse(r#"{"turn_limit": 10}"#).is_err());

    // The retired flat keys (ADR-0037: base_url and token moved into the
    // providers table) are rejected, not silently honored.
    assert!(FileConfig::parse(r#"{"base_url": "http://x/v1"}"#).is_err());
    assert!(FileConfig::parse(r#"{"token": "sekrit"}"#).is_err());
}

#[test]
fn file_config_parses_a_providers_table() {
    let fc = FileConfig::parse(
        r#"{"providers": {"lmstudio": {
                "base_url": "http://localhost:1234/v1",
                "api": "openai-completions",
                "context_window": 32768
            }}}"#,
    )
    .unwrap();
    let providers = fc.providers.clone().unwrap();
    let lmstudio = &providers["lmstudio"];
    assert_eq!(lmstudio.base_url, "http://localhost:1234/v1");
    assert_eq!(lmstudio.api, Api::OpenaiCompletions);
    assert_eq!(lmstudio.context_window, Some(32_768));
    assert_eq!(lmstudio.token, None);

    // The window is optional (ADR-0037): an entry without one leaves its
    // Models to the global `context_budget` figure.
    let fc = FileConfig::parse(
        r#"{"providers": {"lmstudio": {
                "base_url": "http://localhost:1234/v1",
                "api": "openai-completions"
            }}}"#,
    )
    .unwrap();
    assert_eq!(fc.providers.unwrap()["lmstudio"].context_window, None);

    // A provider entry is deny_unknown_fields too.
    assert!(
        FileConfig::parse(
            r#"{"providers": {"x": {
                    "base_url": "http://x/v1",
                    "api": "anthropic-messages",
                    "context_window": 1000,
                    "endpoint": "nope"
                }}}"#,
        )
        .is_err()
    );
}

#[test]
fn file_config_parses_an_mcp_servers_block_stdio_and_http() {
    // A stdio entry and an HTTP entry round-trip through FileConfig, keyed
    // by the snake_case `mcp_servers` key (F8, ADR-0056).
    let fc = FileConfig::parse(
        r#"{"mcp_servers": {
                "fs": {
                    "command": "mcp-fs",
                    "args": ["--root", "/tmp"],
                    "env": {"LOG": "debug"},
                    "exclude_tools": ["delete"]
                },
                "remote": {
                    "http_url": "https://mcp.example.test/mcp",
                    "headers": {"Authorization": "Bearer x"},
                    "trust": true
                }
            }}"#,
    )
    .unwrap();
    let servers = fc.mcp_servers.clone().unwrap();

    let fs = &servers["fs"];
    assert_eq!(fs.exclude_tools, vec!["delete".to_string()]);
    // The flat `command`/`args`/`env` keys fold into the stdio sum-type at
    // parse time - the both/neither illegal states are unrepresentable here.
    assert!(matches!(
        &fs.transport,
        crate::mcp::McpTransport::Stdio { command, args, env, .. }
            if command == "mcp-fs"
                && *args == vec!["--root".to_string(), "/tmp".to_string()]
                && env["LOG"] == "debug"
    ));

    let remote = &servers["remote"];
    assert_eq!(remote.trust, Some(true));
    assert!(matches!(
        &remote.transport,
        crate::mcp::McpTransport::Http { url, headers }
            if url == "https://mcp.example.test/mcp"
                && headers["Authorization"] == "Bearer x"
    ));
}

#[test]
fn file_config_mcp_server_entry_rejects_an_unknown_key() {
    // Each server entry is deny_unknown_fields too - a typo'd key is a loud
    // parse error (ADR-0056).
    assert!(
        FileConfig::parse(r#"{"mcp_servers": {"x": {"command": "cmd", "bogus": 1}}}"#,).is_err()
    );
}

#[test]
fn file_config_rejects_a_malformed_mcp_server_transport() {
    // A malformed entry (more than one transport key) is a LOUD PARSE failure now
    // that the transport is a sum type resolved at deserialize time - the illegal
    // state is unrepresentable, so it never reaches build (ADR-0056). This replaces
    // the old build-time transport-validation pass.
    let err = FileConfig::parse(
        r#"{"mcp_servers": {"broken": {"command": "cmd", "http_url": "https://x.test"}}}"#,
    )
    .unwrap_err();
    assert!(err.0.contains("more than one"));
}

#[test]
fn file_config_parse_rejects_a_wrong_typed_value() {
    assert!(FileConfig::parse(r#"{"max_tokens": "lots"}"#).is_err());
}

#[test]
fn file_config_apply_overlays_only_present_fields() {
    let mut cfg = SessionConfig::test_defaults();
    let before_budget = cfg.context_budget;
    let fc = FileConfig {
        model: Some("overlaid/model".into()),
        loop_stall_limit: Some(11),
        ..Default::default()
    };
    fc.apply(&mut cfg);
    assert_eq!(cfg.model, "overlaid/model");
    assert_eq!(cfg.loop_stall_limit, 11);
    // Absent fields untouched.
    assert_eq!(cfg.context_budget, before_budget);
}

#[test]
fn out_of_range_file_value_surfaces_via_the_build_path() {
    // Range errors are NOT caught by parse(); they surface at validate().
    let mut cfg = SessionConfig::test_defaults();
    FileConfig::parse(r#"{"compaction_slack": 1.0}"#)
        .unwrap()
        .apply(&mut cfg);
    let err = Session::build(opts(), &cfg).unwrap_err();
    assert!(err.0.contains(":compaction_slack"));
}

#[test]
fn write_template_omits_token_and_refuses_existing_without_force() {
    let path = std::env::temp_dir()
        .join(format!(
            "suspenders_write_config_{}.json",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);

    SessionConfig::write_template(&path, false).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    // token is never persisted (the "token" substring in "max_tokens" is
    // fine; the standalone key must be absent).
    assert!(!written.contains("\"token\""));
    // The template is full: it parses and round-trips a known key.
    let fc = FileConfig::parse(&written).unwrap();
    assert_eq!(
        fc.model.as_deref(),
        Some(SessionConfig::base().model.as_str())
    );
    assert_eq!(
        fc.loop_stall_limit,
        Some(SessionConfig::base().loop_stall_limit)
    );
    // The providers table rides the template, tokenless.
    let providers = fc.providers.clone().unwrap();
    assert!(providers["local"].token.is_none());

    // Refuses an existing target without force.
    let err = SessionConfig::write_template(&path, false).unwrap_err();
    assert!(err.0.contains(&path));
    // force overwrites.
    SessionConfig::write_template(&path, true).unwrap();

    let _ = std::fs::remove_file(&path);
}

// Builds a Session from a partial opts closure, always injecting the
// shared test root and a valid model so callers need not repeat those.
fn build_session(f: impl FnOnce(SessionOpts) -> SessionOpts) -> Session {
    let base = SessionOpts {
        model: Some(test_model()),
        ..opts()
    };
    Session::build(f(base), &cfg()).unwrap()
}

// A temp path namespaced by PID + a caller label, so parallel tests never
// collide on the filesystem seam.
fn temp_config_path(label: &str) -> String {
    std::env::temp_dir()
        .join(format!(
            "suspenders_cfg_{}_{}.json",
            label,
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

#[test]
fn load_file_overlay_applies_a_value_onto_base() {
    let path = temp_config_path("overlay_applies");
    std::fs::write(&path, r#"{"model": "from/file", "context_budget": 12345}"#).unwrap();

    let mut cfg = SessionConfig::test_defaults();
    load_file_overlay(&mut cfg, &path).unwrap();
    assert_eq!(cfg.model, "from/file");
    assert_eq!(cfg.context_budget, Some(12345));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_file_overlay_missing_file_is_ok_and_leaves_config_unchanged() {
    // Proves "absent file = defaults, no file touched" (ADR-0031).
    let path = temp_config_path("missing");
    let _ = std::fs::remove_file(&path);

    let mut cfg = SessionConfig::test_defaults();
    let before = cfg.clone();
    load_file_overlay(&mut cfg, &path).unwrap();
    assert_eq!(cfg, before);
    assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn write_template_round_trips_every_non_token_field_as_some() {
    // Lockstep guard (ADR-0031): the writer emits every schema key, and the
    // DTO parses them all back. A field the writer forgets - or a serde
    // rename that drifts - trips this. `token` is the sole intended None.
    let path = temp_config_path("round_trip");
    SessionConfig::write_template(&path, true).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    let fc = FileConfig::parse(&raw).unwrap();

    assert!(fc.providers.is_some());
    assert!(fc.model.is_some());
    assert!(fc.theme.is_some());
    assert!(fc.max_tokens.is_some());
    assert!(fc.temperature.is_some());
    assert!(fc.tool_call_style.is_some());
    // The one deliberate absence besides token (ADR-0037): the base config
    // carries no global budget cap, so the template writes none.
    assert!(fc.context_budget.is_none());
    assert!(fc.compaction_slack.is_some());
    assert!(fc.compaction_keep.is_some());
    assert!(fc.loop_stall_limit.is_some());
    assert!(fc.malformed_retry_budget.is_some());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn resolve_template_path_defaults_an_empty_path_to_the_xdg_config_path() {
    assert_eq!(
        SessionConfig::resolve_template_path(""),
        default_config_path()
    );
    // A non-empty path is used verbatim.
    assert_eq!(
        SessionConfig::resolve_template_path("/tmp/custom.json"),
        "/tmp/custom.json"
    );
}

// ---- apply_env (ADR-0031: the SUSPENDERS_* overlay) ----------------------
//
// These tests mutate this process's environment, which edition 2024 marks
// unsafe (`set_var`/`remove_var` can race a concurrent `getenv`). Safety is
// the runner's execution model: cargo-nextest runs each test in its own
// process, and these tests spawn no threads, so nothing reads the
// environment concurrently.

fn set_env(name: &str, value: &str) {
    // SAFETY: process-per-test (see the section comment above).
    unsafe { std::env::set_var(name, value) };
}

// Clears every SUSPENDERS_* override by walking ENV_OVERRIDES (a new table
// row is cleared for free), so each env test starts ambient-free.
fn clear_suspenders_env() {
    for (name, _) in ENV_OVERRIDES {
        // SAFETY: process-per-test (see the section comment above).
        unsafe { std::env::remove_var(name) };
    }
}

// Sets one env var (after clearing), applies it, and returns the error
// message. Used wherever a test probes one malformed value in isolation.
fn env_error(name: &str, value: &str) -> String {
    clear_suspenders_env();
    set_env(name, value);
    SessionConfig::apply_env(&mut SessionConfig::test_defaults())
        .unwrap_err()
        .0
}

// Asserts a persist path holds valid JSON, contains no standalone "token"
// key, and returns the parsed FileConfig for further assertions.
fn assert_no_token_and_parse(path: &str) -> FileConfig {
    let raw = std::fs::read_to_string(path).unwrap();
    assert!(!raw.contains("\"token\""));
    FileConfig::parse(&raw).unwrap()
}

#[test]
fn apply_env_overlays_the_scoped_model_onto_its_field() {
    clear_suspenders_env();
    set_env("SUSPENDERS_MODEL", "env/model");

    let mut cfg = SessionConfig::test_defaults();
    SessionConfig::apply_env(&mut cfg).unwrap();

    assert_eq!(cfg.model, "env/model");
}

#[test]
fn apply_env_overlays_each_numeric_var_onto_its_field() {
    clear_suspenders_env();
    set_env("SUSPENDERS_CONTEXT_BUDGET", "48000");
    set_env("SUSPENDERS_MAX_TOKENS", "2048");
    set_env("SUSPENDERS_TEMPERATURE", "1.5");
    set_env("SUSPENDERS_COMPACTION_SLACK", "0.25");
    set_env("SUSPENDERS_COMPACTION_KEEP", "0.4");
    // The 0-disables knob proves non-negative (not positive) parsing.
    set_env("SUSPENDERS_MALFORMED_RETRY_BUDGET", "0");

    let mut cfg = SessionConfig::test_defaults();
    SessionConfig::apply_env(&mut cfg).unwrap();

    assert_eq!(cfg.context_budget, Some(48_000));
    assert_eq!(cfg.max_tokens, 2048);
    assert_eq!(cfg.temperature, Some(1.5));
    assert_eq!(cfg.compaction_slack, 0.25);
    assert_eq!(cfg.compaction_keep, 0.4);
    assert_eq!(cfg.malformed_retry_budget, 0);
}

#[test]
fn apply_env_overlays_the_tool_call_style_onto_its_field() {
    clear_suspenders_env();
    // test_defaults has Auto, so landing Structured is visible.
    set_env("SUSPENDERS_TOOL_CALL_STYLE", "structured");
    let mut cfg = SessionConfig::test_defaults();
    SessionConfig::apply_env(&mut cfg).unwrap();
    assert_eq!(cfg.tool_call_style, ToolCallStyle::Structured);
}

#[test]
fn apply_env_rejects_an_unrecognized_tool_call_style() {
    clear_suspenders_env();
    set_env("SUSPENDERS_TOOL_CALL_STYLE", "nope");
    let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
    assert_eq!(
        err.0,
        "SUSPENDERS_TOOL_CALL_STYLE must be \"auto\", \"structured\", or \"text\", got: \"nope\""
    );
}

#[test]
fn apply_env_with_nothing_set_leaves_the_config_untouched() {
    clear_suspenders_env();
    let mut cfg = SessionConfig::test_defaults();
    let before = cfg.clone();
    SessionConfig::apply_env(&mut cfg).unwrap();
    assert_eq!(cfg, before);
}

#[test]
fn apply_env_rejects_a_malformed_integer() {
    clear_suspenders_env();
    set_env("SUSPENDERS_CONTEXT_BUDGET", "soon");
    let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
    assert_eq!(
        err.0,
        "SUSPENDERS_CONTEXT_BUDGET must be an integer, got: \"soon\""
    );
}

#[test]
fn apply_env_rejects_a_non_positive_integer() {
    assert_eq!(
        env_error("SUSPENDERS_MAX_TOKENS", "0"),
        "SUSPENDERS_MAX_TOKENS must be a positive integer, got: \"0\""
    );
}

#[test]
fn apply_env_rejects_an_out_of_range_temperature() {
    clear_suspenders_env();
    set_env("SUSPENDERS_TEMPERATURE", "2.5");
    let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
    assert_eq!(
        err.0,
        "SUSPENDERS_TEMPERATURE must be a float in [0.0, 2.0], got: \"2.5\""
    );
}

#[test]
fn apply_env_rejects_an_out_of_range_fraction() {
    // compaction_slack is half-open [0.0, 1.0): 1.0 falls outside.
    clear_suspenders_env();
    set_env("SUSPENDERS_COMPACTION_SLACK", "1.0");
    let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
    assert_eq!(
        err.0,
        "SUSPENDERS_COMPACTION_SLACK must be a fraction in [0.0, 1.0), got: \"1.0\""
    );

    // compaction_keep is open (0.0, 1.0): the endpoints fall outside.
    clear_suspenders_env();
    set_env("SUSPENDERS_COMPACTION_KEEP", "1.0");
    let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
    assert_eq!(
        err.0,
        "SUSPENDERS_COMPACTION_KEEP must be a fraction in (0.0, 1.0), got: \"1.0\""
    );
}

#[test]
fn apply_env_reports_the_first_malformed_value_in_table_order() {
    // Two malformed values: the error names the one whose row comes first
    // (CONTEXT_BUDGET precedes MALFORMED_RETRY_BUDGET in ENV_OVERRIDES).
    clear_suspenders_env();
    set_env("SUSPENDERS_CONTEXT_BUDGET", "nope");
    set_env("SUSPENDERS_MALFORMED_RETRY_BUDGET", "yes");
    let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
    assert!(err.0.contains("SUSPENDERS_CONTEXT_BUDGET"));
}

#[test]
fn apply_env_treats_a_set_but_empty_theme_as_unset() {
    // The XDG idiom, applied to THEME only: SUSPENDERS_THEME="" must not
    // become a theme named "" (a guaranteed per-launch fallback notice).
    clear_suspenders_env();
    set_env("SUSPENDERS_THEME", "");

    let mut cfg = SessionConfig::test_defaults();
    SessionConfig::apply_env(&mut cfg).unwrap();

    assert_eq!(cfg.theme, SessionConfig::test_defaults().theme);
}

// ---- persist_model (ADR-0033: sparse, sticky /model write) --------------

#[test]
fn persist_model_creates_the_file_when_absent() {
    // The sanctioned exception to no-auto-create: an explicit pick writes a
    // fresh `{"model": ...}` (ADR-0033 / ADR-0031 amendment).
    let path = temp_config_path("persist_creates");
    let _ = std::fs::remove_file(&path);
    assert!(!std::path::Path::new(&path).exists());

    SessionConfig::persist_model(&path, "picked/model").unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    let fc = FileConfig::parse(&raw).unwrap();
    assert_eq!(fc.model.as_deref(), Some("picked/model"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn persist_model_merges_preserving_another_key_and_never_adds_token() {
    // Sparse read-modify-write: only `model` changes; the user's other keys
    // survive and `token` is never introduced by the tool.
    let path = temp_config_path("persist_merges");
    std::fs::write(&path, r#"{"context_budget": 12345, "model": "old/model"}"#).unwrap();

    SessionConfig::persist_model(&path, "new/model").unwrap();

    // token is never persisted (the "token" substring in "max_tokens" is
    // fine; the standalone key must be absent). The result re-parses via
    // the DTO, with the merge applied and the pre-existing key preserved.
    let fc = assert_no_token_and_parse(&path);
    assert_eq!(fc.model.as_deref(), Some("new/model"));
    assert_eq!(fc.context_budget, Some(12345));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn persist_model_errors_on_a_malformed_existing_file() {
    let path = temp_config_path("persist_malformed");
    std::fs::write(&path, "{ not json").unwrap();

    let err = SessionConfig::persist_model(&path, "picked/model").unwrap_err();
    assert!(err.0.contains(&path));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn merge_key_starts_from_empty_when_absent() {
    // The pure seam: absent existing → a lone `model` object.
    let json = merge_json_key(
        None,
        "model",
        serde_json::Value::String("solo/model".into()),
    )
    .unwrap();
    let fc = FileConfig::parse(&json).unwrap();
    assert_eq!(fc.model.as_deref(), Some("solo/model"));
}

// ---- persist_theme (ADR-0038: the same sparse sticky write as /model) ----

#[test]
fn persist_theme_creates_the_file_when_absent() {
    // `/theme` shares `/model`'s sanctioned create-if-absent exception.
    let path = temp_config_path("persist_theme_creates");
    let _ = std::fs::remove_file(&path);

    SessionConfig::persist_theme(&path, "gruvbox").unwrap();

    let fc = FileConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(fc.theme.as_deref(), Some("gruvbox"));
    assert_eq!(fc.model, None, "nothing but the theme key is written");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn persist_theme_sets_only_the_theme_key_preserving_the_rest() {
    let path = temp_config_path("persist_theme_merges");
    std::fs::write(&path, r#"{"model": "kept/model", "theme": "light"}"#).unwrap();

    SessionConfig::persist_theme(&path, "gruvbox").unwrap();

    let fc = assert_no_token_and_parse(&path);
    assert_eq!(fc.theme.as_deref(), Some("gruvbox"));
    assert_eq!(
        fc.model.as_deref(),
        Some("kept/model"),
        "other keys survive"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn persist_key_replaces_via_a_same_dir_temp_file_and_cleans_it_up() {
    // The atomic shape: write-then-rename, so a crash mid-write can tear
    // only the temp file, never config.json. Observable from outside: the
    // write lands whole and no `.tmp` residue survives a clean persist.
    let path = temp_config_path("persist_atomic");
    std::fs::write(&path, r#"{"model": "kept/model"}"#).unwrap();

    SessionConfig::persist_theme(&path, "gruvbox").unwrap();

    let fc = FileConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(fc.theme.as_deref(), Some("gruvbox"));
    assert_eq!(fc.model.as_deref(), Some("kept/model"));
    assert!(
        !std::path::Path::new(&format!("{path}.tmp")).exists(),
        "the temp file was renamed away"
    );

    let _ = std::fs::remove_file(&path);
}

// ---- settings scopes + mcp.excluded (Phase B, ADR-0065) ------------------

#[test]
fn compose_merges_the_two_scopes_mcp_servers_workspace_shadowing_user() {
    // The user scope names `fs` + `shared`; the workspace scope names
    // `remote` + `shared`. The union has three servers, and the workspace's
    // `shared` shadows the user's - with its Source recorded as Workspace.
    let user = temp_config_path("compose_user");
    let workspace = temp_config_path("compose_workspace");
    std::fs::write(
        &user,
        r#"{"mcp_servers": {
                "fs": {"command": "mcp-fs"},
                "shared": {"command": "user-shared"}
            }}"#,
    )
    .unwrap();
    std::fs::write(
        &workspace,
        r#"{"mcp_servers": {
                "remote": {"http_url": "https://mcp.example.test/mcp"},
                "shared": {"command": "workspace-shared"}
            }}"#,
    )
    .unwrap();

    let cfg = SessionConfig::compose(&user, Some(&workspace)).unwrap();

    let names: Vec<&str> = cfg.mcp_servers.keys().map(String::as_str).collect();
    assert_eq!(names, vec!["fs", "remote", "shared"]);
    // The workspace `shared` won.
    assert!(matches!(
        &cfg.mcp_servers["shared"].transport,
        crate::mcp::McpTransport::Stdio { command, .. } if command == "workspace-shared"
    ));
    // Each server's Source is the scope that (last) declared it.
    assert_eq!(cfg.mcp_sources["fs"], crate::mcp::McpSource::User);
    assert_eq!(cfg.mcp_sources["remote"], crate::mcp::McpSource::Workspace);
    assert_eq!(cfg.mcp_sources["shared"], crate::mcp::McpSource::Workspace);

    let _ = std::fs::remove_file(&user);
    let _ = std::fs::remove_file(&workspace);
}

#[test]
fn compose_concatenates_mcp_excluded_across_scopes() {
    // The excluded list MERGES by concatenation (qwen MergeStrategy.CONCAT),
    // not replace: the user's `a` and the workspace's `b` both survive.
    let user = temp_config_path("compose_excl_user");
    let workspace = temp_config_path("compose_excl_workspace");
    std::fs::write(&user, r#"{"mcp_excluded": ["a"]}"#).unwrap();
    std::fs::write(&workspace, r#"{"mcp_excluded": ["b"]}"#).unwrap();

    let cfg = SessionConfig::compose(&user, Some(&workspace)).unwrap();
    assert_eq!(cfg.mcp_excluded, vec!["a".to_string(), "b".to_string()]);

    let _ = std::fs::remove_file(&user);
    let _ = std::fs::remove_file(&workspace);
}

#[test]
fn compose_treats_an_absent_scope_as_empty_and_a_malformed_one_as_an_error() {
    // An absent workspace file is a no-op overlay (the user scope stands);
    // a present-but-malformed one is an error naming its path.
    let user = temp_config_path("compose_absent_user");
    std::fs::write(&user, r#"{"model": "user/model"}"#).unwrap();

    let missing = temp_config_path("compose_missing_workspace");
    let _ = std::fs::remove_file(&missing);
    let cfg = SessionConfig::compose(&user, Some(&missing)).unwrap();
    assert_eq!(cfg.model, "user/model");

    let malformed = temp_config_path("compose_malformed_workspace");
    std::fs::write(&malformed, "{ not json").unwrap();
    let err = SessionConfig::compose(&user, Some(&malformed)).unwrap_err();
    assert!(err.0.contains(&malformed));

    let _ = std::fs::remove_file(&user);
    let _ = std::fs::remove_file(&malformed);
}

#[test]
fn mcp_plans_marks_a_disabled_server_and_stamps_its_source() {
    // A Session whose config names two servers, one of them excluded and
    // sourced from the workspace scope: the plan map carries the disabled
    // flag and the Source, and defaults an unrecorded Source to User.
    let mut config = cfg();
    config.mcp_servers.insert(
        "fs".to_string(),
        crate::mcp::McpServerConfig::new(crate::mcp::McpTransport::Stdio {
            command: "mcp-fs".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }),
    );
    config.mcp_servers.insert(
        "remote".to_string(),
        crate::mcp::McpServerConfig::new(crate::mcp::McpTransport::Stdio {
            command: "mcp-remote".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }),
    );
    config.mcp_excluded = vec!["remote".to_string()];
    config
        .mcp_sources
        .insert("remote".to_string(), crate::mcp::McpSource::Workspace);

    let session = Session::build(opts(), &config).unwrap();
    let plans = session.mcp_plans();

    // `fs` is enabled and, having no recorded Source, defaults to User.
    assert!(!plans["fs"].disabled);
    assert_eq!(plans["fs"].source, crate::mcp::McpSource::User);
    // `remote` is disabled and carries its recorded workspace Source.
    assert!(plans["remote"].disabled);
    assert_eq!(plans["remote"].source, crate::mcp::McpSource::Workspace);
}

#[test]
fn persist_excluded_round_trips_and_preserves_other_keys() {
    // The dialog's disable write: the `mcp_excluded` array lands sparsely,
    // the pre-existing key survives, and `compose` reads it back. The file is
    // created when absent.
    let path = temp_config_path("persist_excluded");
    let _ = std::fs::remove_file(&path);

    // Created when absent.
    SessionConfig::persist_excluded(&path, &["one".to_string()]).unwrap();
    let cfg = SessionConfig::compose(&path, None).unwrap();
    assert_eq!(cfg.mcp_excluded, vec!["one".to_string()]);

    // A pre-existing key survives a second, overwriting write.
    std::fs::write(&path, r#"{"model": "kept/model", "mcp_excluded": ["one"]}"#).unwrap();
    SessionConfig::persist_excluded(&path, &["one".to_string(), "two".to_string()]).unwrap();

    let fc = assert_no_token_and_parse(&path);
    assert_eq!(fc.model.as_deref(), Some("kept/model"));
    let cfg = SessionConfig::compose(&path, None).unwrap();
    assert_eq!(cfg.mcp_excluded, vec!["one".to_string(), "two".to_string()]);

    let _ = std::fs::remove_file(&path);
}

// ---- mcp_servers sparse persist (ADR-0065: the `mcp add/remove` CLI) ------

#[test]
fn merge_mcp_server_creates_the_map_when_absent() {
    // The pure seam: an absent existing file starts `{}`, gets an `mcp_servers`
    // object, and lands the one entry. A realistic Sse server locks the
    // three-transport round-trip through this layer (its flat `url` wire key
    // comes back through the DTO as an Sse transport).
    let server = crate::mcp::McpServerConfig::new(crate::mcp::McpTransport::Sse {
        url: "https://mcp.example.test/sse".into(),
        headers: [("authorization".to_string(), "Bearer tok".to_string())]
            .into_iter()
            .collect(),
    });
    let value = serde_json::to_value(&server).unwrap();

    let json = merge_mcp_server(None, "remote", value).unwrap();

    let fc = FileConfig::parse(&json).unwrap();
    let servers = fc.mcp_servers.clone().unwrap();
    assert!(matches!(
        &servers["remote"].transport,
        crate::mcp::McpTransport::Sse { url, headers }
            if url == "https://mcp.example.test/sse"
                && headers["authorization"] == "Bearer tok"
    ));
}

#[test]
fn merge_mcp_server_preserves_sibling_top_level_keys_and_sibling_servers() {
    // Sparse: a sibling top-level key (`model`) and a sibling server (`fs`)
    // both survive the insert of a second server.
    let existing = r#"{"model": "kept/model", "mcp_servers": {"fs": {"command": "mcp-fs"}}}"#;
    let server = crate::mcp::McpServerConfig::new(crate::mcp::McpTransport::Http {
        url: "https://mcp.example.test/mcp".into(),
        headers: std::collections::BTreeMap::new(),
    });
    let value = serde_json::to_value(&server).unwrap();

    let json = merge_mcp_server(Some(existing), "remote", value).unwrap();

    let fc = FileConfig::parse(&json).unwrap();
    assert_eq!(fc.model.as_deref(), Some("kept/model"));
    let servers = fc.mcp_servers.clone().unwrap();
    let names: Vec<&str> = servers.keys().map(String::as_str).collect();
    assert_eq!(names, vec!["fs", "remote"]);
}

#[test]
fn merge_mcp_server_overwrites_a_same_named_server() {
    // A re-add of an existing name replaces its entry (stdio -> http here),
    // leaving no stale transport keys behind.
    let existing = r#"{"mcp_servers": {"x": {"command": "old-cmd"}}}"#;
    let server = crate::mcp::McpServerConfig::new(crate::mcp::McpTransport::Http {
        url: "https://new.example.test/mcp".into(),
        headers: std::collections::BTreeMap::new(),
    });
    let value = serde_json::to_value(&server).unwrap();

    let json = merge_mcp_server(Some(existing), "x", value).unwrap();

    let fc = FileConfig::parse(&json).unwrap();
    let servers = fc.mcp_servers.clone().unwrap();
    assert!(matches!(
        &servers["x"].transport,
        crate::mcp::McpTransport::Http { url, .. } if url == "https://new.example.test/mcp"
    ));
}

#[test]
fn merge_mcp_server_errors_on_a_malformed_or_non_object_root() {
    // Malformed existing JSON is an Err (path-agnostic message).
    assert!(merge_mcp_server(Some("{ not json"), "x", serde_json::json!({})).is_err());
    // A non-object root is an Err.
    assert!(merge_mcp_server(Some("[]"), "x", serde_json::json!({})).is_err());
    // A non-object `mcp_servers` is an Err.
    assert!(merge_mcp_server(Some(r#"{"mcp_servers": 5}"#), "x", serde_json::json!({})).is_err());
}

#[test]
fn remove_mcp_server_key_reports_present_and_preserves_siblings() {
    // Removing a present name reports `true` and leaves the sibling server
    // (`fs`) and a sibling top-level key (`model`) intact.
    let existing = r#"{"model": "kept/model", "mcp_servers": {"fs": {"command": "mcp-fs"}, "gone": {"command": "bye"}}}"#;

    let (json, present) = remove_mcp_server_key(existing, "gone").unwrap();
    assert!(present);

    let fc = FileConfig::parse(&json).unwrap();
    assert_eq!(fc.model.as_deref(), Some("kept/model"));
    let servers = fc.mcp_servers.clone().unwrap();
    let names: Vec<&str> = servers.keys().map(String::as_str).collect();
    assert_eq!(names, vec!["fs"]);
}

#[test]
fn remove_mcp_server_key_reports_absent_name_and_absent_map() {
    // An unknown name in a present map reports `false`.
    let existing = r#"{"mcp_servers": {"fs": {"command": "mcp-fs"}}}"#;
    let (_, present) = remove_mcp_server_key(existing, "nope").unwrap();
    assert!(!present);

    // A wholly absent `mcp_servers` object also reports `false`, no error.
    let (_, present) = remove_mcp_server_key(r#"{"model": "m"}"#, "nope").unwrap();
    assert!(!present);

    // Malformed existing JSON is an Err.
    assert!(remove_mcp_server_key("{ not json", "x").is_err());
}

#[test]
fn persist_mcp_server_creates_the_file_and_round_trips_through_compose() {
    // The impure add: an absent file is created (the sanctioned exception),
    // the entry lands under `mcp_servers`, and `compose` reads it back with the
    // User source. A remove then reports it existed and empties the map.
    let path = temp_config_path("persist_mcp_server");
    let _ = std::fs::remove_file(&path);
    assert!(!std::path::Path::new(&path).exists());

    let server = crate::mcp::McpServerConfig::new(crate::mcp::McpTransport::Stdio {
        command: "mcp-fs".into(),
        args: vec!["--root".into(), "/tmp".into()],
        env: std::collections::BTreeMap::new(),
        cwd: None,
    });
    SessionConfig::persist_mcp_server(&path, "fs", &server).unwrap();

    let cfg = SessionConfig::compose(&path, None).unwrap();
    let listed: Vec<(&str, crate::mcp::McpSource)> = cfg
        .servers_with_source()
        .map(|(name, _cfg, source)| (name, source))
        .collect();
    assert_eq!(listed, vec![("fs", crate::mcp::McpSource::User)]);

    // Remove reports it existed; a second remove reports it did not.
    assert!(SessionConfig::remove_mcp_server(&path, "fs").unwrap());
    assert!(!SessionConfig::remove_mcp_server(&path, "fs").unwrap());
    let cfg = SessionConfig::compose(&path, None).unwrap();
    assert!(cfg.mcp_servers.is_empty());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn remove_mcp_server_on_an_absent_file_reports_false() {
    // No file, nothing to remove: `Ok(false)`, no file created.
    let path = temp_config_path("remove_mcp_absent");
    let _ = std::fs::remove_file(&path);

    assert!(!SessionConfig::remove_mcp_server(&path, "whatever").unwrap());
    assert!(!std::path::Path::new(&path).exists());
}

// ---- the theme key (ADR-0038: file + env, precedence like `model`) -------

#[test]
fn theme_defaults_to_dark_and_the_session_carries_it_unvalidated() {
    assert_eq!(cfg().theme, "dark");
    let session = Session::build(opts(), &cfg()).unwrap();
    assert_eq!(session.theme, "dark");

    // Any name rides through - resolution (and the dark fallback) is the
    // UI's launch concern, never a Session validation failure.
    let mut config = cfg();
    config.theme = "no-such-theme".into();
    let session = Session::build(opts(), &config).unwrap();
    assert_eq!(session.theme, "no-such-theme");
}

#[test]
fn file_config_theme_overlays_like_model() {
    let mut cfg = SessionConfig::test_defaults();
    FileConfig::parse(r#"{"theme": "solarized"}"#)
        .unwrap()
        .apply(&mut cfg);
    assert_eq!(cfg.theme, "solarized");
}

#[test]
fn apply_env_overlays_the_theme_onto_its_field() {
    clear_suspenders_env();
    set_env("SUSPENDERS_THEME", "gruvbox");

    let mut cfg = SessionConfig::test_defaults();
    SessionConfig::apply_env(&mut cfg).unwrap();

    assert_eq!(cfg.theme, "gruvbox");
}

#[test]
fn env_theme_shadows_a_file_theme() {
    // The same precedence as `model`: the file overlay lands first, the
    // env overlay wins per-invocation over it (ADR-0031/0038).
    clear_suspenders_env();
    set_env("SUSPENDERS_THEME", "from-env");

    let path = temp_config_path("theme_precedence");
    std::fs::write(&path, r#"{"theme": "from-file"}"#).unwrap();

    let mut cfg = SessionConfig::test_defaults();
    load_file_overlay(&mut cfg, &path).unwrap();
    assert_eq!(cfg.theme, "from-file");
    SessionConfig::apply_env(&mut cfg).unwrap();
    assert_eq!(cfg.theme, "from-env");

    let _ = std::fs::remove_file(&path);
}

// ---- default_memory_root (P5, ADR-0062) ----
//
// These tests mutate the process environment (SUSPENDERS_MEMORY_*), so they
// set-then-clear within one test body. Safe under both nextest
// (process-per-test) and `--test-threads=1` (serial, one process): each test
// clears the two vars before it reads them and after it is done, and spawns
// no threads, so nothing reads them concurrently.

fn clear_memory_env() {
    // SAFETY: process-per-test / serial single-threaded (see above).
    unsafe {
        std::env::remove_var("SUSPENDERS_MEMORY_LOCAL");
        std::env::remove_var("SUSPENDERS_MEMORY_BASE_DIR");
    }
}

#[test]
fn default_memory_root_slugs_the_project_under_a_projects_dir() {
    clear_memory_env();
    set_env("SUSPENDERS_MEMORY_BASE_DIR", "/tmp/mem-base");

    // A non-git tmp dir (no `.git` ancestor up to /tmp): the canonical root
    // falls back to the project root itself, and the slug replaces every
    // non-alphanumeric char with `-`.
    let proj = std::env::temp_dir().join("suspenders_mem_root_test_no_git");
    let _ = std::fs::create_dir_all(&proj);
    let proj = proj.to_string_lossy().into_owned();

    let root = default_memory_root(&proj);
    let expected_slug = sanitize_cwd(&proj);
    assert_eq!(
        root,
        format!("/tmp/mem-base/projects/{expected_slug}/memory")
    );
    // Every path separator and dot became a hyphen (qwen sanitizeCwd).
    assert!(!expected_slug.contains('/'));
    assert!(!expected_slug.contains('.'));

    let _ = std::fs::remove_dir_all(&proj);
    clear_memory_env();
}

#[test]
fn default_memory_root_local_override_places_it_in_root() {
    clear_memory_env();
    set_env("SUSPENDERS_MEMORY_LOCAL", "1");

    assert_eq!(
        default_memory_root("/some/project"),
        "/some/project/.suspenders/memory"
    );

    clear_memory_env();
}

#[test]
fn sanitize_cwd_replaces_every_non_alphanumeric_with_a_hyphen() {
    assert_eq!(
        sanitize_cwd("/home/vinnie/Proj_1.2"),
        "-home-vinnie-Proj-1-2"
    );
    assert_eq!(sanitize_cwd("abcXYZ123"), "abcXYZ123");
}

#[test]
fn canonical_git_root_walks_up_to_the_dot_git_bearing_ancestor() {
    // A tmp tree with a `.git` at the top and a nested subdir: the walk
    // finds the `.git`-bearing root, not the leaf.
    let top = std::env::temp_dir().join(format!(
        "suspenders_git_root_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let nested = top.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(top.join(".git")).unwrap();

    let found = canonical_git_root(&nested.to_string_lossy());
    assert_eq!(found, Some(top.to_string_lossy().into_owned()));

    let _ = std::fs::remove_dir_all(&top);
}
