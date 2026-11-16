use super::*;

fn flat(param: &str) -> String {
    format!(
        r#"{{"message":"Unsupported parameter: {param}","param":"{param}","code":"unsupported_parameter"}}"#
    )
}

#[test]
fn detects_flat_responses_error_naming_param() {
    let body = flat("temperature");
    assert_eq!(
        unsupported_parameter(StatusCode::BAD_REQUEST, &body),
        Some(RequestParam::Temperature)
    );
}

#[test]
fn detects_wrapped_chat_completions_error_naming_param() {
    let body = r#"{"error":{"message":"Unsupported parameter: 'top_p' is not supported with this model.","type":"invalid_request_error","param":"top_p","code":"unsupported_parameter"}}"#;
    assert_eq!(
        unsupported_parameter(StatusCode::BAD_REQUEST, body),
        Some(RequestParam::TopP)
    );
}

#[test]
fn detects_phrase_without_structured_param_field() {
    // Older gateways put the reason only in the message text.
    let body = r#"{"message":"temperature is not supported for this model"}"#;
    assert_eq!(
        unsupported_parameter(StatusCode::BAD_REQUEST, body),
        Some(RequestParam::Temperature)
    );
}

#[test]
fn detects_plain_text_body() {
    let body = "400 Bad Request: unexpected parameter: top_p";
    assert_eq!(
        unsupported_parameter(StatusCode::BAD_REQUEST, body),
        Some(RequestParam::TopP)
    );
}

#[test]
fn ignores_invalid_value_that_names_param() {
    // Names the parameter, but it is a *value* problem, not a
    // support problem -- must not match, or we'd strip a
    // parameter the model honours.
    let body = r#"{"message":"Invalid value for 'temperature': temperature must be between 0 and 2.","param":"temperature","code":"invalid_request_error"}"#;
    assert_eq!(unsupported_parameter(StatusCode::BAD_REQUEST, body), None);
}

#[test]
fn ignores_model_not_supported() {
    // Belongs to the model-fallback path, not this one.
    let body = r#"{"message":"The model 'gpt-x' does not exist","code":"model_not_found"}"#;
    assert_eq!(unsupported_parameter(StatusCode::BAD_REQUEST, body), None);
}

#[test]
fn ignores_unsupported_signal_that_names_no_known_param() {
    let body =
        r#"{"message":"Unsupported parameter: foo","param":"foo","code":"unsupported_parameter"}"#;
    assert_eq!(unsupported_parameter(StatusCode::BAD_REQUEST, body), None);
}

#[test]
fn ignores_non_request_status_codes() {
    let body = flat("temperature");
    assert_eq!(
        unsupported_parameter(StatusCode::INTERNAL_SERVER_ERROR, &body),
        None
    );
    assert_eq!(unsupported_parameter(StatusCode::NOT_FOUND, &body), None);
}

#[test]
fn accepts_unprocessable_entity_as_well_as_bad_request() {
    let body = r#"{"message":"temperature not allowed"}"#;
    assert_eq!(
        unsupported_parameter(StatusCode::UNPROCESSABLE_ENTITY, body),
        Some(RequestParam::Temperature)
    );
}

#[test]
fn store_learns_and_reports_novelty_once() {
    let store = CapabilityStore::new();
    assert!(
        !store
            .load()
            .is_unsupported("gpt-6-astra", RequestParam::Temperature)
    );

    assert!(store.record_unsupported("gpt-6-astra", RequestParam::Temperature));
    assert!(!store.record_unsupported("gpt-6-astra", RequestParam::Temperature));

    assert!(
        store
            .load()
            .is_unsupported("gpt-6-astra", RequestParam::Temperature)
    );
    assert!(
        !store
            .load()
            .is_unsupported("gpt-6-astra", RequestParam::TopP)
    );
    assert!(
        !store
            .load()
            .is_unsupported("other-model", RequestParam::Temperature)
    );
}

#[test]
fn store_keeps_facts_for_distinct_models_and_params() {
    let store = CapabilityStore::new();
    store.record_unsupported("a", RequestParam::Temperature);
    store.record_unsupported("a", RequestParam::TopP);
    store.record_unsupported("b", RequestParam::TopP);

    assert_eq!(
        store.load().unsupported_for("a"),
        &[RequestParam::Temperature, RequestParam::TopP]
    );
    assert_eq!(store.load().unsupported_for("b"), &[RequestParam::TopP]);
}

#[test]
fn entries_lists_learned_facts_sorted_by_model() {
    let store = CapabilityStore::new();
    store.record_unsupported("zeta", RequestParam::TopP);
    store.record_unsupported("alpha", RequestParam::Temperature);
    store.record_unsupported("alpha", RequestParam::TopP);

    let listed: Vec<_> = store
        .load()
        .entries()
        .map(|(m, p)| (m.to_owned(), p.to_vec()))
        .collect();
    assert_eq!(
        listed,
        vec![
            (
                "alpha".to_owned(),
                vec![RequestParam::Temperature, RequestParam::TopP]
            ),
            ("zeta".to_owned(), vec![RequestParam::TopP]),
        ]
    );
}

#[test]
fn clear_forgets_everything_and_reports_affected_models() {
    let store = CapabilityStore::new();
    store.record_unsupported("a", RequestParam::Temperature);
    store.record_unsupported("b", RequestParam::TopP);
    assert_eq!(store.clear(), 2);

    assert_eq!(store.load().model_count(), 0);
    assert!(!store.load().is_unsupported("a", RequestParam::Temperature));
    // Clearing an empty store is a no-op, not an error.
    assert_eq!(store.clear(), 0);
}

#[test]
fn clear_from_removes_only_the_named_param() {
    let mut req = sample_request();
    RequestParam::Temperature.clear_from(&mut req);
    assert!(req.temperature.is_none());
    assert_eq!(req.top_p, Some(0.9));
}

#[test]
fn apply_known_strips_every_learned_param_for_the_target_model() {
    let store = CapabilityStore::new();
    store.record_unsupported("gpt-6-astra", RequestParam::Temperature);
    store.record_unsupported("gpt-6-astra", RequestParam::TopP);

    let mut req = sample_request();
    req.model = "gpt-6-astra".to_owned();
    let dropped = store.apply_known(&mut req);

    assert_eq!(dropped, vec![RequestParam::Temperature, RequestParam::TopP]);
    assert!(req.temperature.is_none());
    assert!(req.top_p.is_none());
}

#[test]
fn apply_known_leaves_unknown_models_untouched() {
    let store = CapabilityStore::new();
    store.record_unsupported("gpt-6-astra", RequestParam::Temperature);

    let mut req = sample_request();
    req.model = "gpt-5.4-mini".to_owned();
    assert!(store.apply_known(&mut req).is_empty());
    assert_eq!(req.temperature, Some(0.7));
    assert_eq!(req.top_p, Some(0.9));
}

fn sample_request() -> ResponsesRequest {
    ResponsesRequest {
        model: "gpt-5.4-mini".to_owned(),
        input: crate::responses::Input::Text("hi".to_owned()),
        instructions: None,
        tools: None,
        tool_choice: None,
        reasoning: None,
        text: None,
        temperature: Some(0.7),
        top_p: Some(0.9),
        max_output_tokens: None,
        parallel_tool_calls: None,
        store: None,
        previous_response_id: None,
        user: None,
        stream: false,
        metadata: None,
        prompt_cache_key: None,
        prompt_cache_options: None,
    }
}
