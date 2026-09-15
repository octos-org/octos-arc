use super::*;
use serde_json::json;

#[test]
fn compare_protocol_compatible_for_full_protocol_with_known_features() {
    // The full-protocol capabilities advertise every known feature, so any
    // subset (here: the whole known registry) is satisfied.
    let server = UiProtocolCapabilities::full_protocol();
    let required: Vec<&str> = vec![
        UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
        UI_PROTOCOL_FEATURE_USER_QUESTION_V1,
    ];
    assert_eq!(
        compare_protocol(&server, required),
        ProtocolCompat::Compatible
    );
}

#[test]
fn compare_protocol_empty_required_is_always_compatible() {
    let server = UiProtocolCapabilities::full_protocol();
    let required: Vec<&str> = Vec::new();
    assert_eq!(
        compare_protocol(&server, required),
        ProtocolCompat::Compatible
    );
    assert!(compare_protocol(&server, Vec::<&str>::new()).is_compatible());
}

#[test]
fn compare_protocol_schema_incompatible_when_server_older() {
    if UI_PROTOCOL_SCHEMA_VERSION == 0 {
        return; // can't model an older schema below zero
    }
    let mut server = UiProtocolCapabilities::full_protocol();
    server.version.schema_version = UI_PROTOCOL_SCHEMA_VERSION - 1;
    assert_eq!(
        compare_protocol(&server, [UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1]),
        ProtocolCompat::SchemaIncompatible {
            server: UI_PROTOCOL_SCHEMA_VERSION - 1,
            client: UI_PROTOCOL_SCHEMA_VERSION,
        }
    );
}

#[test]
fn compare_protocol_allows_newer_server_schema() {
    // A server ahead of the client (additive forward-compat) is fine.
    let mut server = UiProtocolCapabilities::full_protocol();
    server.version.schema_version = UI_PROTOCOL_SCHEMA_VERSION + 5;
    assert_eq!(
        compare_protocol(&server, [UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1]),
        ProtocolCompat::Compatible
    );
}

#[test]
fn should_roundtrip_optional_turn_tool_context() {
    let notebook = json!({
        "session_id": "local:demo",
        "turn_id": "00000000-0000-0000-0000-000000000001",
        "input": [],
        "tool_context": "notebook"
    });
    let parsed: TurnStartParams = serde_json::from_value(notebook).unwrap();
    assert_eq!(parsed.tool_context.as_deref(), Some("notebook"));
    let wire = serde_json::to_value(parsed).unwrap();
    assert_eq!(wire["tool_context"], json!("notebook"));

    let ordinary: TurnStartParams = serde_json::from_value(json!({
        "session_id": "local:demo",
        "turn_id": "00000000-0000-0000-0000-000000000002",
        "input": []
    }))
    .unwrap();
    assert_eq!(ordinary.tool_context, None);
}

#[test]
fn ui_command_method_matches_expected_transport_name() {
    let cmd = UiCommand::TurnInterrupt(TurnInterruptParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId::new(),
    });

    assert_eq!(cmd.method(), methods::TURN_INTERRUPT);
}

#[test]
fn session_open_params_topic_cwd_and_sandbox_are_additive_and_round_trip() {
    let params = SessionOpenParams {
        session_id: SessionKey("local:demo".into()),
        topic: Some("research".into()),
        profile_id: Some("coding".into()),
        cwd: Some("/repo".into()),
        sandbox: Some(SessionSandboxParams {
            enabled: Some(true),
            network_access: Some(false),
            read_allow_paths: vec!["/repo/docs".into()],
        }),
        after: None,
    };

    let wire = serde_json::to_value(&params).expect("serialize session/open params");
    assert_eq!(wire["topic"], json!("research"));
    assert_eq!(wire["cwd"], json!("/repo"));
    assert_eq!(wire["sandbox"]["enabled"], json!(true));
    assert_eq!(wire["sandbox"]["network_access"], json!(false));
    assert_eq!(wire["sandbox"]["read_allow_paths"], json!(["/repo/docs"]));

    let decoded: SessionOpenParams =
        serde_json::from_value(wire).expect("deserialize session/open params");
    assert_eq!(decoded, params);

    let legacy = json!({
        "session_id": "local:demo",
        "profile_id": "coding"
    });
    let decoded_legacy: SessionOpenParams =
        serde_json::from_value(legacy).expect("legacy session/open params");
    assert!(decoded_legacy.topic.is_none());
    assert!(decoded_legacy.cwd.is_none());
    assert!(decoded_legacy.sandbox.is_none());
}

#[test]
fn profile_local_create_params_legacy_shape_still_deserializes() {
    // Old client shape: {name, username, email}, NO requested_id.
    let legacy = json!({
        "name": "Ada Lovelace",
        "username": "ada",
        "email": "ada@example.com"
    });
    let decoded: ProfileLocalCreateParams =
        serde_json::from_value(legacy).expect("legacy profile/local/create params decode");
    assert!(decoded.requested_id.is_none());
    assert_eq!(decoded.name, "Ada Lovelace");
    assert_eq!(decoded.username, "ada");
    assert_eq!(decoded.email, "ada@example.com");

    // A `None` requested_id serializes to exactly the legacy wire shape
    // (the key is skipped), so an OLDER server sees the bytes unchanged.
    let wire = serde_json::to_value(&decoded).expect("serialize legacy-shaped params");
    assert!(wire.get("requested_id").is_none());
    assert_eq!(wire["name"], json!("Ada Lovelace"));
    assert_eq!(wire["username"], json!("ada"));
    assert_eq!(wire["email"], json!("ada@example.com"));
}

// ----- UPCR-2026-007: capability advertisement on `SessionOpened` -----

#[test]
fn session_open_result_includes_capabilities_field() {
    // Golden: `SessionOpened` serializes a `capabilities` payload that
    // covers protocol version, method/notification surface, and the
    // negotiated feature set so clients can discover the contract
    // in-band per spec § 4 / UPCR-2026-007.
    let opened = SessionOpened {
        session_id: SessionKey("local:demo".into()),
        active_profile_id: None,
        workspace_root: None,
        context: None,
        context_state: None,
        cursor: None,
        panes: None,
        capabilities: UiProtocolCapabilities::first_server_slice(),
        reasoning_effort: None,
    };
    let wire = serde_json::to_value(&opened).expect("serialize SessionOpened");
    let capabilities = wire
        .get("capabilities")
        .expect("SessionOpened must serialize a capabilities field");
    assert_eq!(capabilities["version"]["protocol"], json!(UI_PROTOCOL_V1));
    assert_eq!(
        capabilities["capabilities_schema_version"],
        json!(UI_PROTOCOL_CAPABILITIES_SCHEMA_VERSION)
    );
    assert!(
        capabilities["supported_methods"]
            .as_array()
            .expect("supported_methods array")
            .iter()
            .any(|method| method == &json!(methods::SESSION_OPEN))
    );
    let supported_features = capabilities["supported_features"]
        .as_array()
        .expect("supported_features array");
    for feature in UI_PROTOCOL_KNOWN_FEATURES {
        if *feature == UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2
            || *feature == UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1
        {
            continue;
        }
        assert!(
            supported_features
                .iter()
                .any(|advertised| advertised == &json!(*feature)),
            "first_server_slice must advertise {feature}"
        );
    }
    assert!(
        !supported_features
            .iter()
            .any(|advertised| advertised == &json!(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2)),
        "strictly opt-in v2 must not change the no-header SessionOpened wire"
    );
    assert!(
        !supported_features
            .iter()
            .any(|advertised| advertised == &json!(UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1)),
        "strictly opt-in semantic cache diagnostics must not change the no-header SessionOpened wire"
    );

    // Older payloads (e.g. ledger replays from before the field
    // existed) decode successfully because the field carries
    // `serde(default = "first_server_slice")`.
    let legacy = json!({
        "session_id": "local:demo",
    });
    let decoded: SessionOpened =
        serde_json::from_value(legacy).expect("legacy SessionOpened decode");
    assert_eq!(
        decoded.capabilities,
        UiProtocolCapabilities::first_server_slice()
    );
}

#[test]
fn negotiated_capabilities_advertise_agent_artifact_methods_when_agent_control_requested() {
    let capabilities = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1,
        UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1,
    ]);
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1));
    assert!(capabilities.supports_method(methods::AGENT_ARTIFACT_LIST));
    assert!(capabilities.supports_method(methods::AGENT_ARTIFACT_READ));
}

#[test]
fn unknown_typed_approval_kind_decodes_for_generic_fallback() {
    let value = json!({
        "session_id": "local:demo",
        "approval_id": ApprovalId(Uuid::from_u128(2)),
        "turn_id": TurnId(Uuid::from_u128(1)),
        "tool_name": "future",
        "title": "Future approval",
        "body": "Fallback body remains actionable",
        "approval_kind": "future_kind",
        "typed_details": {
            "kind": "future_kind"
        }
    });

    let decoded: ApprovalRequestedEvent =
        serde_json::from_value(value).expect("unknown typed approval decodes");

    assert_eq!(decoded.approval_kind.as_deref(), Some("future_kind"));
    assert_eq!(
        decoded
            .typed_details
            .as_ref()
            .map(|details| details.kind.as_str()),
        Some("future_kind")
    );
    assert_eq!(decoded.title, "Future approval");
    assert_eq!(decoded.body, "Fallback body remains actionable");
}

// ---- UPCR-2026-023 AskUserQuestion protocol round-trips ----

#[test]
fn user_question_methods_and_feature_are_registered() {
    assert_eq!(methods::USER_QUESTION_RESPOND, "user_question/respond");
    assert_eq!(methods::USER_QUESTION_REQUESTED, "user_question/requested");
    assert_eq!(UI_PROTOCOL_FEATURE_USER_QUESTION_V1, "user_question.v1");
    assert!(UI_PROTOCOL_COMMAND_METHODS.contains(&methods::USER_QUESTION_RESPOND));
    assert!(UI_PROTOCOL_NOTIFICATION_METHODS.contains(&methods::USER_QUESTION_REQUESTED));
    assert!(UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_USER_QUESTION_V1));
    assert_eq!(
        method_capability_gate(methods::USER_QUESTION_RESPOND),
        Some(UI_PROTOCOL_FEATURE_USER_QUESTION_V1)
    );
}

#[test]
fn user_question_respond_decodes_minimal_and_unknown_fields() {
    // Minimal params: client_note omitted, free-text-only answer with no
    // selected_labels, plus an unknown forward-compat sibling field.
    let value = json!({
        "session_id": "local:demo",
        "question_id": QuestionId(Uuid::from_u128(7)),
        "answers": [
            { "free_text": "something else" }
        ],
        "future_field": { "anything": true }
    });
    let decoded: UserQuestionRespondParams =
        serde_json::from_value(value).expect("minimal user_question/respond decodes");
    assert_eq!(decoded.client_note, None);
    assert_eq!(decoded.answers.len(), 1);
    assert!(decoded.answers[0].selected_labels.is_empty());
    assert_eq!(
        decoded.answers[0].free_text.as_deref(),
        Some("something else")
    );
}

#[test]
fn approval_respond_accepts_legacy_and_typed_metadata() {
    let legacy = json!({
        "session_id": "local:demo",
        "approval_id": ApprovalId(Uuid::from_u128(2)),
        "decision": "approve"
    });
    let legacy: ApprovalRespondParams =
        serde_json::from_value(legacy).expect("legacy approval/respond decodes");
    assert_eq!(legacy.approval_scope, None);
    assert_eq!(legacy.client_note, None);

    let typed = json!({
        "session_id": "local:demo",
        "approval_id": ApprovalId(Uuid::from_u128(2)),
        "decision": "deny",
        "approval_scope": "request",
        "client_note": "Denied for this invocation"
    });
    let typed: ApprovalRespondParams =
        serde_json::from_value(typed).expect("typed approval/respond decodes");
    assert_eq!(
        typed.approval_scope.as_deref(),
        Some(approval_scopes::REQUEST)
    );
    assert_eq!(
        typed.client_note.as_deref(),
        Some("Denied for this invocation")
    );
}

#[test]
fn ui_command_builds_and_parses_json_rpc_request() {
    let command = UiCommand::TurnStart(TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId(Uuid::from_u128(1)),
        input: vec![InputItem::Text {
            text: "hello".into(),
        }],
        media: Vec::new(),
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        tool_context: None,
        live_video: false,
    });

    let request = command
        .clone()
        .into_rpc_request("req-1")
        .expect("serialize command params");

    assert_eq!(request.jsonrpc, JSON_RPC_VERSION);
    assert_eq!(request.id, "req-1");
    assert_eq!(request.method, methods::TURN_START);

    let wire = serde_json::to_value(&request).expect("serialize request");
    assert_eq!(wire["jsonrpc"], json!(JSON_RPC_VERSION));
    assert_eq!(wire["params"]["session_id"], json!("local:demo"));
    assert_eq!(wire["params"]["input"][0]["kind"], json!("text"));
    assert!(wire["params"].get("kind").is_none());
    // UPCR-2026-015 (M9-β-1): the three new optional fields are
    // ABSENT on the wire when at their default (empty / None).
    // This locks the back-compat shape — pre-β-1 servers and
    // clients see exactly the bytes they used to.
    assert!(
        wire["params"].get("media").is_none(),
        "empty media MUST be omitted on the wire"
    );
    assert!(
        wire["params"].get("topic").is_none(),
        "absent topic MUST be omitted on the wire"
    );
    assert!(
        wire["params"].get("rewrite_for").is_none(),
        "absent rewrite_for MUST be omitted on the wire"
    );

    let decoded_request: RpcRequest<Value> =
        serde_json::from_value(wire).expect("deserialize request");
    let decoded = UiCommand::from_rpc_request(decoded_request).expect("parse request params");

    assert_eq!(decoded, command);
}

/// UPCR-2026-015 (M9-β-1): the three β-1 fields can co-exist on
/// one envelope (e.g. a `/queue` rewrite that swaps in new media
/// and lands under a topic-scoped session). All three round-trip
/// together.
#[test]
fn turn_start_round_trips_with_all_beta1_fields() {
    let command = UiCommand::TurnStart(TurnStartParams {
        session_id: SessionKey("local:demo".into()),
        turn_id: TurnId(Uuid::from_u128(5)),
        input: vec![InputItem::Text {
            text: "redo with this image".into(),
        }],
        media: vec![FileRef {
            path: "/tmp/replacement.png".into(),
            mime: "image/png".into(),
            size_bytes: 8192,
        }],
        topic: Some("research".into()),
        rewrite_for: Some("cmid-original".into()),
        reasoning_effort: None,
        tool_context: None,
        live_video: false,
    });

    let wire = serde_json::to_value(
        command
            .clone()
            .into_rpc_request("req-all")
            .expect("serialize"),
    )
    .expect("to_value");

    assert_eq!(wire["params"]["topic"], json!("research"));
    assert_eq!(wire["params"]["rewrite_for"], json!("cmid-original"));
    let media = wire["params"]["media"].as_array().expect("media");
    assert_eq!(media.len(), 1);
    assert_eq!(media[0]["path"], json!("/tmp/replacement.png"));

    let decoded_request: RpcRequest<Value> = serde_json::from_value(wire).expect("deserialize");
    let decoded = UiCommand::from_rpc_request(decoded_request).expect("parse");
    assert_eq!(decoded, command);
}

/// Golden: minimal `interrupted: true` round-trip omits the optional
/// diagnostic fields (`reason`, `terminal_state`, `ack_timeout`) so the
/// canonical happy-path wire shape is preserved.
#[test]
fn turn_interrupt_result_minimal_omits_optional_fields() {
    let result = UiRpcResult::TurnInterrupt(TurnInterruptResult::interrupted_ok());
    let value = result
        .clone()
        .into_result_value()
        .expect("serialize turn/interrupt result");
    assert_eq!(value, json!({ "interrupted": true }));
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TURN_INTERRUPT, value)
            .expect("decode turn/interrupt result"),
        result
    );
}

/// Golden: declined interrupt with a `reason` string round-trips through
/// serde without dropping the diagnostic field.
#[test]
fn turn_interrupt_result_round_trips_with_reason() {
    let result = UiRpcResult::TurnInterrupt(TurnInterruptResult::declined("turn_id_mismatch"));
    let value = result
        .clone()
        .into_result_value()
        .expect("serialize turn/interrupt result");
    assert_eq!(
        value,
        json!({ "interrupted": false, "reason": "turn_id_mismatch" })
    );
    assert_eq!(
        UiRpcResult::from_method_and_result(methods::TURN_INTERRUPT, value)
            .expect("decode turn/interrupt result"),
        result
    );
}

#[test]
fn ui_command_parser_reports_invalid_method_and_params() {
    let unknown = RpcRequest::new("req-1", "turn/unknown", json!({}));
    let err = UiCommand::from_rpc_request(unknown).expect_err("reject unknown method");
    assert_eq!(err.code, rpc_error_codes::METHOD_NOT_FOUND);

    let malformed = RpcRequest::new(
        "req-2",
        methods::TURN_INTERRUPT,
        json!({ "session_id": "local:demo" }),
    );
    let err = UiCommand::from_rpc_request(malformed).expect_err("reject malformed params");
    assert_eq!(err.code, rpc_error_codes::INVALID_PARAMS);
    assert!(err.message.contains(methods::TURN_INTERRUPT));
}

#[test]
fn rich_progress_metadata_round_trips_with_extra_fields() {
    let value = json!({
        "kind": "token_cost_update",
        "message": "usage updated",
        "token_cost": {
            "input_tokens": 12,
            "output_tokens": 7,
            "session_cost": 0.0025,
            "currency": "USD"
        },
        "provider": "openai"
    });

    let metadata: UiProgressMetadata =
        serde_json::from_value(value).expect("deserialize rich progress metadata");

    assert_eq!(metadata.kind, progress_kinds::TOKEN_COST_UPDATE);
    assert_eq!(metadata.message.as_deref(), Some("usage updated"));
    assert_eq!(
        metadata
            .token_cost
            .as_ref()
            .and_then(|cost| cost.input_tokens),
        Some(12)
    );
    assert_eq!(
        metadata.extra.get("provider"),
        Some(&Value::String("openai".into()))
    );

    let encoded = serde_json::to_value(&metadata).expect("serialize rich progress metadata");
    assert_eq!(encoded["provider"], json!("openai"));
    assert_eq!(encoded["token_cost"]["session_cost"], json!(0.0025));
}

#[test]
fn rich_progress_event_uses_standalone_notification_method() {
    let metadata = UiProgressMetadata::file_mutation(UiFileMutationNotice::new(
        "src/main.rs",
        file_mutation_operations::WRITE,
    ));
    let event = UiProgressEvent::new(
        SessionKey("local:demo".into()),
        Some(TurnId(Uuid::from_u128(3))),
        metadata,
    );

    let notification = event
        .clone()
        .into_rpc_notification()
        .expect("serialize progress notification");

    assert_eq!(notification.method, methods::PROGRESS_UPDATED);
    assert_eq!(
        notification.params["metadata"]["kind"],
        json!("file_mutation")
    );
    assert_eq!(
        notification.params["metadata"]["file_mutation"]["operation"],
        json!("write")
    );

    let decoded =
        UiProgressEvent::from_rpc_notification(notification).expect("decode progress notification");
    assert_eq!(decoded, event);
}

#[test]
fn rpc_success_and_error_responses_use_json_rpc_v2() {
    let success = RpcResponse::success("req-1", json!({ "ok": true }));
    assert_eq!(success.jsonrpc, JSON_RPC_VERSION);
    assert!(success.is_jsonrpc_v2());

    let error = RpcErrorResponse::new(None, RpcError::parse_error("invalid json"));
    let wire = serde_json::to_value(&error).expect("serialize error response");

    assert_eq!(
        wire,
        json!({
            "jsonrpc": JSON_RPC_VERSION,
            "id": null,
            "error": {
                "code": rpc_error_codes::PARSE_ERROR,
                "message": "invalid json"
            }
        })
    );
}

#[test]
fn ui_notification_builds_and_parses_json_rpc_notification() {
    let event = UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        turn_id: TurnId(Uuid::from_u128(2)),
        text: "partial".into(),
    });

    let notification = event
        .clone()
        .into_rpc_notification()
        .expect("serialize notification params");

    assert_eq!(notification.jsonrpc, JSON_RPC_VERSION);
    assert_eq!(notification.method, methods::MESSAGE_DELTA);

    let wire = serde_json::to_value(&notification).expect("serialize notification");
    assert_eq!(wire["params"]["text"], json!("partial"));
    assert!(wire["params"].get("kind").is_none());

    let decoded_notification: RpcNotification<Value> =
        serde_json::from_value(wire).expect("deserialize notification");
    let decoded = UiNotification::from_rpc_notification(decoded_notification)
        .expect("parse notification params");

    assert_eq!(decoded, event);
}

/// #2019 — `background/activity` is the HUMAN sink over events that today
/// only wake the model. The routing key is the session that OWNS the emitter,
/// and it must survive the wire boundary intact: an event that loses (or never
/// carries) `session_id` renders in whichever session happens to be focused
/// (octos-tui#461/#466/#483). Attribution and the visible drop marker must
/// survive too.
#[test]
fn should_route_on_owning_session_when_background_activity_crosses_the_wire() {
    let owner = SessionKey("dev:local:tui#coding".into());
    let other = SessionKey("dev:local:tui#review".into());
    let event = BackgroundActivityEvent {
        session_id: owner.clone(),
        profile_id: Some("dev".into()),
        origin_kind: "monitor".into(),
        origin_id: "mon-7".into(),
        origin_label: Some("ci-tail".into()),
        text: "ERROR bus test flaked".into(),
        emitted_at_ms: 1_760_000_000_000,
        dropped_count: None,
        suppressed: false,
    };
    let notification = UiNotification::BackgroundActivity(event.clone());

    assert_eq!(notification.method(), methods::BACKGROUND_ACTIVITY);
    assert!(UI_PROTOCOL_NOTIFICATION_METHODS.contains(&methods::BACKGROUND_ACTIVITY));
    assert!(UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_BACKGROUND_ACTIVITY_V1));

    let rpc = notification
        .clone()
        .into_rpc_notification()
        .expect("serialize background/activity");
    assert_eq!(rpc.method, methods::BACKGROUND_ACTIVITY);
    // ROUTING, not merely emission: the wire frame names the OWNING session,
    // never the sibling a client might happen to have focused.
    assert_eq!(rpc.params["session_id"], owner.0);
    assert_ne!(rpc.params["session_id"], other.0);
    // Attribution rides along — an unattributed line reads as the master.
    assert_eq!(rpc.params["origin_kind"], "monitor");
    assert_eq!(rpc.params["origin_id"], "mon-7");
    assert_eq!(rpc.params["origin_label"], "ci-tail");

    let decoded = UiNotification::from_rpc_notification(rpc).expect("decode background/activity");
    assert_eq!(decoded, notification);
    assert_eq!(decoded.session_id(), &owner);
    assert_eq!(decoded.topic(), Some("coding"));
}

#[test]
fn notification_round_trips_through_json() {
    let event = UiNotification::Warning(WarningEvent {
        session_id: SessionKey("local:demo".into()),
        turn_id: None,
        code: "mock_warning".into(),
        message: "mock payload".into(),
    });

    let json = serde_json::to_string(&event).expect("serialize event");
    let decoded: UiNotification = serde_json::from_str(&json).expect("deserialize event");

    assert_eq!(decoded, event);
}

#[test]
fn progress_updated_round_trip_minimal() {
    let event = UiNotification::ProgressUpdated(ProgressUpdatedEvent::new(
        SessionKey("local:demo".into()),
        None,
        UiProgressMetadata::new(progress_kinds::STATUS),
    ));

    let notification = event
        .clone()
        .into_rpc_notification()
        .expect("serialize progress/updated notification");
    assert_eq!(notification.method, methods::PROGRESS_UPDATED);

    let wire = serde_json::to_value(&notification).expect("serialize wire");
    assert_eq!(
        wire,
        json!({
            "jsonrpc": "2.0",
            "method": "progress/updated",
            "params": {
                "session_id": "local:demo",
                "metadata": { "kind": "status" }
            }
        })
    );

    let decoded_notification: RpcNotification<Value> =
        serde_json::from_value(wire).expect("deserialize wire");
    let decoded = UiNotification::from_rpc_notification(decoded_notification)
        .expect("decode progress/updated notification");
    assert_eq!(decoded, event);
}

// ----- M9-FIX-08 approval/cancelled wire registration -----

#[test]
fn approval_cancelled_notification_registers_method_and_round_trips() {
    let event = UiNotification::ApprovalCancelled(ApprovalCancelledEvent::turn_interrupted(
        SessionKey("local:demo".into()),
        ApprovalId::new(),
        TurnId::new(),
    ));
    assert_eq!(event.method(), methods::APPROVAL_CANCELLED);
    assert!(UI_PROTOCOL_NOTIFICATION_METHODS.contains(&methods::APPROVAL_CANCELLED));

    let rpc = event
        .clone()
        .into_rpc_notification()
        .expect("serialize approval/cancelled");
    let decoded =
        UiNotification::from_rpc_notification(rpc).expect("deserialize approval/cancelled");
    assert_eq!(decoded, event);
}

#[test]
fn approval_decision_unknown_falls_through() {
    let decoded: ApprovalDecision =
        serde_json::from_value(json!("future_decision_kind")).expect("decode unknown decision");
    assert_eq!(
        decoded,
        ApprovalDecision::Unknown("future_decision_kind".into())
    );

    let re_encoded = serde_json::to_value(&decoded).expect("encode unknown decision");
    assert_eq!(re_encoded, json!("future_decision_kind"));

    // Known wire values still hit the typed variants.
    let approve: ApprovalDecision =
        serde_json::from_value(json!("approve")).expect("decode approve");
    assert_eq!(approve, ApprovalDecision::Approve);
    assert_eq!(
        serde_json::to_value(&ApprovalDecision::Deny).expect("encode deny"),
        json!("deny")
    );
}

// ----- Spec §10 typed error taxonomy round-trips (M9-FIX-02) -----

/// Helper: serialize an `RpcError` and decode it back, asserting that
/// `code` survives the trip and `data` is preserved (or absent).
fn round_trip_rpc_error(err: &RpcError) -> RpcError {
    let value = serde_json::to_value(err).expect("serialize RpcError");
    serde_json::from_value(value).expect("deserialize RpcError")
}

#[test]
fn approval_not_pending_carries_recorded_decision() {
    let approve = RpcError::approval_not_pending(ApprovalDecision::Approve);
    let json = serde_json::to_value(&approve).expect("serialize approval_not_pending");
    assert_eq!(json["code"], json!(-32011));
    assert_eq!(json["data"]["recorded_decision"], json!("approve"));
    assert_eq!(
        round_trip_rpc_error(&approve).recorded_decision(),
        Some(ApprovalDecision::Approve),
    );

    let deny = RpcError::approval_not_pending(ApprovalDecision::Deny);
    assert_eq!(
        round_trip_rpc_error(&deny).recorded_decision(),
        Some(ApprovalDecision::Deny),
    );

    // Wrong code must not pretend to carry a recorded decision.
    let mislabeled = RpcError::new(rpc_error_codes::INTERNAL_ERROR, "x")
        .with_data(json!({ "recorded_decision": "approve" }));
    assert_eq!(mislabeled.recorded_decision(), None);
}

#[test]
fn cursor_out_of_range_round_trip() {
    let cursor = UiCursor {
        stream: "local:demo".into(),
        seq: 7,
    };
    let head = UiCursor {
        stream: "local:demo".into(),
        seq: 12,
    };
    let err = RpcError::cursor_out_of_range(&cursor, &head);
    assert_eq!(err.code, rpc_error_codes::CURSOR_OUT_OF_RANGE);
    let data = round_trip_rpc_error(&err).data.expect("carries data");
    assert_eq!(data["cursor"]["seq"], json!(7));
    assert_eq!(data["ledger_head"]["seq"], json!(12));
    assert_eq!(data["cursor"]["stream"], json!("local:demo"));
}

#[test]
fn decode_malformed_result_returns_malformed_result_not_invalid_params() {
    // Bad inbound result must surface MALFORMED_RESULT, never INVALID_PARAMS.
    let bad = json!({ "definitely_not": "a session_open result" });
    let err = UiRpcResult::from_method_and_result(methods::SESSION_OPEN, bad)
        .expect_err("malformed result should fail to decode");
    assert_eq!(err.code, rpc_error_codes::MALFORMED_RESULT);
    assert_ne!(err.code, rpc_error_codes::INVALID_PARAMS);
    assert!(err.message.contains(methods::SESSION_OPEN));
}

#[test]
fn unsupported_capability_result_round_trips() {
    // `from_method_and_result` must reconstruct UnsupportedCapability
    // even though the originating method is `approval/respond`.
    let result = UiRpcResult::UnsupportedCapability(UnsupportedCapabilityResult::method(
        methods::APPROVAL_RESPOND,
        "approval is pending",
    ));
    let value = result
        .clone()
        .into_result_value()
        .expect("serialize unsupported result");
    let decoded = UiRpcResult::from_method_and_result(methods::APPROVAL_RESPOND, value)
        .expect("decode unsupported result");
    assert_eq!(decoded, result);
    assert_eq!(decoded.kind(), UiResultKind::UnsupportedCapability);

    // Regular ApprovalRespond payload must still route to its typed variant.
    let regular = UiRpcResult::ApprovalRespond(ApprovalRespondResult::accepted(ApprovalId::new()))
        .into_result_value()
        .expect("serialize approval respond");
    let decoded_regular = UiRpcResult::from_method_and_result(methods::APPROVAL_RESPOND, regular)
        .expect("decode approval respond");
    assert_eq!(decoded_regular.kind(), UiResultKind::ApprovalRespond);
}

#[test]
fn unknown_id_constructors_round_trip_with_typed_data() {
    // One round-trip per `unknown_*` constant.
    let turn = TurnId(Uuid::from_u128(42));
    let approval = ApprovalId(Uuid::from_u128(7));
    let preview = PreviewId(Uuid::from_u128(11));
    let task = TaskId(Uuid::from_u128(99));
    let cases: [(RpcError, i64, &str, &str, Value); 5] = [
        (
            RpcError::unknown_session("local:demo"),
            -32100,
            "unknown_session",
            "session_id",
            json!("local:demo"),
        ),
        (
            RpcError::unknown_turn(&turn),
            -32101,
            "unknown_turn",
            "turn_id",
            json!(turn.0.to_string()),
        ),
        (
            RpcError::unknown_approval_id(&approval),
            -32102,
            "unknown_approval",
            "approval_id",
            json!(approval.0.to_string()),
        ),
        (
            RpcError::unknown_preview_id(&preview),
            -32103,
            "unknown_preview",
            "preview_id",
            json!(preview.0.to_string()),
        ),
        (
            RpcError::unknown_task_id(&task),
            -32104,
            "unknown_task",
            "task_id",
            json!(task.to_string()),
        ),
    ];
    for (err, code, kind, key, value) in cases {
        assert_eq!(err.code, code);
        let decoded = round_trip_rpc_error(&err);
        assert_eq!(decoded.code, code);
        let data = decoded.data.unwrap();
        assert_eq!(data["kind"], json!(kind));
        assert_eq!(data[key], value);
    }
}

#[test]
fn closed_string_enums_capture_unknown_wire_values() {
    // ApprovalRespondStatus
    let status: ApprovalRespondStatus =
        serde_json::from_value(json!("queued_for_review")).expect("decode status");
    assert_eq!(
        status,
        ApprovalRespondStatus::Unknown("queued_for_review".into())
    );
    assert_eq!(
        serde_json::to_value(&status).expect("encode status"),
        json!("queued_for_review")
    );
    assert_eq!(
        serde_json::to_value(&ApprovalRespondStatus::Accepted).expect("encode accepted"),
        json!("accepted")
    );

    // DiffPreviewFileStatus
    let file_status: DiffPreviewFileStatus =
        serde_json::from_value(json!("type_changed")).expect("decode file status");
    assert_eq!(
        file_status,
        DiffPreviewFileStatus::Unknown("type_changed".into())
    );
    assert_eq!(
        serde_json::to_value(&file_status).expect("encode file status"),
        json!("type_changed")
    );
    assert_eq!(
        serde_json::to_value(&DiffPreviewFileStatus::Renamed).expect("encode renamed"),
        json!("renamed")
    );
}

#[test]
fn input_item_unknown_kind_falls_through() {
    // Tagged input items with future kinds decode to the Unknown unit
    // variant rather than erroring. Known kinds still decode normally.
    let unknown: InputItem = serde_json::from_value(json!({
        "kind": "voice",
        "audio_url": "https://example.test/clip.wav"
    }))
    .expect("decode unknown input item kind");
    assert_eq!(unknown, InputItem::Unknown);

    let known: InputItem = serde_json::from_value(json!({
        "kind": "text",
        "text": "hello"
    }))
    .expect("decode text input item");
    assert_eq!(
        known,
        InputItem::Text {
            text: "hello".into()
        }
    );
}

#[test]
fn rpc_error_codes_partition_is_disjoint() {
    // Application-layer codes must live in -32100..=-32199; the
    // spec-pinned APPROVAL_NOT_PENDING is the documented exception.
    for code in [
        rpc_error_codes::UNKNOWN_SESSION,
        rpc_error_codes::UNKNOWN_TURN,
        rpc_error_codes::UNKNOWN_APPROVAL_ID,
        rpc_error_codes::UNKNOWN_PREVIEW_ID,
        rpc_error_codes::UNKNOWN_TASK_ID,
        rpc_error_codes::APPROVAL_CANCELLED,
        rpc_error_codes::CURSOR_OUT_OF_RANGE,
        rpc_error_codes::CURSOR_INVALID,
        rpc_error_codes::PERMISSION_DENIED,
        rpc_error_codes::UNSUPPORTED_CAPABILITY,
        rpc_error_codes::RUNTIME_NOT_READY,
        rpc_error_codes::MALFORMED_RESULT,
        rpc_error_codes::RATE_LIMITED,
    ] {
        assert!(
            (-32199..=-32100).contains(&code),
            "{code} outside -32100..=-32199",
        );
    }
    assert_eq!(rpc_error_codes::APPROVAL_NOT_PENDING, -32011);
    assert_eq!(rpc_error_codes::APPROVAL_CANCELLED, -32105);
}

#[test]
fn first_server_capabilities_advertise_approval_cancelled() {
    let capabilities = UiProtocolCapabilities::first_server_slice();
    assert!(
        capabilities
            .supported_notifications
            .iter()
            .any(|method| method == methods::APPROVAL_CANCELLED),
        "approval/cancelled must be advertised so clients can render it",
    );
}

// ----- M9 review fix MEDIUM #4 (UPCR-2026-004): Cancelled task state -----

#[test]
fn task_runtime_state_cancelled_round_trips_as_snake_case_cancelled() {
    // Wire form must be exactly `"cancelled"` so the agent's
    // `TaskLifecycleState::Cancelled` (also `snake_case`-serialized as
    // `"cancelled"`) flows through the protocol mapper without falling
    // back to `Running`. UPCR-2026-004 promises `"cancelled"` (the British
    // spelling) as the wire literal.
    let value = serde_json::to_value(TaskRuntimeState::Cancelled).expect("serialize Cancelled");
    assert_eq!(value, json!("cancelled"));
    let parsed: TaskRuntimeState = serde_json::from_value(value).expect("round-trip Cancelled");
    assert_eq!(parsed, TaskRuntimeState::Cancelled);
}

#[test]
fn plan_updated_event_round_trips_and_advertises() {
    let event = UiNotification::PlanUpdated(PlanUpdatedEvent {
        // No topic in the key → `stamp_topic_from_session` is a no-op, so the
        // absent-`topic` assertion below holds and the round-trip is exact.
        session_id: SessionKey("acct:web:tui".into()),
        topic: None,
        turn_id: None,
        plan: UiPlanRecord {
            items: vec![
                UiPlanItem {
                    id: "1".into(),
                    title: "web P3: PWA manifest + bridge hygiene".into(),
                    status: PlanItemStatus::Completed,
                    priority: Some("P3".into()),
                },
                UiPlanItem {
                    id: "2".into(),
                    title: "memory panel (octos endpoints + web UI)".into(),
                    status: PlanItemStatus::InProgress,
                    priority: None,
                },
            ],
            title: Some("Building memory panel…".into()),
            updated_at_ms: 1_700_000_000_000,
        },
    });
    assert_eq!(event.method(), methods::PLAN_UPDATED);

    let rpc = event
        .clone()
        .into_rpc_notification()
        .expect("serialize plan/updated");
    // Status is snake_case on the wire; an absent `priority`/`topic` stays
    // absent (no `null` leakage that would clobber a cached value).
    assert_eq!(rpc.params["plan"]["items"][1]["status"], "in_progress");
    assert!(rpc.params["plan"]["items"][1].get("priority").is_none());
    assert!(rpc.params.get("topic").is_none());

    let decoded = UiNotification::from_rpc_notification(rpc).expect("deserialize plan/updated");
    assert_eq!(decoded, event);

    // Advertised in both the notification and feature registries.
    assert!(UI_PROTOCOL_NOTIFICATION_METHODS.contains(&methods::PLAN_UPDATED));
    assert!(UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_PLAN_TODOS_V1));
}

// ===== UPCR-2026-009 / -010 / -011 / -012 golden tests (PR G) =====

fn sample_session_id() -> SessionKey {
    SessionKey("local:demo".into())
}

fn sample_cursor() -> UiCursor {
    UiCursor {
        stream: "local:demo".into(),
        seq: 142,
    }
}

fn sample_persisted_at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-04-30T12:00:00Z")
        .expect("rfc3339 parse")
        .with_timezone(&Utc)
}

#[test]
fn session_rollback_command_and_result_round_trip() {
    // Command decodes from its wire method name.
    let command = UiCommand::SessionRollback(SessionRollbackParams {
        session_id: sample_session_id(),
        num_turns: 1,
    });
    assert_eq!(command.method(), methods::SESSION_ROLLBACK);
    let request = command.clone().into_rpc_request("r1").expect("encode");
    assert_eq!(request.method, methods::SESSION_ROLLBACK);
    let decoded = UiCommand::from_rpc_request(request).expect("decode command");
    assert_eq!(decoded, command);

    // Result carries the trimmed hydrate projection and round-trips through
    // the method-keyed decode path.
    let result = SessionRollbackResult {
        dropped_turns: 1,
        thread: SessionHydrateResult {
            session_id: sample_session_id(),
            cursor: sample_cursor(),
            context: None,
            context_state: None,
            messages: Some(vec![]),
            threads: None,
            turns: None,
            pending_approvals: None,
            pending_questions: None,
            replayed_envelopes: None,
            replayed_tool_envelopes: None,
        },
    };
    let wire = UiRpcResult::SessionRollback(result.clone());
    assert_eq!(wire.method(), Some(methods::SESSION_ROLLBACK));
    assert_eq!(wire.kind(), UiResultKind::SessionRollback);
    let value = wire.into_result_value().expect("encode result");
    let decoded =
        UiRpcResult::from_method_and_result(methods::SESSION_ROLLBACK, value).expect("decode");
    assert_eq!(decoded, UiRpcResult::SessionRollback(result));
    assert_eq!(
        first_server_result_kind_for_method(methods::SESSION_ROLLBACK),
        Some(UiResultKind::SessionRollback)
    );
}

/// Asserts the new `tool_call_id` field round-trips through serde on
/// both `task/updated` and `turn/spawn_complete`. The chip-flip
/// race in the client browser hinged on having this field on the
/// wire, so a regression here would silently re-introduce the bug
/// even if the constructors still type-check.
#[test]
fn task_updated_and_spawn_complete_events_round_trip_tool_call_id() {
    // `TaskUpdatedEvent` with `tool_call_id` set.
    let task_event = TaskUpdatedEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        task_id: TaskId(Uuid::from_u128(0xDEADBEEF)),
        tool_call_id: Some("call_podcast_generate_42".into()),
        title: "podcast_generate".into(),
        state: TaskRuntimeState::Completed,
        runtime_detail: Some("rendered output.mp3".into()),
        source: None,
        role: None,
        summary: None,
        artifact_count: None,
        runtime_policy_stamp: None,
        turn_id: None,
    };
    let task_value = serde_json::to_value(&task_event).expect("serialize task_updated");
    assert_eq!(
        task_value.get("tool_call_id"),
        Some(&json!("call_podcast_generate_42")),
        "tool_call_id must appear on the wire",
    );
    let parsed_task: TaskUpdatedEvent =
        serde_json::from_value(task_value).expect("deserialize task_updated");
    assert_eq!(parsed_task, task_event);

    // `TaskUpdatedEvent` with `tool_call_id == None` omits on the wire
    // (legacy daemons / synthetic paths).
    let task_legacy = TaskUpdatedEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        task_id: TaskId(Uuid::from_u128(1)),
        tool_call_id: None,
        title: "legacy".into(),
        state: TaskRuntimeState::Running,
        runtime_detail: None,
        source: None,
        role: None,
        summary: None,
        artifact_count: None,
        runtime_policy_stamp: None,
        turn_id: None,
    };
    let legacy_value = serde_json::to_value(&task_legacy).expect("serialize legacy");
    assert!(
        legacy_value.get("tool_call_id").is_none(),
        "absent tool_call_id must omit so legacy daemons parse",
    );
    // Defensive: a JSON payload from an even-older daemon that lacks
    // the field entirely still parses via `#[serde(default)]`.
    let bare_legacy_json = json!({
        "session_id": "local:demo",
        "task_id": TaskId(Uuid::from_u128(1)),
        "title": "legacy",
        "state": "running",
    });
    let parsed_legacy: TaskUpdatedEvent =
        serde_json::from_value(bare_legacy_json).expect("deserialize legacy bare");
    assert_eq!(parsed_legacy.tool_call_id, None);

    // `TurnSpawnCompleteEvent` with `tool_call_id` set.
    let spawn_event = TurnSpawnCompleteEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        turn_id: None,
        thread_id: None,
        task_id: "task_podcast_42".into(),
        tool_call_id: Some("call_podcast_generate_42".into()),
        response_to_client_message_id: None,
        seq: 7,
        message_id: "msg-spawn-7".into(),
        source: "background".into(),
        cursor: UiCursor {
            stream: "local:demo".into(),
            seq: 7,
        },
        persisted_at: sample_persisted_at(),
        content: "🎙 podcast delivered".into(),
        media: vec!["output.mp3".into()],
    };
    let spawn_value = serde_json::to_value(&spawn_event).expect("serialize spawn_complete");
    assert_eq!(
        spawn_value.get("tool_call_id"),
        Some(&json!("call_podcast_generate_42")),
        "tool_call_id must appear on the wire",
    );
    let parsed_spawn: TurnSpawnCompleteEvent =
        serde_json::from_value(spawn_value).expect("deserialize spawn_complete");
    assert_eq!(parsed_spawn, spawn_event);

    // Defensive: a `turn/spawn_complete` payload from an older daemon
    // that lacks the field entirely still parses via
    // `#[serde(default)]`.
    let bare_spawn_json = json!({
        "session_id": "local:demo",
        "task_id": "task_legacy",
        "seq": 1,
        "message_id": "msg-legacy",
        "source": "background",
        "cursor": { "stream": "local:demo", "seq": 1 },
        "persisted_at": "2026-01-01T00:00:00Z",
        "content": "done",
    });
    let parsed_legacy_spawn: TurnSpawnCompleteEvent =
        serde_json::from_value(bare_spawn_json).expect("deserialize legacy spawn bare");
    assert_eq!(parsed_legacy_spawn.tool_call_id, None);
}

// ===== M12 Phase D-1 auxiliary REST → WS frames =====

#[test]
fn launch_resolve_round_trips_through_rpc_envelope() {
    let command = UiCommand::LaunchResolve(LaunchResolveParams {
        cwd: "/tmp/project".into(),
        profile_id: Some("glm".into()),
    });
    let rpc = command
        .clone()
        .into_rpc_request("req")
        .expect("serialize command");
    assert_eq!(rpc.method, methods::LAUNCH_RESOLVE);
    let decoded = UiCommand::from_rpc_request(rpc).expect("decode command");
    assert_eq!(decoded, command);
}

#[test]
fn launch_resolve_is_gated_on_session_workspace_cwd_v1() {
    let none = UiProtocolCapabilities::for_negotiated_features(Vec::<String>::new());
    assert!(
        !none.supports_method(methods::LAUNCH_RESOLVE),
        "launch/resolve must NOT be advertised without session.workspace_cwd.v1"
    );
    let with_feature = UiProtocolCapabilities::for_negotiated_features([
        UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
    ]);
    assert!(
        with_feature.supports_method(methods::LAUNCH_RESOLVE),
        "launch/resolve must be advertised once session.workspace_cwd.v1 is negotiated"
    );
}

/// Codex review 2026-05-12 (MEDIUM 1): the new
/// `RpcError::not_found(resource_type, identifier)` constructor
/// must carry the resource tag + identifier in `data` so clients
/// can distinguish a content-row miss from a session miss without
/// parsing message strings. Pinned via JSON golden.
#[test]
fn rpc_error_not_found_carries_typed_resource_data() {
    let err = RpcError::not_found("content", "c-99");
    assert_eq!(err.code, rpc_error_codes::RESOURCE_NOT_FOUND);
    let value = serde_json::to_value(&err).expect("serialize");
    assert_eq!(value.get("code"), Some(&json!(-32170)));
    let data = value.get("data").expect("data present");
    assert_eq!(data.get("kind"), Some(&json!("not_found")));
    assert_eq!(data.get("resource_type"), Some(&json!("content")));
    assert_eq!(data.get("identifier"), Some(&json!("c-99")));
}

// ===== UPCR-2026-014 M9-γ projection envelope golden tests =====

fn envelope(seq: u64, payload: Payload) -> Envelope {
    Envelope {
        thread_id: "thread-1".into(),
        seq,
        client_message_id: None,
        payload,
    }
}

#[test]
fn golden_envelope_assistant_delta_round_trips() {
    let env = envelope(
        1,
        Payload::AssistantDelta {
            text: "hello".into(),
        },
    );
    let value = serde_json::to_value(&env).expect("serialize");
    assert_eq!(value.get("thread_id"), Some(&json!("thread-1")));
    assert_eq!(value.get("seq"), Some(&json!(1)));
    assert!(
        value.get("client_message_id").is_none(),
        "client_message_id is absent on internal events"
    );
    let payload = value.get("payload").expect("payload");
    assert_eq!(payload.get("type"), Some(&json!("assistant_delta")));
    assert_eq!(
        payload.get("data").and_then(|d| d.get("text")),
        Some(&json!("hello"))
    );
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_user_message_round_trips() {
    // user_message envelopes are the turn root — server-mirrored
    // from the client send. They carry `client_message_id` (and
    // ONLY they do, per UPCR-2026-014 § 14.1) so the optimistic
    // <GhostBubble> overlay can match its server reflection. The
    // projection itself MUST NOT consult the field.
    let env = Envelope {
        thread_id: "thread-1".into(),
        seq: 1,
        client_message_id: Some("cmid-abc".into()),
        payload: Payload::UserMessage {
            text: "Q1 — what's 2+2?".into(),
            files: vec![FileRef {
                path: "/tmp/upload.png".into(),
                mime: "image/png".into(),
                size_bytes: 2048,
            }],
        },
    };
    let value = serde_json::to_value(&env).expect("serialize");
    assert_eq!(value.get("client_message_id"), Some(&json!("cmid-abc")));
    let payload = value.get("payload").expect("payload");
    assert_eq!(payload.get("type"), Some(&json!("user_message")));
    let data = payload.get("data").expect("data");
    assert_eq!(data.get("text"), Some(&json!("Q1 — what's 2+2?")));
    let files = data.get("files").and_then(|f| f.as_array()).expect("files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].get("path"), Some(&json!("/tmp/upload.png")));
    assert_eq!(files[0].get("mime"), Some(&json!("image/png")));
    assert_eq!(files[0].get("size_bytes"), Some(&json!(2048)));
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_tool_fidelity_round_trip_and_legacy_decode() {
    // Enriched shape: arguments/output previews + duration survive the
    // wire round-trip.
    let start = envelope(
        4,
        Payload::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "shell".into(),
            arguments_preview: Some("command: \"cargo test\"".into()),
        },
    );
    let end = envelope(
        5,
        Payload::ToolEnd {
            tool_call_id: "tc-1".into(),
            status: EnvelopeToolEndStatus::Complete,
            error: None,
            reason: None,
            output_preview: Some("test result: ok. 815 passed".into()),
            duration_ms: Some(1234),
        },
    );
    for env in [start, end] {
        let value = serde_json::to_value(&env).expect("serialize");
        let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
        assert_eq!(parsed, env);
    }

    // Legacy wire (envelopes persisted before the fidelity fields
    // existed) must still decode — fields default to None. Build the
    // legacy shape by stripping the new keys from a modern envelope so
    // the fixture tracks the real tag/content encoding.
    let strip = |env: &Envelope, keys: &[&str]| -> Envelope {
        let mut value = serde_json::to_value(env).expect("serialize");
        let data = value["payload"]["data"]
            .as_object_mut()
            .expect("payload data object");
        for key in keys {
            data.remove(*key);
        }
        serde_json::from_value(value).expect("legacy envelope decodes")
    };
    let start = envelope(
        8,
        Payload::ToolStart {
            tool_call_id: "tc-9".into(),
            name: "read_file".into(),
            arguments_preview: Some("path: \"x\"".into()),
        },
    );
    match strip(&start, &["arguments_preview"]).payload {
        Payload::ToolStart {
            arguments_preview, ..
        } => assert_eq!(arguments_preview, None),
        other => panic!("expected ToolStart, got {other:?}"),
    }
    let end = envelope(
        9,
        Payload::ToolEnd {
            tool_call_id: "tc-9".into(),
            status: EnvelopeToolEndStatus::Complete,
            error: None,
            reason: None,
            output_preview: Some("ok".into()),
            duration_ms: Some(1),
        },
    );
    match strip(&end, &["output_preview", "duration_ms"]).payload {
        Payload::ToolEnd {
            output_preview,
            duration_ms,
            ..
        } => {
            assert_eq!(output_preview, None);
            assert_eq!(duration_ms, None);
        }
        other => panic!("expected ToolEnd, got {other:?}"),
    }
}

#[test]
fn golden_envelope_turn_completed_round_trips() {
    let env = envelope(
        9,
        Payload::TurnCompleted {
            token_usage: EnvelopeTokenUsage {
                input_tokens: 100,
                output_tokens: 250,
                reasoning_tokens: 0,
                cache_read_tokens: 80,
                cache_write_tokens: 0,
            },
        },
    );
    let value = serde_json::to_value(&env).expect("serialize");
    assert_eq!(
        value.get("payload").and_then(|p| p.get("type")),
        Some(&json!("turn_completed"))
    );
    let usage = value
        .get("payload")
        .and_then(|p| p.get("data"))
        .and_then(|d| d.get("token_usage"))
        .expect("token_usage");
    assert_eq!(usage.get("input_tokens"), Some(&json!(100)));
    assert_eq!(usage.get("output_tokens"), Some(&json!(250)));
    // Zero fields are omitted on the wire.
    assert!(usage.get("reasoning_tokens").is_none());
    assert!(usage.get("cache_write_tokens").is_none());
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_token_usage_zero_default_round_trips() {
    // turn_completed with all-zero usage emits an empty `token_usage: {}`.
    let env = envelope(
        10,
        Payload::TurnCompleted {
            token_usage: EnvelopeTokenUsage::default(),
        },
    );
    let value = serde_json::to_value(&env).expect("serialize");
    let usage = value
        .get("payload")
        .and_then(|p| p.get("data"))
        .and_then(|d| d.get("token_usage"))
        .expect("token_usage");
    assert!(usage.as_object().expect("object").is_empty());
    let parsed: Envelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, env);
}

#[test]
fn golden_envelope_capability_feature_flag_registered() {
    // The projection feature flag must be in the known-features
    // registry so capability negotiation honours it.
    assert!(
        UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1),
        "projection.envelope.v1 must be registered for capability negotiation"
    );
    assert!(
        UI_PROTOCOL_KNOWN_FEATURES.contains(&UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2),
        "projection.envelope.v2 must be registered for capability negotiation"
    );
    assert!(
        !UiProtocolCapabilities::first_server_slice()
            .supports_feature(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2),
        "v2 is strictly opt-in and must not alter the no-header capability baseline"
    );
}

#[test]
fn envelope_notification_method_is_projection_envelope() {
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        envelope: Envelope {
            thread_id: "thread-1".into(),
            seq: 1,
            client_message_id: None,
            payload: Payload::AssistantDelta { text: "hi".into() },
        },
    });
    assert_eq!(notif.method(), "projection/envelope");
    assert_eq!(notif.session_id(), &SessionKey("local:demo".into()));
}

#[test]
fn envelope_notification_round_trips_through_rpc_envelope_with_routing() {
    // feat(envelope-wire-routing): the wire now carries `session_id`
    // (the bare base key) + optional `topic` FLATTENED alongside the
    // bare Envelope fields so a multi-session client can route the
    // envelope to the right session. The envelope fields stay at the
    // top level (no `envelope` nesting) so the existing tolerant web
    // SPA bridge — which reads `thread_id`/`seq`/`payload` top-level
    // and ignores unknown keys — keeps decoding it unchanged.
    let envelope = Envelope {
        thread_id: "thread-7".into(),
        seq: 42,
        client_message_id: Some("cmid-x".into()),
        payload: Payload::UserMessage {
            text: "hi".into(),
            files: vec![FileRef {
                path: "/tmp/a.png".into(),
                mime: "image/png".into(),
                size_bytes: 12,
            }],
        },
    };
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:demo".into()),
        topic: Some("planning".into()),
        envelope: envelope.clone(),
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    assert_eq!(rpc.method, "projection/envelope");
    // Wire shape: flattened — bare Envelope keys PLUS routing keys.
    let params = &rpc.params;
    assert_eq!(
        params.get("session_id"),
        Some(&json!("local:demo")),
        "session_id must reach the wire so the client can route",
    );
    assert_eq!(
        params.get("topic"),
        Some(&json!("planning")),
        "topic must reach the wire for topic-scoped routing",
    );
    // Bare Envelope keys stay at the top level (web-bridge compat).
    assert_eq!(params.get("thread_id"), Some(&json!("thread-7")));
    assert_eq!(params.get("seq"), Some(&json!(42)));
    assert_eq!(params.get("client_message_id"), Some(&json!("cmid-x")));
    // No `envelope` nesting on the wire — the flatten keeps the bare
    // shape the web bridge already reads.
    assert!(
        params.get("envelope").is_none(),
        "wire is flattened, not nested under `envelope`",
    );

    // Round-trip decode: session_id + topic survive byte-for-byte and
    // the envelope is byte-equal.
    let parsed = UiNotification::from_rpc_notification(rpc).expect("decode");
    match parsed {
        UiNotification::Envelope(ev) => {
            assert_eq!(ev.envelope, envelope);
            assert_eq!(
                ev.session_id,
                SessionKey("local:demo".into()),
                "decode must recover the routing session_id from the wire",
            );
            assert_eq!(ev.topic, Some("planning".into()));
        }
        other => panic!("expected Envelope variant, got {other:?}"),
    }
}

/// feat(envelope-wire-routing) backward-compat: an OLD bare-envelope
/// wire frame (no `session_id` / `topic` keys — emitted by a server
/// before this change) must still decode without error. The routing
/// fields default to empty/None; the consumer is expected to fall
/// back to ambient connection context for those legacy frames.
#[test]
fn envelope_notification_decodes_legacy_bare_wire_frame_without_routing() {
    // OLD wire shape: bare Envelope, no session_id/topic.
    let legacy_params = json!({
        "thread_id": "thread-legacy",
        "seq": 3,
        "payload": { "type": "assistant_delta", "data": { "text": "hi" } }
    });
    let decoded =
        UiNotification::from_method_and_params(methods::PROJECTION_ENVELOPE, legacy_params)
            .expect("legacy bare-envelope frame must still decode");
    match decoded {
        UiNotification::Envelope(ev) => {
            assert_eq!(
                ev.session_id,
                SessionKey(String::new()),
                "absent session_id defaults to empty for legacy frames",
            );
            assert_eq!(ev.topic, None, "absent topic defaults to None");
            assert_eq!(ev.envelope.thread_id, "thread-legacy");
            assert_eq!(ev.envelope.seq, 3);
        }
        other => panic!("expected Envelope variant, got {other:?}"),
    }
}

/// feat(envelope-wire-routing) — wire shape guard. The wire is the
/// FLATTENED form: bare Envelope keys (`thread_id`, `seq`, `payload`,
/// no `envelope` nesting) PLUS the routing keys `session_id` +
/// `topic` so a multi-session client can route. Codex #1336
/// BLOCKER-4's actual invariant — that the DISK derive preserves
/// routing — is pinned by
/// `envelope_notification_serde_preserves_routing_fields_for_disk_persistence`
/// above; that disk path is untouched by un-stripping the wire.
#[test]
fn envelope_notification_into_rpc_notification_flattens_routing_onto_wire() {
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:wire-route".into()),
        topic: Some("planning".into()),
        envelope: Envelope {
            thread_id: "thread-wire".into(),
            seq: 5,
            client_message_id: None,
            payload: Payload::AssistantDelta { text: "x".into() },
        },
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    assert_eq!(rpc.method, methods::PROJECTION_ENVELOPE);
    let params = &rpc.params;
    assert_eq!(
        params.get("session_id"),
        Some(&json!("local:wire-route")),
        "wire carries session_id for routing",
    );
    assert_eq!(
        params.get("topic"),
        Some(&json!("planning")),
        "wire carries topic for topic-scoped routing",
    );
    // Bare envelope fields stay top-level (no `envelope` nesting) so
    // the existing web-bridge top-level reader is unaffected.
    assert_eq!(params.get("thread_id"), Some(&json!("thread-wire")));
    assert_eq!(params.get("seq"), Some(&json!(5)));
    assert!(
        params.get("envelope").is_none(),
        "wire is flattened, not nested under `envelope`",
    );
}

/// feat(envelope-wire-routing): a `topic: None` envelope omits the
/// `topic` key on the wire (compact shape) but still carries
/// `session_id`. Decode recovers session_id and defaults topic.
#[test]
fn envelope_notification_wire_omits_absent_topic_but_keeps_session_id() {
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:no-topic".into()),
        topic: None,
        envelope: Envelope {
            thread_id: "thread-nt".into(),
            seq: 9,
            client_message_id: None,
            payload: Payload::AssistantDelta { text: "y".into() },
        },
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    let params = &rpc.params;
    assert_eq!(params.get("session_id"), Some(&json!("local:no-topic")));
    assert!(
        params.get("topic").is_none(),
        "absent topic omitted on the wire",
    );
    let parsed = UiNotification::from_rpc_notification(rpc).expect("decode");
    match parsed {
        UiNotification::Envelope(ev) => {
            assert_eq!(ev.session_id, SessionKey("local:no-topic".into()));
            assert_eq!(ev.topic, None);
        }
        other => panic!("expected Envelope variant, got {other:?}"),
    }
}

/// feat(envelope-wire-routing) — codex BLOCKER: on a TOPIC turn the
/// `turn/start` flow folds the topic into `session_id` as
/// `"base#topic"`, which is carried forward into the emitted
/// `EnvelopeNotification.session_id`. The WIRE `session_id` MUST be
/// normalized to the bare base key (`"base"`) — a client only knows
/// the base key, so a `"base#topic"` wire key misroutes the message
/// and defeats the orphan-chip self-heal. The topic MUST NOT be lost:
/// it is preserved on the wire's separate `topic` field (recovered
/// from the suffix when the explicit `topic` field is empty). The
/// DISK derive on `EnvelopeNotification` keeps `"base#topic"`
/// untouched (pinned by the disk-persistence test above).
#[test]
fn envelope_wire_session_id_is_normalized_to_base_key_with_topic_preserved() {
    let envelope = Envelope {
        thread_id: "thread-topic".into(),
        seq: 11,
        client_message_id: None,
        payload: Payload::AssistantDelta {
            text: "topic delta".into(),
        },
    };

    // Case 1: topic folded into session_id ("base#topic"), explicit
    // `topic` field is None — the suffix must be recovered onto the
    // wire's separate `topic` field while session_id is stripped.
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:demo#research".into()),
        topic: None,
        envelope: envelope.clone(),
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    let params = &rpc.params;
    assert_eq!(
        params.get("session_id"),
        Some(&json!("local:demo")),
        "wire session_id must be the bare base key, not base#topic",
    );
    assert_eq!(
        params.get("topic"),
        Some(&json!("research")),
        "topic must be preserved on the wire (recovered from suffix)",
    );
    // Decode must round-trip to the bare base key + separate topic.
    let parsed = UiNotification::from_rpc_notification(rpc).expect("decode");
    match parsed {
        UiNotification::Envelope(ev) => {
            assert_eq!(
                ev.session_id,
                SessionKey("local:demo".into()),
                "decode recovers the bare base key from the wire",
            );
            assert_eq!(ev.topic, Some("research".into()));
            assert_eq!(ev.envelope, envelope);
        }
        other => panic!("expected Envelope variant, got {other:?}"),
    }

    // Case 2: topic folded into session_id AND an explicit `topic`
    // field also set — the explicit topic wins, session_id still
    // strips to the base key.
    let notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: SessionKey("local:demo#research".into()),
        topic: Some("research".into()),
        envelope: envelope.clone(),
    });
    let rpc = notif.into_rpc_notification().expect("serialize");
    let params = &rpc.params;
    assert_eq!(
        params.get("session_id"),
        Some(&json!("local:demo")),
        "wire session_id must be the bare base key even with explicit topic",
    );
    assert_eq!(params.get("topic"), Some(&json!("research")));
}

// -----------------------------------------------------------------
// #1329 — topic-scope routing class fix
//
// The 6 events listed below (ToolStarted/Progress/Completed,
// ApprovalAutoResolved/Decided/Cancelled), plus FileAttached
// (already covered by the P0-A regression), gained an explicit
// `topic: Option<String>` field. `UiNotification::topic()` now
// consults that field FIRST and only falls back to
// `SessionKey.topic()`. Each test:
//   1. Builds the event with an explicit `topic` field — `topic()`
//      returns the field's value, even when `session_id` was
//      stripped to `base_key()` (the P0-A failure mode).
//   2. Builds the same event with a topic-suffixed session_id but
//      NO explicit topic — `topic()` falls back to the suffix
//      (backward compat; `stamp_topic_from_session` then promotes
//      it to the explicit field at append time).
//   3. Builds the event with neither — `topic()` returns `None`.
// -----------------------------------------------------------------

fn topic_session() -> SessionKey {
    SessionKey("local:slides-soak#slides".into())
}

fn bare_session() -> SessionKey {
    SessionKey("local:slides-soak".into())
}

#[test]
fn tool_started_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId::new(),
        tool_call_id: "tc-1".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    assert_eq!(
        with_field.topic(),
        Some("slides"),
        "explicit topic field wins over base_key() session_id"
    );

    let fallback = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-2".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    assert_eq!(
        fallback.topic(),
        Some("slides"),
        "missing explicit topic falls back to session_id suffix"
    );

    let neither = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: bare_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-3".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    assert_eq!(neither.topic(), None, "no topic anywhere → None");
}

#[test]
fn tool_progress_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ToolProgress(ToolProgressEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId::new(),
        tool_call_id: "tc-1".into(),
        message: Some("step 1".into()),
        progress_pct: Some(25.0),
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ToolProgress(ToolProgressEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-2".into(),
        message: None,
        progress_pct: None,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

#[test]
fn tool_completed_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ToolCompleted(ToolCompletedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId::new(),
        tool_call_id: "tc-1".into(),
        tool_name: "shell".into(),
        success: Some(true),
        output_preview: None,
        duration_ms: Some(10),
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ToolCompleted(ToolCompletedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-2".into(),
        tool_name: "shell".into(),
        success: Some(true),
        output_preview: None,
        duration_ms: None,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

#[test]
fn approval_auto_resolved_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ApprovalAutoResolved(ApprovalAutoResolvedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        tool_name: "shell".into(),
        scope: approval_scopes::SESSION.into(),
        scope_match: "exact".into(),
        decision: ApprovalDecision::Approve,
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ApprovalAutoResolved(ApprovalAutoResolvedEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        tool_name: "shell".into(),
        scope: approval_scopes::SESSION.into(),
        scope_match: "exact".into(),
        decision: ApprovalDecision::Approve,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

#[test]
fn approval_decided_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        decision: ApprovalDecision::Approve,
        scope: None,
        decided_at: Utc::now(),
        decided_by: "user:test".into(),
        auto_resolved: false,
        policy_id: None,
        client_note: None,
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        decision: ApprovalDecision::Approve,
        scope: None,
        decided_at: Utc::now(),
        decided_by: "user:test".into(),
        auto_resolved: false,
        policy_id: None,
        client_note: None,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

#[test]
fn approval_cancelled_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        reason: approval_cancelled_reasons::TURN_INTERRUPTED.into(),
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        reason: approval_cancelled_reasons::TURN_INTERRUPTED.into(),
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

/// #1329 sibling test: FileAttached gained the same `topic` field
/// as the 6 ApprovalDecided-class events; verify the same access
/// rule (explicit first, suffix fallback). This was the bug the
/// P0-A exemption patched; with explicit field, the exemption is
/// no longer needed.
#[test]
fn file_attached_topic_method_reads_explicit_field_then_session_suffix() {
    let with_field = UiNotification::FileAttached(FileAttachedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId::new(),
        path: "/tmp/deck.pptx".into(),
        tool_call_id: Some("tc-slides".into()),
        attachment_owner: None,
        mime: None,
    });
    assert_eq!(with_field.topic(), Some("slides"));

    let fallback = UiNotification::FileAttached(FileAttachedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        path: "/tmp/deck.pptx".into(),
        tool_call_id: None,
        attachment_owner: None,
        mime: None,
    });
    assert_eq!(fallback.topic(), Some("slides"));
}

/// `stamp_topic_from_session` MUST populate the new explicit
/// `topic` field for the 6 vulnerable variants (and FileAttached)
/// from the SessionKey suffix when the field is absent. This is
/// the safety net that runs in `into_rpc_notification`: even if a
/// caller forgets to stamp, the wire-emit path stamps it for them
/// so a topic-scoped subscriber always routes the event correctly.
#[test]
fn stamp_topic_from_session_populates_new_topic_class_events() {
    // ToolStarted
    let mut event = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    event.stamp_topic_from_session();
    assert_eq!(event.topic(), Some("slides"));
    if let UiNotification::ToolStarted(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    } else {
        panic!("event variant changed unexpectedly");
    }

    // ToolProgress
    let mut event = UiNotification::ToolProgress(ToolProgressEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc".into(),
        message: None,
        progress_pct: None,
    });
    event.stamp_topic_from_session();
    if let UiNotification::ToolProgress(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // ToolCompleted
    let mut event = UiNotification::ToolCompleted(ToolCompletedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc".into(),
        tool_name: "shell".into(),
        success: Some(true),
        output_preview: None,
        duration_ms: None,
    });
    event.stamp_topic_from_session();
    if let UiNotification::ToolCompleted(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // ApprovalAutoResolved
    let mut event = UiNotification::ApprovalAutoResolved(ApprovalAutoResolvedEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        tool_name: "shell".into(),
        scope: approval_scopes::SESSION.into(),
        scope_match: "exact".into(),
        decision: ApprovalDecision::Approve,
    });
    event.stamp_topic_from_session();
    if let UiNotification::ApprovalAutoResolved(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // ApprovalDecided
    let mut event = UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        decision: ApprovalDecision::Approve,
        scope: None,
        decided_at: Utc::now(),
        decided_by: "user:test".into(),
        auto_resolved: false,
        policy_id: None,
        client_note: None,
    });
    event.stamp_topic_from_session();
    if let UiNotification::ApprovalDecided(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // ApprovalCancelled
    let mut event = UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
        session_id: topic_session(),
        topic: None,
        approval_id: ApprovalId::new(),
        turn_id: TurnId::new(),
        reason: approval_cancelled_reasons::TURN_INTERRUPTED.into(),
    });
    event.stamp_topic_from_session();
    if let UiNotification::ApprovalCancelled(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }

    // FileAttached (sibling)
    let mut event = UiNotification::FileAttached(FileAttachedEvent {
        session_id: topic_session(),
        topic: None,
        turn_id: TurnId::new(),
        path: "/tmp/deck.pptx".into(),
        tool_call_id: None,
        attachment_owner: None,
        mime: None,
    });
    event.stamp_topic_from_session();
    if let UiNotification::FileAttached(inner) = &event {
        assert_eq!(inner.topic.as_deref(), Some("slides"));
    }
}

/// #1329 wire-shape guarantee: the new `topic` field must
/// serialize when present and stay omitted when absent (so v0
/// clients never see a surprise field). Verified for one
/// representative variant (the same `skip_serializing_if` is
/// applied uniformly across all 7).
#[test]
fn tool_started_topic_field_round_trips_on_the_wire() {
    let event = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: bare_session(),
        topic: Some("slides".into()),
        turn_id: TurnId(Uuid::from_u128(0x1329)),
        tool_call_id: "tc-1329".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    let wire = serde_json::to_value(event.clone().into_rpc_notification().expect("serialize"))
        .expect("to_value");
    assert_eq!(wire["params"]["topic"], json!("slides"));

    let decoded: RpcNotification<Value> = serde_json::from_value(wire).expect("deserialize wire");
    let decoded_event = UiNotification::from_rpc_notification(decoded).expect("decode");
    assert_eq!(decoded_event.topic(), Some("slides"));

    // Absent topic stays omitted.
    let bare_event = UiNotification::ToolStarted(ToolStartedEvent {
        session_id: bare_session(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-bare".into(),
        tool_name: "shell".into(),
        arguments: None,
    });
    let wire = serde_json::to_value(bare_event.into_rpc_notification().expect("serialize bare"))
        .expect("to_value");
    assert!(
        wire["params"].get("topic").is_none(),
        "absent topic field must stay omitted on the wire (no v0 breakage)"
    );
}

/// #1977 — monitor typed error kinds mirror the loop_* error family shape.
#[test]
fn monitor_error_kinds_are_registered() {
    assert_eq!(autonomy_error_kinds::MONITOR_NOT_FOUND, "monitor_not_found");
    assert_eq!(
        autonomy_error_kinds::MONITOR_INVALID_SPEC,
        "monitor_invalid_spec"
    );
    assert_eq!(
        autonomy_error_kinds::MONITOR_POLICY_DENIED,
        "monitor_policy_denied"
    );
    assert_eq!(autonomy_error_kinds::MONITOR_FLOODED, "monitor_flooded");
}

// ---- task-return-unconsumed-steer-inputs: `turn/steer_dropped` shape ----

#[test]
fn turn_steer_dropped_round_trips_through_method_and_params() {
    let event = TurnSteerDroppedEvent {
        session_id: SessionKey("local:demo".into()),
        topic: None,
        turn_id: TurnId(Uuid::from_u128(7)),
        inputs: vec!["first steer".into(), "second steer".into()],
        reason: "interrupted".into(),
    };
    let notification = UiNotification::TurnSteerDropped(event.clone());
    assert_eq!(notification.method(), methods::TURN_STEER_DROPPED);
    assert_eq!(methods::TURN_STEER_DROPPED, "turn/steer_dropped");

    let rpc = notification
        .into_rpc_notification()
        .expect("serialize turn/steer_dropped");
    assert_eq!(rpc.method, "turn/steer_dropped");
    assert_eq!(
        rpc.params,
        json!({
            "session_id": "local:demo",
            "turn_id": TurnId(Uuid::from_u128(7)),
            "inputs": ["first steer", "second steer"],
            "reason": "interrupted"
        })
    );

    let decoded = UiNotification::from_method_and_params("turn/steer_dropped", rpc.params)
        .expect("decode turn/steer_dropped");
    assert_eq!(decoded, UiNotification::TurnSteerDropped(event));
}

#[test]
fn turn_steer_dropped_topic_routing_matches_other_turn_events() {
    let mut notification = UiNotification::TurnSteerDropped(TurnSteerDroppedEvent {
        session_id: SessionKey("local:demo#coding".into()),
        topic: None,
        turn_id: TurnId(Uuid::from_u128(8)),
        inputs: vec!["late steer".into()],
        reason: "turn_ended".into(),
    });
    // Absent topic falls back to the session key's topic suffix, like TurnError.
    assert_eq!(notification.topic(), Some("coding"));
    assert_eq!(
        notification.session_id(),
        &SessionKey("local:demo#coding".into())
    );

    // An explicit topic is never overwritten by the routing stamp.
    if let UiNotification::TurnSteerDropped(event) = &mut notification {
        event.topic = Some("explicit".into());
    }
    let rpc = notification
        .into_rpc_notification()
        .expect("serialize with explicit topic");
    assert_eq!(rpc.params["topic"], json!("explicit"));
}

#[test]
fn context_state_semantic_cache_diagnostics_are_optional_and_backward_compatible() {
    let legacy = json!({
        "session_id": "local:demo",
        "generation": 2,
        "transcript_hash": "sha256:history",
        "item_count": 3,
        "token_estimate": 42,
        "recovery_state": "exact"
    });
    let legacy_state: UiContextState = serde_json::from_value(legacy).unwrap();
    assert!(legacy_state.cache_epoch_id.is_none());
    assert!(legacy_state.semantic_head_id.is_none());

    let current = UiContextState {
        session_id: SessionKey("local:demo".into()),
        thread_id: None,
        generation: 3,
        transcript_hash: "sha256:history".into(),
        item_count: 4,
        token_estimate: 64,
        recovery_state: "exact".into(),
        last_checkpoint_id: None,
        last_compaction_id: None,
        cache_epoch_id: Some("sha256:epoch".into()),
        last_cache_invalidation_reason: Some("tool_schema_changed".into()),
        semantic_head_id: Some("semblk_000004".into()),
        semantic_head_kind: Some("tool_interaction".into()),
    };
    let encoded = serde_json::to_value(current).unwrap();
    assert_eq!(encoded["cache_epoch_id"], "sha256:epoch");
    assert_eq!(encoded["semantic_head_kind"], "tool_interaction");
}
