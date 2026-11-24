use super::*;

fn env_with_required() -> EnvInputs {
    EnvInputs {
        upstream_base_url: Some("https://api.example.com".into()),
        upstream_api_key: Some("sk-test".into()),
        ..EnvInputs::default()
    }
}

/// A raw advisor limit, exactly as the JSON layer holds it: the number
/// the operator wrote, with NO sign, fraction or range check. The
/// RESOLVER is the only place that judges it.
fn limit(n: u64) -> Option<serde_json::Number> {
    Some(serde_json::Number::from(n))
}

/// A raw advisor limit written as a fraction (e.g. `2.5`).
fn fractional_limit(n: f64) -> Option<serde_json::Number> {
    Some(serde_json::Number::from_f64(n).expect("a finite fraction"))
}

#[test]
fn url_must_parse() {
    let env = EnvInputs {
        upstream_base_url: Some("not a url".into()),
        upstream_api_key: Some("sk-test".into()),
        ..EnvInputs::default()
    };
    let result = Config::resolve(None, &env);
    assert!(result.is_err(), "expected URL parse error");
}

#[test]
fn missing_required_env_yields_error() {
    let env = EnvInputs::default();
    let result = Config::resolve(None, &env);
    assert!(result.is_err());
}

#[test]
fn defaults_apply_when_only_required_set() {
    assert_eq!(DEFAULT_MAX_REQUEST_BODY_BYTES, 33_554_432);
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(cfg.max_request_body_bytes, DEFAULT_MAX_REQUEST_BODY_BYTES);
    assert_eq!(cfg.listen_addr.to_string(), DEFAULT_LISTEN_ADDR);
    assert_eq!(cfg.upstream_path, DEFAULT_UPSTREAM_PATH);
    assert_eq!(
        cfg.request_timeout,
        Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS)
    );
    assert_eq!(cfg.reasoning_effort.as_deref(), Some("none"));
}

#[test]
fn max_request_body_limit_resolves_file_then_env_then_default() {
    let file = JsonConfig {
        max_request_body_bytes: limit(12_345),
        ..JsonConfig::default()
    };
    let from_file = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(from_file.max_request_body_bytes, 12_345);

    let env = EnvInputs {
        max_request_body_bytes: limit(67_890),
        ..env_with_required()
    };
    let from_env = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(from_env.max_request_body_bytes, 67_890);
    assert!(from_env.warnings.is_empty());
}

#[test]
fn invalid_max_request_body_numeric_values_warn_and_fall_back() {
    // Zero, negative, fractional, and out-of-u64-range values all remain
    // parseable raw JSON numbers and are rejected by the resolver.
    for raw in ["0", "-1", "2.5", "18446744073709551616"] {
        let number: serde_json::Number = serde_json::from_str(raw).unwrap();
        let env = EnvInputs {
            max_request_body_bytes: Some(number),
            ..env_with_required()
        };
        let cfg = Config::resolve(None, &env).unwrap();
        assert_eq!(
            cfg.max_request_body_bytes, DEFAULT_MAX_REQUEST_BODY_BYTES,
            "raw={raw}"
        );
        assert_eq!(cfg.warnings.len(), 1, "raw={raw}");
        assert!(cfg.warnings[0].message.contains("max_request_body_bytes"));
        assert!(cfg.warnings[0].message.contains("using the default"));

        let raw_file = format!(r#"{{"max_request_body_bytes":{raw}}}"#);
        let file = JsonConfig::parse(&raw_file).expect("raw numeric file value parses");
        let from_file = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(
            from_file.max_request_body_bytes, DEFAULT_MAX_REQUEST_BODY_BYTES,
            "file raw={raw}"
        );
        assert_eq!(from_file.warnings.len(), 1, "file raw={raw}");
    }

    // An invalid environment number still overrides the file, and thus
    // falls back to the default rather than accepting the file's value.
    let file = JsonConfig {
        max_request_body_bytes: limit(8192),
        ..JsonConfig::default()
    };
    let env = EnvInputs {
        max_request_body_bytes: limit(0),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(cfg.max_request_body_bytes, DEFAULT_MAX_REQUEST_BODY_BYTES);
    assert_eq!(cfg.warnings.len(), 1);
}

#[test]
fn non_numeric_body_limit_env_value_is_ignored() {
    let env = EnvInputs {
        max_request_body_bytes: parse_env_limit("not-a-number"),
        ..env_with_required()
    };
    assert!(env.max_request_body_bytes.is_none());
    // The capture convention means non-numeric env text contributes no
    // override, so normal file/default resolution applies.
    let file = JsonConfig {
        max_request_body_bytes: limit(8192),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(cfg.max_request_body_bytes, 8192);
}

#[test]
fn max_request_body_limit_field_is_optional_and_round_trips_for_tui() {
    let saved_default = serde_json::to_value(JsonConfig::default()).unwrap();
    assert!(saved_default.get("max_request_body_bytes").is_none());

    let parsed = JsonConfig::parse(r#"{"max_request_body_bytes":12345}"#).unwrap();
    let saved = serde_json::to_value(&parsed).unwrap();
    assert_eq!(saved["max_request_body_bytes"], 12345);

    // Invalid raw values must also survive the TUI's whole-file rewrite
    // verbatim, even though the resolver will warn and use the default.
    for raw in ["0", "-1", "2.5"] {
        let text = format!(r#"{{"max_request_body_bytes":{raw}}}"#);
        let parsed = JsonConfig::parse(&text).expect("raw numeric value parses");
        let saved = serde_json::to_string(&parsed).unwrap();
        assert!(saved.contains(raw), "raw value {raw} was changed: {saved}");
        assert_eq!(JsonConfig::parse(&saved).unwrap(), parsed);
    }
}

#[test]
fn default_reasoning_effort_is_none() {
    // Pins the default so a future refactor can't silently change
    // it; airia-backed reasoning models 400 without this.
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(
        cfg.reasoning_effort.as_deref(),
        Some(DEFAULT_REASONING_EFFORT)
    );
}

#[test]
fn env_overrides_file_reasoning_effort() {
    let file = JsonConfig {
        reasoning_effort: Some("low".into()),
        ..JsonConfig::default()
    };
    let env = EnvInputs {
        reasoning_effort: Some("high".into()),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn per_model_reasoning_picks_exact_match() {
    // `reasoning.models` entry wins over the default and the
    // legacy reasoning_effort field.
    let file = JsonConfig {
        reasoning: Some(JsonReasoningConfig {
            default: Some("medium".into()),
            models: BTreeMap::from([
                ("gpt-5.4-mini".into(), "high".into()),
                ("gpt-5.6-luna".into(), "none".into()),
            ]),
            ..JsonReasoningConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(
        cfg.reasoning_for_model("gpt-5.4-mini").as_deref(),
        Some("high")
    );
    assert_eq!(
        cfg.reasoning_for_model("gpt-5.6-luna").as_deref(),
        Some("none")
    );
}

#[test]
fn per_model_reasoning_falls_back_to_default() {
    // Model not in the map falls back to reasoning.default.
    let file = JsonConfig {
        reasoning: Some(JsonReasoningConfig {
            default: Some("low".into()),
            models: BTreeMap::from([("gpt-5.4-mini".into(), "high".into())]),
            ..JsonReasoningConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.reasoning_for_model("gpt-4o").as_deref(), Some("low"));
}

#[test]
fn per_model_reasoning_falls_back_to_hardcoded_default() {
    // No `reasoning` object, no env, no legacy field — still gets
    // "none" so airia doesn't 400. This is the regression guard
    // for the bug that started this whole change.
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(
        cfg.reasoning_for_model("anything").as_deref(),
        Some(DEFAULT_REASONING_EFFORT)
    );
}

#[test]
fn per_model_reasoning_legacy_field_used_when_no_table() {
    // Legacy `reasoning_effort` (file or env) still works as the
    // global default when no `reasoning` object is present.
    let file = JsonConfig {
        reasoning_effort: Some("medium".into()),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(
        cfg.reasoning_for_model("any-model").as_deref(),
        Some("medium")
    );
}

#[test]
fn model_alias_hit() {
    // The common case: Claude Code sends `claude-sonnet-5` (its
    // subagent model), the proxy rewrites it to `gpt-5.4-mini`
    // for the gateway.
    let file = JsonConfig {
        model_aliases: Some(JsonModelAliases {
            map: BTreeMap::from([("claude-sonnet-5".into(), "gpt-5.4-mini".into())]),
            default_model: None,
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.upstream_model_for("claude-sonnet-5"), "gpt-5.4-mini");
}

#[test]
fn model_alias_miss_passes_through() {
    // No alias for this model → name goes out unchanged. This is
    // the default behavior; aliases are opt-in.
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(cfg.upstream_model_for("gpt-5.6-luna"), "gpt-5.6-luna");
}

#[test]
fn alias_and_reasoning_compose() {
    // The user aliases `claude-sonnet-5` → `gpt-5.4-mini`, and
    // also sets a per-model reasoning entry for `gpt-5.4-mini`.
    // The proxy should look up reasoning by the *resolved*
    // upstream name, not the inbound one — so a subagent request
    // for `claude-sonnet-5` lands on `gpt-5.4-mini` with
    // `high` reasoning, exactly as if the request had arrived
    // with `model: gpt-5.4-mini` directly.
    let file = JsonConfig {
        model_aliases: Some(JsonModelAliases {
            map: BTreeMap::from([("claude-sonnet-5".into(), "gpt-5.4-mini".into())]),
            default_model: None,
        }),
        reasoning: Some(JsonReasoningConfig {
            default: Some("none".into()),
            models: BTreeMap::from([("gpt-5.4-mini".into(), "high".into())]),
            ..JsonReasoningConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    let resolved = cfg.upstream_model_for("claude-sonnet-5");
    assert_eq!(resolved, "gpt-5.4-mini");
    assert_eq!(cfg.reasoning_for_model(&resolved).as_deref(), Some("high"));
}

#[test]
fn default_model_field_round_trips() {
    // The JSON `model_aliases.default_model` value should land on
    // Config::model_aliases.default_model verbatim. This is the
    // round-trip half of the contract: the field gets read.
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "model_aliases": {
                "default_model": "gpt-4o-mini"
            }
        }"#;
    let parsed = JsonConfig::parse(json_raw).expect("parse json");
    let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
    assert_eq!(
        cfg.model_aliases.default_model.as_deref(),
        Some("gpt-4o-mini")
    );
}

#[test]
fn default_model_method_returns_value() {
    // The Config::default_model() accessor must reflect whatever
    // landed on model_aliases.default_model. The proxy call site
    // uses this accessor; if it returns None when the field is
    // set, the fallback path will silently never run.
    let file = JsonConfig {
        model_aliases: Some(JsonModelAliases {
            map: BTreeMap::new(),
            default_model: Some("claude-haiku-4-5".into()),
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.default_model(), Some("claude-haiku-4-5"));
}

// ── Reasoning replay (BUG 1) ───────────────────────────────────────────

#[test]
fn reasoning_replay_defaults_to_text() {
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k"
        }"#;
    let parsed = JsonConfig::parse(json_raw).expect("parse json");
    let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
    assert_eq!(cfg.reasoning_replay.default, ReasoningReplayMode::Text);
    // A model that is not listed gets the default.
    let for_model = cfg.reasoning_replay_for_model("some-unlisted-model");
    assert_eq!(for_model.mode, ReasoningReplayMode::Text);
    assert_eq!(for_model.max_chars, 0);
}

#[test]
fn reasoning_replay_model_override_wins() {
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k",
            "reasoning_replay": {
                "default": "off",
                "models": {"gpt-5.6-luna": "native"},
                "max_chars": 512
            }
        }"#;
    let parsed = JsonConfig::parse(json_raw).expect("parse json");
    let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
    assert_eq!(
        cfg.reasoning_replay_for_model("gpt-5.6-luna").mode,
        ReasoningReplayMode::Native
    );
    assert_eq!(
        cfg.reasoning_replay_for_model("other-model").mode,
        ReasoningReplayMode::Off
    );
    assert_eq!(cfg.reasoning_replay_for_model("other-model").max_chars, 512);
}

#[test]
fn reasoning_replay_off_is_honoured() {
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k",
            "reasoning_replay": {"default": "off"}
        }"#;
    let parsed = JsonConfig::parse(json_raw).expect("parse json");
    let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
    assert_eq!(
        cfg.reasoning_replay_for_model("any-model").mode,
        ReasoningReplayMode::Off
    );
}

#[test]
fn reasoning_replay_rejects_an_unknown_mode() {
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k",
            "reasoning_replay": {"default": "nonsense"}
        }"#;
    assert!(
        JsonConfig::parse(json_raw).is_err(),
        "expected an unknown reasoning_replay mode to be rejected"
    );
}

#[test]
fn reasoning_replay_rejects_an_unknown_key() {
    // `JsonConfig` is `deny_unknown_fields`, so a typo must not be
    // silently ignored.
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k",
            "reasoning_replay": {"defualt": "text"}
        }"#;
    assert!(JsonConfig::parse(json_raw).is_err());
}

// ── tool_result_images ──────────────────────────────────

#[test]
fn tool_result_images_defaults_to_array() {
    let cfg = cfg_from_json(
        r#"{"upstream_base_url": "https://api.example.com",
                 "upstream_api_key": "k"}"#,
    );
    assert_eq!(cfg.tool_result_images, ToolResultImages::Array);
    assert!(cfg.warnings.is_empty());
}

#[test]
fn tool_result_images_accepts_lift_and_drop() {
    for (raw, want) in [
        ("lift", ToolResultImages::Lift),
        ("drop", ToolResultImages::Drop),
    ] {
        let cfg = cfg_from_json(&format!(
            r#"{{"upstream_base_url": "https://api.example.com",
                     "upstream_api_key": "k",
                     "tool_result_images": "{raw}"}}"#
        ));
        assert_eq!(cfg.tool_result_images, want);
        assert!(cfg.warnings.is_empty());
    }
}

#[test]
fn tool_result_images_value_is_case_insensitive() {
    let cfg = cfg_from_json(
        r#"{"upstream_base_url": "https://api.example.com",
                 "upstream_api_key": "k",
                 "tool_result_images": "LIFT"}"#,
    );
    assert_eq!(cfg.tool_result_images, ToolResultImages::Lift);
}

#[test]
fn tool_result_images_unknown_value_falls_back_to_array_with_a_warning() {
    let cfg = cfg_from_json(
        r#"{"upstream_base_url": "https://api.example.com",
                 "upstream_api_key": "k",
                 "tool_result_images": "banana"}"#,
    );
    assert_eq!(cfg.tool_result_images, ToolResultImages::Array);
    assert_eq!(cfg.warnings.len(), 1);
    assert!(
        cfg.warnings[0]
            .fields
            .iter()
            .any(|(k, v)| k == "tool_result_images" && v == "banana")
    );
}

#[test]
fn tool_result_images_env_overrides_file() {
    let parsed = JsonConfig::parse(
        r#"{"upstream_base_url": "https://api.example.com",
                 "upstream_api_key": "k",
                 "tool_result_images": "drop"}"#,
    )
    .unwrap();
    let env = EnvInputs {
        tool_result_images: Some("lift".to_string()),
        ..EnvInputs::default()
    };
    let cfg = Config::resolve(Some(&parsed), &env).unwrap();
    assert_eq!(cfg.tool_result_images, ToolResultImages::Lift);
}

#[test]
fn the_saved_config_does_not_carry_a_key_the_previous_binary_would_reject() {
    // The TUI rewrites `proxy.json` from `JsonConfig`, and
    // `JsonConfig` is `deny_unknown_fields`. If the save path wrote a
    // `reasoning_replay: null` key, the PREVIOUS binary (which does
    // not know the key) would refuse to start — a rollback hazard for
    // anyone who saved from the TUI.
    let saved = serde_json::to_string_pretty(&JsonConfig::default()).unwrap();
    assert!(
        !saved.contains("reasoning_replay"),
        "the saved config carries a key the previous binary rejects: {saved}"
    );
    // ...but an explicitly-configured replay policy must survive the
    // round-trip.
    let parsed = JsonConfig::parse(
        r#"{"upstream_base_url":"https://api.example.com","upstream_api_key":"k",
                "reasoning_replay":{"default":"native","models":{"m":"off"},"max_chars":42}}"#,
    )
    .expect("parse json");
    let saved = serde_json::to_string_pretty(&parsed).unwrap();
    assert!(saved.contains("reasoning_replay"), "{saved}");
    let reparsed = JsonConfig::parse(&saved).expect("the saved config must re-parse");
    assert_eq!(
        reparsed.reasoning_replay.as_ref().and_then(|r| r.default),
        Some(ReasoningReplayMode::Native)
    );
    assert_eq!(
        reparsed.reasoning_replay.as_ref().and_then(|r| r.max_chars),
        Some(42)
    );
}

// ── advisor config ────────────────────────────────────────────────

#[test]
fn advisor_is_off_by_default_with_cheap_model() {
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(cfg.advisor, AdvisorConfig::default());
    assert!(!cfg.advisor.enabled, "advisor must be opt-in");
    assert_eq!(cfg.advisor.model, "beam", "default advisor must be cheap");
    assert_eq!(cfg.advisor.effort, None);
    assert_eq!(cfg.advisor.max_uses, Some(2));
    assert_eq!(cfg.advisor.max_tokens, Some(4096));
    assert_eq!(cfg.advisor.reasoning_max_tokens, Some(16384));
    assert_eq!(
        cfg.advisor.timeout_secs, None,
        "the advisor has no timeout of its own by default"
    );
}

#[test]
fn advisor_json_section_parses_and_resolves() {
    // The exact shape must parse despite JsonConfig's deny_unknown_fields,
    // and every value must reach the resolved config.
    let raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k",
            "advisor": {
                "enabled": true,
                "model": "glm-5.3-flash",
                "effort": "medium",
                "max_uses": 3,
                "max_tokens": 1024,
                "reasoning_max_tokens": 8192,
                "timeout_secs": 20
            }
        }"#;
    let file = JsonConfig::parse(raw).expect("proxy.json with advisor must parse");
    assert!(file.advisor.is_some());
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(
        cfg.advisor,
        AdvisorConfig {
            enabled: true,
            model: "glm-5.3-flash".into(),
            effort: Some("medium".into()),
            max_uses: Some(3),
            max_tokens: Some(1024),
            reasoning_max_tokens: Some(8192),
            timeout_secs: Some(20),
        }
    );
}

#[test]
fn advisor_env_overrides_file_then_default() {
    let file = JsonConfig {
        advisor: Some(JsonAdvisorConfig {
            enabled: Some(true),
            model: Some("file-model".into()),
            effort: Some("low".into()),
            max_uses: limit(5),
            max_tokens: limit(999),
            reasoning_max_tokens: limit(888),
            timeout_secs: limit(30),
        }),
        ..JsonConfig::default()
    };
    let env = EnvInputs {
        advisor_enabled: Some(false),
        advisor_model: Some("env-model".into()),
        advisor_effort: Some("high".into()),
        advisor_max_uses: limit(1),
        advisor_max_tokens: limit(77),
        advisor_reasoning_max_tokens: limit(66),
        advisor_timeout_secs: limit(9),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(
        cfg.advisor,
        AdvisorConfig {
            enabled: false,
            model: "env-model".into(),
            effort: Some("high".into()),
            max_uses: Some(1),
            max_tokens: Some(77),
            reasoning_max_tokens: Some(66),
            timeout_secs: Some(9),
        }
    );

    // With no env override, the file wins; with neither, the defaults.
    let from_file = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(from_file.advisor.model, "file-model");
    assert_eq!(from_file.advisor.max_uses, Some(5));
    let defaults = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(defaults.advisor, AdvisorConfig::default());
}

#[test]
fn advisor_zero_limits_mean_no_limit() {
    // The operator's rule: `0` means "no limit" (the house precedent of
    // `reasoning_replay.max_chars`), and an ABSENT key means the
    // shipped default. The two are NOT the same thing.
    let file = JsonConfig {
        advisor: Some(JsonAdvisorConfig {
            enabled: Some(true),
            max_uses: limit(0),
            max_tokens: limit(0),
            timeout_secs: limit(0),
            ..JsonAdvisorConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert!(cfg.advisor.enabled, "the section itself stays enabled");
    assert_eq!(cfg.advisor.max_uses, None, "0 = no limit");
    assert_eq!(cfg.advisor.max_tokens, None, "0 = no limit");
    assert_eq!(cfg.advisor.timeout_secs, None, "0 = no limit");

    // An ABSENT key is the default, NOT "no limit".
    let absent = JsonConfig {
        advisor: Some(JsonAdvisorConfig {
            enabled: Some(true),
            ..JsonAdvisorConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&absent), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
    assert_eq!(cfg.advisor.max_tokens, Some(DEFAULT_ADVISOR_MAX_TOKENS));
    assert_eq!(cfg.advisor.timeout_secs, None);

    // An explicit `n > 0` is that bound.
    let explicit = JsonConfig {
        advisor: Some(JsonAdvisorConfig {
            enabled: Some(true),
            max_uses: limit(3),
            max_tokens: limit(1024),
            timeout_secs: limit(20),
            ..JsonAdvisorConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&explicit), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.max_uses, Some(3));
    assert_eq!(cfg.advisor.max_tokens, Some(1024));
    assert_eq!(cfg.advisor.timeout_secs, Some(20));
}

#[test]
fn advisor_zero_limits_from_the_env_mean_no_limit() {
    // The env override follows the same rule as the file: `0` = no
    // limit, absent = the default.
    let env = EnvInputs {
        advisor_max_uses: limit(0),
        advisor_max_tokens: limit(0),
        advisor_timeout_secs: limit(0),
        ..env_with_required()
    };
    let cfg = Config::resolve(None, &env).unwrap();
    assert_eq!(cfg.advisor.max_uses, None);
    assert_eq!(cfg.advisor.max_tokens, None);
    assert_eq!(cfg.advisor.timeout_secs, None);

    // An env `0` also beats a file value: env > file.
    let file = JsonConfig {
        advisor: Some(JsonAdvisorConfig {
            enabled: Some(true),
            max_uses: limit(5),
            max_tokens: limit(999),
            timeout_secs: limit(30),
            ..JsonAdvisorConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(cfg.advisor.max_uses, None, "env 0 wins over the file");
    assert_eq!(cfg.advisor.max_tokens, None);
    assert_eq!(cfg.advisor.timeout_secs, None);
}

#[test]
fn advisor_out_of_range_limit_falls_back_to_the_default() {
    // A value that does not fit `u32` is not silently dropped: it falls
    // back to the default (the house style — a bad value must not stop
    // the proxy from starting) AND it raises a warning.
    let env = EnvInputs {
        advisor_max_uses: limit(u64::from(u32::MAX) + 1),
        advisor_max_tokens: limit(u64::from(u32::MAX) + 1),
        ..env_with_required()
    };
    let cfg = Config::resolve(None, &env).unwrap();
    assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
    assert_eq!(cfg.advisor.max_tokens, Some(DEFAULT_ADVISOR_MAX_TOKENS));
    // The warning is COLLECTED (the resolver runs before the tracing
    // subscriber exists, so it cannot emit it itself) and the binary
    // prints it after `init_tracing`.
    let warnings: Vec<&str> = cfg.warnings.iter().map(|w| w.message.as_str()).collect();
    assert_eq!(warnings.len(), 2, "one warning per out-of-range limit");
    assert!(
        warnings[0].contains("advisor.max_uses")
            && warnings[0].contains(&(u64::from(u32::MAX) + 1).to_string()),
        "the warning names the field and the offending value: {}",
        warnings[0]
    );
    assert!(
        warnings[1].contains("advisor.max_tokens"),
        "the warning names the field: {}",
        warnings[1]
    );
    for warning in &cfg.warnings {
        assert!(
            warning.message.contains("using the default"),
            "the warning says what happened: {}",
            warning.message
        );
    }
}

#[test]
fn an_out_of_range_json_limit_still_loads_and_warns() {
    // DEFECT 2: an out-of-range JSON number used to be a HARD STARTUP
    // FAILURE (the JSON type was `u32`, so `"max_uses": 4294967296`
    // failed to parse and the proxy exited). The JSON type is now wide
    // enough to parse, and the resolver range-checks it: the proxy
    // starts, the value falls back to the default, and a warning is
    // raised.
    let raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k",
            "advisor": {
                "enabled": true,
                "max_uses": 4294967296,
                "max_tokens": 4294967296
            }
        }"#;
    let file = JsonConfig::parse(raw).expect("an out-of-range limit must still parse");
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
    assert_eq!(cfg.advisor.max_tokens, Some(DEFAULT_ADVISOR_MAX_TOKENS));
    assert_eq!(cfg.warnings.len(), 2, "both limits warn");
    assert!(
        cfg.warnings
            .iter()
            .all(|w| w.message.contains("out of range"))
    );
}

#[test]
fn an_out_of_range_json_limit_loads_through_the_real_entry_point() {
    // Same as above, but through `Config::load_from` (fs read + parse +
    // resolve) — the path the binary actually takes. The proxy must
    // START: the file is read, the JSON parses, the limit falls back
    // to the default, and the warning is collected.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("advisor-out-of-range-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("proxy.json");
    std::fs::write(
        &path,
        r#"{
                "upstream_base_url": "https://api.example.com",
                "upstream_api_key":  "k",
                "advisor": { "enabled": true, "max_uses": 4294967296 }
            }"#,
    )
    .unwrap();

    let cfg = Config::load_from(&path).expect("the proxy must still start");
    assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
    assert_eq!(cfg.warnings.len(), 1, "the out-of-range limit warns once");
    assert!(cfg.warnings[0].message.contains("advisor.max_uses"));
    assert!(cfg.warnings[0].message.contains("4294967296"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_negative_json_limit_still_loads_and_warns() {
    // DEFECT: a NEGATIVE limit used to be a HARD STARTUP FAILURE — the
    // JSON type was an unsigned integer, so serde rejected `-1` at
    // PARSE time (`invalid value: integer -1, expected u64`) and the
    // proxy EXITED. The JSON layer now accepts any number and the
    // RESOLVER judges it: the proxy starts, the value falls back to
    // the shipped default, and a warning is raised.
    let raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k",
            "advisor": {
                "enabled": true,
                "max_uses": -1,
                "max_tokens": -2048,
                "timeout_secs": -45
            }
        }"#;
    let file = JsonConfig::parse(raw).expect("a negative limit must still parse");
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
    assert_eq!(cfg.advisor.max_tokens, Some(DEFAULT_ADVISOR_MAX_TOKENS));
    assert_eq!(cfg.advisor.timeout_secs, None);
    assert_eq!(cfg.warnings.len(), 3, "one warning per invalid limit");
    for warning in &cfg.warnings {
        assert!(
            warning.message.contains("is not a valid limit"),
            "the warning says what happened: {}",
            warning.message
        );
        assert!(
            warning.message.contains("using the default"),
            "the warning says what happened: {}",
            warning.message
        );
        assert!(
            warning.message.contains("negative"),
            "the warning says WHY: {}",
            warning.message
        );
    }
    assert!(cfg.warnings[0].message.contains("advisor.max_uses"));
    assert!(cfg.warnings[0].message.contains("-1"));
    assert!(cfg.warnings[1].message.contains("advisor.max_tokens"));
    assert!(cfg.warnings[2].message.contains("advisor.timeout_secs"));
    // The structured fields name the field, the offending value and
    // the fallback, so a postmortem can grep for them.
    assert_eq!(
        cfg.warnings[0].fields[0],
        ("field".to_owned(), "advisor.max_uses".to_owned())
    );
    assert_eq!(
        cfg.warnings[0].fields[1],
        ("value".to_owned(), "-1".to_owned())
    );
    assert_eq!(
        cfg.warnings[0].fields[2],
        ("fallback".to_owned(), DEFAULT_ADVISOR_MAX_USES.to_string())
    );
}

#[test]
fn a_fractional_json_limit_still_loads_and_warns() {
    // The same for a FRACTIONAL limit: `2.5` is not a number of
    // consultations, so it cannot be a limit — but it must not stop
    // the proxy from starting either.
    let raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k",
            "advisor": { "enabled": true, "max_uses": 2.5, "max_tokens": 0.5 }
        }"#;
    let file = JsonConfig::parse(raw).expect("a fractional limit must still parse");
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
    assert_eq!(cfg.advisor.max_tokens, Some(DEFAULT_ADVISOR_MAX_TOKENS));
    assert_eq!(cfg.warnings.len(), 2, "one warning per invalid limit");
    for warning in &cfg.warnings {
        assert!(
            warning.message.contains("is not a valid limit"),
            "{}",
            warning.message
        );
        assert!(
            warning.message.contains("not a whole number"),
            "the warning says WHY: {}",
            warning.message
        );
    }
    assert!(cfg.warnings[0].message.contains("advisor.max_uses"));
    assert!(cfg.warnings[0].message.contains("2.5"));
    assert!(cfg.warnings[1].message.contains("advisor.max_tokens"));
}

#[test]
fn an_invalid_json_limit_loads_through_the_real_entry_point() {
    // The same as above, but through `Config::load_from` (fs read +
    // parse + resolve) — the path the binary actually takes. The proxy
    // must START with a negative limit in `proxy.json`.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("advisor-negative-limit-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("proxy.json");
    std::fs::write(
        &path,
        r#"{
                "upstream_base_url": "https://api.example.com",
                "upstream_api_key":  "k",
                "advisor": { "enabled": true, "max_uses": -1 }
            }"#,
    )
    .unwrap();

    let cfg = Config::load_from(&path).expect("the proxy must still start");
    assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
    assert_eq!(cfg.warnings.len(), 1, "the negative limit warns once");
    assert!(cfg.warnings[0].message.contains("advisor.max_uses"));
    assert!(cfg.warnings[0].message.contains("-1"));

    // ...and the same for a fractional one.
    std::fs::write(
        &path,
        r#"{
                "upstream_base_url": "https://api.example.com",
                "upstream_api_key":  "k",
                "advisor": { "enabled": true, "max_uses": 2.5 }
            }"#,
    )
    .unwrap();
    let cfg = Config::load_from(&path).expect("the proxy must still start");
    assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
    assert_eq!(cfg.warnings.len(), 1, "the fractional limit warns once");
    assert!(cfg.warnings[0].message.contains("2.5"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_invalid_env_limit_warns_like_an_invalid_file_limit() {
    // The ENV path follows the FILE path: a negative or fractional
    // value is not silently dropped (as it was when the env var was
    // parsed as an unsigned integer) — it reaches the resolver, which
    // falls back to the default WITH a warning.
    let env = EnvInputs {
        advisor_max_uses: Some(serde_json::Number::from(-1)),
        advisor_max_tokens: fractional_limit(2.5),
        ..env_with_required()
    };
    let cfg = Config::resolve(None, &env).unwrap();
    assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
    assert_eq!(cfg.advisor.max_tokens, Some(DEFAULT_ADVISOR_MAX_TOKENS));
    assert_eq!(cfg.warnings.len(), 2, "one warning per invalid limit");
    assert!(cfg.warnings[0].message.contains("advisor.max_uses"));
    assert!(cfg.warnings[0].message.contains("negative"));
    assert!(cfg.warnings[1].message.contains("advisor.max_tokens"));
    assert!(cfg.warnings[1].message.contains("not a whole number"));
}

#[test]
fn a_valid_limit_is_used_unchanged_and_raises_no_warning() {
    // The other side of the contract: a valid limit is that bound, and
    // nothing is said about it.
    let file = JsonConfig {
        advisor: Some(JsonAdvisorConfig {
            enabled: Some(true),
            max_uses: limit(3),
            max_tokens: limit(1024),
            timeout_secs: limit(20),
            ..JsonAdvisorConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.max_uses, Some(3));
    assert_eq!(cfg.advisor.max_tokens, Some(1024));
    assert_eq!(cfg.advisor.timeout_secs, Some(20));
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn a_whole_number_written_as_a_float_is_a_valid_limit() {
    // `2.0` is a whole number in VALUE, so it is that bound — not an
    // invalid value. The file keeps the operator's own spelling
    // (`2.0`), the resolved limit is 2.
    let raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k",
            "advisor": { "enabled": true, "max_uses": 2.0 }
        }"#;
    let file = JsonConfig::parse(raw).expect("parse json");
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.max_uses, Some(2));
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn config_warnings_carry_the_unbounded_loop_warning_once() {
    // The unbounded-loop warning is COLLECTED like every other
    // config-time warning, so the binary can print it after the
    // tracing subscriber exists (exactly once, on stderr and in the
    // log file).
    let file = JsonConfig {
        advisor: Some(JsonAdvisorConfig {
            enabled: Some(true),
            max_uses: limit(0),
            ..JsonAdvisorConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    let unbounded: Vec<&ConfigWarning> = cfg
        .warnings
        .iter()
        .filter(|w| w.message == UNBOUNDED_ADVISOR_LOOP_WARNING)
        .collect();
    assert_eq!(unbounded.len(), 1, "exactly one unbounded warning");
    assert_eq!(
        unbounded[0].fields,
        vec![("advisor_model".to_owned(), "beam".to_owned())]
    );

    // With the advisor DISABLED the warning is not raised at all: an
    // unused section must not shout at the operator.
    let disabled = JsonConfig {
        advisor: Some(JsonAdvisorConfig {
            enabled: Some(false),
            max_uses: limit(0),
            ..JsonAdvisorConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&disabled), &env_with_required()).unwrap();
    assert!(
        cfg.warnings.is_empty(),
        "a disabled advisor warns about nothing: {:?}",
        cfg.warnings
    );
}

#[test]
fn a_normal_config_raises_no_warnings() {
    // The collection must stay quiet in the common case, or the startup
    // banner would fill with noise.
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn a_real_proxy_json_with_and_without_the_advisor_section_loads() {
    // Goes through the real `Config::load_from` entry point (fs read +
    // JsonConfig::parse + resolve), not just the resolver. Proves the
    // gate: an operator can drop the new section into proxy.json and the
    // binary still starts; and a legacy file without it is unchanged.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("phase2-advisor-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("proxy.json");

    let legacy = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k"
        }"#;
    std::fs::write(&path, legacy).unwrap();
    let cfg = Config::load_from(&path).expect("legacy proxy.json must load");
    assert_eq!(cfg.advisor, AdvisorConfig::default());
    assert!(!cfg.advisor.enabled);

    let with_advisor = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k",
            "advisor": {
                "enabled": true,
                "model": "beam",
                "effort": "medium",
                "max_uses": 2,
                "max_tokens": 2048,
                "timeout_secs": 45
            }
        }"#;
    std::fs::write(&path, with_advisor).unwrap();
    let cfg = Config::load_from(&path).expect("proxy.json WITH the advisor section must load");
    assert_eq!(
        cfg.advisor,
        AdvisorConfig {
            enabled: true,
            model: "beam".into(),
            effort: Some("medium".into()),
            max_uses: Some(2),
            max_tokens: Some(2048),
            reasoning_max_tokens: Some(DEFAULT_ADVISOR_REASONING_MAX_TOKENS),
            timeout_secs: Some(45),
        }
    );
    // Every other resolved setting is identical to the legacy file.
    assert_eq!(cfg.upstream_base_url, "https://api.example.com");
    assert_eq!(cfg.upstream_path, DEFAULT_UPSTREAM_PATH);
    assert_eq!(cfg.reasoning_effort.as_deref(), Some("none"));

    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(&dir).unwrap();
}

#[test]
fn advisor_unknown_json_key_is_rejected() {
    // deny_unknown_fields on the sub-object too: a typo fails fast.
    let raw = r#"{"advisor": {"enabled": true, "max_use": 1}}"#;
    assert!(JsonConfig::parse(raw).is_err());
}

#[test]
fn advisor_absent_key_is_rollback_safe_and_behaves_as_before() {
    // (1) A saved config with `advisor: None` must NOT emit the key —
    // the TUI rewrites proxy.json wholesale and the previous binary's
    // deny_unknown_fields would refuse to start.
    let saved = serde_json::to_string_pretty(&JsonConfig::default()).unwrap();
    assert!(
        !saved.contains("advisor"),
        "the saved config carries a key the previous binary rejects: {saved}"
    );
    // (2) A file with NO advisor key must resolve to exactly the same
    // AdvisorConfig as no file at all — zero behaviour change.
    let raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k"
        }"#;
    let file = JsonConfig::parse(raw).expect("parse");
    assert!(file.advisor.is_none());
    let env = env_with_required();
    let absent = Config::resolve(Some(&file), &env).unwrap();
    let no_file = Config::resolve(None, &env).unwrap();
    assert_eq!(absent.advisor, no_file.advisor);
    assert!(!absent.advisor.enabled);
}

#[test]
fn advisor_explicit_section_survives_a_save_round_trip() {
    let parsed = JsonConfig::parse(
        r#"{"upstream_base_url":"https://api.example.com","upstream_api_key":"k",
                "advisor":{"enabled":true,"model":"beam","max_uses":2}}"#,
    )
    .expect("parse json");
    let saved = serde_json::to_string_pretty(&parsed).unwrap();
    assert!(saved.contains("advisor"), "{saved}");
    let reparsed = JsonConfig::parse(&saved).expect("the saved config must re-parse");
    assert_eq!(
        reparsed.advisor.as_ref().and_then(|a| a.enabled),
        Some(true)
    );
    assert_eq!(
        reparsed.advisor.as_ref().and_then(|a| a.model.as_deref()),
        Some("beam")
    );
}

#[test]
fn the_raw_json_limits_survive_the_tui_save_round_trip() {
    // The TUI rewrites `proxy.json` wholesale from the parsed
    // `JsonConfig`. The RAW JSON numbers must not change that
    // round-trip: an in-range value is written back unchanged, and an
    // out-of-range value — which the resolver falls back on — is
    // PRESERVED verbatim instead of being silently rewritten, so the
    // operator can still see (and fix) their typo.
    let raw = r#"{"upstream_base_url":"https://api.example.com","upstream_api_key":"k",
            "advisor":{"enabled":true,"model":"beam","max_uses":2,"max_tokens":2048,
                       "timeout_secs":45}}"#;
    let parsed = JsonConfig::parse(raw).expect("parse json");
    let saved = serde_json::to_string_pretty(&parsed).unwrap();
    let reparsed = JsonConfig::parse(&saved).expect("the saved config must re-parse");
    assert_eq!(reparsed, parsed, "the whole config round-trips");
    let advisor = reparsed.advisor.as_ref().unwrap();
    assert_eq!(advisor.max_uses, Some(serde_json::Number::from(2)));
    assert_eq!(advisor.max_tokens, Some(serde_json::Number::from(2048)));
    assert_eq!(advisor.timeout_secs, Some(serde_json::Number::from(45)));
    // The saved form is the plain number an operator wrote — the
    // widened type does not leak into the file.
    assert!(saved.contains("\"max_uses\": 2"), "{saved}");
    assert!(saved.contains("\"max_tokens\": 2048"), "{saved}");

    // An out-of-range value round-trips verbatim (it is NOT clamped
    // on the way through the file).
    let oor = JsonConfig::parse(
        r#"{"upstream_base_url":"https://api.example.com","upstream_api_key":"k",
                "advisor":{"enabled":true,"max_uses":4294967296}}"#,
    )
    .expect("parse json");
    let saved = serde_json::to_string_pretty(&oor).unwrap();
    assert!(saved.contains("4294967296"), "{saved}");
    let reparsed = JsonConfig::parse(&saved).expect("the saved config must re-parse");
    assert_eq!(reparsed, oor, "the whole config round-trips");
    assert_eq!(
        reparsed.advisor.as_ref().and_then(|a| a.max_uses.as_ref()),
        Some(&serde_json::Number::from(4294967296u64))
    );
}

#[test]
fn an_invalid_json_limit_survives_the_tui_save_round_trip_verbatim() {
    // A NEGATIVE or FRACTIONAL limit cannot be a limit (the resolver
    // falls back to the default with a warning), but the TUI must
    // write it back EXACTLY as the operator wrote it: the file is the
    // operator's record of their intent, and silently rewriting the
    // value would hide the typo.
    let raw = r#"{"upstream_base_url":"https://api.example.com","upstream_api_key":"k",
            "advisor":{"enabled":true,"max_uses":-1,"max_tokens":2.5,"timeout_secs":-45}}"#;
    let parsed = JsonConfig::parse(raw).expect("an invalid limit must still parse");
    let saved = serde_json::to_string_pretty(&parsed).unwrap();
    assert!(saved.contains("\"max_uses\": -1"), "{saved}");
    assert!(saved.contains("\"max_tokens\": 2.5"), "{saved}");
    assert!(saved.contains("\"timeout_secs\": -45"), "{saved}");
    let reparsed = JsonConfig::parse(&saved).expect("the saved config must re-parse");
    assert_eq!(reparsed, parsed, "the whole config round-trips unchanged");
    let advisor = reparsed.advisor.as_ref().unwrap();
    assert_eq!(advisor.max_uses, Some(serde_json::Number::from(-1)));
    assert_eq!(
        advisor.max_tokens,
        Some(serde_json::Number::from_f64(2.5).unwrap())
    );
    assert_eq!(advisor.timeout_secs, Some(serde_json::Number::from(-45)));
}

#[test]
fn advisor_model_is_not_an_inbound_alias() {
    // The advisor model must be usable verbatim even when the alias map
    // would rewrite the same string on an ordinary client request.
    let file = JsonConfig {
        model_aliases: Some(JsonModelAliases {
            map: BTreeMap::from([("beam".into(), "gpt-5.6-luna".into())]),
            ..JsonModelAliases::default()
        }),
        advisor: Some(JsonAdvisorConfig {
            model: Some("beam".into()),
            ..JsonAdvisorConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.model, "beam");
    assert_eq!(
        cfg.upstream_model_for("beam"),
        "gpt-5.6-luna",
        "the alias map still applies to inbound names"
    );
}

#[test]
fn the_shipped_example_config_parses() {
    // `proxy.json.example` is the template operators copy. `JsonConfig`
    // is `deny_unknown_fields`, so a key the example uses but the schema
    // does not declare would make the example unusable.
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/proxy.json.example"))
        .expect("read proxy.json.example");
    let parsed = JsonConfig::parse(&raw).expect("proxy.json.example should parse");
    // The example deliberately does NOT set `reasoning_replay`: the
    // default is already the safe mode, and `JsonConfig` is
    // `deny_unknown_fields`, so a key in the example would make the
    // PREVIOUS binary refuse to start — a rollback hazard for anyone
    // who copied it.
    assert!(parsed.reasoning_replay.is_none());
}

#[test]
fn prompt_caching_defaults_to_disabled() {
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert!(cfg.prompt_caching.models.is_empty());
    assert!(cfg.prompt_caching.cache_key.is_none());
}

#[test]
fn prompt_caching_json_sets_models_and_key() {
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "prompt_caching": {
                "models": ["gpt-5.6-luna", "gpt-5.6-terra"],
                "cache_key": "my-app"
            }
        }"#;
    let parsed = JsonConfig::parse(json_raw).expect("parse json");
    let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
    assert_eq!(cfg.prompt_caching.models.len(), 2);
    assert!(
        cfg.prompt_caching
            .models
            .contains(&"gpt-5.6-luna".to_string())
    );
    assert!(
        cfg.prompt_caching
            .models
            .contains(&"gpt-5.6-terra".to_string())
    );
    assert_eq!(cfg.prompt_caching.cache_key.as_deref(), Some("my-app"));
}

#[test]
fn prompt_caching_for_model_filters_by_model() {
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "prompt_caching": {
                "models": ["gpt-5.6-luna", "gpt-5.6-terra"],
                "cache_key": "my-app"
            }
        }"#;
    let parsed = JsonConfig::parse(json_raw).expect("parse json");
    let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
    // Model in the list gets caching
    let for_luna = cfg.prompt_caching_for_model("gpt-5.6-luna");
    assert!(!for_luna.models.is_empty());
    assert_eq!(for_luna.cache_key.as_deref(), Some("my-app"));
    // Model NOT in the list gets no caching
    let for_other = cfg.prompt_caching_for_model("gpt-5.4-mini");
    assert!(for_other.models.is_empty());
    assert_eq!(for_other.cache_key.as_deref(), Some("my-app"));
}

#[test]
fn prompt_caching_env_overrides_json() {
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "prompt_caching": {
                "models": ["gpt-5.6-luna"],
                "cache_key": "from-file"
            }
        }"#;
    let parsed = JsonConfig::parse(json_raw).expect("parse json");
    let env = EnvInputs {
        prompt_caching_models: Some("".into()),
        prompt_cache_key: Some("from-env".into()),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&parsed), &env).unwrap();
    assert!(cfg.prompt_caching.models.is_empty());
    assert_eq!(cfg.prompt_caching.cache_key.as_deref(), Some("from-env"));
}

#[test]
fn default_model_unset_returns_none() {
    // No `model_aliases` object at all → accessor returns None,
    // the proxy surfaces upstream errors unchanged.
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(cfg.default_model(), None);
}

#[test]
fn reasoning_object_with_only_default_parses() {
    // A `reasoning` object containing just `default` must be
    // accepted; the `models` sub-object is optional. Without
    // `#[serde(default)]` on `JsonReasoningConfig.models`, this
    // shape would fail to deserialize and the proxy would refuse
    // to start.
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "reasoning": {
                "default": "none"
            }
        }"#;
    let parsed = JsonConfig::parse(json_raw).expect("parse json");
    let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
    assert_eq!(cfg.reasoning.default.as_deref(), Some("none"));
    assert!(cfg.reasoning.models.is_empty());
    assert_eq!(
        cfg.reasoning_for_model("any-model").as_deref(),
        Some("none")
    );
}

#[test]
fn env_overrides_file() {
    let file = JsonConfig {
        listen_addr: Some("0.0.0.0:9999".into()),
        ..JsonConfig::default()
    };
    let env = EnvInputs {
        listen_addr: Some("0.0.0.0:1234".into()),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(cfg.listen_addr.to_string(), "0.0.0.0:1234");
}

#[test]
fn file_fills_in_when_env_unset() {
    let file = JsonConfig {
        listen_addr: Some("0.0.0.0:9999".into()),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.listen_addr.to_string(), "0.0.0.0:9999");
}

#[test]
fn log_to_disk_defaults_to_false() {
    // Default is off so a fresh install never silently writes a
    // log file (PII concern: request bodies used to be logged at
    // WARN on every upstream error).
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert!(!cfg.log_to_disk);
}

#[test]
fn log_to_disk_env_true_enables() {
    // `LOG_TO_DISK=1` is the documented "on" trigger.
    let env = EnvInputs {
        log_to_disk: Some(true),
        ..env_with_required()
    };
    let cfg = Config::resolve(None, &env).unwrap();
    assert!(cfg.log_to_disk);
}

#[test]
fn log_to_disk_env_truthy_values_enable() {
    // All of these should turn it on.
    for v in ["1", "true", "yes", "on", "TRUE", "Yes", " on "] {
        let env = EnvInputs {
            log_to_disk: Some(true), // already parsed by capture
            ..env_with_required()
        };
        // Re-parse the value through the same logic capture() uses.
        let parsed = {
            let v = v.to_ascii_lowercase();
            let v = v.trim();
            matches!(v, "1" | "true" | "yes" | "on")
        };
        assert!(parsed, "expected {v:?} to enable log_to_disk");
        // Sanity: the resolved config respects the env when set.
        let cfg = Config::resolve(None, &env).unwrap();
        assert!(cfg.log_to_disk, "value {v:?} should be on");
    }
}

#[test]
fn log_to_disk_env_false_disables() {
    // Anything other than the truthy set disables. A typo in the
    // env var name should NOT turn logging on.
    let env = EnvInputs {
        log_to_disk: Some(false),
        ..env_with_required()
    };
    let cfg = Config::resolve(None, &env).unwrap();
    assert!(!cfg.log_to_disk);
}

#[test]
fn log_to_disk_json_fills_in_when_env_unset() {
    // JSON fallback: if env is unset, the file's value applies.
    let file = JsonConfig {
        log_to_disk: Some(true),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert!(cfg.log_to_disk);
}

#[test]
fn log_to_disk_env_overrides_json() {
    // Env wins over file (consistent with every other field).
    let file = JsonConfig {
        log_to_disk: Some(true),
        ..JsonConfig::default()
    };
    let env = EnvInputs {
        log_to_disk: Some(false),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert!(!cfg.log_to_disk);
}

#[test]
fn proxy_key_defaults_to_none() {
    // No key set → `None` → no client auth. The startup warning
    // is emitted by main.rs, not by the config layer.
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert!(cfg.proxy_key.is_none());
}

#[test]
fn proxy_key_json_round_trips() {
    let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "proxy_key": "shared-secret-1234"
        }"#;
    let parsed = JsonConfig::parse(json_raw).expect("parse json");
    let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
    assert_eq!(cfg.proxy_key.as_deref(), Some("shared-secret-1234"));
}

#[test]
fn proxy_key_env_overrides_json() {
    let file = JsonConfig {
        proxy_key: Some("from-file".into()),
        ..JsonConfig::default()
    };
    let env = EnvInputs {
        proxy_key: Some("from-env".into()),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(cfg.proxy_key.as_deref(), Some("from-env"));
}

#[test]
fn blank_proxy_key_values_disable_auth() {
    let file = JsonConfig {
        proxy_key: Some("   ".into()),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert!(cfg.proxy_key.is_none());

    let env = EnvInputs {
        proxy_key: Some(String::new()),
        ..env_with_required()
    };
    let file = JsonConfig {
        proxy_key: Some("from-file".into()),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert!(cfg.proxy_key.is_none());
}

// ── reasoning_for_request (effort_map / thinking_disabled) ──────

fn cfg_from_json(json: &str) -> Config {
    let parsed = JsonConfig::parse(json).expect("parse json");
    Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap()
}

const REASONING_JSON: &str = r#"{
        "upstream_base_url": "https://api.example.com",
        "upstream_api_key":  "sk-test",
        "reasoning": {
            "default": "high",
            "effort_map": {
                "default": {
                    "low": "none",
                    "medium": "low",
                    "high": "medium",
                    "xhigh": "high",
                    "max": "high"
                },
                "models": {
                    "gpt-5.6-luna": {
                        "low": "low",
                        "medium": "medium",
                        "high": "high",
                        "xhigh": "xhigh",
                        "max": "max"
                    }
                }
            },
            "thinking_disabled": {
                "default": {
                    "disabled": "none"
                }
            }
        }
    }"#;

#[test]
fn effort_map_default_entry_translates() {
    let cfg = cfg_from_json(REASONING_JSON);
    // gpt-5.4-mini has no per-model entry → uses effort_map.default.
    let d = cfg.reasoning_for_request("gpt-5.4-mini", Some("high"), false);
    assert_eq!(d, ReasoningDecision::Effort("medium".into()));
    let d = cfg.reasoning_for_request("gpt-5.4-mini", Some("low"), false);
    assert_eq!(d, ReasoningDecision::Effort("none".into()));
}

#[test]
fn effort_map_model_entry_overrides_default() {
    let cfg = cfg_from_json(REASONING_JSON);
    // gpt-5.6-luna has a per-model entry that overrides default.
    let d = cfg.reasoning_for_request("gpt-5.6-luna", Some("high"), false);
    assert_eq!(d, ReasoningDecision::Effort("high".into()));
    let d = cfg.reasoning_for_request("gpt-5.6-luna", Some("xhigh"), false);
    assert_eq!(d, ReasoningDecision::Effort("xhigh".into()));
}

#[test]
fn effort_map_null_omits_reasoning() {
    let json = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "reasoning": {
                "default": "high",
                "effort_map": {
                    "default": {"high": null}
                }
            }
        }"#;
    let cfg = cfg_from_json(json);
    let d = cfg.reasoning_for_request("any-model", Some("high"), false);
    assert_eq!(d, ReasoningDecision::Omit);
    assert_eq!(d.upstream_effort(), None);
}

#[test]
fn effort_map_identity_fallback_for_known_values() {
    let cfg = cfg_from_json(REASONING_JSON);
    // A requested value with no mapping and no per-model entry falls
    // back to identity when it's a known upstream value. "minimal" is
    // not in the default map, so it hits the identity path.
    let d = cfg.reasoning_for_request("some-other-model", Some("minimal"), false);
    assert_eq!(d, ReasoningDecision::Effort("minimal".into()));
}

#[test]
fn thinking_disabled_takes_precedence_over_effort() {
    let cfg = cfg_from_json(REASONING_JSON);
    // Even when an effort is requested, explicit disabled thinking wins.
    let d = cfg.reasoning_for_request("gpt-5.4-mini", Some("max"), true);
    assert_eq!(d, ReasoningDecision::Disabled("none".into()));
}

#[test]
fn no_client_signal_uses_fixed_default() {
    let cfg = cfg_from_json(REASONING_JSON);
    let d = cfg.reasoning_for_request("gpt-5.4-mini", None, false);
    assert_eq!(d, ReasoningDecision::Effort("high".into()));
}

#[test]
fn thinking_disabled_with_no_config_falls_through() {
    // If no thinking_disabled mapping is configured, a disabled
    // request still falls through to fixed resolution rather than
    // inventing a value.
    let json = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "reasoning": {"default": "medium"}
        }"#;
    let cfg = cfg_from_json(json);
    let d = cfg.reasoning_for_request("any-model", None, true);
    assert_eq!(d, ReasoningDecision::Effort("medium".into()));
}

#[test]
fn normalize_effort_lowercases_and_trims() {
    assert_eq!(
        Config::normalize_effort(Some("  HIGH ")),
        Some("high".into())
    );
    assert_eq!(Config::normalize_effort(Some("")), None);
    assert_eq!(Config::normalize_effort(None), None);
}

// ─── `web_search` ───────────────────────────────────────────────────────────

fn web_search_file(json: &str) -> JsonConfig {
    serde_json::from_str(&format!(r#"{{ "web_search": {json} }}"#)).unwrap()
}

#[test]
fn web_search_defaults_are_off_and_bounded() {
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    let ws = &cfg.web_search;
    assert!(!ws.enabled);
    assert_eq!(ws.compaction_model, None);
    assert_eq!(ws.compaction_threshold_chars, 5_000);
    assert_eq!(ws.max_queries, 3);
    assert_eq!(ws.max_results_per_query, 5);
    assert_eq!(ws.search_endpoint, DEFAULT_WEB_SEARCH_ENDPOINT);
    assert_eq!(ws.search_min_interval_ms, 1_500);
    assert_eq!(ws.block_wait_minutes, 10);
    assert_eq!(ws.cache_ttl_secs, 600);
    assert_eq!(ws.cache_max_entries, 128);
    assert_eq!(ws.session_ttl_hours, 168);
    assert_eq!(
        ws.state_dir.as_deref(),
        Some(std::path::Path::new(DEFAULT_WEB_SEARCH_STATE_DIR)),
        "a resolved configuration keeps the session beside proxy.json"
    );
    assert!(cfg.warnings.is_empty());
}

#[test]
fn web_search_session_and_cache_values_resolve_and_are_bounded() {
    let file = web_search_file(
        r#"{ "state_dir": " /var/lib/proxy ", "cache_ttl_secs": 30,
             "cache_max_entries": 9, "session_ttl_hours": 2 }"#,
    );
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    let ws = &cfg.web_search;
    assert_eq!(
        ws.state_dir.as_deref(),
        Some(std::path::Path::new("/var/lib/proxy"))
    );
    assert_eq!(ws.cache_ttl_secs, 30);
    assert_eq!(ws.cache_max_entries, 9);
    assert_eq!(ws.session_ttl_hours, 2);
    assert!(cfg.warnings.is_empty());

    let file = web_search_file(
        r#"{ "cache_ttl_secs": 999999, "cache_max_entries": 0, "session_ttl_hours": 99999 }"#,
    );
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.web_search.cache_ttl_secs, 86_400, "clamped");
    assert_eq!(cfg.web_search.cache_max_entries, 128, "0 = default");
    assert_eq!(cfg.web_search.session_ttl_hours, 720, "clamped");
    assert_eq!(cfg.warnings.len(), 3);
}

#[test]
fn web_search_state_dir_env_overrides_the_file_and_blank_means_default() {
    let file = web_search_file(r#"{ "state_dir": "from-file" }"#);
    let mut env = env_with_required();
    env.web_search_state_dir = Some("from-env".to_owned());
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(
        cfg.web_search.state_dir.as_deref(),
        Some(std::path::Path::new("from-env"))
    );

    env.web_search_state_dir = Some("   ".to_owned());
    let file = web_search_file(r#"{}"#);
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(
        cfg.web_search.state_dir.as_deref(),
        Some(std::path::Path::new(DEFAULT_WEB_SEARCH_STATE_DIR))
    );
}

#[test]
fn web_search_pacing_values_resolve_and_are_bounded() {
    let file = web_search_file(r#"{ "search_min_interval_ms": 3000, "block_wait_minutes": 15 }"#);
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.web_search.search_min_interval_ms, 3_000);
    assert_eq!(cfg.web_search.block_wait_minutes, 15);
    assert!(cfg.warnings.is_empty());

    let file = web_search_file(r#"{ "search_min_interval_ms": 0, "block_wait_minutes": 9999 }"#);
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.web_search.search_min_interval_ms, 1_500, "0 = default");
    assert_eq!(cfg.web_search.block_wait_minutes, 120, "clamped");
    assert_eq!(cfg.warnings.len(), 2);
}

#[test]
fn web_search_file_values_resolve() {
    let file = web_search_file(
        r#"{ "enabled": true, "compaction_model": " gpt-mini ", "compaction_threshold_chars": 8000,
             "max_queries": 2, "max_results_per_query": 4, "search_timeout_secs": 9,
             "page_timeout_secs": 11, "max_page_bytes": 1000, "search_endpoint": "http://x/html/" }"#,
    );
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    let ws = &cfg.web_search;
    assert!(ws.enabled);
    assert_eq!(ws.compaction_model.as_deref(), Some("gpt-mini"));
    assert_eq!(ws.compaction_threshold_chars, 8_000);
    assert_eq!((ws.max_queries, ws.max_results_per_query), (2, 4));
    assert_eq!((ws.search_timeout_secs, ws.page_timeout_secs), (9, 11));
    assert_eq!(ws.max_page_bytes, 1000);
    assert_eq!(ws.search_endpoint, "http://x/html/");
    assert!(cfg.warnings.is_empty());
}

#[test]
fn web_search_env_beats_file() {
    let file =
        web_search_file(r#"{ "enabled": false, "max_queries": 2, "compaction_model": "a" }"#);
    let env = EnvInputs {
        web_search_enabled: Some(true),
        web_search_max_queries: limit(4),
        web_search_compaction_model: Some("b".into()),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert!(cfg.web_search.enabled);
    assert_eq!(cfg.web_search.max_queries, 4);
    assert_eq!(cfg.web_search.compaction_model.as_deref(), Some("b"));
}

#[test]
fn web_search_blank_compaction_model_means_none() {
    let file = web_search_file(r#"{ "compaction_model": "   " }"#);
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.web_search.compaction_model, None);
}

#[test]
fn web_search_has_no_unlimited_setting() {
    // `0` is "no limit" for the other tools; here it is just invalid.
    let file = web_search_file(r#"{ "max_queries": 0, "max_results_per_query": 0 }"#);
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.web_search.max_queries, 3);
    assert_eq!(cfg.web_search.max_results_per_query, 5);
    assert_eq!(cfg.warnings.len(), 2);
}

#[test]
fn web_search_invalid_numbers_fall_back_with_a_warning() {
    let file = web_search_file(r#"{ "max_queries": -1, "compaction_threshold_chars": 2.5 }"#);
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.web_search.max_queries, 3);
    assert_eq!(cfg.web_search.compaction_threshold_chars, 5_000);
    assert_eq!(cfg.warnings.len(), 2);
}

#[test]
fn web_search_values_above_the_ceiling_are_clamped() {
    let file = web_search_file(
        r#"{ "max_queries": 500, "max_results_per_query": 500, "search_timeout_secs": 9999 }"#,
    );
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.web_search.max_queries, 10);
    assert_eq!(cfg.web_search.max_results_per_query, 10);
    assert_eq!(cfg.web_search.search_timeout_secs, 120);
    assert_eq!(cfg.warnings.len(), 3);
}

#[test]
fn web_search_unknown_field_is_rejected() {
    let parsed: Result<JsonConfig, _> = serde_json::from_str(r#"{ "web_search": { "typo": 1 } }"#);
    assert!(parsed.is_err());
}

// ─── `server_tools` ─────────────────────────────────────────────────────────

fn server_tools_file(json: &str) -> JsonConfig {
    serde_json::from_str(&format!(r#"{{ "server_tools": {json} }}"#)).unwrap()
}

#[test]
fn server_tools_defaults_match_anthropics_loop_and_do_not_pause() {
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(cfg.server_tools.max_rounds, Some(10));
    assert!(!cfg.server_tools.pause_turn, "Claude Code does not resume");
    assert!(cfg.warnings.is_empty());
}

#[test]
fn server_tools_file_values_resolve() {
    let file = server_tools_file(r#"{ "max_rounds": 4, "pause_turn": true }"#);
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.server_tools.max_rounds, Some(4));
    assert!(cfg.server_tools.pause_turn);
}

#[test]
fn server_tools_env_beats_file() {
    let file = server_tools_file(r#"{ "max_rounds": 4, "pause_turn": true }"#);
    let env = EnvInputs {
        server_tools_max_rounds: limit(6),
        server_tools_pause_turn: Some(false),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&file), &env).unwrap();
    assert_eq!(cfg.server_tools.max_rounds, Some(6));
    assert!(!cfg.server_tools.pause_turn);
}

#[test]
fn server_tools_zero_rounds_means_no_limit() {
    let file = server_tools_file(r#"{ "max_rounds": 0 }"#);
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.server_tools.max_rounds, None);
    assert!(cfg.warnings.is_empty(), "0 is the documented spelling");
}

#[test]
fn server_tools_invalid_rounds_fall_back_to_the_default_with_a_warning() {
    for raw in ["-3", "2.5"] {
        let file = server_tools_file(&format!(r#"{{ "max_rounds": {raw} }}"#));
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(cfg.server_tools.max_rounds, Some(10), "{raw}");
        assert!(
            cfg.warnings
                .iter()
                .any(|w| w.message.contains("server_tools.max_rounds")),
            "{raw} must warn"
        );
    }
}

#[test]
fn server_tools_a_large_explicit_limit_is_honoured() {
    let file = server_tools_file(r#"{ "max_rounds": 500 }"#);
    let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
    assert_eq!(cfg.server_tools.max_rounds, Some(500));
}

#[test]
fn server_tools_rejects_unknown_fields() {
    assert!(
        serde_json::from_str::<JsonConfig>(r#"{ "server_tools": { "max_round": 3 } }"#).is_err()
    );
}

#[test]
fn advisor_reasoning_budget_follows_the_limit_rules() {
    // Absent = the default; 0 = no limit; n > 0 = that bound; env > file.
    let absent = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(
        absent.advisor.reasoning_max_tokens,
        Some(DEFAULT_ADVISOR_REASONING_MAX_TOKENS)
    );

    let file = |n: u64| JsonConfig {
        advisor: Some(JsonAdvisorConfig {
            enabled: Some(true),
            reasoning_max_tokens: limit(n),
            ..JsonAdvisorConfig::default()
        }),
        ..JsonConfig::default()
    };
    let cfg = Config::resolve(Some(&file(0)), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.reasoning_max_tokens, None, "0 = no limit");
    let cfg = Config::resolve(Some(&file(5000)), &env_with_required()).unwrap();
    assert_eq!(cfg.advisor.reasoning_max_tokens, Some(5000));

    let env = EnvInputs {
        advisor_reasoning_max_tokens: limit(7000),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&file(5000)), &env).unwrap();
    assert_eq!(cfg.advisor.reasoning_max_tokens, Some(7000), "env > file");
}

#[test]
fn system_message_placement_defaults_to_inline() {
    let cfg = Config::resolve(None, &env_with_required()).unwrap();
    assert_eq!(cfg.system_message_placement, SystemMessagePlacement::Inline);
    assert!(cfg.warnings.is_empty());
}

#[test]
fn system_message_placement_file_env_and_case() {
    let parsed = JsonConfig::parse(
        r#"{"upstream_base_url": "https://api.example.com", "upstream_api_key": "k",
            "system_message_placement": "Instructions"}"#,
    )
    .unwrap();
    let cfg = Config::resolve(Some(&parsed), &env_with_required()).unwrap();
    assert_eq!(
        cfg.system_message_placement,
        SystemMessagePlacement::Instructions
    );
    // The env var wins over the file.
    let env = EnvInputs {
        system_message_placement: Some("inline".to_owned()),
        ..env_with_required()
    };
    let cfg = Config::resolve(Some(&parsed), &env).unwrap();
    assert_eq!(cfg.system_message_placement, SystemMessagePlacement::Inline);
}

#[test]
fn system_message_placement_unknown_value_warns_and_uses_the_default() {
    let parsed = JsonConfig::parse(
        r#"{"upstream_base_url": "https://api.example.com", "upstream_api_key": "k",
            "system_message_placement": "sideways"}"#,
    )
    .unwrap();
    let cfg = Config::resolve(Some(&parsed), &env_with_required()).unwrap();
    assert_eq!(cfg.system_message_placement, SystemMessagePlacement::Inline);
    assert_eq!(cfg.warnings.len(), 1);
    assert!(cfg.warnings[0].message.contains("system_message_placement"));
}

// ── forward_documents ───────────────────────────────────

#[test]
fn forward_documents_is_off_by_default() {
    let cfg = cfg_from_json(
        r#"{"upstream_base_url": "https://api.example.com", "upstream_api_key": "k"}"#,
    );
    assert!(!cfg.forward_documents);
}

#[test]
fn forward_documents_can_be_turned_on_in_the_file_and_overridden_by_env() {
    let parsed = JsonConfig::parse(
        r#"{"upstream_base_url": "https://api.example.com",
                 "upstream_api_key": "k", "forward_documents": true}"#,
    )
    .unwrap();
    assert!(
        Config::resolve(Some(&parsed), &EnvInputs::default())
            .unwrap()
            .forward_documents
    );
    let env = EnvInputs {
        forward_documents: Some(false),
        ..EnvInputs::default()
    };
    assert!(
        !Config::resolve(Some(&parsed), &env)
            .unwrap()
            .forward_documents
    );
}
