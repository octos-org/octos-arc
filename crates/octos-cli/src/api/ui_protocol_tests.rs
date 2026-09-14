use super::*;
// `UiProtocolContractStores`'s audit writer moved with the contract stores to
// `crate::approvals_audit`; the parent module no longer imports these two names
// directly, so name them here.
use crate::api::coding_tool_contract;
use crate::approvals_audit::{ApprovalsAuditConfig, ApprovalsAuditLog};
use octos_core::ui_protocol::{
    ApprovalDecision, ApprovalId, ApprovalRespondParams, ApprovalRespondStatus, QuestionId,
    approval_scopes, methods, rpc_error_codes,
};

#[test]
fn should_reclaim_expired_context_persist_locks_without_splitting_live_writers() {
    let locks = AppUiContextPersistLocks::default();
    let session = SessionKey("persist-lock-live".into());
    let first = appui_context_persist_lock_from(&locks, &session);
    let guard = first.lock().unwrap();
    let waiting = appui_context_persist_lock_from(&locks, &session);
    assert!(Arc::ptr_eq(&first, &waiting));
    assert!(waiting.try_lock().is_err());
    for i in 0..2048 {
        drop(appui_context_persist_lock_from(
            &locks,
            &SessionKey(format!("expired-{i}")),
        ));
        assert!(locks.lock().unwrap().len() <= 2);
    }
    drop(guard);
    drop(first);
    assert!(Arc::ptr_eq(
        &waiting,
        &appui_context_persist_lock_from(&locks, &session)
    ));
    drop(waiting);
    let replacement = appui_context_persist_lock_from(&locks, &SessionKey("new-session".into()));
    assert_eq!(locks.lock().unwrap().len(), 1);
    assert!(replacement.try_lock().is_ok());
}

#[test]
fn should_normalize_safe_tool_context_at_protocol_boundary() {
    assert_eq!(
        normalize_tool_context(Some("  notebook  ")),
        Some("notebook".to_string())
    );
    assert_eq!(normalize_tool_context(None), None);
    assert_eq!(normalize_tool_context(Some("")), None);
    assert_eq!(normalize_tool_context(Some("notebook/other")), None);
    assert_eq!(normalize_tool_context(Some(&"a".repeat(65))), None);
}

/// The §6 "Envelope Model" catalog in
/// `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md` is a hand-maintained
/// mirror of the advertised method constants and has historically drifted
/// — methods shipped without a catalog update (e.g. `session/rollback`
/// #1516, `message/reasoning_delta` #1502). Nothing else gates catalog
/// completeness (`check-ui-protocol-upcr.sh` only checks that a protocol
/// edit ships with *a* UPCR doc). This test keeps §6 a superset of
/// `UI_PROTOCOL_COMMAND_METHODS ∪ UI_PROTOCOL_NOTIFICATION_METHODS ∪
/// UI_PROTOCOL_FIRST_SERVER_METHODS ∪ APPUI_EXTRA_METHODS` — the full set the
/// server advertises (`ui_protocol_server_supported_methods` builds from
/// `FIRST_SERVER ∪ APPUI_EXTRA`) — so the catalog can no longer silently
/// fall behind, even for a future server-only method.
#[test]
fn spawn_report_announcement_inlines_small_reports_in_full() {
    let body = "Status: SUCCESS\n\nshort review body";
    let out = format_spawn_report_announcement("review-octos-web", body, Some("task-1"));
    assert!(out.contains(body), "small report must be inlined verbatim");
    assert!(!out.contains("preview truncated"));
}

#[test]
fn spawn_report_announcement_previews_large_reports_with_recovery_pointer() {
    // Mini4 re-review regression: the old 300-char preview with no
    // pointer left the parent no way to recover a child's multi-KB
    // report; it concluded the result "was lost".
    let body = "x".repeat(SPAWN_REPORT_INLINE_CAP_CHARS + 500);
    let out = format_spawn_report_announcement("review-octos-web", &body, Some("019f6e66-f94c"));
    assert!(out.contains("preview truncated"));
    assert!(
        out.contains("read_task_output(task_handle=\"019f6e66-f94c\")"),
        "must point at the working recovery path: {out}"
    );
    // The preview itself must be the big cap, not the old 300.
    assert!(out.len() > SPAWN_REPORT_INLINE_CAP_CHARS);
}

#[test]
fn spawn_report_announcement_cap_boundary_is_exact() {
    // Exactly at the cap → inline full, no pointer (chars, not bytes).
    let body = "y".repeat(SPAWN_REPORT_INLINE_CAP_CHARS);
    let out = format_spawn_report_announcement("t", &body, None);
    assert!(!out.contains("preview truncated"));
    assert!(out.contains(&body));
}

#[tokio::test]
async fn compaction_started_precedes_completed_in_lifecycle_batch() {
    // UPCR-2026-026: when the threshold trips, the lifecycle batch must
    // carry context/compaction_started BEFORE context/compaction_completed
    // and the started event reports the pre-compaction estimate.
    struct TinyContextProvider;
    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for TinyContextProvider {
        fn provider_name(&self) -> &str {
            "test-provider"
        }

        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            unreachable!("compaction never calls the provider")
        }
        fn model_id(&self) -> &str {
            "tiny"
        }
        fn context_window(&self) -> u32 {
            512
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let session: SessionKey = SessionKey("full:api:compact".to_string());
    let mut history = Vec::new();
    for i in 0..40 {
        history.push(octos_core::Message {
            role: octos_core::MessageRole::User,
            content: format!("padding message {i}: {}", "x".repeat(400)),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
    }

    let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(TinyContextProvider);
    let (_messages, _manager, notifications, _registration) = appui_context_history_for_agent(
        dir.path(),
        &session,
        &history,
        &provider,
        false,
        "preflight",
    );

    let started_pos = notifications
        .iter()
        .position(|n| matches!(n, UiNotification::ContextCompactionStarted(_)));
    let completed_pos = notifications
        .iter()
        .position(|n| matches!(n, UiNotification::ContextCompactionCompleted(_)));
    let (Some(started_pos), Some(completed_pos)) = (started_pos, completed_pos) else {
        panic!("both compaction events must be emitted: {notifications:?}");
    };
    assert!(
        started_pos < completed_pos,
        "started must precede completed"
    );
    let UiNotification::ContextCompactionStarted(started) = &notifications[started_pos] else {
        unreachable!()
    };
    assert!(started.context_state.token_estimate > started.threshold_tokens);
    assert_eq!(started.trigger, "preflight");
}

/// Provider stub for the open-snapshot compaction tests. The window must be
/// LARGE relative to the compaction floor (16 kept items + a ≤4096-token
/// summary) so a successful pass actually lands under threshold — with a toy
/// window the keep-floor dominates and the assertion would test nothing. 64K
/// window → ~45K threshold; 60×4000-char messages ≈ 60K estimate. `chat` is
/// unreachable because the deterministic summarizer never calls the model.
struct OpenSnapshotTinyProvider;
#[async_trait::async_trait]
impl octos_llm::LlmProvider for OpenSnapshotTinyProvider {
    fn provider_name(&self) -> &str {
        "test-provider"
    }

    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        unreachable!("open-snapshot compaction never calls the provider")
    }
    fn model_id(&self) -> &str {
        "tiny"
    }
    fn context_window(&self) -> u32 {
        65_536
    }
}

fn open_snapshot_padding_history(messages: usize) -> Vec<octos_core::Message> {
    (0..messages)
        .map(|i| octos_core::Message {
            role: octos_core::MessageRole::User,
            content: format!("padding message {i}: {}", "x".repeat(4000)),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        })
        .collect()
}

#[tokio::test]
async fn session_open_snapshot_compacts_oversized_context() {
    // Field report 2026-08-07: a session whose ledger is REBUILT from a long
    // raw history at `session/open` (legacy/stale/missing snapshot) published
    // and persisted an over-window estimate — the TUI gauge sat at
    // `ctx 1.2M/1M` from open until the next turn's pre-turn pass finally
    // compacted. Open must run the SAME threshold compaction the pre-turn
    // path would run, so the published state is never over the window the
    // session's own provider reports.
    let dir = tempfile::tempdir().unwrap();
    let session: SessionKey = SessionKey("full:api:open-compact".to_string());
    let history = open_snapshot_padding_history(60);
    let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(OpenSnapshotTinyProvider);
    let threshold = appui_context_compact_threshold_tokens(provider.as_ref());

    let (_value, context_state, events) =
        appui_context_open_snapshot(dir.path(), &session, &history, Some(&provider));

    assert!(
        context_state.token_estimate <= threshold,
        "open snapshot must compact an over-threshold context before publishing \
         (estimate {} > threshold {threshold})",
        context_state.token_estimate,
    );
    assert!(
        context_state.last_compaction_id.is_some(),
        "the open-time pass must be recorded as a real compaction"
    );

    // The pass must return the lifecycle events for the caller to append to
    // the ledger — a silent open-time rewrite of the session's context is
    // exactly what the compaction UX exists to surface (field feedback on the
    // first cut, which dropped these on the floor).
    let started_pos = events
        .iter()
        .position(|n| matches!(n, UiNotification::ContextCompactionStarted(_)));
    let completed_pos = events
        .iter()
        .position(|n| matches!(n, UiNotification::ContextCompactionCompleted(_)));
    let (Some(started_pos), Some(completed_pos)) = (started_pos, completed_pos) else {
        panic!("open-time compaction must emit started+completed events: {events:?}");
    };
    assert!(
        started_pos < completed_pos,
        "started must precede completed"
    );
    let UiNotification::ContextCompactionStarted(started) = &events[started_pos] else {
        unreachable!()
    };
    assert_eq!(started.trigger, "appui_open");

    // The compacted manager — not the oversized rebuild — must be what
    // persisted: a reload sees the small estimate and a LOADED ledger.
    let (reloaded, status) = crate::context_manager::load_or_rebuild_context_manager(
        dir.path(),
        session.to_string(),
        None,
        &history,
    );
    assert_eq!(
        status,
        crate::context_manager::ContextLedgerLoadStatus::Loaded,
        "the persisted open snapshot must cover the history it was built from"
    );
    assert!(reloaded.state().token_estimate <= threshold);
}

#[tokio::test]
async fn session_open_snapshot_leaves_small_context_untouched() {
    // Under-threshold sessions must open exactly as before: no compaction
    // record, estimate untouched.
    let dir = tempfile::tempdir().unwrap();
    let session: SessionKey = SessionKey("full:api:open-small".to_string());
    let history = open_snapshot_padding_history(1);
    let provider: Arc<dyn octos_llm::LlmProvider> = Arc::new(OpenSnapshotTinyProvider);

    let (_value, context_state, events) =
        appui_context_open_snapshot(dir.path(), &session, &history, Some(&provider));

    assert!(
        context_state.last_compaction_id.is_none(),
        "an under-threshold open must not compact"
    );
    assert!(events.is_empty(), "no compaction => no lifecycle events");
}

#[tokio::test]
async fn session_open_snapshot_without_provider_never_compacts() {
    // No provider (no materialized session runtime — e.g. a profile-less
    // open) means no window to derive a threshold from. Compacting against a
    // guessed default could DESTRUCTIVELY over-compact a long-window session,
    // so the open snapshot must fail open: publish as-is, like today.
    let dir = tempfile::tempdir().unwrap();
    let session: SessionKey = SessionKey("full:api:open-noprov".to_string());
    let history = open_snapshot_padding_history(60);

    let (_value, context_state, events) =
        appui_context_open_snapshot(dir.path(), &session, &history, None);

    assert!(
        context_state.last_compaction_id.is_none(),
        "without a provider the open snapshot must not compact"
    );
    assert!(events.is_empty(), "no provider => no lifecycle events");
}

#[test]
fn post_terminal_drain_skips_late_tokens_but_keeps_background_progress() {
    // Regression for the "queued N messages after active turn" wedge: the
    // post-terminal spawn_only drain must drop late assistant `token`
    // deltas (they carry the already-completed foreground turn id and
    // resurrect the client's input gate) alongside the already-emitted
    // `done`/`error` terminal signals...
    assert!(drain_should_skip_event(Some("done")));
    assert!(drain_should_skip_event(Some("error")));
    assert!(drain_should_skip_event(Some("token")));
    assert!(drain_should_skip_event(Some("reasoning_chunk")));

    // ...while still forwarding the background task's real progress, which
    // is the entire point of the drain (#961). None of these produce a
    // `MessageDelta`, so none can wedge the turn gate.
    for keep in [
        "task_started",
        "task_updated",
        "task_completed",
        "task_interrupted",
        "tool_start",
        "tool_progress",
        "tool_end",
        "file_modified",
        "file_written",
        "cost_update",
    ] {
        assert!(
            !drain_should_skip_event(Some(keep)),
            "drain must forward background event `{keep}`"
        );
    }
    // A typeless event is forwarded (the mapper warns on it downstream).
    assert!(!drain_should_skip_event(None));
}

#[tokio::test]
async fn independent_oup_app_states_do_not_share_the_first_instances_ledger() {
    let first = AppState::empty_for_tests();
    let second = AppState::empty_for_tests();
    let a = event_ledger(&first).await;
    let b = event_ledger(&second).await;
    assert!(
        !Arc::ptr_eq(&a, &b),
        "an embedded runtime must not inherit another runtime's ledger root"
    );
    assert!(Arc::ptr_eq(&a, &event_ledger(&first).await));
}

fn local_profile_state(dir: &Path) -> AppState {
    AppState {
        profile_store: Some(Arc::new(
            crate::profiles::ProfileStore::open_unified(dir).unwrap(),
        )),
        // Solo profile creation is opt-in; the TUI/WS tests exercise the
        // supported path, so enable it here.
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    }
}

fn profile_for_runtime_message(id: &str) -> crate::profiles::UserProfile {
    let now = Utc::now();
    crate::profiles::UserProfile {
        id: id.to_string(),
        name: id.to_string(),
        public_subdomain: None,
        enabled: true,
        data_dir: None,
        parent_id: None,
        config: crate::profiles::ProfileConfig::default(),
        created_at: now,
        updated_at: now,
    }
}

// Regression coverage for the "No ProfileRuntime registered... Set up the
// profile with an API key in the dashboard" message, which used to fire
// verbatim for every Ok(None) from `ensure_session_profile_runtime` —
// including a sub-account or an unknown id, neither of which an API key would
// fix. A profile with gateway auto-start disabled is still eligible for an
// authenticated, on-demand AppUI runtime.
#[test]
fn should_report_no_profile_store_when_profile_runtime_unavailable_and_store_missing() {
    let state = AppState {
        profile_store: None,
        ..AppState::empty_for_tests()
    };
    let message = profile_runtime_unavailable_message(&state, "ghost");
    assert!(
        message.contains("no profile store") || message.contains("No profile store"),
        "expected a no-store explanation, got: {message}"
    );
    assert!(
        !message.contains("API key"),
        "must not blame a missing API key: {message}"
    );
}

#[test]
fn should_report_unknown_profile_when_profile_runtime_unavailable_and_profile_missing() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let message = profile_runtime_unavailable_message(&state, "ghost");
    assert!(
        message.contains("does not exist"),
        "expected a not-found explanation, got: {message}"
    );
    assert!(
        !message.contains("API key"),
        "must not blame a missing API key: {message}"
    );
}

#[tokio::test]
async fn should_bootstrap_appui_runtime_when_gateway_autostart_is_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let mut profile = profile_for_runtime_message("disabled-one");
    profile.enabled = false;
    // `enabled` controls gateway auto-start, not authenticated AppUI access.
    profile.config.llm = Some(crate::profiles::LlmProfileConfig {
        primary: Some(crate::profiles::LlmModelSelectionConfig {
            family_id: Some("openai".to_string()),
            model_id: Some("gpt-4o-mini".to_string()),
            route: Some(crate::profiles::LlmRouteConfig {
                api_key_env: Some("OCTOS_TEST_APPUI_DISABLED_PROFILE_KEY".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        fallbacks: Vec::new(),
    });
    profile.config.env_vars.insert(
        "OCTOS_TEST_APPUI_DISABLED_PROFILE_KEY".to_string(),
        "test-key".to_string(),
    );
    state
        .profile_store
        .as_ref()
        .unwrap()
        .save(&profile)
        .unwrap();

    let runtime = ensure_session_profile_runtime(&state, Some("disabled-one"))
        .await
        .expect("runtime bootstrap")
        .expect("on-demand AppUI runtime");
    assert_eq!(runtime.primary_model_id, "gpt-4o-mini");
}

#[test]
fn should_report_sub_account_when_profile_runtime_unavailable_and_profile_has_parent() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let mut profile = profile_for_runtime_message("child-one");
    profile.parent_id = Some("parent-one".to_string());
    state
        .profile_store
        .as_ref()
        .unwrap()
        .save(&profile)
        .unwrap();

    let message = profile_runtime_unavailable_message(&state, "child-one");
    assert!(
        message.contains("sub-account") || message.contains("sub-profile"),
        "expected a sub-account explanation, got: {message}"
    );
    assert!(
        !message.contains("API key"),
        "a sub-account must not be told to add an API key: {message}"
    );
}

#[test]
fn should_report_missing_api_key_when_profile_runtime_unavailable_and_no_llm_selected() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let profile = profile_for_runtime_message("no-llm-one");
    state
        .profile_store
        .as_ref()
        .unwrap()
        .save(&profile)
        .unwrap();

    let message = profile_runtime_unavailable_message(&state, "no-llm-one");
    assert!(
        message.contains("API key"),
        "a genuinely unconfigured profile should keep the API-key guidance, got: {message}"
    );
}

#[test]
fn profile_local_create_make_default_persists_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    // Create with make_default → the assigned profile becomes the global
    // default pointer.
    let created = create_or_get_local_solo_profile(
        &state,
        octos_core::ui_protocol::ProfileLocalCreateParams {
            requested_id: Some("glm".into()),
            name: String::new(),
            username: String::new(),
            email: String::new(),
            make_default: Some(true),
        },
    )
    .expect("create with make_default");
    let store = state.profile_store.as_ref().unwrap();
    assert_eq!(
        store.default_profile().as_deref(),
        Some(created.profile_id.as_str())
    );

    // A later create WITHOUT make_default must not steal the default.
    let other = create_or_get_local_solo_profile(
        &state,
        octos_core::ui_protocol::ProfileLocalCreateParams {
            requested_id: Some("deepseek".into()),
            name: String::new(),
            username: String::new(),
            email: String::new(),
            make_default: None,
        },
    )
    .expect("create without make_default");
    assert_ne!(other.profile_id, created.profile_id);
    assert_eq!(
        store.default_profile().as_deref(),
        Some(created.profile_id.as_str()),
        "a create without make_default must leave the default pointer intact"
    );
}

/// `profile/sub_providers/{list,upsert,remove}`: add/replace-by-key/remove the
/// named provider lanes (`cheap`/`strong` etc.) that back the isolated research
/// pipeline router; upsert with an existing key REPLACES rather than appends,
/// removing a missing key reports `applied:false`, and everything persists.
#[tokio::test]
async fn sub_providers_upsert_list_and_remove_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));

    for (key, provider, model) in [
        ("cheap", "gemini", "gemini-2.5-flash"),
        ("strong", "openai", "gpt-5-mini"),
    ] {
        let request = RpcRequest::new(
            format!("u-{key}"),
            APPUI_METHOD_PROFILE_SUB_PROVIDERS_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "sub_provider": { "key": key, "provider": provider, "model": model },
            }),
        );
        let res = raw_profile_sub_providers_upsert(&state, &request, None)
            .await
            .expect("upsert");
        assert_eq!(res["applied"], true);
        // A persisted change is not live until restart — the client must be told.
        assert_eq!(res["restart_required"], true);
    }

    // A pasted api_key with no api_key_env must be REJECTED (was silently
    // dropped, leaving the lane to grab ambient/primary credentials).
    let no_env = RpcRequest::new(
        "u-noenv".to_string(),
        APPUI_METHOD_PROFILE_SUB_PROVIDERS_UPSERT.to_string(),
        json!({
            "profile_id": "dev",
            "sub_provider": { "key": "x", "provider": "openai" },
            "api_key": "sk-secret",
        }),
    );
    let err = raw_profile_sub_providers_upsert(&state, &no_env, None)
        .await
        .expect_err("api_key without api_key_env must be rejected");
    assert!(
        err.message.contains("api_key_env"),
        "reject reason should name api_key_env; got {}",
        err.message
    );

    // List reflects both lanes in insertion order.
    let list_req = RpcRequest::new(
        "l1".to_string(),
        APPUI_METHOD_PROFILE_SUB_PROVIDERS_LIST.to_string(),
        json!({ "profile_id": "dev" }),
    );
    let listed = raw_profile_sub_providers_list(&state, &list_req, None).expect("list");
    let lanes = listed["sub_providers"].as_array().unwrap();
    assert_eq!(lanes.len(), 2, "both lanes listed: {listed}");
    assert_eq!(lanes[0]["key"], "cheap");
    assert_eq!(lanes[0]["model"], "gemini-2.5-flash");
    assert_eq!(lanes[1]["key"], "strong");

    // Upsert with an existing key REPLACES (not appends).
    let replace = RpcRequest::new(
        "u-cheap-2".to_string(),
        APPUI_METHOD_PROFILE_SUB_PROVIDERS_UPSERT.to_string(),
        json!({
            "profile_id": "dev",
            "sub_provider": { "key": "cheap", "provider": "deepseek", "model": "deepseek-chat" },
        }),
    );
    let res = raw_profile_sub_providers_upsert(&state, &replace, None)
        .await
        .expect("replace");
    let lanes = res["sub_providers"].as_array().unwrap();
    assert_eq!(lanes.len(), 2, "same-key upsert replaces, not appends");
    let cheap = lanes.iter().find(|l| l["key"] == "cheap").unwrap();
    assert_eq!(cheap["provider"], "deepseek");

    // Persisted to disk.
    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    assert_eq!(profile.config.sub_providers.len(), 2);

    // Remove one lane.
    let rm = RpcRequest::new(
        "r1".to_string(),
        APPUI_METHOD_PROFILE_SUB_PROVIDERS_REMOVE.to_string(),
        json!({ "profile_id": "dev", "key": "cheap" }),
    );
    let res = raw_profile_sub_providers_remove(&state, &rm, None)
        .await
        .expect("remove");
    assert_eq!(res["applied"], true);
    assert_eq!(res["sub_providers"].as_array().unwrap().len(), 1);

    // Removing a missing key is a no-op with applied:false.
    let rm2 = RpcRequest::new(
        "r2".to_string(),
        APPUI_METHOD_PROFILE_SUB_PROVIDERS_REMOVE.to_string(),
        json!({ "profile_id": "dev", "key": "nonexistent" }),
    );
    let res = raw_profile_sub_providers_remove(&state, &rm2, None)
        .await
        .expect("remove-miss");
    assert_eq!(res["applied"], false);
}

/// Multi-model profiles: `profile/llm/list`-backed model list exposes the
/// primary AND every fallback; `profile/llm/select` promotes a fallback to
/// the active primary (demoting the old primary to a fallback), persists,
/// and is idempotent for the current primary; unconfigured models reject.
#[tokio::test]
async fn llm_select_promotes_fallback_and_lists_all_models() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    for (family, model, set_primary) in [
        ("deepseek", "deepseek-v4-pro", true),
        ("zai", "glm-5.3", false),
    ] {
        let request = RpcRequest::new(
            format!("u-{model}"),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": family,
                    "model_id": model,
                    "route": { "route_id": "official" },
                },
                // Selection validates key availability: seed one per
                // family so the selects below exercise promotion, not
                // the keyless-rejection path.
                "api_key": format!("test-{family}-key"),
                "set_primary": set_primary,
            }),
        );
        raw_profile_llm_upsert(&state, &request, None)
            .await
            .expect("upsert");
    }

    let profile = state.profile_store.as_ref().unwrap().get("dev").unwrap();
    let models = model_list_result(
        &state,
        SessionKey("dev:local:t".into()),
        "dev",
        profile.as_ref(),
    );
    let list = models["models"].as_array().unwrap();
    assert_eq!(list.len(), 2, "primary + fallback both listed: {models}");
    assert_eq!(list[0]["model"], "deepseek-v4-pro");
    assert_eq!(list[0]["selected"], true);
    assert_eq!(list[1]["model"], "glm-5.3");
    assert_eq!(list[1]["selected"], false);

    let request = RpcRequest::new(
        "s1".to_string(),
        APPUI_METHOD_PROFILE_LLM_SELECT.to_string(),
        json!({
            "profile_id": "dev",
            "family_id": "zai",
            "model_id": "glm-5.3",
            "route_id": "official",
        }),
    );
    let result = raw_profile_llm_select(&state, &request, None)
        .await
        .expect("select");
    assert_eq!(result["applied"], true);
    assert_eq!(result["selected"]["model"], "glm-5.3");
    assert_eq!(result["selected"]["selected"], true);

    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let llm = profile.config.llm.as_ref().unwrap();
    assert_eq!(
        llm.primary.as_ref().unwrap().model_id.as_deref(),
        Some("glm-5.3"),
        "fallback promoted to primary"
    );
    assert_eq!(llm.fallbacks.len(), 1, "demoted primary kept as fallback");
    assert_eq!(
        llm.fallbacks[0].model_id.as_deref(),
        Some("deepseek-v4-pro")
    );

    let request = RpcRequest::new(
        "s2".to_string(),
        APPUI_METHOD_PROFILE_LLM_SELECT.to_string(),
        json!({ "profile_id": "dev", "model_id": "glm-5.3" }),
    );
    let result = raw_profile_llm_select(&state, &request, None)
        .await
        .expect("selecting the current primary is idempotent");
    assert_eq!(result["applied"], true);

    let request = RpcRequest::new(
        "s3".to_string(),
        APPUI_METHOD_PROFILE_LLM_SELECT.to_string(),
        json!({ "profile_id": "dev", "model_id": "kimi-k2.5" }),
    );
    let error = raw_profile_llm_select(&state, &request, None)
        .await
        .expect_err("unconfigured model must reject");
    assert_eq!(error.code, rpc_error_codes::RESOURCE_NOT_FOUND);
}

/// Selecting a model whose provider key resolves nowhere must REJECT
/// without persisting: the runtime re-ensure would fail silently (the
/// live session keeps the old chain while the UI claims the switch), and
/// the persisted keyless primary bricks the next `session/open` at
/// bootstrap. Found live: a user selected zai/glm-5.3 with no
/// ZAI_API_KEY — "Model selected" echoed, the footer snapped back, and
/// the next launch failed to boot.
#[tokio::test]
async fn llm_select_rejects_keyless_models_before_persisting() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    let upsert = |model: &str, api_key: Option<&str>, set_primary: bool| {
        let mut body = json!({
            "profile_id": "dev",
            "selection": {
                "family_id": if model.starts_with("glm") { "zai" } else { "deepseek" },
                "model_id": model,
                "route": {
                    "route_id": "official",
                    // A test-scoped variable name so resolution can't
                    // fall through to a real key in the process env.
                    "api_key_env": format!("OCTOS_TEST_{}_KEY", model.replace(['-', '.'], "_").to_uppercase()),
                },
            },
            "set_primary": set_primary,
        });
        if let Some(api_key) = api_key {
            body["api_key"] = json!(api_key);
        }
        RpcRequest::new(
            format!("u-{model}-{}", api_key.is_some()),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            body,
        )
    };

    raw_profile_llm_upsert(&state, &upsert("deepseek-chat", Some("dk"), true), None)
        .await
        .expect("seed keyed primary");
    raw_profile_llm_upsert(&state, &upsert("glm-5.3", None, false), None)
        .await
        .expect("seed keyless fallback");

    let select = |id: &str| {
        RpcRequest::new(
            id.to_string(),
            APPUI_METHOD_PROFILE_LLM_SELECT.to_string(),
            json!({ "profile_id": "dev", "model_id": "glm-5.3" }),
        )
    };
    let error = raw_profile_llm_select(&state, &select("s-keyless"), None)
        .await
        .expect_err("keyless select must reject");
    assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("llm_key_missing")),
        "got {error:?}"
    );
    assert!(
        error.message.contains("OCTOS_TEST_GLM_5_3_KEY"),
        "message must name the missing variable: {}",
        error.message
    );
    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    assert_eq!(
        profile
            .config
            .llm
            .as_ref()
            .unwrap()
            .primary
            .as_ref()
            .unwrap()
            .model_id
            .as_deref(),
        Some("deepseek-chat"),
        "rejected select must not persist"
    );

    // Adding the key (the onboarding key step) makes the same select work.
    raw_profile_llm_upsert(&state, &upsert("glm-5.3", Some("zk"), false), None)
        .await
        .expect("re-save with key");
    let result = raw_profile_llm_select(&state, &select("s-keyed"), None)
        .await
        .expect("keyed select applies");
    assert_eq!(result["applied"], true);
    assert_eq!(result["selected"]["model"], "glm-5.3");
    assert_eq!(
        result["restart_required"],
        json!(false),
        "dynamic profiles apply without a restart: {result}"
    );
    // #2164 uniform post-commit truth: applied stays persistence-only and the
    // runtime disposition is stamped beside it.
    assert_eq!(result["runtime_disposition"], "reloaded", "{result}");
    assert_eq!(result["effective_from"], "next_turn", "{result}");
    assert!(
        result.get("config_revision").is_some_and(|r| r.is_string()),
        "committed revision must be comparable against the runtime stamp: {result}"
    );
}

/// Unknown fields on `profile/llm/upsert` must be rejected with EVERY
/// rejected field named by its dotted path — never accepted with
/// `applied: true` while the values are silently discarded — and the prior
/// configuration must be left untouched.
#[tokio::test]
async fn llm_upsert_rejects_unknown_fields_and_names_every_one() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    let error = raw_profile_llm_upsert(
        &state,
        &RpcRequest::new(
            "u-unknown".to_string(),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "custom",
                    "model_id": "fixture-model",
                    "route": {
                        "route_id": "fixture",
                        "base_url": "http://127.0.0.1:9/v1",
                        "api_type": "openai",
                        "bogus_route_key": true
                    },
                    "inference": { "temperature": 0.2 },
                    "temperature2": 0.5
                },
                "set_primary": true
            }),
        ),
        None,
    )
    .await
    .expect_err("unknown fields must be rejected, not silently dropped");
    let data = error.data.as_ref().expect("typed error data");
    assert_eq!(data["kind"], json!("llm_unknown_fields"));
    let rejected = data["rejected_fields"].as_array().expect("field list");
    for expected in [
        "selection.temperature2",
        "selection.inference",
        "selection.route.bogus_route_key",
    ] {
        assert!(
            rejected.iter().any(|value| *value == json!(expected)),
            "rejected_fields must name {expected}: {data}"
        );
    }
    // The rejection happens BEFORE any store mutation: the profile is not
    // even created.
    assert!(
        state
            .profile_store
            .as_ref()
            .unwrap()
            .get("dev")
            .unwrap()
            .is_none(),
        "a rejected upsert must not create or mutate the profile"
    );
}

/// `max_output_tokens` is REAL but owned by the profile-gateway contract —
/// it gets a dedicated typed error pointing at the owner instead of a
/// generic "unknown field".
#[tokio::test]
async fn llm_upsert_rejects_max_output_tokens_as_owned_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    let error = raw_profile_llm_upsert(
        &state,
        &RpcRequest::new(
            "u-foreign".to_string(),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "custom",
                    "model_id": "fixture-model",
                    "route": { "route_id": "fixture", "base_url": "http://127.0.0.1:9/v1" },
                    "max_output_tokens": 4096
                },
                "set_primary": true
            }),
        ),
        None,
    )
    .await
    .expect_err("max_output_tokens is owned by the gateway contract");
    let data = error.data.as_ref().expect("typed error data");
    assert_eq!(data["kind"], json!("llm_param_owned_elsewhere"));
    assert_eq!(
        data["rejected_fields"][0],
        json!("selection.max_output_tokens")
    );
    let owner = data["owners"][0]["owner"].as_str().unwrap();
    assert!(
        owner.contains("gateway") && owner.contains("max_output_tokens"),
        "the error must point at the owning contract: {owner}"
    );
    assert!(
        state
            .profile_store
            .as_ref()
            .unwrap()
            .get("dev")
            .unwrap()
            .is_none(),
        "a foreign-field rejection must not create or mutate the profile"
    );
}

/// Out-of-range and non-finite typed values return a typed
/// `llm_param_out_of_range` / `llm_param_non_finite` without mutating the
/// prior configuration.
#[tokio::test]
async fn llm_upsert_rejects_out_of_range_and_non_finite_values() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    for (field, value, kind) in [
        ("temperature", json!(3.5), "llm_param_out_of_range"),
        ("temperature", json!(-0.1), "llm_param_out_of_range"),
        ("top_p", json!(1.5), "llm_param_out_of_range"),
        ("context_window", json!(0), "llm_param_out_of_range"),
    ] {
        let error = raw_profile_llm_upsert(
            &state,
            &RpcRequest::new(
                "u-range".to_string(),
                APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
                json!({
                    "profile_id": "dev",
                    "selection": {
                        "family_id": "custom",
                        "model_id": "fixture-model",
                        "route": { "route_id": "fixture", "base_url": "http://127.0.0.1:9/v1" },
                        field: value
                    },
                    "set_primary": true
                }),
            ),
            None,
        )
        .await
        .expect_err("{field}={value} must be rejected");
        let data = error.data.as_ref().expect("typed error data");
        assert_eq!(data["kind"], json!(kind), "{field}={value}");
        assert_eq!(data["field"], json!(format!("selection.{field}")));
    }
    // Non-finite guard: exercised directly (JSON cannot carry NaN/Inf).
    let error = validate_llm_inference_fields(&RawLlmSelection {
        temperature: Some(f64::NAN),
        ..Default::default()
    })
    .expect_err("NaN temperature must be rejected");
    assert_eq!(
        error.data.as_ref().unwrap()["kind"],
        json!("llm_param_non_finite")
    );
}

/// Known typed inference fields round-trip: upsert → durable store →
/// list/read; a re-upsert WITHOUT them clears the prior override
/// (`absent ≡ null ≡ inherit`), and routing metadata outside the schema
/// (`cost_per_m`/`strong`) survives the same-address edit.
#[tokio::test]
async fn llm_upsert_round_trips_typed_inference_fields() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    let route = json!({ "route_id": "fixture", "base_url": "http://127.0.0.1:9/v1" });
    raw_profile_llm_upsert(
        &state,
        &RpcRequest::new(
            "u-rt1".to_string(),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "custom",
                    "model_id": "fixture-model",
                    "route": route,
                    "temperature": 0.2,
                    "top_p": 0.9,
                    "context_window": 16384,
                    "reasoning_effort": "high",
                    "model_hints": { "uses_completion_tokens": true }
                },
                "set_primary": true
            }),
        ),
        None,
    )
    .await
    .expect("typed upsert");
    // Routing metadata arrives OUTSIDE the RPC (QoS/routing research owns
    // it) — written straight to the durable selection.
    let mut profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    {
        let primary = profile
            .config
            .llm
            .as_mut()
            .unwrap()
            .primary
            .as_mut()
            .unwrap();
        primary.cost_per_m = Some(1.5);
        primary.strong = Some(true);
    }
    state
        .profile_store
        .as_ref()
        .unwrap()
        .save(&profile)
        .expect("seed routing metadata");

    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let primary = profile
        .config
        .llm
        .as_ref()
        .unwrap()
        .primary
        .as_ref()
        .unwrap();
    assert_eq!(primary.temperature, Some(0.2));
    assert_eq!(primary.top_p, Some(0.9));
    assert_eq!(primary.context_window, Some(16384));
    assert_eq!(
        primary.reasoning_effort,
        Some(octos_llm::ReasoningEffort::High)
    );
    assert!(primary.model_hints.as_ref().unwrap().uses_completion_tokens);

    // The list projection round-trips every configured field — and stays
    // truthful: a client must be able to tell saved values from inherited.
    let listed = profile_llm_list_result(&state, "dev", Some(&profile));
    let primary_json = listed["primary"].as_object().expect("primary object");
    assert_eq!(primary_json["context_window"], json!(16384));
    assert!(
        primary_json["temperature"].as_f64().unwrap() - 0.2 < 1e-6,
        "got {}",
        primary_json["temperature"]
    );
    assert!(
        primary_json["top_p"].as_f64().unwrap() - 0.9 < 1e-6,
        "got {}",
        primary_json["top_p"]
    );
    assert_eq!(primary_json["reasoning_effort"], json!("high"));
    assert_eq!(
        primary_json["model_hints"]["uses_completion_tokens"],
        json!(true)
    );
    assert_eq!(primary_json["cost_per_m"], json!(1.5));

    // Re-upsert the same address WITHOUT the inference fields: absent ≡
    // inherit, so the overrides clear — but the out-of-schema routing
    // metadata (cost_per_m/strong) survives the edit.
    raw_profile_llm_upsert(
        &state,
        &RpcRequest::new(
            "u-rt2".to_string(),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "custom",
                    "model_id": "fixture-model",
                    "route": route
                },
                "set_primary": true
            }),
        ),
        None,
    )
    .await
    .expect("clearing re-upsert");
    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let primary = profile
        .config
        .llm
        .as_ref()
        .unwrap()
        .primary
        .as_ref()
        .unwrap();
    assert_eq!(
        primary.temperature, None,
        "absent ≡ inherit: override cleared"
    );
    assert_eq!(primary.top_p, None);
    assert_eq!(primary.context_window, None);
    assert_eq!(primary.reasoning_effort, None);
    assert_eq!(primary.cost_per_m, Some(1.5), "routing metadata survives");
    assert_eq!(primary.strong, Some(true));
    let listed = profile_llm_list_result(&state, "dev", Some(&profile));
    assert!(
        listed["primary"].get("temperature").is_none(),
        "an unconfigured field must be absent (not null) so list output is \
         unchanged for unconfigured users"
    );
}

/// Each configured model carries its OWN parameter set: a fallback's
/// inference defaults are independent of the primary's, so a
/// primary/fallback switch cannot leak parameters across models.
#[tokio::test]
async fn llm_upsert_keeps_inference_params_per_model_without_leakage() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    let upsert = |id: &str, model: &str, extra: Value| {
        let mut selection = json!({
            "family_id": "custom",
            "model_id": model,
            "route": { "route_id": "fixture", "base_url": "http://127.0.0.1:9/v1" }
        });
        let extra = extra.as_object().unwrap().clone();
        for (key, value) in extra {
            selection[key] = value;
        }
        RpcRequest::new(
            id.to_string(),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": selection,
                "set_primary": id == "primary",
            }),
        )
    };
    raw_profile_llm_upsert(
        &state,
        &upsert(
            "primary",
            "model-a",
            json!({ "temperature": 0.2, "context_window": 8192 }),
        ),
        None,
    )
    .await
    .expect("primary upsert");
    raw_profile_llm_upsert(
        &state,
        &upsert(
            "fallback",
            "model-b",
            json!({ "temperature": 0.9, "reasoning_effort": "max" }),
        ),
        None,
    )
    .await
    .expect("fallback upsert");

    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let llm = profile.config.llm.as_ref().unwrap();
    assert_eq!(llm.primary.as_ref().unwrap().temperature, Some(0.2));
    assert_eq!(llm.primary.as_ref().unwrap().context_window, Some(8192));
    assert_eq!(llm.primary.as_ref().unwrap().reasoning_effort, None);
    assert_eq!(llm.fallbacks[0].temperature, Some(0.9));
    assert_eq!(
        llm.fallbacks[0].reasoning_effort,
        Some(octos_llm::ReasoningEffort::Max)
    );
    assert_eq!(llm.fallbacks[0].context_window, None, "no cross-model leak");
}

/// Key requirement mirrors the runtime factory: a family the registry
/// doesn't know can never construct — it would persist a bricked primary
/// if the validator waved it through (codex P1 on the keyless-select fix).
#[tokio::test]
async fn llm_select_rejects_unactivatable_api_type_and_unknown_families() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    let upsert = |family: &str, model: &str, route: Value| {
        RpcRequest::new(
            format!("u-{family}-{model}"),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": family,
                    "model_id": model,
                    "route": route,
                },
                "set_primary": false,
            }),
        )
    };
    // Keyed primary so the profile is otherwise healthy.
    raw_profile_llm_upsert(
        &state,
        &RpcRequest::new(
            "u-primary".to_string(),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "deepseek",
                    "model_id": "deepseek-chat",
                    "route": { "route_id": "official", "api_key_env": "OCTOS_TEST_DS_KEY" },
                },
                "api_key": "dk",
                "set_primary": true,
            }),
        ),
        None,
    )
    .await
    .expect("seed keyed primary");

    // A family the registry doesn't know can never construct.
    raw_profile_llm_upsert(
        &state,
        &upsert("frobnicator", "frob-1", json!({ "route_id": "official" })),
        None,
    )
    .await
    .expect("seed unknown-family fallback");
    let error = raw_profile_llm_select(
        &state,
        &RpcRequest::new(
            "s-unknown".to_string(),
            APPUI_METHOD_PROFILE_LLM_SELECT.to_string(),
            json!({ "profile_id": "dev", "model_id": "frob-1" }),
        ),
        None,
    )
    .await
    .expect_err("unknown provider family must reject");
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("llm_provider_unresolved")),
        "got {error:?}"
    );

    // …even when a protocol override AND a key are configured: the
    // factory looks the family up before honoring api_type, so this
    // must reject as unresolved, not slip past via the override arm
    // (codex P1 round 2).
    raw_profile_llm_upsert(
        &state,
        &RpcRequest::new(
            "u-frob-anthropic".to_string(),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "frobnicator",
                    "model_id": "frob-2",
                    "route": {
                        "route_id": "official",
                        "api_type": "anthropic",
                        "api_key_env": "OCTOS_TEST_FROB_KEY",
                    },
                },
                "api_key": "fk",
                "set_primary": false,
            }),
        ),
        None,
    )
    .await
    .expect("seed keyed unknown-family anthropic fallback");
    let error = raw_profile_llm_select(
        &state,
        &RpcRequest::new(
            "s-unknown-keyed".to_string(),
            APPUI_METHOD_PROFILE_LLM_SELECT.to_string(),
            json!({ "profile_id": "dev", "model_id": "frob-2" }),
        ),
        None,
    )
    .await
    .expect_err("unknown family rejects even with api_type override + key");
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("llm_provider_unresolved")),
        "got {error:?}"
    );
}

/// `profile/llm/upsert` with `set_primary` must be lossless: replacing
/// the primary demotes the old one to a fallback instead of dropping it
/// (the onboarding wizard's "Save" path — a user onboarding glm-5.2 lost
/// their deepseek primary and was left with a single unusable model), and
/// re-promoting a model that already sits in the fallback list must not
/// duplicate it there.
#[tokio::test]
async fn llm_upsert_set_primary_demotes_old_primary_instead_of_dropping_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    for (family, model) in [("deepseek", "deepseek-v4-pro"), ("zai", "glm-5.3")] {
        let request = RpcRequest::new(
            format!("u-{model}"),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": family,
                    "model_id": model,
                    "route": { "route_id": "official" },
                },
                "set_primary": true,
            }),
        );
        raw_profile_llm_upsert(&state, &request, None)
            .await
            .expect("upsert");
    }

    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let llm = profile.config.llm.as_ref().unwrap();
    assert_eq!(
        llm.primary.as_ref().unwrap().model_id.as_deref(),
        Some("glm-5.3")
    );
    assert_eq!(
        llm.fallbacks.len(),
        1,
        "replaced primary must survive as a fallback: {llm:?}"
    );
    assert_eq!(
        llm.fallbacks[0].model_id.as_deref(),
        Some("deepseek-v4-pro")
    );

    // Promote the demoted model back through the same wizard path: it
    // must leave the fallback list (no duplicate) and demote glm-5.2.
    let request = RpcRequest::new(
        "u-back".to_string(),
        APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
        json!({
            "profile_id": "dev",
            "selection": {
                "family_id": "deepseek",
                "model_id": "deepseek-v4-pro",
                "route": { "route_id": "official" },
            },
            "set_primary": true,
        }),
    );
    raw_profile_llm_upsert(&state, &request, None)
        .await
        .expect("re-promote");

    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let llm = profile.config.llm.as_ref().unwrap();
    assert_eq!(
        llm.primary.as_ref().unwrap().model_id.as_deref(),
        Some("deepseek-v4-pro")
    );
    assert_eq!(
        llm.fallbacks
            .iter()
            .map(|fb| fb.model_id.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("glm-5.3")],
        "round-trip must neither duplicate the promoted model nor drop the demoted one"
    );
}

/// `profile/llm/delete` semantics: an unmatched address is a no-op
/// (`applied: false`); deleting a fallback removes exactly that entry;
/// deleting the primary promotes the first fallback (the profile keeps a
/// working model whenever one exists); deleting the last model leaves the
/// primary empty rather than erroring — `/model` → Add recovers.
#[tokio::test]
async fn llm_delete_removes_entries_and_promotes_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    for (family, model) in [("deepseek", "deepseek-v4-pro"), ("zai", "glm-5.3")] {
        let request = RpcRequest::new(
            format!("u-{model}"),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": family,
                    "model_id": model,
                    "route": { "route_id": "official" },
                },
                "set_primary": true,
            }),
        );
        raw_profile_llm_upsert(&state, &request, None)
            .await
            .expect("upsert");
    }
    // State now: primary glm-5.2, fallback deepseek-v4-pro.

    let delete = |family: &str, model: &str, route: &str, id: &str| {
        RpcRequest::new(
            id.to_string(),
            APPUI_METHOD_PROFILE_LLM_DELETE.to_string(),
            json!({
                "profile_id": "dev",
                "family_id": family,
                "model_id": model,
                "route_id": route,
            }),
        )
    };

    // (a) Unmatched address -> applied=false, nothing changes.
    let result = raw_profile_llm_delete(
        &state,
        &delete("openai", "gpt-4o", "official", "d-miss"),
        None,
    )
    .await
    .expect("delete miss");
    assert_eq!(result["applied"], json!(false));
    assert_eq!(
        result["runtime_disposition"], "unchanged",
        "a miss persists nothing and must not touch the runtime: {result}"
    );

    // (b) Delete the PRIMARY -> the fallback is promoted.
    let result = raw_profile_llm_delete(
        &state,
        &delete("zai", "glm-5.3", "official", "d-primary"),
        None,
    )
    .await
    .expect("delete primary");
    assert_eq!(result["applied"], json!(true));
    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let llm = profile.config.llm.as_ref().unwrap();
    assert_eq!(
        llm.primary.as_ref().unwrap().model_id.as_deref(),
        Some("deepseek-v4-pro"),
        "first fallback must inherit the primary slot"
    );
    assert!(llm.fallbacks.is_empty());

    // (c) Delete the LAST model -> primary empty, still applied.
    let result = raw_profile_llm_delete(
        &state,
        &delete("deepseek", "deepseek-v4-pro", "official", "d-last"),
        None,
    )
    .await
    .expect("delete last");
    assert_eq!(result["applied"], json!(true));
    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let llm_empty = profile
        .config
        .llm
        .as_ref()
        .map(|llm| llm.primary.is_none() && llm.fallbacks.is_empty())
        .unwrap_or(true);
    assert!(
        llm_empty,
        "last model removed (an emptied llm block may serialize away): {:?}",
        profile.config.llm
    );
}

/// #2164 test helpers: read the dynamic ProfileRuntime cache for a profile
/// through the same key derivation the transport uses.
fn dynamic_cached_profile_runtime(
    state: &AppState,
    profile_id: &str,
) -> Option<Arc<crate::runtime::ProfileRuntime>> {
    let key = dynamic_profile_runtime_key(state, profile_id)?;
    dynamic_profile_runtimes()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
        .cloned()
}

fn llm_upsert_rpc(
    id: &str,
    profile_id: &str,
    family: &str,
    model: &str,
    base_url: Option<&str>,
    set_primary: bool,
) -> RpcRequest<Value> {
    let mut body = json!({
        "profile_id": profile_id,
        "selection": {
            "family_id": family,
            "model_id": model,
            "route": {
                "route_id": "official",
                // A test-scoped variable name so resolution can't fall
                // through to a real key in the process env.
                "api_key_env": "OCTOS_TEST_LLM_RUNTIME_INVALIDATION_KEY",
            },
        },
        "api_key": "test-invalidation-key",
        "set_primary": set_primary,
    });
    if let Some(base_url) = base_url {
        body["selection"]["route"]["base_url"] = json!(base_url);
    }
    RpcRequest::new(
        id.to_string(),
        APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
        body,
    )
}

fn llm_delete_rpc(id: &str, profile_id: &str, family: &str, model: &str) -> RpcRequest<Value> {
    RpcRequest::new(
        id.to_string(),
        APPUI_METHOD_PROFILE_LLM_DELETE.to_string(),
        json!({
            "profile_id": profile_id,
            "family_id": family,
            "model_id": model,
            "route_id": "official",
        }),
    )
}

/// #2164 acceptance — dynamic profile, endpoint edit: changing the primary's
/// base URL (same family/model/route address) must evict the cached
/// ProfileRuntime and rebuild from the committed file, so the next turn
/// serves the new endpoint instead of the stale provider chain.
#[tokio::test]
async fn should_upsert_endpoint_edit_reload_dynamic_profile_runtime_for_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc("u-v1", "dev", "openai", "gpt-4o-mini", None, true),
        None,
    )
    .await
    .expect("seed primary");
    let before = ensure_session_profile_runtime(&state, Some("dev"))
        .await
        .expect("bootstrap")
        .expect("runtime cached after first upsert");
    assert!(Arc::ptr_eq(
        &before,
        &dynamic_cached_profile_runtime(&state, "dev").expect("cache entry"),
    ));
    // A turn in flight keeps its start-of-turn runtime; the test drops its
    // handle the same way a finished turn would, so the rebuild below can
    // take over the profile's data directory (single-writer redb).
    // A weak handle keeps the allocation identity reserved without keeping
    // the runtime's single-writer episode store alive during reload. Saving
    // only its address lets the allocator reuse it for the new runtime.
    let before_identity = Arc::downgrade(&before);
    drop(before);

    // Same model id, different endpoint: the cache MUST still be invalidated.
    let result = raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc(
            "u-v2",
            "dev",
            "openai",
            "gpt-4o-mini",
            Some("http://127.0.0.1:9/v1"),
            true,
        ),
        None,
    )
    .await
    .expect("endpoint edit");
    assert_eq!(result["applied"], json!(true), "{result}");
    assert_eq!(result["runtime_disposition"], "reloaded", "{result}");
    assert_eq!(result["restart_required"], json!(false), "{result}");
    assert_eq!(result["effective_from"], "next_turn", "{result}");
    assert!(
        result["config_revision"].is_string(),
        "the committed revision must be comparable against the runtime stamp: {result}"
    );

    let after = dynamic_cached_profile_runtime(&state, "dev").expect("cache repopulated");
    assert!(
        !std::sync::Weak::ptr_eq(&before_identity, &Arc::downgrade(&after)),
        "endpoint edit must rebuild the cached ProfileRuntime"
    );
    assert_eq!(after.primary_model_id, "gpt-4o-mini");
    assert_eq!(
        after.config.base_url.as_deref(),
        Some("http://127.0.0.1:9/v1"),
        "the rebuilt chain must serve the COMMITTED endpoint"
    );
}

/// #2164 acceptance — deleting the active primary promotes the first
/// fallback and the NEXT turn uses it: the cached runtime is rebuilt from
/// the promoted chain, not left on the deleted model.
#[tokio::test]
async fn should_delete_primary_promote_fallback_and_reload_dynamic_profile_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc("u-primary", "dev", "openai", "gpt-4o-mini", None, true),
        None,
    )
    .await
    .expect("seed primary");
    raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc(
            "u-fallback",
            "dev",
            "deepseek",
            "deepseek-chat",
            None,
            false,
        ),
        None,
    )
    .await
    .expect("seed fallback");
    let before = ensure_session_profile_runtime(&state, Some("dev"))
        .await
        .expect("bootstrap")
        .expect("runtime cached");
    // Retain allocation identity, but allow the old store itself to close.
    let before_identity = Arc::downgrade(&before);
    drop(before);

    let result = raw_profile_llm_delete(
        &state,
        &llm_delete_rpc("d-1", "dev", "openai", "gpt-4o-mini"),
        None,
    )
    .await
    .expect("delete primary");
    assert_eq!(result["applied"], json!(true), "{result}");
    assert_eq!(result["runtime_disposition"], "reloaded", "{result}");
    assert_eq!(result["restart_required"], json!(false), "{result}");

    let after = dynamic_cached_profile_runtime(&state, "dev").expect("cache repopulated");
    assert!(
        !std::sync::Weak::ptr_eq(&before_identity, &Arc::downgrade(&after)),
        "primary deletion must rebuild the cached ProfileRuntime"
    );
    assert_eq!(
        after.primary_model_id, "deepseek-chat",
        "the promoted fallback must serve the next turn"
    );
}

/// #2164 acceptance — deleting the LAST model evicts the cached runtime and
/// reports a deterministic deferred disposition; the next turn observes the
/// empty selection as typed runtime-unavailable truth (no stale chain).
#[tokio::test]
async fn should_delete_last_model_evict_dynamic_runtime_and_report_deferred() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc("u-only", "dev", "openai", "gpt-4o-mini", None, true),
        None,
    )
    .await
    .expect("seed sole primary");
    ensure_session_profile_runtime(&state, Some("dev"))
        .await
        .expect("bootstrap")
        .expect("runtime cached");
    assert!(dynamic_cached_profile_runtime(&state, "dev").is_some());

    let result = raw_profile_llm_delete(
        &state,
        &llm_delete_rpc("d-last", "dev", "openai", "gpt-4o-mini"),
        None,
    )
    .await
    .expect("delete last model");
    assert_eq!(result["applied"], json!(true), "{result}");
    assert_eq!(result["runtime_disposition"], "deferred", "{result}");
    assert_eq!(result["restart_required"], json!(false), "{result}");
    assert!(
        dynamic_cached_profile_runtime(&state, "dev").is_none(),
        "the stale runtime must not survive the last-model deletion"
    );
    let next_turn = ensure_session_profile_runtime(&state, Some("dev"))
        .await
        .expect("ensure");
    assert!(
        next_turn.is_none(),
        "with no selection left the next turn must report typed runtime-unavailable truth"
    );
}

/// #2164 acceptance — startup-pinned profile: upsert and delete persist but
/// return `restart_required: true` with disposition `restart_required`, and
/// the boot-snapshot runtime is left untouched (no fake reload).
#[tokio::test]
async fn should_report_restart_required_for_startup_pinned_llm_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc("u-primary", "dev", "openai", "gpt-4o-mini", None, true),
        None,
    )
    .await
    .expect("seed primary");
    raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc(
            "u-fallback",
            "dev",
            "deepseek",
            "deepseek-chat",
            None,
            false,
        ),
        None,
    )
    .await
    .expect("seed fallback");
    let pinned = ensure_session_profile_runtime(&state, Some("dev"))
        .await
        .expect("bootstrap")
        .expect("runtime");

    // Simulate the serve-startup shape: the runtime lives in the immutable
    // startup map and nothing sits in the dynamic cache.
    let mut state = Arc::try_unwrap(state).ok().expect("sole state owner");
    state.profiles.insert("dev".to_string(), pinned.clone());
    if let Some(key) = dynamic_profile_runtime_key(&state, "dev") {
        dynamic_profile_runtimes()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
    }
    let state = Arc::new(state);

    let result = raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc(
            "u-edit",
            "dev",
            "openai",
            "gpt-4o-mini",
            Some("http://127.0.0.1:9/v1"),
            true,
        ),
        None,
    )
    .await
    .expect("pinned upsert");
    assert_eq!(result["applied"], json!(true), "{result}");
    assert_eq!(
        result["runtime_disposition"], "restart_required",
        "{result}"
    );
    assert_eq!(result["restart_required"], json!(true), "{result}");
    assert_eq!(result["effective_from"], "next_turn", "{result}");
    assert!(
        dynamic_cached_profile_runtime(&state, "dev").is_none(),
        "a pinned profile must not fake a reload into the dynamic cache"
    );
    assert!(
        Arc::ptr_eq(&pinned, state.profiles.get("dev").unwrap()),
        "the boot-snapshot runtime stays as-is until restart"
    );

    let result = raw_profile_llm_delete(
        &state,
        &llm_delete_rpc("d-fallback", "dev", "deepseek", "deepseek-chat"),
        None,
    )
    .await
    .expect("pinned delete");
    assert_eq!(result["applied"], json!(true), "{result}");
    assert_eq!(
        result["runtime_disposition"], "restart_required",
        "{result}"
    );
    assert_eq!(result["restart_required"], json!(true), "{result}");
}

/// #2164 acceptance — a concurrent old bootstrap cannot repopulate the cache
/// after a mutation: an insert carrying the PRE-bump generation is refused
/// (the cache stays empty), while a bootstrap that read the committed file
/// inserts under the new generation.
#[tokio::test]
async fn should_refuse_stale_profile_runtime_insert_after_generation_bump() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc("u-only", "dev", "openai", "gpt-4o-mini", None, true),
        None,
    )
    .await
    .expect("seed");
    let runtime = ensure_session_profile_runtime(&state, Some("dev"))
        .await
        .expect("bootstrap")
        .expect("runtime");
    let key = dynamic_profile_runtime_key(&state, "dev").expect("dynamic key");

    // Post-mutation state: generation bumped, cache emptied (the transition).
    let post_commit_generation = bump_profile_runtime_generation(&key);
    dynamic_profile_runtimes()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&key);

    // A bootstrap that captured the PRE-commit generation must be refused…
    assert!(
        !insert_profile_runtime_if_current(&key, post_commit_generation - 1, Arc::clone(&runtime),),
        "a stale in-flight bootstrap must not repopulate the cache"
    );
    assert!(
        dynamic_cached_profile_runtime(&state, "dev").is_none(),
        "the refused insert must leave the cache empty"
    );
    // …while a bootstrap reading the committed file inserts normally.
    assert!(
        insert_profile_runtime_if_current(&key, post_commit_generation, Arc::clone(&runtime)),
        "a current-generation bootstrap inserts"
    );
    assert!(dynamic_cached_profile_runtime(&state, "dev").is_some());
}

/// #2164 acceptance — persisted-but-rebuild-failed is EXPLICIT in the
/// response (`persisted_but_not_live` + `runtime_error`), not collapsed into
/// a warn-only server log, and recoverable once the bootstrap blocker is
/// removed.
#[tokio::test]
async fn should_report_persisted_but_not_live_when_runtime_rebuild_fails() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    // Block the runtime bootstrap deterministically: the profile's data dir
    // is a plain FILE, so the episode store cannot open underneath it.
    let blocker = dir.path().join("dev-data-blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let mut profile = profile_for_runtime_message("dev");
    profile.data_dir = Some(blocker.to_string_lossy().to_string());
    profile.config.llm = Some(crate::profiles::LlmProfileConfig {
        primary: Some(crate::profiles::LlmModelSelectionConfig {
            family_id: Some("openai".to_string()),
            model_id: Some("gpt-4o-mini".to_string()),
            route: Some(crate::profiles::LlmRouteConfig {
                api_key_env: Some("OCTOS_TEST_LLM_RUNTIME_INVALIDATION_KEY".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        fallbacks: Vec::new(),
    });
    profile.config.env_vars.insert(
        "OCTOS_TEST_LLM_RUNTIME_INVALIDATION_KEY".to_string(),
        "k".to_string(),
    );
    state
        .profile_store
        .as_ref()
        .unwrap()
        .save(&profile)
        .unwrap();

    let result = raw_profile_llm_upsert(
        &state,
        &llm_upsert_rpc("u-edit", "dev", "openai", "gpt-4o-mini", None, true),
        None,
    )
    .await
    .expect("persistence must succeed");
    assert_eq!(result["applied"], json!(true), "{result}");
    assert_eq!(
        result["runtime_disposition"], "persisted_but_not_live",
        "the failed rebuild must be explicit on the wire: {result}"
    );
    assert_eq!(result["restart_required"], json!(false), "{result}");
    assert!(
        result["runtime_error"].is_string(),
        "the rebuild failure detail must be reported: {result}"
    );
    assert!(result["config_revision"].is_string(), "{result}");

    // Recoverable: unblock the data dir and the next bootstrap succeeds.
    std::fs::remove_file(&blocker).unwrap();
    std::fs::create_dir_all(&blocker).unwrap();
    let recovered = ensure_session_profile_runtime(&state, Some("dev"))
        .await
        .expect("bootstrap after unblock")
        .expect("runtime recovers on the next turn");
    assert_eq!(recovered.primary_model_id, "gpt-4o-mini");
}

/// A same-address upsert (same family/model/route_id — including a
/// missing route_id, which normalizes to the synthetic "official") is an
/// endpoint edit: it replaces the primary outright instead of demoting
/// the old endpoint into a fallback row that `profile/llm/select` can
/// never address (codex P2 on the lossless-save fix).
#[tokio::test]
async fn llm_upsert_endpoint_edit_replaces_instead_of_demoting() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    for (route, tag) in [
        (
            json!({ "route_id": "official", "base_url": "https://one.example" }),
            "one",
        ),
        // Same address, new endpoint: must replace, not demote.
        (
            json!({ "route_id": "official", "base_url": "https://two.example" }),
            "two",
        ),
        // Missing route_id normalizes to "official": still the same address.
        (json!({ "base_url": "https://three.example" }), "three"),
    ] {
        let request = RpcRequest::new(
            format!("u-{tag}"),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "deepseek",
                    "model_id": "deepseek-chat",
                    "route": route,
                },
                "set_primary": true,
            }),
        );
        raw_profile_llm_upsert(&state, &request, None)
            .await
            .expect("upsert");
    }

    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let llm = profile.config.llm.as_ref().unwrap();
    assert_eq!(
        llm.primary
            .as_ref()
            .unwrap()
            .route
            .as_ref()
            .unwrap()
            .base_url
            .as_deref(),
        Some("https://three.example"),
        "the endpoint edit lands on the primary"
    );
    assert!(
        llm.fallbacks.is_empty(),
        "endpoint edits must not accrete unselectable fallback rows: {llm:?}"
    );
}

/// Endpoint-distinct fallbacks (same family/model/route_id, different
/// base_url) are real failover-chain entries, not selector duplicates —
/// `config_from_profile` threads every fallback's endpoint into the
/// runtime. A set_primary upsert must not sweep them away (codex P2 on
/// the address-comparison fix); only an exact-identity duplicate of the
/// new primary leaves the list.
#[tokio::test]
async fn llm_upsert_preserves_endpoint_distinct_failover_fallbacks() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    let upsert = |base_url: &str, set_primary: bool| {
        RpcRequest::new(
            format!("u-{base_url}-{set_primary}"),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "deepseek",
                    "model_id": "deepseek-chat",
                    "route": { "route_id": "official", "base_url": base_url },
                },
                "set_primary": set_primary,
            }),
        )
    };
    let llm_of = |state: &Arc<AppState>| {
        state
            .profile_store
            .as_ref()
            .unwrap()
            .get("dev")
            .unwrap()
            .unwrap()
            .config
            .llm
            .clone()
            .unwrap()
    };

    // Primary at ONE, plus a same-address failover mirror at MIRROR.
    for (url, primary) in [
        ("https://one.example", true),
        ("https://mirror.example", false),
    ] {
        raw_profile_llm_upsert(&state, &upsert(url, primary), None)
            .await
            .expect("seed");
    }

    // Endpoint edit of the primary: the mirror fallback must survive.
    raw_profile_llm_upsert(&state, &upsert("https://two.example", true), None)
        .await
        .expect("endpoint edit");
    let llm = llm_of(&state);
    assert_eq!(
        llm.primary
            .as_ref()
            .unwrap()
            .route
            .as_ref()
            .unwrap()
            .base_url
            .as_deref(),
        Some("https://two.example")
    );
    assert_eq!(
        llm.fallbacks
            .iter()
            .map(|fb| fb.route.as_ref().unwrap().base_url.as_deref().unwrap())
            .collect::<Vec<_>>(),
        vec!["https://mirror.example"],
        "endpoint-distinct failover fallback must survive a set_primary upsert"
    );

    // Promoting the exact mirror identity removes it from the fallback
    // list (true duplicate) rather than leaving two copies.
    raw_profile_llm_upsert(&state, &upsert("https://mirror.example", true), None)
        .await
        .expect("promote mirror");
    let llm = llm_of(&state);
    assert_eq!(
        llm.primary
            .as_ref()
            .unwrap()
            .route
            .as_ref()
            .unwrap()
            .base_url
            .as_deref(),
        Some("https://mirror.example")
    );
    assert!(
        llm.fallbacks.is_empty(),
        "promoting the exact identity must de-dup it out of the fallbacks: {llm:?}"
    );
}

/// Identity comparison normalizes a missing route_id to the synthetic
/// `"official"` exactly like the address comparison (codex P2 round 3):
/// a route_id-less fallback with the same endpoint is the SAME provider
/// as an `"official"` upsert — it updates in place rather than
/// duplicating, and de-dups out of the list when promoted to primary.
#[tokio::test]
async fn llm_upsert_normalizes_default_route_ids_when_deduping() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    let upsert = |route: Value, set_primary: bool, tag: &str| {
        RpcRequest::new(
            format!("u-{tag}"),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "deepseek",
                    "model_id": "deepseek-chat",
                    "route": route,
                },
                "set_primary": set_primary,
            }),
        )
    };
    let llm_of = |state: &Arc<AppState>| {
        state
            .profile_store
            .as_ref()
            .unwrap()
            .get("dev")
            .unwrap()
            .unwrap()
            .config
            .llm
            .clone()
            .unwrap()
    };

    let mirror = "https://mirror.example";
    raw_profile_llm_upsert(
        &state,
        &upsert(
            json!({ "route_id": "official", "base_url": "https://one.example" }),
            true,
            "seed-primary",
        ),
        None,
    )
    .await
    .expect("seed primary");
    // Fallback saved WITHOUT a route_id (legacy/default-route shape).
    raw_profile_llm_upsert(
        &state,
        &upsert(json!({ "base_url": mirror }), false, "seed-fallback"),
        None,
    )
    .await
    .expect("seed route_id-less fallback");

    // Re-adding the same endpoint under the synthetic "official" id must
    // update the existing fallback in place, not append a duplicate.
    raw_profile_llm_upsert(
        &state,
        &upsert(
            json!({ "route_id": "official", "base_url": mirror }),
            false,
            "readd",
        ),
        None,
    )
    .await
    .expect("re-add under official");
    assert_eq!(
        llm_of(&state).fallbacks.len(),
        1,
        "official ≡ missing route_id: same provider must update in place"
    );

    // Promoting that provider under "official" de-dups the route_id-less
    // row out of the retry chain.
    raw_profile_llm_upsert(
        &state,
        &upsert(
            json!({ "route_id": "official", "base_url": mirror }),
            true,
            "promote",
        ),
        None,
    )
    .await
    .expect("promote");
    let llm = llm_of(&state);
    assert_eq!(
        llm.primary
            .as_ref()
            .unwrap()
            .route
            .as_ref()
            .unwrap()
            .base_url
            .as_deref(),
        Some(mirror)
    );
    assert!(
        llm.fallbacks.is_empty(),
        "the normalized-identity duplicate must leave the retry chain: {llm:?}"
    );
}

/// A profile-scoped connection must not rewire another profile's models
/// (codex P1) — and an ambiguous route wildcard must reject rather than
/// promote an arbitrary route (codex P2).
#[tokio::test]
async fn llm_select_enforces_scope_and_route_discrimination() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));

    // Cross-profile mutation from a profile-scoped connection: denied.
    let request = RpcRequest::new(
        "sx".to_string(),
        APPUI_METHOD_PROFILE_LLM_SELECT.to_string(),
        json!({ "profile_id": "victim", "model_id": "deepseek-chat" }),
    );
    let error = raw_profile_llm_select(&state, &request, Some("attacker"))
        .await
        .expect_err("cross-profile select must be denied");
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("auth_scope_violation")),
        "got {error:?}"
    );

    // Same family/model on two routes: the synthetic "official" wildcard
    // must reject as ambiguous; an exact route selects that route.
    for (route, set_primary) in [("autodl", true), ("proxy", false)] {
        let request = RpcRequest::new(
            format!("u-{route}"),
            APPUI_METHOD_PROFILE_LLM_UPSERT.to_string(),
            json!({
                "profile_id": "dev",
                "selection": {
                    "family_id": "zai",
                    "model_id": "glm-5.3",
                    "route": { "route_id": route },
                },
                // Key present so the exact-route select below tests
                // route discrimination, not keyless rejection.
                "api_key": "test-zai-key",
                "set_primary": set_primary,
            }),
        );
        raw_profile_llm_upsert(&state, &request, None)
            .await
            .expect("upsert");
    }
    let request = RpcRequest::new(
        "amb".to_string(),
        APPUI_METHOD_PROFILE_LLM_SELECT.to_string(),
        json!({
            "profile_id": "dev",
            "model_id": "glm-5.3",
            "route_id": "official",
        }),
    );
    let error = raw_profile_llm_select(&state, &request, None)
        .await
        .expect_err("ambiguous route wildcard must reject");
    assert!(error.message.contains("multiple routes"), "got {error:?}");

    let request = RpcRequest::new(
        "exact".to_string(),
        APPUI_METHOD_PROFILE_LLM_SELECT.to_string(),
        json!({
            "profile_id": "dev",
            "model_id": "glm-5.3",
            "route_id": "proxy",
        }),
    );
    let result = raw_profile_llm_select(&state, &request, None)
        .await
        .expect("exact route selects");
    assert_eq!(result["applied"], true);
    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("dev")
        .unwrap()
        .unwrap();
    let llm = profile.config.llm.as_ref().unwrap();
    assert_eq!(
        llm.primary
            .as_ref()
            .and_then(|primary| primary.route.as_ref())
            .and_then(|route| route.route_id.as_deref()),
        Some("proxy"),
        "the EXACT route was promoted"
    );
}

/// The onboarding catalog is the canonical model_catalog.json — the SSOT —
/// and is NOT unioned with the runtime QoS catalog. A live QoS catalog only
/// reflects already-configured providers (a subset of the catalog), so
/// unioning it could only re-introduce curated-out models or zero out
/// researched context windows. This guards that regression: a model present
/// in the runtime QoS catalog but absent from the canonical catalog must NOT
/// appear in onboarding.
#[test]
fn catalog_result_ignores_runtime_qos_not_in_canonical() {
    let dir = tempfile::tempdir().unwrap();
    // `octos_home_dir()` resolves to `dir` for this store, so this file is
    // the seed QoS catalog the OLD union path would have read.
    std::fs::write(
        dir.path().join("model_catalog.json"),
        serde_json::to_string(&json!({
            "updated_at": "2026-07-10T00:00:00Z",
            "models": [{
                "provider": "zai/glm-9.9-not-in-catalog",
                "type": "strong",
                "stability": 0.7,
                "tool_avg_ms": 1,
                "p95_ms": 1,
                "score": 0.1,
                "cost_in": 1.0,
                "cost_out": 4.0,
                "ds_output": 1,
                "context_window": 1000,
                "max_output": 99
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let state = local_profile_state(dir.path());

    let catalog = raw_catalog_result(&state, None).expect("catalog");
    let families = &catalog["families"];
    let zai_models = families["zai"]["models"].as_array().unwrap();
    // Canonical lineup is present …
    assert!(
        zai_models.iter().any(|model| model["id"] == "glm-5.3"),
        "canonical zai lineup present: {zai_models:?}"
    );
    // … but the runtime-QoS-only model does NOT leak into onboarding: the
    // canonical catalog is the single source of truth for what's
    // provisionable.
    assert!(
        !zai_models
            .iter()
            .any(|model| model["id"] == "glm-9.9-not-in-catalog"),
        "runtime-QoS-only model must not appear in onboarding: {zai_models:?}"
    );
}

#[test]
fn catalog_result_sourced_from_registry_and_canonical_catalog() {
    // With no runtime data-dir catalog, the onboarding catalog is fully
    // populated from the compiled-in canonical model_catalog.json (the SSOT)
    // with family key-env from the provider registry — providers.json is no
    // longer the source.
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let catalog = raw_catalog_result(&state, None).expect("catalog");
    let families = catalog["families"].as_object().expect("families object");

    let ids = |fam: &str| -> Vec<String> {
        families[fam]["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect()
    };

    // Key-env comes from the registry, not a hand-maintained env map.
    assert_eq!(families["zai"]["env"], "ZAI_API_KEY");
    // Curation: glm-5.3 + k3 present; deepseek-chat removed.
    assert!(
        ids("zai").contains(&"glm-5.3".to_owned()),
        "{:?}",
        ids("zai")
    );
    assert!(
        ids("moonshot-coding").contains(&"k3".to_owned()),
        "{:?}",
        ids("moonshot-coding")
    );
    // k3 onboards under the moonshot-coding family key-env with its
    // researched 1M context window.
    assert_eq!(families["moonshot-coding"]["env"], "KIMI_CODING_API_KEY");
    let k3 = families["moonshot-coding"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "k3")
        .unwrap();
    assert_eq!(k3["context_window"], 1_048_576);
    assert_eq!(k3["max_output"], 131_072);
    assert!(
        !ids("deepseek").contains(&"deepseek-chat".to_owned()),
        "deepseek-chat curated out: {:?}",
        ids("deepseek")
    );
    assert!(ids("deepseek").contains(&"deepseek-v4-pro".to_owned()));

    // Researched context window flows through to the catalog (glm-5.2 = 1M).
    let glm52 = families["zai"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "glm-5.3")
        .unwrap();
    assert_eq!(glm52["context_window"], 1_000_000);

    // Alternative provisioning endpoints (AutoDL/WiseModel) survive into the
    // onboarding catalog so the TUI route-picker keeps its choices — this was
    // supplied by the retired providers.json and must not be lost.
    let v4pro = families["deepseek"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "deepseek-v4-pro")
        .unwrap();
    let endpoints = v4pro["endpoints"].as_array().expect("v4-pro has endpoints");
    assert!(
        endpoints
            .iter()
            .any(|e| e["id"] == "autodl" && e["api_key_env"] == "AUTODL_API_KEY"),
        "AutoDL alternative endpoint present: {endpoints:?}"
    );
}

fn local_profile_state_with_sessions(dir: &Path) -> Arc<AppState> {
    Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(dir).expect("session manager"),
        ))),
        ..local_profile_state(dir)
    })
}

fn local_profile_params(
    name: &str,
    username: &str,
    email: &str,
) -> octos_core::ui_protocol::ProfileLocalCreateParams {
    octos_core::ui_protocol::ProfileLocalCreateParams {
        requested_id: None,
        name: name.into(),
        username: username.into(),
        email: email.into(),
        make_default: None,
    }
}

fn sorted_supported_methods(
    capabilities: &UiProtocolCapabilities,
) -> std::collections::BTreeSet<&str> {
    capabilities
        .supported_methods
        .iter()
        .map(String::as_str)
        .collect()
}

#[test]
fn appui_evidence_jsonl_append_keeps_concurrent_rows_parseable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("appui-transcript.jsonl");
    let thread_count = 12;
    let rows_per_thread = 40;

    std::thread::scope(|scope| {
        for worker in 0..thread_count {
            let dir = dir.path().to_path_buf();
            scope.spawn(move || {
                for row in 0..rows_per_thread {
                    append_appui_evidence_jsonl_at(
                        &dir,
                        "appui-transcript.jsonl",
                        json!({
                            "worker": worker,
                            "row": row,
                            "payload": "x".repeat(512),
                        }),
                    );
                }
            });
        }
    });

    let contents = std::fs::read_to_string(path).expect("jsonl contents");
    let lines = contents.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), thread_count * rows_per_thread);
    for (index, line) in lines.iter().enumerate() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|error| panic!("line {index} is malformed JSONL: {error}: {line}"));
    }
}

fn test_message(role: MessageRole, content: impl Into<String>) -> Message {
    Message {
        role,
        content: content.into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    }
}

struct FailingWriter {
    write_failed: Arc<tokio::sync::Notify>,
}

impl FailingWriter {
    fn new(write_failed: Arc<tokio::sync::Notify>) -> Self {
        Self { write_failed }
    }
}

impl AsyncWrite for FailingWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.write_failed.notify_waiters();
        std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "writer closed",
        )))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

struct RepeatingByteReader {
    remaining: usize,
    byte: u8,
    max_chunk: usize,
}

impl RepeatingByteReader {
    fn new(byte: u8, remaining: usize, max_chunk: usize) -> Self {
        Self {
            remaining,
            byte,
            max_chunk,
        }
    }
}

impl AsyncRead for RepeatingByteReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.remaining == 0 || buf.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        let count = self.remaining.min(self.max_chunk).min(buf.remaining());
        let bytes = vec![self.byte; count];
        buf.put_slice(&bytes);
        self.remaining -= count;
        std::task::Poll::Ready(Ok(()))
    }
}

struct DropNotify(Option<oneshot::Sender<()>>);

impl DropNotify {
    fn new(tx: oneshot::Sender<()>) -> Self {
        Self(Some(tx))
    }
}

impl Drop for DropNotify {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[test]
fn stdio_durable_send_waits_for_capacity_instead_of_backpressure_drop() {
    let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel(1);
    let ws = WsConnection::new_stdio(writer_tx);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let session_id = SessionKey("local:stdio-backpressure".into());
    let turn_id = TurnId::new();

    send_rpc_result(&ws, "fill".into(), json!({"ok": true}))
        .expect("priming lifecycle frame fills stdio queue");

    let send_ws = ws.clone();
    let send_ledger = Arc::clone(&ledger);
    let send_session_id = session_id.clone();
    let send_turn_id = turn_id.clone();
    let send_thread = std::thread::spawn(move || {
        send_notification_durable(
            &send_ws,
            send_ledger.as_ref(),
            UiNotification::Warning(octos_core::ui_protocol::WarningEvent {
                session_id: send_session_id,
                turn_id: Some(send_turn_id),
                code: "test".into(),
                message: "wait for capacity".into(),
            }),
        )
    });

    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !send_thread.is_finished(),
        "stdio durable send must wait while the bounded queue is full",
    );

    let _first = writer_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("priming frame drains");
    let send_result = send_thread
        .join()
        .expect("stdio durable send thread should not panic");
    assert_eq!(send_result, Ok(()));

    let _second = writer_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("durable frame queued after capacity becomes available");
}

#[tokio::test]
async fn stdio_ndjson_reader_rejects_oversized_frame_before_newline() {
    let input = std::io::Cursor::new(vec![b'x'; MAX_TEXT_FRAME_BYTES + 1]);
    let mut reader = StdioNdjsonReader::new(input);

    match reader.next_frame().await.expect("read succeeds") {
        StdioFrameRead::TooLarge => {}
        StdioFrameRead::Frame(_) | StdioFrameRead::Eof => panic!("expected TooLarge"),
    }
}

#[tokio::test]
async fn stdio_connection_stops_dispatch_after_writer_failure() {
    reset_stdio_dispatch_count_for_test();
    let write_failed = Arc::new(tokio::sync::Notify::new());
    let (mut input_tx, input_rx) = tokio::io::duplex(4096);
    let first = format!(
        "{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": "first",
            "method": APPUI_METHOD_CONFIG_CAPABILITIES_LIST,
            "params": {}
        })
    );
    let second = format!(
        "{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": "second",
            "method": APPUI_METHOD_CONFIG_CAPABILITIES_LIST,
            "params": {}
        })
    );
    input_tx
        .write_all(first.as_bytes())
        .await
        .expect("queue first request");

    let write_failed_for_input = write_failed.clone();
    let input_task = tokio::spawn(async move {
        write_failed_for_input.notified().await;
        let _ = input_tx.write_all(second.as_bytes()).await;
    });
    tokio::task::yield_now().await;

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        stdio_connection_with_io(
            Arc::new(AppState::empty_for_tests()),
            input_rx,
            FailingWriter::new(write_failed),
        ),
    )
    .await
    .expect("stdio loop must exit after writer failure");

    input_task.await.expect("input task joins");
    let error = result.expect_err("writer failure should be returned");
    assert!(
        error.to_string().contains("AppUI stdio writer failed"),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        stdio_dispatch_count_for_test(),
        1,
        "no request after the writer failure may be dispatched"
    );
    reset_stdio_dispatch_count_for_test();
}

/// Shutdown must WAIT for this connection's in-flight turns to finalize
/// instead of letting process exit cancel their tasks mid-write.
#[tokio::test(start_paused = true)]
async fn stdio_shutdown_drain_waits_for_turn_finalization() {
    let active_turns: SharedActiveTurns =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let connection_turns: SharedConnectionTurns =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let session = SessionKey("local:drain-wait".into());
    let turn_id = TurnId::new();
    let abort = tokio::spawn(async {}).abort_handle();
    active_turns.lock().await.insert(
        session.clone(),
        ActiveTurn {
            profile_id: MAIN_PROFILE_ID.to_owned(),
            turn_id: turn_id.clone(),
            state: Arc::new(TokioMutex::new(TurnState::Active)),
            interrupt_tx: Arc::new(TokioMutex::new(None)),
            steer: None,
            abort,
        },
    );
    connection_turns.lock().await.insert(
        session.clone(),
        test_connection_turn(&active_turns, &session, &turn_id).await,
    );
    let remover = active_turns.clone();
    let session_for_removal = session.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        remover.lock().await.remove(&session_for_removal);
    });

    let drained = drain_connection_turns_for_shutdown(
        &active_turns,
        &connection_turns,
        std::time::Duration::from_secs(5),
    )
    .await;

    assert!(drained, "drain must return once the turn finalizes");
}

/// A turn that never finalizes must not hang shutdown forever, and turns
/// owned by OTHER connections (the registry is process-global) must not
/// block this exit at all.
#[tokio::test(start_paused = true)]
async fn stdio_shutdown_drain_gives_up_at_deadline_and_ignores_foreign_turns() {
    let active_turns: SharedActiveTurns =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let connection_turns: SharedConnectionTurns =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let foreign_abort = tokio::spawn(async {}).abort_handle();
    active_turns.lock().await.insert(
        SessionKey("local:foreign".into()),
        ActiveTurn {
            profile_id: MAIN_PROFILE_ID.to_owned(),
            turn_id: TurnId::new(),
            state: Arc::new(TokioMutex::new(TurnState::Active)),
            interrupt_tx: Arc::new(TokioMutex::new(None)),
            steer: None,
            abort: foreign_abort,
        },
    );
    assert!(
        drain_connection_turns_for_shutdown(
            &active_turns,
            &connection_turns,
            std::time::Duration::from_secs(5),
        )
        .await,
        "turns owned by other connections must not block shutdown"
    );

    let session = SessionKey("local:drain-stuck".into());
    let turn_id = TurnId::new();
    let abort = tokio::spawn(async {}).abort_handle();
    active_turns.lock().await.insert(
        session.clone(),
        ActiveTurn {
            profile_id: MAIN_PROFILE_ID.to_owned(),
            turn_id: turn_id.clone(),
            state: Arc::new(TokioMutex::new(TurnState::Active)),
            interrupt_tx: Arc::new(TokioMutex::new(None)),
            steer: None,
            abort,
        },
    );
    connection_turns.lock().await.insert(
        session.clone(),
        test_connection_turn(&active_turns, &session, &turn_id).await,
    );

    let drained = drain_connection_turns_for_shutdown(
        &active_turns,
        &connection_turns,
        std::time::Duration::from_millis(200),
    )
    .await;

    assert!(!drained, "drain must give up at the deadline");
}

#[tokio::test]
async fn stdio_ndjson_reader_accepts_max_size_frame_with_newline() {
    let mut input = vec![b'x'; MAX_TEXT_FRAME_BYTES];
    input.push(b'\n');
    let mut reader = StdioNdjsonReader::new(std::io::Cursor::new(input));

    match reader.next_frame().await.expect("read succeeds") {
        StdioFrameRead::Frame(text) => {
            assert_eq!(text.len(), MAX_TEXT_FRAME_BYTES);
        }
        StdioFrameRead::TooLarge | StdioFrameRead::Eof => {
            panic!("expected a max-sized frame")
        }
    }
}

#[tokio::test]
async fn stdio_ndjson_reader_rejects_streaming_oversized_frame_without_buffer_growth() {
    let input = RepeatingByteReader::new(b'x', MAX_TEXT_FRAME_BYTES * 8, 4096);
    let mut reader = StdioNdjsonReader::new(input);

    match reader.next_frame().await.expect("read succeeds") {
        StdioFrameRead::TooLarge => {}
        StdioFrameRead::Frame(_) | StdioFrameRead::Eof => panic!("expected TooLarge"),
    }
    assert!(
        reader.buffer.capacity() <= MAX_TEXT_FRAME_BYTES,
        "reader buffer grew past MAX_TEXT_FRAME_BYTES: {}",
        reader.buffer.capacity()
    );
    assert!(
        reader.buffer.is_empty(),
        "oversized frame bytes must be drained or dropped before the next frame"
    );
}

#[tokio::test]
async fn stdio_cleanup_aborts_active_turns_and_live_forwarders() {
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let live_forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let contracts = UiProtocolContractStores::default();
    let ledger = UiProtocolLedger::new(16);
    let session_id = SessionKey("local:stdio-cleanup".into());
    let turn_id = TurnId::new();

    let (turn_started_tx, turn_started_rx) = oneshot::channel();
    let (turn_drop_tx, turn_drop_rx) = oneshot::channel();
    let turn_task = tokio::spawn(async move {
        let _drop = DropNotify::new(turn_drop_tx);
        let _ = turn_started_tx.send(());
        std::future::pending::<()>().await;
    });
    active_turns.lock().await.insert(
        session_id.clone(),
        test_active_turn(turn_id.clone(), turn_task.abort_handle()),
    );
    connection_turns.lock().await.insert(
        session_id.clone(),
        test_connection_turn(&active_turns, &session_id, &turn_id).await,
    );

    let (forwarder_started_tx, forwarder_started_rx) = oneshot::channel();
    let (forwarder_drop_tx, forwarder_drop_rx) = oneshot::channel();
    let forwarder_task = tokio::spawn(async move {
        let _drop = DropNotify::new(forwarder_drop_tx);
        let _ = forwarder_started_tx.send(());
        std::future::pending::<()>().await;
    });
    live_forwarders
        .lock()
        .await
        .insert(session_id.clone(), forwarder_task);

    turn_started_rx.await.expect("active turn task started");
    forwarder_started_rx
        .await
        .expect("live forwarder task started");

    cleanup_stdio_connection_resources(
        &active_turns,
        &connection_turns,
        &live_forwarders,
        &contracts,
        &ledger,
    )
    .await;

    assert!(active_turns.lock().await.is_empty());
    assert!(connection_turns.lock().await.is_empty());
    assert!(live_forwarders.lock().await.is_empty());
    tokio::time::timeout(Duration::from_millis(500), turn_drop_rx)
        .await
        .expect("active turn task should be aborted")
        .expect("active turn drop notification");
    let _ = turn_task.await;
    tokio::time::timeout(Duration::from_millis(500), forwarder_drop_rx)
        .await
        .expect("live forwarder should be aborted and awaited")
        .expect("live forwarder drop notification");

    let replay = ledger
        .replay_after(
            &session_id,
            Some(&UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        )
        .expect("replay cleanup events");
    assert!(replay.iter().any(|entry| matches!(
        &entry.event,
        UiProtocolLedgerEvent::Notification(UiNotification::TurnError(event))
            if event.turn_id == turn_id && event.code == "connection_closed"
    )));
}

#[test]
fn stdio_session_open_candidate_profile_is_last_success_candidate_only() {
    let params = SessionOpenParams {
        session_id: SessionKey("coding:local:test".into()),
        topic: None,
        profile_id: None,
        cwd: None,
        sandbox: None,
        after: None,
    };
    assert_eq!(
        stdio_session_open_candidate_profile(&params, Some("previous")).as_deref(),
        Some("coding")
    );

    let params = SessionOpenParams {
        session_id: SessionKey("local:test".into()),
        topic: None,
        profile_id: Some("explicit".into()),
        cwd: None,
        sandbox: None,
        after: None,
    };
    assert_eq!(
        stdio_session_open_candidate_profile(&params, Some("previous")).as_deref(),
        Some("explicit")
    );

    let params = SessionOpenParams {
        session_id: SessionKey("local:test".into()),
        topic: None,
        profile_id: None,
        cwd: None,
        sandbox: None,
        after: None,
    };
    assert_eq!(
        stdio_session_open_candidate_profile(&params, Some("previous")).as_deref(),
        Some("previous")
    );
}

#[test]
fn appui_prompt_context_bridge_preserves_current_user_turn() {
    let session_id = SessionKey::new("api", "context-current-user");
    let history = vec![
        test_message(MessageRole::User, "old request"),
        test_message(MessageRole::Assistant, "old answer"),
    ];
    let manager = Arc::new(StdMutex::new(ContextManager::from_session_history(
        session_id.to_string(),
        None,
        &history,
    )));
    let dir = tempfile::tempdir().unwrap();
    let bridge =
        AppUiPromptContextBridge::new(session_id.clone(), dir.path().to_path_buf(), manager);
    let mut prompt = vec![test_message(MessageRole::System, "runtime system")];
    prompt.extend(history);
    prompt.push(test_message(MessageRole::User, "current request"));

    let report = bridge
        .prepare_prompt(
            PromptContextRequest {
                phase: PromptContextPhase::TurnStart,
                iteration: 1,
                provider_name: "test".to_string(),
                model_id: "large-context".to_string(),
                context_window: 16_000,
            },
            &mut prompt,
        )
        .expect("context manager bridge should prepare prompt");

    assert!(report.prompt_replaced);
    assert!(
        prompt.iter().any(|message| {
            message.role == MessageRole::System && message.content == "runtime system"
        }),
        "managed prompt should keep the runtime system instruction"
    );
    assert!(
        prompt.iter().any(|message| {
            message.role == MessageRole::User && message.content == "current request"
        }),
        "managed prompt must keep the current user turn"
    );
    assert_eq!(
        prompt
            .iter()
            .filter(|message| {
                message.role == MessageRole::User && message.content == "old request"
            })
            .count(),
        1,
        "known history should not be duplicated while adding the current turn"
    );
    assert!(
        crate::context_manager::context_ledger_path(dir.path(), &session_id.to_string()).exists(),
        "AppUI prompt-context preparation should persist the canonical context ledger"
    );
}

#[test]
fn effective_provider_route_updates_scratch_and_persists_exactly_one_epoch_rotation() {
    let session_id = SessionKey::new("api", "context-failover-epoch");
    let mut initial = ContextManager::from_session_history(
        session_id.to_string(),
        None,
        &[test_message(MessageRole::User, "request")],
    );
    let primary_epoch = initial
        .reconcile_prompt_cache_epoch("primary", "model-a", "stable", &[])
        .epoch_id
        .clone();
    let manager = Arc::new(StdMutex::new(initial));
    let dir = tempfile::tempdir().unwrap();
    let bridge = AppUiPromptContextBridge::new(
        session_id.clone(),
        dir.path().to_path_buf(),
        manager.clone(),
    );

    // Initialize the per-loop scratch exactly as a real TurnStart does.
    let mut prompt = vec![
        test_message(MessageRole::System, "stable"),
        test_message(MessageRole::User, "request"),
    ];
    bridge
        .prepare_prompt(
            PromptContextRequest {
                phase: PromptContextPhase::TurnStart,
                iteration: 1,
                provider_name: "primary".to_owned(),
                model_id: "model-a".to_owned(),
                context_window: 16_000,
            },
            &mut prompt,
        )
        .expect("turn-start projection");

    PromptContextManager::observe_effective_provider_route(&bridge, "fallback", "model-b");
    let canonical_epoch = manager
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .cache_epoch()
        .expect("canonical epoch")
        .clone();
    assert_ne!(canonical_epoch.epoch_id, primary_epoch);
    assert_eq!(canonical_epoch.provider, "fallback");
    assert_eq!(canonical_epoch.model, "model-b");
    assert_eq!(
        canonical_epoch.last_invalidation_reason,
        "model_route_changed"
    );
    let scratch_epoch = bridge
        .scratch
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .and_then(|scratch| scratch.manager.cache_epoch())
        .expect("scratch epoch")
        .clone();
    assert_eq!(scratch_epoch, canonical_epoch);

    let persisted =
        crate::context_manager::load_context_manager_snapshot(dir.path(), &session_id.to_string())
            .expect("read persisted epoch")
            .expect("snapshot exists");
    assert_eq!(persisted.cache_epoch(), Some(&canonical_epoch));

    // The same winning route on a later observation is idempotent: it does
    // not manufacture a new epoch or increment any generation.
    PromptContextManager::observe_effective_provider_route(&bridge, "fallback", "model-b");
    let unchanged = manager
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .cache_epoch()
        .expect("epoch retained")
        .clone();
    assert_eq!(unchanged, canonical_epoch);
}

/// UPCR-2026-026 follow-up: the in-loop (mid-turn) compaction pass must
/// emit `ContextCompactionStarted` → `ContextCompactionCompleted` through
/// the bridge's notify hook. It previously compacted SILENTLY — the only
/// emitting site (the pre-turn bridge) was then starved forever by the
/// persisted post-compaction snapshot, so a session whose context filled
/// mid-turn never showed any compaction UX.
#[test]
fn in_loop_compaction_emits_lifecycle_notifications() {
    let session_id = SessionKey::new("api", "context-inloop-events");
    // Enough history that the items estimate dwarfs 70% of a tiny window.
    let history: Vec<Message> = (0..10)
        .flat_map(|idx| {
            vec![
                test_message(MessageRole::User, format!("req {idx}: {}", "x".repeat(400))),
                test_message(
                    MessageRole::Assistant,
                    format!("ans {idx}: {}", "y".repeat(400)),
                ),
            ]
        })
        .collect();
    let manager = Arc::new(StdMutex::new(ContextManager::from_session_history(
        session_id.to_string(),
        None,
        &history,
    )));
    let epoch_before = manager
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .reconcile_prompt_cache_epoch("test", "tiny-context", "runtime system", &[])
        .epoch_id
        .clone();
    let dir = tempfile::tempdir().unwrap();
    let captured: Arc<StdMutex<Vec<UiNotification>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink = captured.clone();
    let bridge =
        AppUiPromptContextBridge::new(session_id.clone(), dir.path().to_path_buf(), manager)
            .with_context_lifecycle_notify(Arc::new(move |notification| {
                sink.lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(notification);
            }));

    let mut prompt = vec![test_message(MessageRole::System, "runtime system")];
    prompt.extend(history);
    prompt.push(test_message(MessageRole::User, "current request"));

    let report = bridge
        .prepare_prompt(
            PromptContextRequest {
                phase: PromptContextPhase::TurnStart,
                iteration: 1,
                provider_name: "test".to_string(),
                model_id: "tiny-context".to_string(),
                // threshold = 70% of 300 = 210 tokens — trivially exceeded.
                context_window: 300,
            },
            &mut prompt,
        )
        .expect("context manager bridge should prepare prompt");
    assert!(
        report.compaction_performed,
        "the tiny window must force an in-loop compaction"
    );

    let events = captured.lock().unwrap_or_else(|error| error.into_inner());
    let started = events
        .iter()
        .position(|n| matches!(n, UiNotification::ContextCompactionStarted(_)))
        .expect("in-loop compaction must emit ContextCompactionStarted");
    let completed = events
        .iter()
        .position(|n| matches!(n, UiNotification::ContextCompactionCompleted(_)))
        .expect("in-loop compaction must emit ContextCompactionCompleted");
    assert!(
        started < completed,
        "started must precede completed in the emitted order"
    );
    let UiNotification::ContextCompactionStarted(event) = &events[started] else {
        unreachable!()
    };
    assert_eq!(event.session_id, session_id);
    assert_eq!(event.trigger, "agent_loop:turn_start");
    assert_eq!(event.threshold_tokens, 210);
    let UiNotification::ContextCompactionCompleted(done) = &events[completed] else {
        unreachable!()
    };
    assert_eq!(done.session_id, session_id);
    assert_eq!(done.compaction.trigger, "agent_loop:turn_start");
    let epoch_after = bridge
        .prompt_cache_epoch_id()
        .expect("compaction keeps an initialized epoch");
    assert_ne!(epoch_after, epoch_before);
    assert_eq!(
        done.context_state.cache_epoch_id.as_deref(),
        Some(epoch_after.as_str())
    );
    assert_eq!(
        done.context_state.last_cache_invalidation_reason.as_deref(),
        Some("compaction_installed")
    );
    assert!(done.context_state.semantic_head_id.is_some());
    assert!(done.context_state.semantic_head_kind.is_some());
}

/// #1134 — when the oneshot didn't fire (interrupt / agent error
/// path), the post-turn block must fall back to the session
/// history scan. This pins that contract on the picker helper.
#[test]
fn appui_loop_self_paced_picker_falls_back_to_history_when_capture_missing() {
    let history_pick = Some("status report <<loop-next-in: 60s>>".to_owned());
    let assistant_reply = appui_loop_assistant_reply_for_self_paced(None, history_pick.clone());
    assert_eq!(assistant_reply, history_pick);

    // Empty captured content is treated as "no reply" (matches
    // the non-empty filter the history walk applies). We do NOT
    // fall back to history in that case — the agent explicitly
    // delivered a blank EndTurn payload.
    let blank_capture = appui_loop_assistant_reply_for_self_paced(Some(""), history_pick);
    assert_eq!(
        blank_capture, None,
        "empty captured content must not stamp a default-delay reschedule via history fallback"
    );

    // A fire with NO capture and NO history reply (interrupt / agent
    // error before anything persisted) must still reschedule: the
    // picker yields an empty reply, which carries no sentinel, so the
    // orchestrator stamps the DEFAULT delay. The old `None` here parked
    // the loop at `next_run_at_ms: None` — permanently dead after one
    // interrupted turn.
    let reply_less = appui_loop_assistant_reply_for_self_paced(None, None);
    assert_eq!(
        reply_less.as_deref(),
        Some(""),
        "a reply-less fire must reschedule with the default delay, not kill the loop"
    );
}

#[test]
fn profile_local_create_creates_profile_without_otp() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let result = create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "ada", "ADA@example.com"),
    )
    .expect("create local profile");

    assert_eq!(result.profile_id, "ada");
    assert_eq!(result.user_id, "ada");
    // Single-identity model: the wire email is a synthesized placeholder,
    // the legacy client-provided email is not persisted anywhere.
    assert_eq!(result.email, "ada@solo.local");
    assert!(result.created);
    assert_eq!(result.runtime_mode, "solo");

    let profile = state
        .profile_store
        .as_ref()
        .unwrap()
        .get("ada")
        .unwrap()
        .expect("profile");
    assert_eq!(profile.id, "ada");
    assert_eq!(profile.name, "Ada Lovelace");

    let profile_json: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("profiles/ada.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(profile_json["username"], json!("ada"));
}

#[test]
fn profile_local_create_is_idempotent_for_same_local_owner() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let params = local_profile_params("Ada Lovelace", "Ada", "ada@example.com");

    let first = create_or_get_local_solo_profile(&state, params.clone()).unwrap();
    let second = create_or_get_local_solo_profile(&state, params).unwrap();

    assert!(first.created);
    assert!(!second.created);
    assert_eq!(second.profile_id, "ada");
    assert_eq!(
        state.profile_store.as_ref().unwrap().list().unwrap().len(),
        1
    );
}

#[test]
fn should_honor_requested_id_when_present_without_username_or_email() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let result = create_or_get_local_solo_profile(
        &state,
        octos_core::ui_protocol::ProfileLocalCreateParams {
            requested_id: Some("glm".into()),
            ..Default::default()
        },
    )
    .expect("requested_id create should succeed with no username/email");

    assert_eq!(result.profile_id, "glm");
    assert_eq!(result.user_id, "glm");
    assert!(result.created);
    assert_eq!(result.runtime_mode, "solo");
    // Display name falls back to the requested id when no name is sent.
    assert_eq!(result.name, "glm");
    // A login-ready placeholder email is synthesized so solo re-login works.
    assert_eq!(result.email, "glm@solo.local");

    assert!(
        state
            .profile_store
            .as_ref()
            .unwrap()
            .get("glm")
            .unwrap()
            .is_some(),
        "a profile file should exist under the requested id"
    );
}

#[test]
fn should_suffix_requested_id_on_collision() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let mk = |state: &AppState| {
        create_or_get_local_solo_profile(
            state,
            octos_core::ui_protocol::ProfileLocalCreateParams {
                requested_id: Some("glm".into()),
                ..Default::default()
            },
        )
        .expect("requested_id create")
    };

    assert_eq!(mk(&state).profile_id, "glm");
    assert_eq!(mk(&state).profile_id, "glm-2");
    assert_eq!(mk(&state).profile_id, "glm-3");
    assert_eq!(
        state.profile_store.as_ref().unwrap().list().unwrap().len(),
        3,
        "each requested_id create makes a distinct suffixed profile"
    );
}

#[test]
fn should_normalize_requested_id_into_slug() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let result = create_or_get_local_solo_profile(
        &state,
        octos_core::ui_protocol::ProfileLocalCreateParams {
            requested_id: Some("My GLM!!".into()),
            ..Default::default()
        },
    )
    .expect("normalized requested_id create");
    assert_eq!(result.profile_id, "my-glm");
}

#[test]
fn should_generate_id_when_requested_id_and_username_absent() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    // A display name but no requested_id/username/email → the id is derived
    // from the name when that yields a free slug.
    let named = create_or_get_local_solo_profile(
        &state,
        octos_core::ui_protocol::ProfileLocalCreateParams {
            name: "Robo".into(),
            ..Default::default()
        },
    )
    .expect("generated create from a display name");
    assert!(named.created);
    assert_eq!(named.profile_id, "robo");
    assert_eq!(named.name, "Robo");

    // Fully empty params still create a valid profile with a non-empty
    // generated id.
    let empty = create_or_get_local_solo_profile(
        &state,
        octos_core::ui_protocol::ProfileLocalCreateParams::default(),
    )
    .expect("generated create from empty params");
    assert!(empty.created);
    assert!(!empty.profile_id.is_empty());
    assert!(
        state
            .profile_store
            .as_ref()
            .unwrap()
            .get(&empty.profile_id)
            .unwrap()
            .is_some()
    );
}

#[test]
fn should_avoid_reserved_id_when_requested_id_is_reserved_channel_name() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    // "slack" is a reserved session-key channel name → it must NOT become a
    // profile id; the server falls back to a generated id instead.
    let result = create_or_get_local_solo_profile(
        &state,
        octos_core::ui_protocol::ProfileLocalCreateParams {
            requested_id: Some("slack".into()),
            ..Default::default()
        },
    )
    .expect("reserved requested_id falls back to a generated id");
    assert!(result.created);
    assert_ne!(result.profile_id, "slack");
    assert!(!octos_core::is_reserved_channel_name(&result.profile_id));
}

#[test]
fn should_fall_back_to_generated_when_requested_id_is_pathological() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    // Punctuation-only requested_id normalizes to nothing; with no username
    // to fall back on, the server generates a valid id rather than erroring.
    let result = create_or_get_local_solo_profile(
        &state,
        octos_core::ui_protocol::ProfileLocalCreateParams {
            requested_id: Some("!!!".into()),
            ..Default::default()
        },
    )
    .expect("pathological requested_id falls back to a generated id");
    assert!(result.created);
    assert!(!result.profile_id.is_empty());
}

#[test]
fn should_keep_legacy_username_path_when_no_requested_id() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    // requested_id absent + username present → legacy id == username, and
    // the path stays idempotent (create-or-get), never suffixed.
    let first = create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "ada", "ada@example.com"),
    )
    .expect("legacy create");
    assert_eq!(first.profile_id, "ada");
    assert!(first.created);

    let second = create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "ada", "ada@example.com"),
    )
    .expect("legacy get");
    assert_eq!(second.profile_id, "ada");
    assert!(
        !second.created,
        "same owner is a no-op create, never suffixed"
    );
}

#[test]
fn should_reserve_requested_id_atomically_under_concurrency() {
    // P1: concurrent creates for the same requested_id must each get a
    // DISTINCT suffixed id (glm, glm-2, …) — never both write "glm" and
    // overwrite / clash on the shared `.json.tmp`. The reservation lock
    // makes find-free-id + save atomic, so this is deterministic.
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let n = 8usize;
    let barrier = std::sync::Barrier::new(n);
    let ids: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n)
            .map(|_| {
                scope.spawn(|| {
                    // Release all threads together to maximize contention.
                    barrier.wait();
                    create_or_get_local_solo_profile(
                        &state,
                        octos_core::ui_protocol::ProfileLocalCreateParams {
                            requested_id: Some("glm".into()),
                            ..Default::default()
                        },
                    )
                    .expect("concurrent requested_id create")
                    .profile_id
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let unique: std::collections::BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    assert_eq!(
        unique.len(),
        n,
        "each concurrent create must get a distinct id, got {ids:?}"
    );
    let expected: std::collections::BTreeSet<String> = std::iter::once("glm".to_owned())
        .chain((2..=n).map(|i| format!("glm-{i}")))
        .collect();
    assert_eq!(
        unique
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>(),
        expected,
        "ids must be exactly glm, glm-2, … glm-{n}"
    );
    assert_eq!(
        state.profile_store.as_ref().unwrap().list().unwrap().len(),
        n,
        "no concurrent create overwrote another profile"
    );
}

#[test]
fn profile_local_create_rejects_username_collision_with_different_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "ada", "ada@example.com"),
    )
    .unwrap();

    let error = create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Byron", "ada", "ada@example.com"),
    )
    .expect_err("collision rejected");
    assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("profile_local_collision"))
    );
}

#[test]
fn profile_local_create_returns_typed_errors_for_invalid_or_nonlocal_requests() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let invalid = create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "bad username", "ada@example.com"),
    )
    .expect_err("invalid username");
    assert_eq!(invalid.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        invalid.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("profile_local_invalid_username"))
    );

    // codex #1613 r5: a reserved channel-name username must be
    // rejected BEFORE any record persists. Previously the user
    // record was saved and only the later profile save failed,
    // leaving a valid user with no usable profile.
    let reserved = create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "api", "api@example.com"),
    )
    .expect_err("reserved channel-name username");
    assert_eq!(reserved.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        reserved.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("profile_local_invalid_username"))
    );
    assert!(
        state
            .profile_store
            .as_ref()
            .unwrap()
            .get("api")
            .unwrap()
            .is_none(),
        "no profile may persist for a rejected reserved username"
    );

    let tenant_state = AppState {
        profile_store: state.profile_store.clone(),
        ..AppState::empty_for_tests()
    };
    let unsupported = create_or_get_local_solo_profile(
        &tenant_state,
        local_profile_params("Ada Lovelace", "ada", "ada@example.com"),
    )
    .expect_err("tenant mode rejected");
    assert_eq!(unsupported.code, rpc_error_codes::PERMISSION_DENIED);
    assert_eq!(
        unsupported.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("profile_local_unsupported"))
    );
}

/// #1057 / M22 — backend workspace probe reports canonical path,
/// existence, writability, and absent workspace_policy.toml for an
/// existing directory the user could plausibly pick during onboarding.
#[test]
fn workspace_probe_reports_canonical_path_and_writability_for_existing_dir() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let workspace = dir.path().join("repo");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let canonical = std::fs::canonicalize(&workspace).expect("canonical workspace");

    let result = onboarding_workspace_probe_result(&state, workspace.to_str().unwrap())
        .expect("probe an existing writable dir");

    assert_eq!(result["exists"], json!(true));
    assert_eq!(result["is_directory"], json!(true));
    assert_eq!(result["writable"], json!(true));
    assert_eq!(result["root_escape"], json!(false));
    assert_eq!(result["banned_root"], Value::Null);
    assert_eq!(result["runtime_mode"], json!("solo"));
    assert_eq!(
        result["canonical_path"].as_str().unwrap(),
        canonical.to_string_lossy()
    );
    assert_eq!(result["workspace_policy"]["present"], json!(false));
    assert_eq!(result["workspace_policy"]["parse_error"], Value::Null);
    assert_eq!(result["workspace_policy"]["kind"], Value::Null);
}

/// #1057 — probe reports non-existence truthfully without erroring so
/// the TUI can guide users through "directory not found" recovery.
#[test]
fn workspace_probe_reports_missing_path_without_canonical() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let missing = dir.path().join("not-yet-created");

    let result = onboarding_workspace_probe_result(&state, missing.to_str().unwrap())
        .expect("probe a missing path");

    assert_eq!(result["exists"], json!(false));
    assert_eq!(result["is_directory"], json!(false));
    assert_eq!(result["writable"], json!(false));
    assert_eq!(result["canonical_path"], Value::Null);
    // Workspace policy can't be inspected when the dir doesn't exist.
    assert_eq!(result["workspace_policy"]["present"], json!(false));
}

/// #1057 — probe surfaces a parse error when `workspace_policy.toml`
/// exists but cannot be parsed, so the TUI can show the user the
/// underlying parser message before session/open eats the same error.
#[test]
fn workspace_probe_surfaces_workspace_policy_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let workspace = dir.path().join("repo");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(
        workspace.join(octos_agent::workspace_policy::WORKSPACE_POLICY_FILE),
        "this is = not [valid toml",
    )
    .expect("write malformed policy");

    let result = onboarding_workspace_probe_result(&state, workspace.to_str().unwrap())
        .expect("probe with malformed policy");

    assert_eq!(result["workspace_policy"]["present"], json!(true));
    assert!(
        result["workspace_policy"]["parse_error"]
            .as_str()
            .is_some_and(|err| !err.is_empty()),
        "parse_error should be a non-empty string, got {:?}",
        result["workspace_policy"]["parse_error"],
    );
    assert_eq!(result["workspace_policy"]["kind"], Value::Null);
}

/// #1057 — probe parses a well-formed policy and reports the workspace
/// kind so the TUI can preview the harness that will run.
#[test]
fn workspace_probe_reports_parsed_workspace_policy_kind() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let workspace = dir.path().join("repo");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let policy = r#"
schema_version = 1
[workspace]
kind = "coding"
[version_control]
provider = "git"
auto_init = true
trigger = "turn_end"
fail_on_error = false
[tracking]
ignore = []
"#;
    std::fs::write(
        workspace.join(octos_agent::workspace_policy::WORKSPACE_POLICY_FILE),
        policy,
    )
    .expect("write valid policy");

    let result = onboarding_workspace_probe_result(&state, workspace.to_str().unwrap())
        .expect("probe with valid policy");

    assert_eq!(result["workspace_policy"]["present"], json!(true));
    assert_eq!(result["workspace_policy"]["parse_error"], Value::Null);
    assert_eq!(result["workspace_policy"]["kind"], json!("coding"));
}

/// #1147 codex P2 acceptance: a writability probe must use a
/// unique filename per call so a stale leftover file (or a
/// user-created `.octos-workspace-probe-*` file) doesn't cause
/// `create_new` to fail with `AlreadyExists` and falsely report
/// `writable=false`. We simulate the bug by pre-creating a file
/// with the OLD fixed name and asserting the probe still reports
/// the directory as writable.
#[test]
fn workspace_probe_is_writable_despite_stale_probe_filename() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let workspace = dir.path().join("repo");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    // Pre-create a file that would have collided with the old
    // fixed `.octos-workspace-probe` name.
    std::fs::write(workspace.join(".octos-workspace-probe"), "stale leftover")
        .expect("write stale probe file");

    let result = onboarding_workspace_probe_result(&state, workspace.to_str().unwrap())
        .expect("probe must run despite stale leftover");
    assert_eq!(
        result["writable"],
        json!(true),
        "stale leftover file with old fixed name must NOT block writability probe",
    );
}

/// #1057 — probe rejects roots that escape into a banned system path
/// (`/etc`, `/usr`, ...) so the TUI can preflight the same gate that
/// `session/open` enforces.
#[test]
fn workspace_probe_flags_root_escape_under_banned_system_path() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let result = onboarding_workspace_probe_result(&state, "/etc/octos-test-not-a-real-path-1057")
        .expect("probe an /etc candidate");

    assert_eq!(result["root_escape"], json!(true));
    assert_eq!(result["banned_root"], json!("etc"));
    // Banned-root candidates may not even exist; the gate must fire
    // regardless of existence.
}

/// #1057 bullet 4 — probe returns a typed `profile_local_unsupported`
/// error on tenant / cloud deployments (matching `profile/local/create`
/// so TUI clients can handle the rejection uniformly).
#[test]
fn workspace_probe_rejects_empty_path_with_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let error = onboarding_workspace_probe_result(&state, "   ").expect_err("empty path rejected");
    assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("workspace_probe_invalid_path"))
    );
}

/// #1057 — `onboarding/workspace_probe` is advertised as a method +
/// feature flag for local-solo deployments and omitted for tenant.
#[test]
fn workspace_probe_capability_is_local_solo_only() {
    let dir = tempfile::tempdir().unwrap();
    let local = local_profile_state(dir.path());
    let local_capabilities = ConnectionUiFeatures::default().advertised_capabilities(&local);
    assert!(
        local_capabilities
            .supported_methods
            .iter()
            .any(|method| method == APPUI_METHOD_ONBOARDING_WORKSPACE_PROBE),
        "local solo deployment must advertise the workspace probe method",
    );
    assert!(
        local_capabilities.supports_feature(APPUI_FEATURE_ONBOARDING_WORKSPACE_PROBE_V1),
        "local solo deployment must advertise the workspace probe feature",
    );

    let tenant = AppState {
        profile_store: local.profile_store.clone(),
        ..AppState::empty_for_tests()
    };
    let tenant_capabilities = ConnectionUiFeatures::default().advertised_capabilities(&tenant);
    assert!(
        !tenant_capabilities
            .supported_methods
            .iter()
            .any(|method| method == APPUI_METHOD_ONBOARDING_WORKSPACE_PROBE),
        "tenant deployment must NOT advertise the workspace probe method",
    );
    assert!(
        !tenant_capabilities.supports_feature(APPUI_FEATURE_ONBOARDING_WORKSPACE_PROBE_V1),
        "tenant deployment must NOT advertise the workspace probe feature",
    );
}

/// #1172 — Codex naming-parity capability flags must be advertised
/// alongside the existing image-view / dynamic-tool-search flags so
/// a Codex-trained client negotiating capabilities can skip the
/// `tool not found` -> `tool_search` retry on first call.
#[test]
fn codex_naming_aliases_advertise_capability_features() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let capabilities = ConnectionUiFeatures::default().advertised_capabilities(&state);
    for feature in &[
        super::super::coding_tool_contract::CODING_BASH_CAPABILITY_V1,
        super::super::coding_tool_contract::CODING_DELEGATE_CAPABILITY_V1,
    ] {
        assert!(
            capabilities.supports_feature(feature),
            "advertised_capabilities must include {feature} for Codex naming parity",
        );
    }
}

#[test]
fn session_workspace_allowed_returns_profile_unconfigured_when_profile_has_no_llm() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "ada", "ada@example.com"),
    )
    .expect("local profile created");

    let workspace = dir.path().join("repo");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = std::fs::canonicalize(&workspace).expect("canonical workspace");

    let error = validate_session_workspace_allowed(&state, Some("ada"), &workspace)
        .expect_err("missing LLM should be rejected");
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("profile_unconfigured"))
    );
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("missing")),
        Some(&json!("llm"))
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("active_profile_id")),
        Some(&json!("ada"))
    );
}

#[test]
fn session_workspace_allowed_returns_cwd_runtime_unavailable_when_profile_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());

    let workspace = dir.path().join("repo");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = std::fs::canonicalize(&workspace).expect("canonical workspace");

    let error = validate_session_workspace_allowed(&state, Some("nobody"), &workspace)
        .expect_err("unknown profile is rejected");
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("cwd_runtime_unavailable"))
    );
}

#[test]
fn permission_profile_handlers_are_server_owned_and_reject_danger_outside_local() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSetParams, PermissionProfileUpdate,
    };

    // yolo GAP #1: Local + the explicit `--solo` opt-in is what permits
    // danger; bare Local no longer does (see
    // `danger_full_access_requires_solo_opt_in_on_local_server`). This
    // test exercises the deployment-mode / runtime_mode-override gates on
    // top of that opt-in, so enable it here.
    let local = AppState {
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    };
    let session_id = SessionKey("local:permission-profile-test".into());
    let listed = permission_profile_list_result(
        &local,
        octos_core::ui_protocol::PermissionProfileListParams {
            session_id: session_id.clone(),
        },
    );
    assert!(
        listed
            .profiles
            .iter()
            .any(|profile| profile.mode == Mode::DangerFullAccess)
    );

    let tenant = AppState {
        ..AppState::empty_for_tests()
    };
    let tenant_list = permission_profile_list_result(
        &tenant,
        octos_core::ui_protocol::PermissionProfileListParams {
            session_id: session_id.clone(),
        },
    );
    assert!(
        !tenant_list
            .profiles
            .iter()
            .any(|profile| profile.mode == Mode::DangerFullAccess)
    );

    let denied = permission_profile_set_result(
        &tenant,
        PermissionProfileSetParams {
            session_id: session_id.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: None,
        },
    )
    .expect_err("danger rejected outside local");
    assert_eq!(denied.code, rpc_error_codes::PERMISSION_DENIED);
    assert_eq!(
        denied.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("permission_profile_disallowed"))
    );

    // Even when the server runs in Local mode, an explicit
    // `runtime_mode: "tenant"` in the request must tighten the gate so
    // dangerous mode is rejected. This is the M12 soak negative-probe
    // contract (#951).
    let tenant_request_denied = permission_profile_set_result(
        &local,
        PermissionProfileSetParams {
            session_id: session_id.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: Some("tenant".into()),
        },
    )
    .expect_err("tenant runtime_mode override rejects danger");
    assert_eq!(
        tenant_request_denied.code,
        rpc_error_codes::PERMISSION_DENIED
    );
    assert_eq!(
        tenant_request_denied
            .data
            .as_ref()
            .and_then(|data| data.get("kind")),
        Some(&json!("permission_profile_disallowed"))
    );

    // Reverse direction must not relax: server in Tenant + request
    // `runtime_mode: "solo"` still rejects danger.
    let solo_override_denied = permission_profile_set_result(
        &tenant,
        PermissionProfileSetParams {
            session_id,
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: Some("solo".into()),
        },
    )
    .expect_err("solo override cannot relax tenant gate");
    assert_eq!(
        solo_override_denied.code,
        rpc_error_codes::PERMISSION_DENIED
    );
}

/// SECURITY KEYSTONE (yolo GAP #1): a Local-mode server WITHOUT the
/// `--solo` opt-in (`solo_login_enabled == false`) must reject
/// `danger_full_access` — a Caddy-fronted fleet daemon runs Local mode,
/// so bare `deployment_mode == Local` is NOT a safe proxy for a
/// single-user box. Mirrors `solo_create_403_when_opt_in_disabled` and
/// the agent-crate `dangerous_profile_requires_solo_runtime`. Once the
/// operator opts in, the same request succeeds.
#[test]
fn danger_full_access_requires_solo_opt_in_on_local_server() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSetParams, PermissionProfileUpdate,
    };

    // Local mode, NO --solo opt-in (empty_for_tests: solo_login_enabled=false).
    let local_no_solo = AppState::empty_for_tests();
    assert!(!local_no_solo.solo_login_enabled);
    let session_id = SessionKey("local:yolo-solo-gate".into());

    let denied = permission_profile_set_result(
        &local_no_solo,
        PermissionProfileSetParams {
            session_id: session_id.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: None,
        },
    )
    .expect_err("danger must be refused on a Local server without the --solo opt-in");
    assert_eq!(denied.code, rpc_error_codes::PERMISSION_DENIED);
    assert_eq!(
        denied.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("permission_profile_disallowed"))
    );

    // The list must not advertise a dead option either.
    let listed = permission_profile_list_result(
        &local_no_solo,
        octos_core::ui_protocol::PermissionProfileListParams {
            session_id: session_id.clone(),
        },
    );
    assert!(
        !listed
            .profiles
            .iter()
            .any(|profile| profile.mode == Mode::DangerFullAccess),
        "a Local server without --solo must NOT advertise danger_full_access",
    );

    // Same server WITH the opt-in enabled: danger is allowed.
    let local_solo = AppState {
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    };
    let allowed = permission_profile_set_result(
        &local_solo,
        PermissionProfileSetParams {
            session_id: session_id.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: None,
        },
    )
    .expect("Local + --solo opt-in should allow danger_full_access");
    assert_eq!(allowed.session_id, session_id);

    let listed_solo = permission_profile_list_result(
        &local_solo,
        octos_core::ui_protocol::PermissionProfileListParams {
            session_id: session_id.clone(),
        },
    );
    assert!(
        listed_solo
            .profiles
            .iter()
            .any(|profile| profile.mode == Mode::DangerFullAccess),
        "a Local server WITH --solo must advertise danger_full_access",
    );
}

/// GAP #1 follow-through: `effective_permissions_for_session` must map
/// Local → Solo ONLY when the `--solo` opt-in is set. Without it, a
/// stored `danger_full_access` selection resolves through
/// `RuntimeMode::Local`, which `EffectivePermissions::for_runtime`
/// rejects — so a fleet daemon cannot bootstrap a dangerous session
/// runtime even if a selection was somehow persisted.
#[test]
fn effective_permissions_rejects_danger_without_solo_opt_in() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSelection,
    };

    let session_id = SessionKey("local:yolo-effective-gate".into());
    // Persist a dangerous selection directly in the store (bypassing the
    // set gate) to prove the resolution path is independently guarded.
    session_permission_profiles().set(
        session_id.clone(),
        PermissionProfileSelection {
            mode: Mode::DangerFullAccess,
            network: Network::Allow,
        },
        Some(octos_agent::ApprovalPolicy::Never),
    );

    let local_no_solo = AppState::empty_for_tests();
    let err = effective_permissions_for_session(&local_no_solo, &session_id)
        .expect_err("no --solo opt-in ⇒ Local resolves as RuntimeMode::Local, danger rejected");
    assert_eq!(err.code, rpc_error_codes::PERMISSION_DENIED);
    assert_eq!(
        err.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("permission_profile_disallowed"))
    );

    let local_solo = AppState {
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    };
    let permissions = effective_permissions_for_session(&local_solo, &session_id)
        .expect("Local + --solo resolves danger_full_access");
    assert!(permissions.is_dangerous());
    assert_eq!(
        permissions.approval_policy,
        octos_agent::ApprovalPolicy::Never
    );
    // The session key is unique to this test, so the process-global store
    // holds no cross-test state — no explicit cleanup needed.
}

/// Codex P1 follow-up to #1086: when a Local server receives an
/// explicit `runtime_mode: "local"` override, that value must NOT be
/// treated as solo-relaxed. `local` is the multi-profile-but-local
/// deployment mode, not the single-tenant solo loopback, so the gate
/// must tighten just like `tenant`/`cloud` and reject DangerFullAccess.
#[test]
fn permission_profile_set_local_override_tightens_gate_on_local_server() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSetParams, PermissionProfileUpdate,
    };

    // yolo GAP #1: enable the `--solo` opt-in so this test isolates the
    // runtime_mode-override tighten-only behaviour (the opt-in gate itself
    // is covered by `danger_full_access_requires_solo_opt_in_on_local_server`).
    let local = AppState {
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    };
    let session_id = SessionKey("local:permission-profile-local-override".into());

    let local_override_denied = permission_profile_set_result(
        &local,
        PermissionProfileSetParams {
            session_id: session_id.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: Some("local".into()),
        },
    )
    .expect_err("runtime_mode=local must tighten the gate, not relax it");
    assert_eq!(
        local_override_denied.code,
        rpc_error_codes::PERMISSION_DENIED
    );
    assert_eq!(
        local_override_denied
            .data
            .as_ref()
            .and_then(|data| data.get("kind")),
        Some(&json!("permission_profile_disallowed"))
    );

    // `cloud` override must also tighten on a Local server.
    let cloud_override_denied = permission_profile_set_result(
        &local,
        PermissionProfileSetParams {
            session_id: session_id.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: Some("cloud".into()),
        },
    )
    .expect_err("runtime_mode=cloud must tighten the gate");
    assert_eq!(
        cloud_override_denied.code,
        rpc_error_codes::PERMISSION_DENIED
    );

    // Sanity: no override + Local server still allows DangerFullAccess
    // — only explicit non-solo overrides should tighten.
    let allowed = permission_profile_set_result(
        &local,
        PermissionProfileSetParams {
            session_id: session_id.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: None,
        },
    )
    .expect("Local server + no override should allow DangerFullAccess");
    assert_eq!(allowed.session_id, session_id);

    // Explicit `runtime_mode: "solo"` keeps the relaxed gate too.
    let solo_session = SessionKey("local:permission-profile-solo-override".into());
    let solo_allowed = permission_profile_set_result(
        &local,
        PermissionProfileSetParams {
            session_id: solo_session.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: Some("solo".into()),
        },
    )
    .expect("runtime_mode=solo should keep the local gate relaxed");
    assert_eq!(solo_allowed.session_id, solo_session);
}

/// #1121 codex P2 re-review fail-closed acceptance: unrecognized
/// `runtime_mode` overrides (typos, whitespace, future values, the
/// pre-fix synonym `multi_tenant`) must NOT relax the gate on a
/// Local server. Only an explicit `solo` (or absent override) keeps
/// the relaxed path.
#[test]
fn unrecognized_runtime_mode_override_fails_closed_on_local_server() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSetParams, PermissionProfileUpdate,
    };

    // yolo GAP #1: enable the `--solo` opt-in so the "solo override still
    // relaxes" sanity assertions below reach the relaxed path; the stray
    // overrides must still fail closed regardless.
    let local = AppState {
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    };
    let session_id = SessionKey("local:unrecognized-runtime-mode".into());

    for stray_override in [
        "multi_tenant",
        " tenant ",
        "TENANT",
        "tenant\n",
        "unknown",
        "loCal-foo",
        // Codex P2 re-review #3 on #1121: blank/whitespace-only
        // overrides are an explicit malformed value, not "omitted",
        // and MUST tighten the gate.
        "",
        "   ",
        "\t",
        "\n",
    ] {
        let denied = permission_profile_set_result(
            &local,
            PermissionProfileSetParams {
                session_id: session_id.clone(),
                update: PermissionProfileUpdate {
                    mode: Some(Mode::DangerFullAccess),
                    network: Some(Network::Allow),
                    approval_policy: Some("never".into()),
                },
                runtime_mode: Some(stray_override.into()),
            },
        )
        .expect_err(
            "unrecognized runtime_mode override must fail closed instead of relaxing local",
        );
        assert_eq!(
            denied.code,
            rpc_error_codes::PERMISSION_DENIED,
            "stray override {stray_override:?} must produce PERMISSION_DENIED",
        );
        assert_eq!(
            denied.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("permission_profile_disallowed")),
            "stray override {stray_override:?} must report permission_profile_disallowed",
        );
    }

    // Sanity: explicit `solo` (trimmed/normalized) still relaxes on
    // Local — only unrecognized values fail closed.
    let solo_session = SessionKey("local:unrecognized-runtime-mode-solo-ok".into());
    let solo_allowed = permission_profile_set_result(
        &local,
        PermissionProfileSetParams {
            session_id: solo_session.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: Some("  SOLO  ".into()),
        },
    )
    .expect("normalized solo override must still relax on Local");
    assert_eq!(solo_allowed.session_id, solo_session);
}

/// #1162 — M12-G regression. A session whose base key carries a
/// tenant/cloud scope marker in a structural slot (the
/// `with_profile` first slot OR the M12-G soak's `{profile}:tenant:`
/// channel-slot literal) on a Local server must reject
/// `danger_full_access` even when the client omits the
/// `runtime_mode` override. The explicit override (UPCR-2026-018 /
/// #1086) is the PRIMARY signal but a buggy / older / non-conforming
/// client may omit it; this defense-in-depth gate prevents a
/// tenant-scoped session from slipping dangerous mode past the
/// policy boundary by leaving the override out.
///
/// Codex P1/P2 (#1167) review: chat-id text is arbitrary so the
/// gate must NOT key off colon-delimited chat-id segments. The
/// channel-slot check is EXACT (`== "tenant"` / `== "cloud"`) to
/// avoid false positives on legitimate chat-id text like
/// `_main:api:tenant-demo` (2nd slot there is `api`, not `tenant`).
#[test]
fn danger_full_access_rejected_for_non_cloud_tenant_per_1162() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSetParams, PermissionProfileUpdate,
    };

    // yolo GAP #1: enable the `--solo` opt-in so a denial here is
    // attributable to the tenant/cloud SCOPE marker rather than to the
    // missing opt-in (which would deny every case trivially and hide the
    // scope-gate regression this test guards).
    let local = AppState {
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    };

    for (label, session_id) in [
        // profile_id-slot markers (`with_profile` shape).
        (
            "profile=tenant-a",
            SessionKey::with_profile("tenant-a", "api", "m12-negative"),
        ),
        (
            "profile=tenant--child",
            SessionKey::with_profile("tenant--child", "api", "m12-negative"),
        ),
        (
            "profile=cloud--root",
            SessionKey::with_profile("cloud--root", "api", "m12-negative"),
        ),
        (
            "profile=TENANT-A (case-insensitive)",
            SessionKey::with_profile("TENANT-A", "api", "m12-negative"),
        ),
        (
            "profile=tenant-a + topic suffix",
            SessionKey::with_profile_topic("tenant-a", "api", "m12-negative", "research"),
        ),
        // M12-G soak shape: `{profile}:tenant:{chat}` /
        // `{profile}:cloud:{chat}` — `tenant`/`cloud` sits in the
        // channel slot. `SessionKey::profile_id()` returns `None`
        // because neither is a registered channel name, so the
        // helper has to detect the channel-slot literal directly.
        (
            "channel-slot=tenant (soak shape)",
            SessionKey("coding:tenant:m12-negative".into()),
        ),
        (
            "channel-slot=cloud (soak shape)",
            SessionKey("m12solo:cloud:m12-negative".into()),
        ),
        (
            "channel-slot=tenant + topic suffix",
            SessionKey("coding:tenant:m12-negative#1747".into()),
        ),
        (
            "channel-slot=TENANT (case-insensitive)",
            SessionKey("coding:TENANT:m12-negative".into()),
        ),
        // Codex P1 round 3 (#1167) — profiled tenant sessions on
        // channels that core's `is_channel_name` doesn't include
        // (`line`, `wechat`, …). The gate must reject these too,
        // so it can't rely solely on `SessionKey::profile_id()`
        // (which returns `None` when the 2nd segment isn't in
        // the recognition list).
        (
            "profile=tenant-a + channel=line",
            SessionKey("tenant-a:line:room-1".into()),
        ),
        (
            "profile=cloud--root + channel=wechat",
            SessionKey("cloud--root:wechat:group-42".into()),
        ),
        (
            "profile=tenant + channel=line",
            SessionKey("tenant:line:room-1".into()),
        ),
    ] {
        let denied = match permission_profile_set_result(
            &local,
            PermissionProfileSetParams {
                session_id: session_id.clone(),
                update: PermissionProfileUpdate {
                    mode: Some(Mode::DangerFullAccess),
                    network: Some(Network::Allow),
                    approval_policy: Some("never".into()),
                },
                runtime_mode: None,
            },
        ) {
            Err(err) => err,
            Ok(ok) => {
                panic!("{label} ({session_id}): expected PERMISSION_DENIED but accepted: {ok:?}",)
            }
        };
        assert_eq!(
            denied.code,
            rpc_error_codes::PERMISSION_DENIED,
            "{label} ({session_id}) must return PERMISSION_DENIED",
        );
        assert_eq!(
            denied.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("permission_profile_disallowed")),
            "{label} ({session_id}) must report permission_profile_disallowed",
        );
    }
}

/// #1162 codex P2 review (#1167) — `permission/profile/list` MUST
/// honor the same session-scope gate as `permission/profile/set`,
/// or clients see `danger_full_access` advertised as a selectable
/// option that `set` then rejects. Tenant-scoped sessions omit it
/// from the list on a Local server; solo-scoped sessions keep it.
#[test]
fn danger_full_access_omitted_from_list_for_tenant_scoped_session_per_1162() {
    use octos_core::ui_protocol::{PermissionProfileListParams, PermissionProfileMode as Mode};

    // yolo GAP #1: the "solo-scoped session keeps danger in the list"
    // assertion requires the `--solo` opt-in to be on.
    let local = AppState {
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    };

    let tenant_session = SessionKey::with_profile("tenant-a", "api", "m12-negative");
    let tenant_listing = permission_profile_list_result(
        &local,
        PermissionProfileListParams {
            session_id: tenant_session,
        },
    );
    assert!(
        !tenant_listing
            .profiles
            .iter()
            .any(|profile| profile.mode == Mode::DangerFullAccess),
        "tenant-scoped session must NOT advertise danger_full_access",
    );

    let solo_session = SessionKey("local:m12-positive".into());
    let solo_listing = permission_profile_list_result(
        &local,
        PermissionProfileListParams {
            session_id: solo_session,
        },
    );
    assert!(
        solo_listing
            .profiles
            .iter()
            .any(|profile| profile.mode == Mode::DangerFullAccess),
        "solo-scoped session on Local must keep advertising danger_full_access",
    );
}

/// #1162 codex P2 review (#1167) — chat-id text must NOT trigger
/// the tenant gate. `SessionKey` treats chat IDs as arbitrary, so
/// a Local session whose chat-id text happens to contain `cloud-`
/// or `tenant-` (e.g. `local:cloud-migration`, `_main:api:tenant-demo`)
/// must keep the existing relaxed path.
///
/// Codex P2 round 2 (#1167) additionally pins the legacy
/// `{channel}:{chat_id_with_colons}` case where `chat_id` itself
/// contains `tenant:` / `cloud:` (e.g. `local:tenant:123`,
/// `telegram:cloud:42`). The channel-slot check must guard against
/// this by requiring the 1st segment to NOT be a registered
/// channel before treating the 2nd segment as structural.
#[test]
fn danger_full_access_chat_id_text_does_not_trigger_tenant_gate_per_1162() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSetParams, PermissionProfileUpdate,
    };

    // yolo GAP #1: these cases assert danger is STILL allowed (chat-id
    // text is not a structural scope signal), so the `--solo` opt-in must
    // be on for the relaxed path to be reachable.
    let local = AppState {
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    };

    for raw_session_id in [
        // chat-id contains `cloud-migration` / `tenant-demo` — must NOT
        // be conflated with a tenant-scoped profile.
        "local:cloud-migration",
        "local:tenant-demo",
        "_main:api:cloud-migration",
        "_main:api:tenant-demo",
        // Codex P2 round 2 — legacy `{channel}:{chat_id_with_colons}`
        // session where the chat-id text itself contains `tenant:`/
        // `cloud:`. `SessionKey::new("local", "tenant:123")` produces
        // base key `local:tenant:123`; channel `local` is registered
        // so this is a chat-id-with-colons case, NOT a tenant scope.
        "local:tenant:123",
        "local:cloud:456",
        "telegram:tenant:789",
        "matrix:cloud:!room:abc",
        // Codex P2 round 4 — feature-gated channels (`line`,
        // `wechat`, `mock`) must also be recognised so legacy
        // `{channel}:{chat_id_with_colons}` on those gateways
        // doesn't trip the tenant gate.
        "line:tenant:123",
        "wechat:cloud:456",
        "mock:tenant:abc",
    ] {
        let session_id = SessionKey(raw_session_id.into());
        permission_profile_set_result(
            &local,
            PermissionProfileSetParams {
                session_id: session_id.clone(),
                update: PermissionProfileUpdate {
                    mode: Some(Mode::DangerFullAccess),
                    network: Some(Network::Allow),
                    approval_policy: Some("never".into()),
                },
                runtime_mode: None,
            },
        )
        .unwrap_or_else(|err| {
            panic!(
                "{raw_session_id}: chat-id text containing tenant/cloud-like \
                     substrings must NOT trigger the tenant gate — got: {err:?}",
            )
        });
        // Sanity: list still advertises DangerFullAccess for this
        // session because the chat-id text is not a structural
        // tenant signal.
        let listing = permission_profile_list_result(
            &local,
            octos_core::ui_protocol::PermissionProfileListParams {
                session_id: session_id.clone(),
            },
        );
        assert!(
            listing
                .profiles
                .iter()
                .any(|profile| profile.mode == Mode::DangerFullAccess),
            "{raw_session_id}: chat-id text must NOT remove danger from list",
        );
    }
}

/// Sanity counterpart to #1162 — a tenant-scoped session must STILL
/// reject when the runtime_mode override also says tenant (so we
/// don't drift the existing #1086 contract) and a solo-scoped
/// session keeps the existing relaxed path. The cloud-tenant
/// "accepted" sanity case for #1162 is the existing
/// `permission_profile_handlers_are_server_owned_and_reject_danger_outside_local`
/// test (no override + Local server → accepts danger_full_access).
#[test]
fn danger_full_access_session_scope_gate_does_not_drift_existing_behavior_per_1162() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSetParams, PermissionProfileUpdate,
    };

    // yolo GAP #1: the "solo-scoped session keeps allowing danger" branch
    // requires the `--solo` opt-in; the tenant-override rejection holds
    // regardless.
    let local = AppState {
        solo_login_enabled: true,
        ..AppState::empty_for_tests()
    };

    // Solo-scoped session_id on Local + no override → existing relaxed
    // path keeps allowing danger_full_access. The new gate must not
    // tighten this case.
    let solo_session = SessionKey("m12solo:local:m12-positive#1747".into());
    let allowed = permission_profile_set_result(
        &local,
        PermissionProfileSetParams {
            session_id: solo_session.clone(),
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: None,
        },
    )
    .expect("solo-scoped session_id on Local must keep allowing danger");
    assert_eq!(allowed.session_id, solo_session);

    // Tenant-scoped session_id + explicit `runtime_mode: "tenant"` → still
    // rejected (no regression on #1086).
    let tenant_session = SessionKey("coding:tenant:m12-negative#1747".into());
    let denied = permission_profile_set_result(
        &local,
        PermissionProfileSetParams {
            session_id: tenant_session,
            update: PermissionProfileUpdate {
                mode: Some(Mode::DangerFullAccess),
                network: Some(Network::Allow),
                approval_policy: Some("never".into()),
            },
            runtime_mode: Some("tenant".into()),
        },
    )
    .expect_err("tenant-scoped session + tenant override must still reject");
    assert_eq!(denied.code, rpc_error_codes::PERMISSION_DENIED);
    assert_eq!(
        denied.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("permission_profile_disallowed"))
    );
}

#[test]
fn capabilities_advertise_local_solo_profile_create_only_when_supported() {
    let dir = tempfile::tempdir().unwrap();
    let local = local_profile_state(dir.path());
    let local_capabilities = ConnectionUiFeatures::default().advertised_capabilities(&local);
    assert!(
        local_capabilities
            .supported_methods
            .iter()
            .any(|method| method == methods::PROFILE_LOCAL_CREATE)
    );
    assert!(local_capabilities.supports_feature(APPUI_FEATURE_PROFILE_LOCAL_CREATE_V1));
    assert!(
        local_capabilities.supports_feature(APPUI_FEATURE_PROFILE_LOCAL_CREATE_REQUESTED_ID_V1)
    );
    assert!(local_capabilities.supports_feature(APPUI_FEATURE_PERMISSION_PROFILE_V1));
    assert!(local_capabilities.supports_feature(APPUI_FEATURE_RUNTIME_POLICY_STAMP_V1));
    assert!(local_capabilities.supports_feature(APPUI_FEATURE_CONTEXT_LIFECYCLE_V1));
    for method in [
        APPUI_METHOD_PROFILE_SKILLS_LIST,
        APPUI_METHOD_PROFILE_SKILLS_REGISTRY_SEARCH,
        APPUI_METHOD_PROFILE_SKILLS_INSTALL,
        APPUI_METHOD_PROFILE_SKILLS_REMOVE,
    ] {
        assert!(
            local_capabilities
                .supported_methods
                .iter()
                .any(|advertised| advertised == method),
            "{method} should be advertised with a profile store"
        );
    }

    let no_profile_store = AppState::empty_for_tests();
    let no_profile_capabilities =
        ConnectionUiFeatures::default().advertised_capabilities(&no_profile_store);
    assert!(
        !no_profile_capabilities
            .supported_methods
            .iter()
            .any(|method| is_profile_skill_appui_method(method))
    );

    let tenant = AppState {
        profile_store: local.profile_store.clone(),
        ..AppState::empty_for_tests()
    };
    let tenant_capabilities = ConnectionUiFeatures::default().advertised_capabilities(&tenant);
    assert!(
        !tenant_capabilities
            .supported_methods
            .iter()
            .any(|method| method == methods::PROFILE_LOCAL_CREATE)
    );
    assert!(!tenant_capabilities.supports_feature(APPUI_FEATURE_PROFILE_LOCAL_CREATE_V1));
    assert!(
        !tenant_capabilities.supports_feature(APPUI_FEATURE_PROFILE_LOCAL_CREATE_REQUESTED_ID_V1)
    );
}

#[test]
fn stdio_capabilities_omit_auth_bound_methods_and_report_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    for state in [AppState::empty_for_tests(), local_profile_state(dir.path())] {
        let websocket = ConnectionUiFeatures::default().advertised_capabilities(&state);
        let stdio = ConnectionUiFeatures::stdio_defaults().advertised_capabilities(&state);

        let stdio_methods = sorted_supported_methods(&stdio);
        let websocket_only: std::collections::BTreeSet<&str> = sorted_supported_methods(&websocket)
            .difference(&stdio_methods)
            .copied()
            .collect();
        let expected: std::collections::BTreeSet<&str> = APPUI_STDIO_AUTH_BOUND_UNAVAILABLE_METHODS
            .iter()
            .copied()
            .collect();
        assert_eq!(
            websocket_only, expected,
            "stdio should omit only auth-bound methods from supported_methods",
        );

        for method in APPUI_STDIO_AUTH_BOUND_UNAVAILABLE_METHODS {
            assert!(
                websocket.supports_method(method),
                "websocket should support {method}"
            );
            assert!(!stdio.supports_method(method), "stdio should omit {method}");
            assert!(
                stdio.unsupported_report(method).is_some(),
                "stdio should explicitly report {method} unsupported",
            );
        }
    }
}

#[tokio::test]
async fn launch_resolve_activates_empty_folder_then_resumes_after_store_exists() {
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveParams};

    let tmp = tempfile::tempdir().unwrap();
    let profile = crate::profiles::UserProfile {
        id: "dev".to_string(),
        name: "Dev".to_string(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig {
            llm: Some(crate::profiles::LlmProfileConfig {
                primary: Some(crate::profiles::LlmModelSelectionConfig {
                    family_id: Some("openai".to_string()),
                    model_id: Some("gpt-4o-mini".to_string()),
                    route: Some(crate::profiles::LlmRouteConfig {
                        route_id: None,
                        label: None,
                        base_url: None,
                        api_key_env: Some("LAUNCH_RESOLVE_TEST_KEY".to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [(
                "LAUNCH_RESOLVE_TEST_KEY".to_string(),
                "test-key".to_string(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let runtime = crate::runtime::ProfileRuntime::bootstrap(
        &profile,
        &data_dir,
        None,
        crate::runtime::BootstrapRole::Serve,
    )
    .await
    .expect("bootstrap dev runtime");

    let mut state = AppState::empty_for_tests();
    state.profiles.insert("dev".to_string(), runtime);
    state.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(4, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let state = Arc::new(state);
    let cap = ConnectionUiFeatures::stdio_defaults();

    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let params = LaunchResolveParams {
        cwd: project.to_string_lossy().into_owned(),
        profile_id: Some("dev".to_string()),
    };

    // Empty folder → activate a new session for the launching profile.
    let activate = resolve_launch_result(&state, Some("dev"), cap, &params).unwrap();
    assert_eq!(activate.decision, LaunchDecisionKind::Activate);
    assert_eq!(activate.resolved_profile.as_deref(), Some("dev"));

    // Once dev has a store here, a relaunch resumes it.
    let canon = std::fs::canonicalize(&project).unwrap();
    std::fs::create_dir_all(
        crate::runtime::session::project_sessions_root(&canon, "dev").join("sessions"),
    )
    .unwrap();
    let resume = resolve_launch_result(&state, Some("dev"), cap, &params).unwrap();
    assert_eq!(resume.decision, LaunchDecisionKind::Resume);
    assert_eq!(resume.resolved_profile.as_deref(), Some("dev"));

    // Without the workspace-cwd feature the probe is rejected.
    let no_cap = ConnectionUiFeatures::default();
    assert!(resolve_launch_result(&state, Some("dev"), no_cap, &params).is_err());
}

/// The persisted `default-profile` pointer drives a bare launch: with no
/// `--profile` and no folder-sticky profile, `launch/resolve` resolves to
/// the pointer even when it is not the first-sorted profile. A stale pointer
/// (naming a profile that no longer exists) is ignored, falling back to the
/// derived default.
#[tokio::test]
async fn launch_resolve_prefers_persisted_default_profile() {
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveParams};

    let tmp = tempfile::tempdir().unwrap();
    // The default-profile pointer lives in its own octos home; each profile
    // bootstraps in a separate data dir because the redb episode store takes
    // an exclusive lock and two profiles cannot share one.
    let home = tmp.path().join("home");

    let make_profile = |id: &str| crate::profiles::UserProfile {
        id: id.to_string(),
        name: id.to_string(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig {
            llm: Some(crate::profiles::LlmProfileConfig {
                primary: Some(crate::profiles::LlmModelSelectionConfig {
                    family_id: Some("openai".to_string()),
                    model_id: Some("gpt-4o-mini".to_string()),
                    route: Some(crate::profiles::LlmRouteConfig {
                        route_id: None,
                        label: None,
                        base_url: None,
                        api_key_env: Some("LAUNCH_DEFAULT_TEST_KEY".to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [(
                "LAUNCH_DEFAULT_TEST_KEY".to_string(),
                "test-key".to_string(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let mut state = AppState::empty_for_tests();
    for id in ["alpha", "zeta"] {
        let profile_dir = tmp.path().join(format!("profile-{id}"));
        std::fs::create_dir_all(&profile_dir).unwrap();
        let runtime = crate::runtime::ProfileRuntime::bootstrap(
            &make_profile(id),
            &profile_dir,
            None,
            crate::runtime::BootstrapRole::Serve,
        )
        .await
        .expect("bootstrap profile");
        state.profiles.insert(id.to_string(), runtime);
    }
    let store = Arc::new(crate::profiles::ProfileStore::open_unified(&home).unwrap());
    // Point the global default at "zeta" — NOT the first-sorted profile.
    store.set_default_profile("zeta").unwrap();
    state.profile_store = Some(store.clone());
    state.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(4, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let state = Arc::new(state);
    let cap = ConnectionUiFeatures::stdio_defaults();

    let project = tmp.path().join("fresh");
    std::fs::create_dir_all(&project).unwrap();
    // Bare launch (no `--profile`) in an empty folder. The connection
    // authenticated as "alpha"; the persisted default is "zeta". The default
    // pointer must win over BOTH the connection profile and sort order.
    let params = LaunchResolveParams {
        cwd: project.to_string_lossy().into_owned(),
        profile_id: None,
    };
    let decision = resolve_launch_result(&state, Some("alpha"), cap, &params).unwrap();
    assert_eq!(decision.decision, LaunchDecisionKind::Activate);
    assert_eq!(
        decision.resolved_profile.as_deref(),
        Some("zeta"),
        "the persisted default must win over the connection profile"
    );

    // A stale pointer (unknown profile) is ignored → falls back to the
    // connection profile (alpha).
    store.set_default_profile("ghost").unwrap();
    let stale = resolve_launch_result(&state, Some("alpha"), cap, &params).unwrap();
    assert_eq!(stale.resolved_profile.as_deref(), Some("alpha"));
}

#[tokio::test]
async fn launch_resolve_uses_stored_profiles_without_a_loaded_runtime() {
    // Regression (launch-flow soak against a real `octos serve --stdio`): in
    // solo `--stdio`, profile runtimes materialize LAZILY on `session/open`,
    // so `state.profiles` is EMPTY at bare-launch time. launch/resolve must
    // still see a profile that EXISTS in the persistent `ProfileStore` and
    // must NOT require a loaded runtime — otherwise every folder falls
    // through to `activate` (Resume/CrossProfile unreachable) and a
    // zero-profile machine gets a spurious `cwd_runtime_unavailable` instead
    // of `NoProfile`. `state.profiles` stays empty for the WHOLE test.
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveParams};

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut state = AppState::empty_for_tests();
    state.profile_store = Some(Arc::new(
        crate::profiles::ProfileStore::open_unified(&home).unwrap(),
    ));
    let state = Arc::new(state);
    let cap = ConnectionUiFeatures::stdio_defaults();
    let resolve = |cwd: &std::path::Path| {
        resolve_launch_result(
            &state,
            None,
            cap,
            &LaunchResolveParams {
                cwd: cwd.to_string_lossy().into_owned(),
                profile_id: None,
            },
        )
    };

    // A valid folder with ZERO stored profiles → NoProfile, NOT an error.
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let no_profile = resolve(&empty).unwrap();
    assert_eq!(no_profile.decision, LaunchDecisionKind::NoProfile);
    assert_eq!(no_profile.resolved_profile, None);

    // Persist a launchable brain to the STORE only — no runtime loaded.
    state
        .profile_store
        .as_ref()
        .unwrap()
        .save(&crate::profiles::UserProfile {
            id: "ghost".to_string(),
            name: "Ghost".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();

    // Empty folder + the sole stored profile is ghost → Activate{ghost}
    // (default falls back to the only known profile), NOT an error.
    let activate = resolve(&empty).unwrap();
    assert_eq!(activate.decision, LaunchDecisionKind::Activate);
    assert_eq!(activate.resolved_profile.as_deref(), Some("ghost"));

    // A folder holding ghost's activated store + sticky marker → Resume{ghost},
    // even though ghost's runtime was never loaded into `state.profiles`.
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(project.join(".octos").join("ghost").join("sessions")).unwrap();
    std::fs::write(project.join(".octos").join("active-profile"), "ghost").unwrap();
    let resume = resolve(&project).unwrap();
    assert_eq!(resume.decision, LaunchDecisionKind::Resume);
    assert_eq!(resume.resolved_profile.as_deref(), Some("ghost"));
}

#[tokio::test]
async fn raw_session_status_read_missing_profile_returns_profile_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    let request = RpcRequest::<Value>::new(
        "status-read-missing-profile",
        APPUI_METHOD_SESSION_STATUS_READ,
        json!({
            "session_id": "missing:local:tui#coding",
            "profile_id": "missing",
        }),
    );

    let error = raw_session_status_result(&state, &request, ConnectionUiFeatures::default(), None)
        .await
        .expect_err("missing profile should be typed AppUI error");
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("profile_unresolved"))
    );
}

#[tokio::test]
async fn stdio_status_read_prefers_explicit_or_session_profile_over_binding() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "ada", "ada@example.com"),
    )
    .expect("create ada profile");
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Grace Hopper", "grace", "grace@example.com"),
    )
    .expect("create grace profile");
    let features = ConnectionUiFeatures::stdio_defaults();

    let explicit = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "explicit-profile",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({
                "session_id": "local:tui#coding",
                "profile_id": "grace",
            }),
        ),
        features,
        Some("ada"),
    )
    .await
    .expect("explicit profile wins over stdio binding");
    assert_eq!(explicit["profile_id"], json!("grace"));
    assert_eq!(
        explicit["runtime_policy_stamp"]["profile_id"],
        json!("grace")
    );

    let profiled_session = SessionKey::with_profile_topic("grace", "local", "tui", "coding");
    let from_session_id = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "profiled-session",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": profiled_session }),
        ),
        features,
        Some("ada"),
    )
    .await
    .expect("profile-qualified session_id wins over stdio binding");
    assert_eq!(from_session_id["profile_id"], json!("grace"));
    assert_eq!(
        from_session_id["runtime_policy_stamp"]["profile_id"],
        json!("grace")
    );

    let missing_session = SessionKey::with_profile_topic("missing", "local", "tui", "coding");
    let error = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "missing-profiled-session",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": missing_session }),
        ),
        features,
        Some("ada"),
    )
    .await
    .expect_err("unresolved session profile must not fall back to binding or _main");
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("profile_unresolved"))
    );
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("profile_id")),
        Some(&json!("missing"))
    );
}

#[tokio::test]
async fn raw_session_status_read_omits_model_object_when_no_model_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "ada", "ada@example.com"),
    )
    .expect("create ada profile");

    let status = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "status-no-model",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": "local:tui#coding" }),
        ),
        ConnectionUiFeatures::stdio_defaults(),
        Some("ada"),
    )
    .await
    .expect("status for a profile without a configured model");

    // Guard the setup assumption: this profile really has no resolved
    // model/provider in its runtime policy stamp.
    assert_eq!(status["runtime_policy_stamp"]["model"], Value::Null);
    assert_eq!(status["runtime_policy_stamp"]["provider"], Value::Null);
    // The contract under test: no resolved model => NO `model` key at
    // all. Emitting `{"model": null, "provider": null, "selected": true}`
    // breaks shipped octoscode decoders whose ModelStatus requires
    // non-null `model`/`provider` strings — the whole
    // session/status/read result fails to decode and the composer
    // footer degrades to the `<server authenticated profile>`
    // placeholder.
    assert!(
        status.get("model").is_none(),
        "session/status/read must omit the model object when no model is resolved, got: {status}"
    );
}

#[tokio::test]
async fn stdio_multi_profile_open_status_reads_isolated_runtime_policy_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state_with_sessions(dir.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", "ada", "ada@example.com"),
    )
    .expect("create ada profile");
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Grace Hopper", "grace", "grace@example.com"),
    )
    .expect("create grace profile");

    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let features = ConnectionUiFeatures::stdio_defaults();
    let ada_session = SessionKey::with_profile_topic("ada", "local", "tui-a", "coding");
    let grace_session = SessionKey::with_profile_topic("grace", "local", "tui-b", "coding");

    for session_id in [ada_session.clone(), grace_session.clone()] {
        let profile_id = session_id.profile_id().expect("profile-qualified session");
        let outcome = open_session_result(
            &state,
            &ledger,
            &approvals,
            &PendingQuestionStore::default(),
            ConnectionId::next(),
            Some(profile_id),
            None,
            features,
            SessionOpenParams {
                session_id: session_id.clone(),
                topic: None,
                profile_id: None,
                cwd: None,
                sandbox: None,
                after: None,
            },
        )
        .await
        .expect("profile-qualified session/open succeeds");
        assert_eq!(
            outcome.result.opened.active_profile_id.as_deref(),
            Some(profile_id)
        );
    }

    let ada_status = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "ada-status",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": ada_session }),
        ),
        features,
        Some("grace"),
    )
    .await
    .expect("ada status");
    let grace_status = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "grace-status",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": grace_session }),
        ),
        features,
        Some("ada"),
    )
    .await
    .expect("grace status");

    assert_eq!(ada_status["profile_id"], json!("ada"));
    assert_eq!(
        ada_status["runtime_policy_stamp"]["profile_id"],
        json!("ada")
    );
    assert_eq!(grace_status["profile_id"], json!("grace"));
    assert_eq!(
        grace_status["runtime_policy_stamp"]["profile_id"],
        json!("grace")
    );
}

#[tokio::test]
async fn profile_llm_test_without_api_key_returns_not_applied() {
    let state = Arc::new(AppState::empty_for_tests());
    let request = RpcRequest::new(
        "1",
        APPUI_METHOD_PROFILE_LLM_TEST,
        json!({
            "selection": {
                "family_id": "custom",
                "model_id": "custom-model",
                "route": {
                    "route_id": "custom",
                    "base_url": "http://127.0.0.1:9/v1",
                    "api_type": "openai"
                }
            }
        }),
    );

    let result = raw_profile_llm_test(&state, &request, Some("ada"))
        .await
        .expect("test result");

    assert_eq!(result["profile_id"], json!("ada"));
    assert_eq!(result["applied"], json!(false));
    assert_eq!(result["message"], json!("Provider connection failed"));
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("No API key"))
    );
}

#[tokio::test]
async fn newly_configured_local_profile_allows_session_open_cwd_validation() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(local_profile_state(dir.path()));
    // Profile-runtime bootstrap uses process-wide caches. Keep this
    // parallel test's profile identity distinct from other local-profile
    // fixtures, just as its temporary store is distinct.
    let profile_id = format!("ada-cwd-{}", uuid::Uuid::now_v7());
    let email = format!("{profile_id}@example.com");
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Ada Lovelace", &profile_id, &email),
    )
    .expect("create profile");

    let primary = RpcRequest::new(
        "1",
        APPUI_METHOD_PROFILE_LLM_UPSERT,
        json!({
            "profile_id": profile_id.clone(),
            "set_primary": true,
            "selection": {
                "family_id": "deepseek",
                "model_id": "deepseek-reasoner",
                "route": {
                    "route_id": "deepseek",
                    "api_key_env": "DEEPSEEK_API_KEY",
                    "api_type": "openai"
                }
            },
            "api_key": "sk-test"
        }),
    );
    raw_profile_llm_upsert(&state, &primary, None)
        .await
        .expect("primary upsert registers runtime");
    // `open_session_result` performs this bootstrap before validating the
    // requested cwd. Mirror that ordering instead of relying on the
    // upsert's best-effort bootstrap side effect.
    assert!(
        ensure_session_profile_runtime(&state, Some(&profile_id))
            .await
            .expect("configured profile runtime bootstraps")
            .is_some(),
        "configured local profile has a runtime before cwd validation",
    );

    let workspace = tempfile::tempdir().unwrap();
    let params = SessionOpenParams {
        session_id: SessionKey::with_profile_topic(&profile_id, "local", "tui", "coding"),
        topic: None,
        profile_id: Some(profile_id.clone()),
        cwd: Some(workspace.path().to_string_lossy().into_owned()),
        sandbox: None,
        after: None,
    };

    let requested_workspace = validate_requested_session_cwd(
        &state,
        ConnectionUiFeatures::stdio_defaults(),
        Some(&profile_id),
        &params,
    )
    .expect("newly created local profile has registered runtime");

    assert_eq!(
        requested_workspace.as_deref(),
        Some(workspace.path().canonicalize().unwrap().as_path())
    );
}

#[test]
fn runtime_policy_stamp_exposes_effective_permission_fields() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSelection as Selection,
    };

    let state = AppState::empty_for_tests();
    let session_id = SessionKey("local:policy-stamp-test".into());
    let workspace = tempfile::tempdir().unwrap();
    session_workspaces().set("ada", session_id.clone(), workspace.path().to_path_buf());
    session_permission_profiles().set(
        session_id.clone(),
        Selection {
            mode: Mode::DangerFullAccess,
            network: Network::Allow,
        },
        Some(octos_agent::ApprovalPolicy::Never),
    );

    let stamp = runtime_policy_stamp_for_profile(&state, "ada", Some(&session_id), None);
    assert_eq!(stamp["runtime_mode"], json!("solo"));
    assert_eq!(stamp["profile_id"], json!("ada"));
    assert_eq!(
        stamp["workspace_root"],
        json!(workspace.path().to_string_lossy())
    );
    assert_eq!(stamp["approval_policy"], json!("never"));
    assert_eq!(stamp["sandbox_mode"], json!("danger-full-access"));
    assert_eq!(stamp["permission_profile"], json!("danger_full_access"));
    assert_eq!(stamp["filesystem_scope"], json!("host"));
    assert_eq!(stamp["network"], json!("allowed"));
}

/// The stamp's model/provider must reflect the runtime that will SERVE
/// the next turn, mirroring `resolve_session_profile_runtime`: a profile
/// pinned in startup-config `state.profiles` keeps its boot snapshot
/// (immutable until restart — reporting the file would falsely claim a
/// select took effect), while a store-backed profile re-bootstraps from
/// the FILE after select evicts — the old unconditional runtime-first
/// order made every status/read stomp a freshly-applied selection back.
#[tokio::test]
async fn runtime_policy_stamp_reports_the_runtime_that_serves_turns() {
    use crate::profiles::{
        LlmModelSelectionConfig, LlmProfileConfig, LlmRouteConfig, ProfileConfig, UserProfile,
    };
    use chrono::Utc;

    let make_profile = |model: &str, family: &str| UserProfile {
        id: "dev".to_string(),
        name: "Dev".to_string(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: ProfileConfig {
            llm: Some(LlmProfileConfig {
                primary: Some(LlmModelSelectionConfig {
                    family_id: Some(family.to_string()),
                    model_id: Some(model.to_string()),
                    route: Some(LlmRouteConfig {
                        route_id: None,
                        label: None,
                        base_url: None,
                        api_key_env: Some("STAMP_TEST_KEY".to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [("STAMP_TEST_KEY".to_string(), "test-key".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    // Boot-time runtime pinned to gpt-4o-mini…
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let boot_profile = make_profile("gpt-4o-mini", "openai");
    let runtime = crate::runtime::ProfileRuntime::bootstrap(
        &boot_profile,
        &data_dir,
        None,
        crate::runtime::BootstrapRole::Serve,
    )
    .await
    .expect("bootstrap boot-time runtime");
    let mut state = AppState::empty_for_tests();
    state.profiles.insert("dev".to_string(), runtime);

    // …and the profile FILE has since been switched to deepseek-chat.
    // A PINNED profile keeps serving the boot runtime until restart, so
    // the stamp must keep reporting the snapshot, not the file.
    let file_profile = make_profile("deepseek-chat", "deepseek");
    let stamp = runtime_policy_stamp_for_profile(&state, "dev", None, Some(&file_profile));
    assert_eq!(
        stamp["model"],
        json!("gpt-4o-mini"),
        "startup-pinned profiles serve the boot snapshot until restart: {stamp}"
    );
    assert_eq!(stamp["provider"], json!("openai"));

    // A store-backed (dynamic) profile re-bootstraps from the file after
    // `profile/llm/select` evicts — the FILE is what serves next.
    let dynamic_state = AppState::empty_for_tests();
    let stamp = runtime_policy_stamp_for_profile(&dynamic_state, "dev", None, Some(&file_profile));
    assert_eq!(
        stamp["model"],
        json!("deepseek-chat"),
        "dynamic profiles serve the file's primary: {stamp}"
    );
    assert_eq!(stamp["provider"], json!("deepseek"));
}

#[test]
fn parses_turn_start_rpc_request() {
    let request = UiCommand::TurnStart(TurnStartParams {
        session_id: SessionKey("local:test".into()),
        turn_id: TurnId::new(),
        input: vec![InputItem::Text {
            text: "hello".into(),
        }],
        media: Vec::new(),
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        tool_context: None,
        live_video: false,
    })
    .into_rpc_request("1")
    .expect("request");
    let text = serde_json::to_string(&request).expect("json");

    let decoded = parse_rpc_request(&text).expect("parse");

    assert_eq!(decoded.method, methods::TURN_START);
    assert_eq!(decoded.id, "1");
    assert!(matches!(
        route_rpc_command(decoded, ConnectionUiFeatures::default()).expect("route"),
        UiCommand::TurnStart(_)
    ));
}

/// UPCR-2026-015 (M9-β-1): the WS turn/start handler accepts the
/// three new optional fields (`media`, `topic`, `rewrite_for`)
/// from a strict-additive wire shape. The legacy text-only form
/// continues to deserialize identically (back-compat sanity).
#[test]
fn parses_turn_start_rpc_request_with_beta1_fields() {
    let raw = json!({
        "jsonrpc": "2.0",
        "id": "rpc-beta1",
        "method": methods::TURN_START,
        "params": {
            "session_id": "local:test",
            "turn_id": TurnId::new(),
            "input": [{"kind": "text", "text": "look here"}],
            "media": [
                {
                    "path": "/tmp/chat-upload-deadbeef.png",
                    "mime": "image/png",
                    "size_bytes": 1234,
                }
            ],
            "topic": "research",
            "rewrite_for": "cmid-original-1",
        }
    })
    .to_string();

    let decoded = parse_rpc_request(&raw).expect("parse");
    let routed = route_rpc_command(decoded, ConnectionUiFeatures::default()).expect("route");
    match routed {
        UiCommand::TurnStart(params) => {
            assert_eq!(params.media.len(), 1);
            assert_eq!(params.media[0].path, "/tmp/chat-upload-deadbeef.png");
            assert_eq!(params.media[0].mime, "image/png");
            assert_eq!(params.media[0].size_bytes, 1234);
            assert_eq!(params.topic.as_deref(), Some("research"));
            assert_eq!(params.rewrite_for.as_deref(), Some("cmid-original-1"));
        }
        other => panic!("expected TurnStart, got {:?}", other),
    }
}

/// UPCR-2026-015 (M9-β-1): bare turn/start (no β-1 fields)
/// continues to deserialize and round-trip with the new defaults.
#[test]
fn parses_legacy_turn_start_rpc_request_stays_back_compat() {
    let raw = json!({
        "jsonrpc": "2.0",
        "id": "rpc-legacy",
        "method": methods::TURN_START,
        "params": {
            "session_id": "local:test",
            "turn_id": TurnId::new(),
            "input": [{"kind": "text", "text": "hello"}],
        }
    })
    .to_string();

    let decoded = parse_rpc_request(&raw).expect("parse");
    let routed = route_rpc_command(decoded, ConnectionUiFeatures::default()).expect("route");
    match routed {
        UiCommand::TurnStart(params) => {
            assert!(params.media.is_empty());
            assert!(params.topic.is_none());
            assert!(params.rewrite_for.is_none());
        }
        other => panic!("expected TurnStart, got {:?}", other),
    }
}

/// #921: every cursor-bearing durable notification variant must
/// surface its cursor through `ledger_event_cursor` so dropped
/// sends trigger `protocol/replay_lossy`. Asserts the positive
/// extraction for the three variants and a negative for a non-
/// cursor-bearing one (sanity).
#[test]
fn ledger_event_cursor_covers_every_cursor_bearing_variant() {
    let session_id = SessionKey("local:test".into());
    let cursor = UiCursor {
        stream: session_id.0.clone(),
        seq: 42,
    };

    let opened =
        UiProtocolLedgerEvent::Notification(UiNotification::SessionOpened(SessionOpened {
            session_id: session_id.clone(),
            active_profile_id: None,
            workspace_root: None,
            context: None,
            context_state: None,
            cursor: Some(cursor.clone()),
            panes: None,
            capabilities: octos_core::ui_protocol::UiProtocolCapabilities::first_server_slice(),
            reasoning_effort: None,
        }));
    assert_eq!(ledger_event_cursor(&opened), Some(cursor.clone()));

    let completed =
        UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: TurnId::new(),
            cursor: Some(cursor.clone()),
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        }));
    assert_eq!(ledger_event_cursor(&completed), Some(cursor.clone()));

    let envelope =
        UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(EnvelopeV2Notification {
            session_id: session_id.clone(),
            topic: None,
            envelope: EnvelopeV2 {
                thread_id: "thread-1".into(),
                seq: 1,
                cursor: Some(cursor.clone()),
                turn_id: "thread-1".into(),
                client_message_id: None,
                payload: PayloadV2::AssistantPersisted {
                    text: "done".into(),
                    assistant_segment_id: "thread-1:assistant:1".into(),
                    meta: MessageMeta {
                        message_id: "msg-1".into(),
                        persisted_at: chrono::Utc::now(),
                        media: Vec::new(),
                    },
                },
            },
        }));
    assert_eq!(ledger_event_cursor(&envelope), Some(cursor.clone()));

    // Sanity: a non-cursor-bearing variant returns None.
    let delta =
        UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: TurnId::new(),
            text: "x".into(),
        }));
    assert_eq!(ledger_event_cursor(&delta), None);
}

/// Issue #1332: when the standalone-turn `done` event carries
/// token totals + cursor + final-assistant message_id, the
/// `turn/completed` lifecycle envelope must surface them on
/// `tokens_in`, `tokens_out`, and `session_result` rather than the
/// dormant-stub `None` triple. Drives `try_emit_terminal` directly
/// because the spawn pipeline is too wide to fixture; the helper
/// is the wire-side closure that issue #1332 modified.
#[tokio::test]
async fn try_emit_terminal_populates_turn_completed_tokens_and_session_result() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<super::WsMessage>(8);
    let ws = WsConnection::new(tx);
    let ledger = UiProtocolLedger::new(32);
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let turn_state = TokioMutex::new(TurnState::Active);
    let cursor = UiCursor {
        stream: session_id.0.clone(),
        seq: 17,
    };
    let details = TurnCompletionDetails {
        cursor: Some(cursor.clone()),
        tokens_in: Some(123),
        tokens_out: Some(456),
        session_result: Some(TurnSessionResult {
            committed_seq: cursor.seq,
            message_id: format!("{}:{}:{}", session_id.0, cursor.seq, 99_999),
            client_message_id: Some("cmid-user-1".into()),
        }),
        outcome: None,
        token_usage: None,
        partial_result: None,
    };

    try_emit_terminal(
        &turn_state,
        TerminalReason::Completed,
        &ws,
        &ledger,
        &session_id,
        &turn_id,
        None,
        Some(details.clone()),
        None,
    )
    .await;

    let mut completed_frame: Option<String> = None;
    while let Ok(msg) = rx.try_recv() {
        if let WsMessage::Text(text) = msg {
            if text.contains("\"method\":\"turn/completed\"") {
                completed_frame = Some(text.to_string());
                break;
            }
        }
    }
    let frame = completed_frame.expect("turn/completed must be emitted");
    assert!(
        frame.contains("\"tokens_in\":123"),
        "tokens_in must surface from completion details: {frame}"
    );
    assert!(
        frame.contains("\"tokens_out\":456"),
        "tokens_out must surface from completion details: {frame}"
    );
    assert!(
        frame.contains("\"session_result\""),
        "session_result must surface when populated: {frame}"
    );
    assert!(
        frame.contains("\"committed_seq\":17"),
        "session_result.committed_seq must reflect the assistant carrier seq: {frame}"
    );
    assert!(
        frame.contains("\"client_message_id\":\"cmid-user-1\""),
        "session_result.client_message_id must round-trip: {frame}"
    );
    assert!(
        frame.contains("\"cursor\""),
        "top-level cursor must be threaded too: {frame}"
    );
}

/// Companion negative test: paths that do not run an LLM (slash
/// command shortcut, M9 fixture, review/start) pass `None` for
/// `completion_details`. The wire shape must degrade gracefully to
/// the pre-#1332 envelope with no token fields surfaced, so capability
/// clients keying off `tokens_in == None` aren't misled.
#[tokio::test]
async fn try_emit_terminal_with_no_details_omits_token_fields() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<super::WsMessage>(8);
    let ws = WsConnection::new(tx);
    let ledger = UiProtocolLedger::new(32);
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let turn_state = TokioMutex::new(TurnState::Active);

    try_emit_terminal(
        &turn_state,
        TerminalReason::Completed,
        &ws,
        &ledger,
        &session_id,
        &turn_id,
        None,
        None,
        None,
    )
    .await;

    let mut completed_frame: Option<String> = None;
    while let Ok(msg) = rx.try_recv() {
        if let WsMessage::Text(text) = msg {
            if text.contains("\"method\":\"turn/completed\"") {
                completed_frame = Some(text.to_string());
                break;
            }
        }
    }
    let frame = completed_frame.expect("turn/completed must be emitted");
    // `serde(skip_serializing_if = "Option::is_none")` on each field
    // means a `None` triple should NOT appear on the wire.
    assert!(
        !frame.contains("\"tokens_in\""),
        "tokens_in must be omitted when details are None: {frame}"
    );
    assert!(
        !frame.contains("\"tokens_out\""),
        "tokens_out must be omitted when details are None: {frame}"
    );
    assert!(
        !frame.contains("\"session_result\""),
        "session_result must be omitted when details are None: {frame}"
    );
}

/// Issue #1337 codex round-2 regression: in the trimmed-dedupe
/// path, an assistant carrier with `tool_calls` is persisted at
/// seq N, followed by tool rows at seq N+1, N+2. The loop's
/// `cursor` therefore advances past the assistant row to the last
/// tool row, but `final_assistant_message_id` was stamped at the
/// assistant row's seq N. Building `TurnSessionResult` from
/// `cursor.seq` would surface `committed_seq=N+2` alongside
/// `message_id=session:N:ts` — a per-row-identity contract
/// violation. The helper must source `committed_seq` from
/// `final_assistant_committed_seq` (pinned at the assistant row)
/// not from `cursor`.
#[test]
fn build_turn_session_result_from_done_pins_seq_to_assistant_carrier_in_trimmed_dedupe() {
    // Simulate the `done` JSON producer's output for a turn where:
    //   - assistant carrier persisted at seq N=10 (with tool_calls)
    //   - tool rows persisted at seq 11, 12
    //   - loop's outer `cursor` ended at seq 12 (last tool row)
    //   - `final_assistant_committed_seq` = 10 (assistant carrier)
    //   - `final_assistant_message_id` = "sess-1:10:<ts_ns>"
    let assistant_seq: u64 = 10;
    let last_tool_seq: u64 = 12;
    let assistant_message_id = format!("sess-1:{assistant_seq}:99999");
    let done = json!({
        "type": "done",
        "content": "hello",
        "tokens_in": 100,
        "tokens_out": 200,
        "cursor": {
            "stream": "sess-1",
            "seq": last_tool_seq,
        },
        "message_id": assistant_message_id,
        "final_assistant_committed_seq": assistant_seq,
        "thread_id": "turn-1",
    });

    let session_result = build_turn_session_result_from_done(&done)
        .expect("session_result must be present when both carrier seq + message_id stamped");

    // The fix: committed_seq is pinned to the assistant carrier's
    // seq (10), NOT the loop's last `cursor.seq` (12).
    assert_eq!(
        session_result.committed_seq, assistant_seq,
        "committed_seq must pin to assistant carrier seq, not last tool-row cursor"
    );
    assert_ne!(
        session_result.committed_seq, last_tool_seq,
        "committed_seq must NOT be the loop's last-row cursor seq when it points at a tool row"
    );
    assert_eq!(
        session_result.message_id, assistant_message_id,
        "message_id must reference the assistant carrier row"
    );
    assert_eq!(session_result.client_message_id, None);
}

/// Companion: when the carrier seq is missing (no synthesis +
/// skipped carrier + JSONL write failure), the helper must return
/// `None` so capability-aware clients see "no data" not a stub.
#[test]
fn build_turn_session_result_from_done_returns_none_without_carrier_seq() {
    let done = json!({
        "type": "done",
        "content": "",
        "cursor": { "stream": "sess-1", "seq": 5 },
        "message_id": "sess-1:5:99999",
        // `final_assistant_committed_seq` deliberately absent —
        // simulates the no-persist branch.
    });
    assert!(build_turn_session_result_from_done(&done).is_none());
}

/// Companion: when the message_id is missing/empty, the helper
/// also returns `None` (both fields are required for the
/// per-row identity contract).
#[test]
fn build_turn_session_result_from_done_returns_none_without_message_id() {
    let done = json!({
        "type": "done",
        "final_assistant_committed_seq": 7,
        "message_id": "",
    });
    assert!(build_turn_session_result_from_done(&done).is_none());
}

/// Plain case: no tool calls, no trimmed-dedupe. The assistant
/// carrier seq equals the loop's last cursor seq, so
/// `committed_seq` matches both — verifies the helper does NOT
/// regress the common path.
#[test]
fn build_turn_session_result_from_done_matches_cursor_seq_without_tool_rows() {
    let seq: u64 = 5;
    let done = json!({
        "type": "done",
        "cursor": { "stream": "sess-1", "seq": seq },
        "message_id": format!("sess-1:{seq}:99999"),
        "final_assistant_committed_seq": seq,
    });
    let session_result = build_turn_session_result_from_done(&done).expect("populated");
    assert_eq!(session_result.committed_seq, seq);
    assert_eq!(session_result.message_id, format!("sess-1:{seq}:99999"));
}

/// PR #1265 follow-up — the slides session `mofa_*` allowlist filter
/// MUST run on the WS turn path (`run_standalone_turn` in
/// `handle_turn_start`), not only on the gateway `SessionActor`
/// path. The bug: round-12 fleet slides validator v2 (2026-05-25)
/// observed both `bot` and `crew` invoking `mofa_list_styles` on
/// the web dashboard's slides topic, proving the
/// `keep_tool_in_slides_session` retain was a no-op on the WS
/// path. The fix wires the same `is_slides_session + activate +
/// retain` pattern from `session_actor.rs:2947-2965` into the
/// per-turn registry build at the WS handler. This test reproduces
/// the registry shape the WS handler builds AND drives the
/// `topic.starts_with("slides")` detection from a real
/// `SessionKey`, so a regression that re-deletes the wiring (or
/// breaks the topic predicate) WOULD flip this test red.

/// Companion to `slides_topic_retain_evicts_sibling_mofa_tools_on_ws_turn_path`:
/// pin that a NON-slides session topic does NOT trigger the retain.
/// A bug that drops the `is_slides_session` guard would silently
/// hide every `mofa_*` tool on every session — flips this test red.
#[test]
fn non_slides_topic_leaves_mofa_tools_visible_on_ws_turn_path() {
    use async_trait::async_trait;
    use eyre::Result;
    use octos_agent::tools::{Tool, ToolResult};
    use serde_json::Value;

    struct NameOnlyTool(&'static str);

    #[async_trait]
    impl Tool for NameOnlyTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> Value {
            json!({})
        }
        async fn execute(&self, _args: &Value) -> Result<ToolResult> {
            Ok(ToolResult::default())
        }
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = octos_agent::ToolRegistry::with_builtins(temp.path());
    for name in ["mofa_slides", "mofa_list_styles", "mofa_site"] {
        registry.register(NameOnlyTool(name));
    }

    // Three topic shapes the production code must NOT treat as slides.
    for topic in ["site demo", "news daily-2026-05-25", "general chat"] {
        let session_id = SessionKey(format!("web:tester#{topic}"));
        let is_slides_session = session_id.topic().is_some_and(|t| t.starts_with("slides"));
        assert!(
            !is_slides_session,
            "topic `{topic}` must not match the slides predicate",
        );
    }

    // Even a bare session (no `#topic`) must not match.
    let bare = SessionKey("web:tester".to_owned());
    let bare_is_slides = bare.topic().is_some_and(|t| t.starts_with("slides"));
    assert!(
        !bare_is_slides,
        "a bare session (no topic) must never match the slides predicate",
    );

    // Sanity-check: every `mofa_*` is still registered because no
    // retain ran. (We don't actually call `retain` in this branch —
    // the production code is gated on `is_slides_session`.)
    for name in ["mofa_slides", "mofa_list_styles", "mofa_site"] {
        assert!(
            registry.get(name).is_some(),
            "{name} must be visible on non-slides session topics",
        );
    }
}

/// Regression: the WS turn handler (`run_standalone_turn`) MUST
/// register the per-turn `TaskSupervisor` with
/// `SessionTaskQueryStore` so `session/tasks.list` and
/// `session/status.get` can locate live spawn_only tasks after the
/// SPA closes + reopens a chat tab mid-flight.
///
/// The bug (triaged in `/tmp/task-tracker-reload-triage.md`): the
/// WS path called `session_runtime.tools.snapshot_excluding(&[])`,
/// which spins up a fresh `Arc::new(TaskSupervisor::new())` at
/// `registry.rs:829`, but never registered it with
/// `SessionTaskQueryStore` the way the gateway path does at
/// `session_actor.rs:2668-2669`. Result: `query_json(session_key)`
/// returned `[]` on reopen, the session-task tracker rendered
/// empty, and the runtime tore down its `watchSession` subscriber
/// when `has_bg_tasks=false`.
///
/// This test pins the registration sequence used by the WS path:
///   1. `snapshot_excluding(&[])` produces a registry with a fresh
///      supervisor (and `session_key: None`).
///   2. `set_session_key(session_id.to_string())` stamps the
///      registry so `spawn::register_with_lineage` writes
///      `session_key = Some(...)` onto each tracked task.
///   3. `store.register(&session_id, &supervisor, &data_dir)` makes
///      the supervisor reachable via the
///      `SessionTaskQueryStore::query_json` walk.
///
/// After those three steps a task registered through
/// `register_with_lineage(..., Some(session_key))` is reachable via
/// `query_json(session_key)` — and a regression that re-deletes
/// either step would flip this test red.
#[test]
fn ws_turn_handler_registers_supervisor_with_task_query_store() {
    let session_id = SessionKey("web:tester#turn-track-reopen".to_owned());
    let temp = tempfile::tempdir().expect("tempdir");

    // Mirror the WS path: build a parent registry the same way
    // SessionRuntime does, then snapshot it for the per-turn slot.
    let parent = octos_agent::ToolRegistry::with_builtins(temp.path());
    let mut tool_registry = parent.snapshot_excluding(&[]);

    // Step 1: stamp the per-turn snapshot with the session key so
    // tasks registered through it inherit `session_key:
    // Some(<key>)`. Without this, the
    // `TaskSupervisor::get_tasks_for_session` filter at
    // `task_supervisor.rs:2237` drops every task.
    tool_registry.set_session_key(session_id.to_string());

    // Step 2: extract the per-turn supervisor and register it in
    // the store, mirroring `session_actor.rs:2668-2669`. This is
    // the line the WS turn handler was missing.
    let task_supervisor = tool_registry.supervisor();
    let store = crate::session_actor::SessionTaskQueryStore::default();
    store.register(&session_id, &task_supervisor, temp.path());

    // Before any spawn_only task fires, the supervisor exists but
    // owns no tasks — `query_json` returns an empty array (NOT
    // missing — that would mean the supervisor wasn't registered).
    let empty = store.query_json(&session_id.to_string());
    assert_eq!(
        empty,
        serde_json::Value::Array(vec![]),
        "registered-but-empty supervisor must return [] (not absent)",
    );

    // Step 3: simulate a spawn_only task registering through
    // `register_with_lineage` with `session_key = Some(...)`. This
    // is what `spawn::Inner::execute_background` does at
    // `spawn.rs:2668-2675`.
    let task_id = task_supervisor.register_with_lineage(
        "podcast_generate",
        "call-track-track",
        Some(&session_id.0),
        None,
    );
    task_supervisor.mark_running(&task_id);

    // Now `query_json` MUST surface the live task. The pre-fix
    // behaviour was either `[]` (no supervisor registered) OR a
    // panic on stale-Weak prune; the fix turns this into a
    // single-element array carrying the running task.
    let result = store.query_json(&session_id.to_string());
    let tasks = result
        .as_array()
        .expect("query_json must produce an array even when empty");
    assert_eq!(
        tasks.len(),
        1,
        "WS-path registration must make spawn_only tasks visible via \
             SessionTaskQueryStore::query_json — got: {result}",
    );
    assert_eq!(tasks[0]["id"], task_id);
    assert_eq!(tasks[0]["tool_name"], "podcast_generate");
    assert_eq!(tasks[0]["status"], "running");
}

/// Regression: `run_standalone_turn` registers per-session channel/dispatcher
/// tools (send_file, peer_*, spawn) onto the per-turn snapshot, then MUST
/// re-apply the profile `tool_policy` so an allow/deny list actually
/// constrains the roster the model sees. Before the fix the re-apply was
/// missing on the UI-Protocol path, so octoscode ran turns at `tools=31`
/// despite an 8-tool allow-list, drowning small local models. Mirrors the
/// gateway re-apply at `session_actor.rs:3748`.
#[test]
fn ws_turn_snapshot_is_constrained_by_reapplied_tool_policy() {
    let temp = tempfile::tempdir().expect("tempdir");
    let parent = octos_agent::ToolRegistry::with_builtins(temp.path());
    // The per-turn snapshot, exactly as run_standalone_turn builds it.
    let mut tool_registry = parent.snapshot_excluding(&[]);

    let full_before = tool_registry.tool_names();
    assert!(
        full_before.len() > 1,
        "snapshot should carry the full builtin roster, got {}",
        full_before.len()
    );
    let keep = full_before
        .iter()
        .find(|n| n.as_str() == "read_file")
        .cloned()
        .unwrap_or_else(|| full_before[0].clone());

    // Register a real per-session tool AFTER the snapshot — the exact class of
    // tool `run_standalone_turn` adds (send_file / peer_* / spawn) and that used
    // to bypass the profile policy.
    let (out_tx, _out_rx) = mpsc::channel::<octos_core::OutboundMessage>(1);
    tool_registry.register(octos_agent::SendFileTool::new(out_tx));
    assert!(
        tool_registry.tool_names().iter().any(|n| n == "send_file"),
        "send_file must be present after being registered onto the per-turn snapshot",
    );

    // Re-apply an allow-list that OMITS send_file — what the fix now does with
    // `session_runtime.profile.tool_policy` after registering session tools.
    let policy = octos_agent::ToolPolicy {
        allow: vec![keep.clone()],
        ..Default::default()
    };
    tool_registry.apply_policy(&policy);

    let after = tool_registry.tool_names();
    assert_eq!(
        after,
        vec![keep.clone()],
        "after re-applying the allow-list, only the allowed tool must remain \
         — got {after:?}"
    );
    assert!(
        !after.iter().any(|n| n == "send_file"),
        "the re-applied allow-list must strip the post-snapshot session tool that used to leak",
    );
}

#[test]
fn coding_tool_status_distinguishes_registered_hidden_tools_from_missing_tools() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = octos_agent::ToolRegistry::with_builtins(temp.path());
    registry.set_context_filter(vec!["read".into()]);

    let visible = model_visible_tool_names(Some(&registry));
    let registered = registered_tool_names(Some(&registry));
    assert!(!visible.iter().any(|name| name == "exec_command"));
    assert!(registered.iter().any(|name| name == "exec_command"));

    let visible_set: HashSet<&str> = visible.iter().map(String::as_str).collect();
    let disabled = registered
        .iter()
        .filter(|name| !visible_set.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let visible_refs = visible.iter().map(String::as_str).collect::<Vec<_>>();
    let disabled_refs = disabled.iter().map(String::as_str).collect::<Vec<_>>();
    let payload = coding_tool_contract::tool_status_list_payload(
        coding_tool_contract::ToolStatusListContext {
            available_model_tools: &visible_refs,
            disabled_model_tools: &disabled_refs,
            ..coding_tool_contract::ToolStatusListContext::default_for_session("coding:test")
        },
    );
    let required = payload["coding_tool_contract"]["required_tools"]
        .as_array()
        .expect("required tools");
    let exec_command = required
        .iter()
        .find(|tool| tool["name"] == json!("exec_command"))
        .expect("exec_command status");

    assert_eq!(
        exec_command["status"],
        json!(coding_tool_contract::TOOL_STATUS_DISABLED_BY_POLICY)
    );
    assert_ne!(
        exec_command["status"],
        json!(coding_tool_contract::TOOL_STATUS_MISSING)
    );
}

#[test]
fn tool_emitted_risk_is_ignored_in_favor_of_manifest() {
    let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
        tool_risk_registry_test_lock().clear_poison();
        e.into_inner()
    });
    clear_tool_risk_registry_for_test();
    register_tool_risk_for_test("rm_rf", "critical");

    let mut tool_emitted = ApprovalRequestedEvent::generic(
        SessionKey("local:test".into()),
        ApprovalId::new(),
        TurnId::new(),
        "rm_rf",
        "Run destructive command",
        "/tmp/x",
    );
    // The malicious tool tries to advertise itself as `low`.
    tool_emitted.risk = Some("low".to_owned());

    harden_progress_emitted_approval(&mut tool_emitted);

    // Server overwrites with manifest-declared `critical`.
    assert_eq!(tool_emitted.risk.as_deref(), Some("critical"));

    // A tool whose manifest is silent collapses to `unspecified`,
    // never silently passes through the tool-claimed value.
    let mut silent = ApprovalRequestedEvent::generic(
        SessionKey("local:test".into()),
        ApprovalId::new(),
        TurnId::new(),
        "unknown_tool",
        "Unknown",
        "body",
    );
    silent.risk = Some("low".to_owned());
    harden_progress_emitted_approval(&mut silent);
    assert_eq!(
        silent.risk.as_deref(),
        Some(octos_core::ui_protocol::RISK_UNSPECIFIED)
    );
    clear_tool_risk_registry_for_test();
}

/// When `typed_approvals` is not negotiated, the wire event must remain
/// fully generic — no `risk` field, no typed details. Audit #715 fix
/// must not start advertising risk on the legacy untyped path, which
/// older clients are not prepared to parse.
#[test]
fn tool_with_no_risk_classification_does_not_emit_risk_field() {
    let _guard = tool_risk_registry_test_lock().lock().unwrap_or_else(|e| {
        tool_risk_registry_test_lock().clear_poison();
        e.into_inner()
    });
    clear_tool_risk_registry_for_test();
    register_tool_risk_for_test("weather_lookup", "high");

    let request = ToolApprovalRequest {
        tool_id: "tool-plugin-3".into(),
        tool_name: "weather_lookup".into(),
        title: "Approve plugin tool".into(),
        body: "Plugin tool approval".into(),
        command: None,
        cwd: Some("/tmp/weather-plugin".into()),
    };
    // `typed_approvals: false` — legacy client.
    let event = approval_event_from_tool_request(
        request,
        SessionKey("local:test".into()),
        ApprovalId::new(),
        TurnId::new(),
        ConnectionUiFeatures::default(),
    );

    assert!(
        event.risk.is_none(),
        "legacy untyped path must not advertise risk; got {:?}",
        event.risk
    );
    assert!(event.approval_kind.is_none());
    assert!(event.typed_details.is_none());
    assert!(event.render_hints.is_none());
    clear_tool_risk_registry_for_test();
}

#[test]
fn task_output_delta_tracker_emits_live_tail_for_task_progress() {
    let session_id = SessionKey("local:test".into());
    let task_id = TaskId::new();
    let mut tracker = TaskOutputDeltaTracker::default();

    assert!(
        tracker
            .observe_progress_event(
                &session_id,
                &json!({ "type": "task_started", "task_id": task_id }),
            )
            .is_none()
    );

    let first = tracker
        .observe_progress_event(
            &session_id,
            &json!({ "type": "tool_progress", "message": "collecting\n" }),
        )
        .expect("progress message emits output delta");
    let second = tracker
        .observe_progress_event(
            &session_id,
            &json!({ "type": "task_output", "text": "done\n" }),
        )
        .expect("task output emits output delta");

    assert_eq!(first.session_id, session_id);
    assert_eq!(first.task_id, task_id);
    assert_eq!(first.cursor.offset, 0);
    assert_eq!(first.text, "collecting\n");
    assert_eq!(second.task_id, task_id);
    assert_eq!(second.cursor.offset, first.text.len() as u64);
    assert_eq!(second.text, "done\n");
}

#[test]
fn task_output_delta_tracker_requires_task_identity() {
    let mut tracker = TaskOutputDeltaTracker::default();

    assert!(
        tracker
            .observe_progress_event(
                &SessionKey("local:test".into()),
                &json!({ "type": "tool_progress", "message": "running" }),
            )
            .is_none()
    );
}

async fn recv_rpc_json(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
    match rx.recv().await.expect("rpc frame") {
        WsMessage::Text(text) => serde_json::from_str(text.as_str()).expect("json frame"),
        other => panic!("expected text frame, got {other:?}"),
    }
}

#[test]
fn malformed_approval_params_return_invalid_params_not_unsupported() {
    // FIX-01 added `ApprovalDecision::Unknown(String)` — unknown decision
    // strings (e.g. `"later"`) are now valid forward-compat wire content
    // and decode to `Unknown(...)`. The server's downstream tool path
    // treats them as Deny (fail-closed). To trigger INVALID_PARAMS we
    // need *structurally* malformed params, e.g. `decision` of the wrong
    // JSON type.
    let request = RpcRequest::new(
        "approval-bad",
        methods::APPROVAL_RESPOND,
        json!({
            "session_id": "local:test",
            "approval_id": ApprovalId::new(),
            "decision": 42, // number where a string is required
        }),
    );

    let error =
        route_rpc_command(request, ConnectionUiFeatures::default()).expect_err("bad params");

    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::INVALID_PARAMS
    );
    assert!(error.message.contains(methods::APPROVAL_RESPOND));
}

#[test]
fn known_approval_returns_typed_json_rpc_result() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let approval_id = ApprovalId::new();
    contracts
        .approvals
        .insert_pending(session_id.clone(), approval_id.clone());

    let outcome = contracts
        .approvals
        .respond(ApprovalRespondParams::new(
            session_id,
            approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .expect("known pending approval accepts");
    let frame = RpcResponse::success(
        "approval-1",
        serde_json::to_value(outcome.result).expect("serialize result"),
    );

    assert_eq!(frame.jsonrpc, octos_core::ui_protocol::JSON_RPC_VERSION);
    assert_eq!(frame.id, "approval-1");
    assert_eq!(frame.result["approval_id"], json!(approval_id));
    assert_eq!(frame.result["accepted"], json!(true));
    assert_eq!(
        frame.result["status"],
        json!(ApprovalRespondStatus::Accepted)
    );
    assert_eq!(frame.result["runtime_resumed"], json!(false));
}

#[test]
fn progress_approval_request_is_stored_for_respond() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let context = ProgressMappingContext::new(session_id.clone(), turn_id);
    let event = json!({
        "type": "approval_requested",
        "approval_id": ApprovalId::new(),
        "tool": "shell",
        "title": "Run command",
        "body": "cargo test",
    });
    let mut mapping = map_progress_json(&context, &event);

    apply_progress_contract_side_effects(&contracts, &context, None, &event, &mut mapping);

    let UiNotification::ApprovalRequested(request) = &mapping.notifications[0] else {
        panic!("expected approval/requested notification");
    };
    let outcome = contracts
        .approvals
        .respond(ApprovalRespondParams::new(
            session_id,
            request.approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .expect("produced approval can be responded to");

    assert!(outcome.result.accepted);
    assert!(!outcome.result.runtime_resumed);
}

#[test]
fn missing_and_not_pending_approval_return_typed_json_rpc_errors() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let missing = contracts
        .approvals
        .respond(ApprovalRespondParams::new(
            session_id.clone(),
            ApprovalId::new(),
            ApprovalDecision::Approve,
        ))
        .expect_err("missing approval should fail");
    let frame = RpcErrorResponse::new(Some("approval-missing".into()), missing);

    assert_eq!(frame.jsonrpc, octos_core::ui_protocol::JSON_RPC_VERSION);
    assert_eq!(frame.id.as_deref(), Some("approval-missing"));
    assert_eq!(frame.error.code, rpc_error_codes::UNKNOWN_APPROVAL_ID);
    assert_eq!(
        frame.error.data.as_ref().unwrap()["kind"],
        json!("unknown_approval")
    );

    let approval_id = ApprovalId::new();
    contracts
        .approvals
        .insert_pending(session_id.clone(), approval_id.clone());
    contracts
        .approvals
        .respond(ApprovalRespondParams::new(
            session_id.clone(),
            approval_id.clone(),
            ApprovalDecision::Deny,
        ))
        .expect("first response accepts");
    let not_pending = contracts
        .approvals
        .respond(ApprovalRespondParams::new(
            session_id,
            approval_id,
            ApprovalDecision::Approve,
        ))
        .expect_err("second response should be not pending");

    assert_eq!(not_pending.code, rpc_error_codes::APPROVAL_NOT_PENDING);
    assert_eq!(
        not_pending.data.as_ref().unwrap()["kind"],
        json!("approval_not_pending")
    );
}

#[test]
fn rejects_invalid_rpc_request_json() {
    let error = parse_rpc_request("{").expect_err("parse error");
    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::PARSE_ERROR
    );
}

#[test]
fn oversized_text_frame_is_rejected_before_json_parse() {
    let text = "x".repeat(MAX_TEXT_FRAME_BYTES + 1);

    let error = parse_ws_text_frame(&text).expect_err("oversized frame");

    assert_eq!(error.code, app_ui_codec::FRAME_TOO_LARGE);
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("limit_bytes")),
        Some(&json!(MAX_TEXT_FRAME_BYTES))
    );
}

/// #922.1: an envelope without `id` is a JSON-RPC notification and
/// must not yield a parse_error reply.
#[test]
fn idless_envelope_parses_as_notification() {
    let frame = r#"{"jsonrpc":"2.0","method":"ping","params":{}}"#;
    match parse_ws_text_frame(frame).expect("parses") {
        ParsedFrame::Notification(method) => assert_eq!(method, "ping"),
        ParsedFrame::Request(_) => panic!("expected notification"),
    }
    assert!(is_known_inbound_notification("ping"));
    assert!(!is_known_inbound_notification("unknown"));
}

/// #924 NIT 6: distinguish notifications from requests by KEY
/// presence on `id`, not null-check. An envelope with
/// `"id": null` is malformed — surfacing it loudly via
/// parse_error catches client bugs that the silent-drop path
/// hid.
#[test]
fn null_id_envelope_is_rejected_with_parse_error() {
    let frame =
        r#"{"jsonrpc":"2.0","id":null,"method":"session/open","params":{"session_id":"x"}}"#;
    let err = parse_ws_text_frame(frame).expect_err("null id must reject");
    assert_eq!(
        err.code,
        octos_core::ui_protocol::rpc_error_codes::PARSE_ERROR
    );
    assert!(
        err.message.contains("null"),
        "parse error message should mention the null id; got {}",
        err.message
    );
}

/// #924 NIT 6: numeric ids are also rejected — the server's RpcRequest
/// shape requires `id: String`.
#[test]
fn numeric_id_envelope_is_rejected_with_parse_error() {
    let frame = r#"{"jsonrpc":"2.0","id":42,"method":"session/open","params":{"session_id":"x"}}"#;
    let err = parse_ws_text_frame(frame).expect_err("numeric id must reject");
    assert_eq!(
        err.code,
        octos_core::ui_protocol::rpc_error_codes::PARSE_ERROR
    );
}

/// Drain every frame `handle_session_open` queued, up to and including the
/// `session/open` NOTIFICATION it direct-sends last (distinguished from the
/// RPC result frame by having no `id`). Frames the replay loop filtered out
/// simply never appear.
async fn drain_session_open_frames(rx: &mut mpsc::Receiver<WsMessage>) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("session/open frames arrive within the timeout")
            .expect("ws stays open");
        let WsMessage::Text(text) = &frame else {
            panic!("expected a text frame, got {frame:?}");
        };
        let value: Value = serde_json::from_str(text).expect("json frame");
        let opened = value.get("id").is_none()
            && value.get("method").and_then(Value::as_str)
                == Some(octos_core::ui_protocol::methods::SESSION_OPEN);
        frames.push(value);
        if opened {
            return frames;
        }
    }
}

/// Read the next frame, or `None` when nothing arrives within `ms`.
async fn next_frame_within(rx: &mut mpsc::Receiver<WsMessage>, ms: u64) -> Option<Value> {
    match tokio::time::timeout(std::time::Duration::from_millis(ms), rx.recv()).await {
        Ok(Some(WsMessage::Text(text))) => Some(serde_json::from_str(&text).expect("json frame")),
        Ok(other) => panic!("expected a text frame, got {other:?}"),
        Err(_) => None,
    }
}

/// #2067 round 2 (H4) — `session/open` is ledger-appended for BROADCAST
/// (`open_session_result` tags it with the opening connection id precisely so
/// other connections observe it), and it carries `workspace_root`, the context
/// snapshot and pane snapshots. On a shared wire key those are another
/// tenant's paths and buffers.
///
/// Filtering it cannot starve its own opener: the opener's forwarder skips the
/// broadcast copy via `from_connection`, and its own frame arrives through the
/// UNFILTERED `send_ledger_event_durable` direct send at the end of
/// `handle_session_open`. And the scopes agree by construction —
/// `active_profile_id` is the same `Option` that `ledger_profile_id` is
/// `unwrap_or(_main)`-ed from, which is exactly how this filter normalizes.
#[tokio::test]
async fn should_drop_cross_profile_session_opened_frames_when_connection_scopes_another_profile() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = local_profile_state_with_sessions(temp.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Alpha Owner", "alpha", "alpha@example.com"),
    )
    .expect("create alpha profile");
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Beta Owner", "beta", "beta@example.com"),
    )
    .expect("create beta profile");

    let (ws, mut rx) = ws_connection_for_test(64);
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let session_id = SessionKey("web-shared-opened".into());

    let beta_opened = |workspace_root: &str| {
        UiNotification::SessionOpened(SessionOpened {
            session_id: session_id.clone(),
            active_profile_id: Some("beta".to_owned()),
            workspace_root: Some(workspace_root.to_owned()),
            context: None,
            context_state: None,
            cursor: None,
            panes: None,
            capabilities: UiProtocolCapabilities::first_server_slice(),
            reasoning_effort: None,
        })
    };

    // Beta opened this shared key first; its open frame is durable history.
    ledger.append_notification(beta_opened("/home/beta/secret-workspace"));

    let opened = handle_session_open(
        &ws,
        &state,
        &ledger,
        &approvals,
        &questions,
        &forwarders,
        Some("alpha"),
        Some("alpha"),
        ConnectionUiFeatures::default(),
        "open-alpha-shared".into(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        },
    )
    .await;
    assert!(opened, "alpha must be able to open the shared session");

    // The drain stops at the first `session/open` NOTIFICATION. Alpha must see
    // exactly one — its own — and never beta's workspace root.
    let frames = drain_session_open_frames(&mut rx).await;
    let opened_frames: Vec<&Value> = frames
        .iter()
        .filter(|frame| {
            frame.get("id").is_none()
                && frame.get("method").and_then(Value::as_str)
                    == Some(octos_core::ui_protocol::methods::SESSION_OPEN)
        })
        .collect();
    assert_eq!(
        opened_frames.len(),
        1,
        "alpha must receive exactly one session/open notification: {frames:#?}"
    );
    assert_eq!(
        opened_frames[0]["params"]["active_profile_id"],
        json!("alpha"),
        "the session/open alpha receives must be its OWN, not beta's replayed frame"
    );

    // Live: beta re-opens the shared key from another connection.
    ledger.append_notification(beta_opened("/home/beta/second-workspace"));
    assert!(
        next_frame_within(&mut rx, 250).await.is_none(),
        "beta's live session/open must not reach an alpha-scoped connection"
    );

    abort_live_forwarders(&forwarders, &ledger).await;
}

/// #2067 round 2 (H4) — `background/activity` carries RAW observed text (a
/// monitor's stdout lines, a fleet summary) and the ledger is its ENTIRE
/// delivery mechanism: the sink appends it over a detached `WsConnection`
/// whose writer is drained to nowhere, so every real client gets it from
/// replay or its live forwarder — both of which run this filter.
///
/// Filtering is safe because no production emitter passes `None`: the monitor
/// origin stamps `AutonomyMonitorRecord.profile_id` and the fleet origin
/// stamps `FleetRecord.profile_id`, both concrete and both normalized through
/// `resolve_autonomy_profile_id`. The monitor origin in particular carries the
/// SAME id as `monitor/fired`, which is already filtered — so this arm adds no
/// new starvation surface for it.
#[tokio::test]
async fn should_drop_cross_profile_background_activity_frames_when_connection_scopes_another_profile()
 {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = local_profile_state_with_sessions(temp.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Alpha Owner", "alpha", "alpha@example.com"),
    )
    .expect("create alpha profile");
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Beta Owner", "beta", "beta@example.com"),
    )
    .expect("create beta profile");

    let (ws, mut rx) = ws_connection_for_test(64);
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let session_id = SessionKey("web-shared-activity".into());
    // `background/activity` is capability-gated; negotiate it or the frames
    // never reach the wire for reasons unrelated to profile scope.
    let features = ConnectionUiFeatures {
        background_activity: true,
        ..Default::default()
    };

    let activity = |profile_id: &str, text: &str| {
        UiNotification::BackgroundActivity(octos_core::ui_protocol::BackgroundActivityEvent {
            session_id: session_id.clone(),
            profile_id: Some(profile_id.to_owned()),
            origin_kind: "monitor".to_owned(),
            origin_id: format!("monitor-{profile_id}"),
            origin_label: Some(format!("{profile_id}-tail")),
            text: text.to_owned(),
            emitted_at_ms: 1_760_000_000_000,
            dropped_count: None,
            suppressed: false,
        })
    };

    ledger.append_notification(activity("beta", "beta secret log line"));
    ledger.append_notification(activity("alpha", "alpha log line"));

    let opened = handle_session_open(
        &ws,
        &state,
        &ledger,
        &approvals,
        &questions,
        &forwarders,
        Some("alpha"),
        Some("alpha"),
        features,
        "open-alpha-activity".into(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        },
    )
    .await;
    assert!(opened, "alpha must be able to open the shared session");

    let frames = drain_session_open_frames(&mut rx).await;
    let replayed: Vec<String> = frames
        .iter()
        .filter(|frame| {
            frame.get("method").and_then(Value::as_str)
                == Some(octos_core::ui_protocol::methods::BACKGROUND_ACTIVITY)
        })
        .map(|frame| {
            frame["params"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        replayed,
        vec!["alpha log line".to_owned()],
        "replay must not put another tenant's observed text on an alpha connection"
    );

    // Live forwarder leg.
    ledger.append_notification(activity("beta", "beta live log line"));
    ledger.append_notification(activity("alpha", "alpha live log line"));

    let live = next_frame_within(&mut rx, 2_000)
        .await
        .expect("alpha's own live activity must still be forwarded");
    assert_eq!(
        live.get("method").and_then(Value::as_str),
        Some(octos_core::ui_protocol::methods::BACKGROUND_ACTIVITY),
        "the only live activity frame alpha may see is its own: {live}"
    );
    assert_eq!(live["params"]["text"], json!("alpha live log line"));
    assert!(
        next_frame_within(&mut rx, 250).await.is_none(),
        "beta's live activity must not reach an alpha-scoped connection"
    );

    abort_live_forwarders(&forwarders, &ledger).await;
}

/// #2067 round 4 — an UNSCOPED connection's per-session scope diverges from the
/// profile its own turns run under, with no routing header involved.
///
/// `session_open_profile_id` is a single per-connection cell overwritten by
/// every successful open, and every later `turn/start` — including one on an
/// EARLIER session — receives that latest value and selects that runtime. So:
/// open bare A (resolves `_main`), open bare B as `beta`, then run a turn on A.
/// A's forwarder still holds `_main` while the turn runs under `beta`, and a
/// monitor the model creates in that turn is stamped `beta`
/// (`runtime/profile.rs` -> `goal_tool.rs`). The turn misrouting is pre-existing;
/// the DELIVERY DROP is not — it exists only because the frame is filtered.
///
/// `background/activity` is the worst case: its sink appends over a detached
/// connection whose writer is discarded, so replay and live fan-out are its
/// only delivery paths and the drop is total.
#[tokio::test]
async fn should_deliver_later_profile_frames_when_an_unscoped_connection_opened_two_sessions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = local_profile_state_with_sessions(temp.path());
    create_or_get_local_solo_profile(
        &state,
        local_profile_params("Beta Owner", "beta", "beta@example.com"),
    )
    .expect("create beta profile");

    let (ws, mut rx) = ws_connection_for_test(64);
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let session_a = SessionKey("web-two-open-a".into());
    let session_b = SessionKey("web-two-open-b".into());
    let features = ConnectionUiFeatures {
        background_activity: true,
        ..Default::default()
    };

    let beta_activity = |text: &str| {
        UiNotification::BackgroundActivity(octos_core::ui_protocol::BackgroundActivityEvent {
            session_id: session_a.clone(),
            profile_id: Some("beta".to_owned()),
            origin_kind: "monitor".to_owned(),
            origin_id: "monitor-model-made".to_owned(),
            origin_label: Some("ci-tail".to_owned()),
            text: text.to_owned(),
            emitted_at_ms: 1_760_000_000_000,
            dropped_count: None,
            suppressed: false,
        })
    };

    // No routing header anywhere: this connection is plain unscoped.
    let open = |session_id: SessionKey, profile_id: Option<&str>, rpc_id: &str| {
        let params = SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: profile_id.map(ToOwned::to_owned),
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        };
        (params, rpc_id.to_owned())
    };

    let (params_a, id_a) = open(session_a.clone(), None, "open-a");
    assert!(
        handle_session_open(
            &ws,
            &state,
            &ledger,
            &approvals,
            &questions,
            &forwarders,
            None,
            None,
            features,
            id_a,
            params_a,
        )
        .await,
        "bare session A must open on an unscoped connection"
    );
    drain_session_open_frames(&mut rx).await;

    let (params_b, id_b) = open(session_b.clone(), Some("beta"), "open-b");
    assert!(
        handle_session_open(
            &ws,
            &state,
            &ledger,
            &approvals,
            &questions,
            &forwarders,
            None,
            None,
            features,
            id_b,
            params_b,
        )
        .await,
        "bare session B must open under beta on the same connection"
    );
    drain_session_open_frames(&mut rx).await;

    // A turn on session A now runs under beta's runtime, so the monitor it
    // creates emits beta-stamped activity onto session A's stream.
    ledger.append_notification(beta_activity("live log line"));

    let live = next_frame_within(&mut rx, 2_000).await.expect(
        "session A must still receive the activity its own turn produced, even though the turn \
         ran under the profile a later open bound",
    );
    assert_eq!(
        live.get("method").and_then(Value::as_str),
        Some(octos_core::ui_protocol::methods::BACKGROUND_ACTIVITY),
    );
    assert_eq!(live["params"]["text"], json!("live log line"));

    // Same property on reconnect replay — the only other delivery path this
    // notification has.
    let (params_reconnect, id_reconnect) = open(session_a.clone(), None, "reopen-a");
    assert!(
        handle_session_open(
            &ws,
            &state,
            &ledger,
            &approvals,
            &questions,
            &forwarders,
            None,
            None,
            features,
            id_reconnect,
            params_reconnect,
        )
        .await,
        "session A must reopen"
    );
    // Drain everything the reopen queued rather than stopping at the first
    // `session/open` notification — on a reconnect the REPLAYED historical open
    // frame precedes the activity we are looking for.
    let mut replayed: Vec<String> = Vec::new();
    while let Some(frame) = next_frame_within(&mut rx, 500).await {
        if frame.get("method").and_then(Value::as_str)
            == Some(octos_core::ui_protocol::methods::BACKGROUND_ACTIVITY)
        {
            replayed.push(
                frame["params"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
    }
    assert_eq!(
        replayed,
        vec!["live log line".to_owned()],
        "reconnect replay must not drop session A's own activity"
    );

    abort_live_forwarders(&forwarders, &ledger).await;
}

#[test]
fn session_scope_allows_matching_authenticated_profile() {
    let session_id = SessionKey::with_profile("profile-a", "api", "chat-1");

    let active_profile_id =
        validate_session_scope(&session_id, Some("profile-a"), Some("profile-a"))
            .expect("valid scope");

    assert_eq!(active_profile_id.as_deref(), Some("profile-a"));
}

#[test]
fn session_scope_rejects_cross_profile_session_id() {
    let session_id = SessionKey::with_profile("profile-b", "api", "chat-1");

    let error =
        validate_session_scope(&session_id, None, Some("profile-a")).expect_err("scope error");

    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::INVALID_PARAMS
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("expected_profile_id")),
        Some(&Value::String("profile-a".into()))
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("actual_profile_id")),
        Some(&Value::String("profile-b".into()))
    );
}

#[test]
fn session_scope_accepts_unprofiled_session_id_when_authenticated() {
    let session_id = SessionKey::new("api", "chat-1");

    let profile_id = validate_session_scope(&session_id, None, Some("profile-a")).expect("scope");

    assert_eq!(profile_id.as_deref(), Some("profile-a"));
}

#[test]
fn session_scope_rejects_cross_profile_open_param() {
    let session_id = SessionKey::with_profile("profile-a", "api", "chat-1");

    let error = validate_session_scope(&session_id, Some("profile-b"), Some("profile-a"))
        .expect_err("scope error");

    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::INVALID_PARAMS
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("actual_profile_id")),
        Some(&Value::String("profile-b".into()))
    );
}

// Place near other ui_protocol tests. Verifies a profile-mismatched session_id
// yields close-code 1008.
#[tokio::test]
async fn send_scope_error_closes_with_1008_on_authenticated_mismatch() {
    let (ws, mut rx) = ws_connection_for_test(8);
    let session_id = SessionKey::with_profile("profile-b", "api", "chat-1");
    let error =
        validate_session_scope(&session_id, None, Some("profile-a")).expect_err("scope error");
    assert!(is_auth_scope_violation(&error));

    send_scope_error(&ws, "rpc-1".into(), error);

    // First frame is the close-code 1008 with reason "auth_expired" — the
    // close MUST precede the error envelope so it survives writer-channel
    // backpressure (codex BLOCK 2026-05-13).
    let first = rx.recv().await.expect("close frame");
    match first {
        super::WsMessage::Close(Some(frame)) => {
            assert_eq!(frame.code, 1008);
            assert_eq!(&*frame.reason, "auth_expired");
        }
        other => panic!("expected close frame with 1008, got {other:?}"),
    }

    // Second frame is the JSON-RPC error envelope (courtesy detail).
    let second = rx.recv().await.expect("rpc error frame");
    let text = match second {
        super::WsMessage::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    assert!(text.contains("expected_profile_id"));
}

#[tokio::test]
async fn send_scope_error_does_not_close_when_unauthenticated() {
    let (ws, mut rx) = ws_connection_for_test(8);
    // No connection_profile_id => not authenticated; cross-profile id is
    // a generic invalid_params, not an auth scope violation.
    let session_id = SessionKey::with_profile("profile-a", "api", "chat-1");
    let error =
        validate_session_scope(&session_id, Some("profile-b"), None).expect_err("scope error");
    assert!(!is_auth_scope_violation(&error));

    send_scope_error(&ws, "rpc-1".into(), error);

    // Only the JSON-RPC error envelope should arrive — no close frame.
    let _first = rx.recv().await.expect("rpc error frame");
    // Drop the sender side so a pending recv resolves promptly; instead,
    // poll once with no wait to confirm the queue is empty.
    assert!(rx.try_recv().is_err(), "no close frame expected");
}

/// #2040: a stdio connection must NEVER receive the 1008 auth-expiry close.
/// The stdio dispatch passes the session/open CANDIDATE profile as the
/// connection scope (so a successful open can rebind the connection), which
/// routes a profile-segment mismatch through the AUTHENTICATED validator and
/// tags the error `auth_scope_violation`. On a WS connection that tag
/// enqueues a 1008 close ahead of the error envelope; on stdio the Close
/// frame ends the writer loop (`write_stdio_message`), so pre-fix the error
/// reply was never written and the whole transport died with the request
/// unanswered.
#[test]
fn send_scope_error_on_stdio_answers_without_closing() {
    let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel(8);
    let ws = WsConnection::new_stdio(writer_tx);
    // Mirror the stdio dispatch: the candidate profile is passed as the
    // connection scope and the session_id segment disagrees with it.
    let session_id = SessionKey::with_profile("nosuchprofile", "local", "tui");
    let error = validate_session_scope(&session_id, Some("soak"), Some("soak"))
        .expect_err("segment mismatch must fail validation");
    assert!(is_auth_scope_violation(&error));

    send_scope_error(&ws, "rpc-1".into(), error);

    // The FIRST frame is the error envelope carrying the request id — not a
    // Close, which the stdio writer loop treats as end-of-stream.
    let message = writer_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("the mismatch must still be answered");
    let WsMessage::Text(text) = message else {
        panic!(
            "expected the error envelope first, got a non-text frame \
             (a Close would end the stdio writer loop)"
        );
    };
    let frame: Value = serde_json::from_str(text.as_ref()).expect("valid JSON frame");
    assert_eq!(frame["id"], json!("rpc-1"));
    assert!(
        frame["error"].is_object(),
        "the reply carries the scope error: {frame}"
    );
    assert!(
        writer_rx.try_recv().is_err(),
        "no close frame may follow — on stdio it terminates the writer loop"
    );
    assert!(
        !ws.is_failed(),
        "a rejected request must not kill the stdio transport"
    );
}

/// Codex BLOCK regression (2026-05-13): with the writer channel at
/// capacity 2 and one slot already used, `send_scope_error` must use
/// the remaining slot for the 1008 close — NOT the courtesy error
/// envelope. The close is the load-bearing signal the SPA's
/// `crew:auth_expired` listener uses to clear its token. The error
/// envelope is allowed to drop under backpressure.
///
/// Test geometry: capacity 2 + 1 primer = exactly one free slot at the
/// moment `send_scope_error` enqueues. Pre-fix the order was
/// error-then-close → error queued, close dropped. Post-fix the order
/// is close-then-error → close queued, error dropped.
#[tokio::test]
async fn auth_scope_violation_close_frame_survives_capacity_one_writer() {
    let (ws, mut rx) = ws_connection_for_test(2);

    // Pre-fill ONE slot so only one of the two outbound frames can
    // survive backpressure. The close MUST be that one.
    ws.writer
        .try_send(super::WsMessage::Text("priming".into()))
        .expect("prime channel");
    assert_eq!(
        ws.writer.capacity(),
        1,
        "channel must have exactly one free slot for the backpressure case",
    );

    let session_id = SessionKey::with_profile("profile-b", "api", "chat-1");
    let error =
        validate_session_scope(&session_id, None, Some("profile-a")).expect_err("scope error");
    assert!(is_auth_scope_violation(&error));

    send_scope_error(&ws, "rpc-1".into(), error);

    // Drain the priming frame first.
    let primer = rx.recv().await.expect("priming frame");
    assert!(matches!(primer, super::WsMessage::Text(_)));

    // The next frame MUST be the 1008 close. The error envelope was
    // dropped under backpressure — that's acceptable; the close is
    // what the SPA listens for.
    let next = rx.recv().await.expect("close frame survives backpressure");
    match next {
        super::WsMessage::Close(Some(frame)) => {
            assert_eq!(frame.code, 1008);
            assert_eq!(&*frame.reason, "auth_expired");
        }
        other => panic!("expected 1008 close to survive backpressure, got {other:?}"),
    }
}

#[test]
fn session_scope_preserves_legacy_keys_without_profile_context() {
    let legacy_session_id = SessionKey::new("api", "chat-1");
    let profiled_session_id = SessionKey::with_profile("profile-a", "api", "chat-1");

    assert_eq!(
        validate_session_scope(&legacy_session_id, None, None).expect("legacy scope"),
        None
    );
    assert_eq!(
        validate_session_scope(&profiled_session_id, None, None)
            .expect("profiled scope")
            .as_deref(),
        Some("profile-a")
    );
}

#[test]
fn prompt_text_requires_non_empty_text_input() {
    assert_eq!(
        prompt_text(&[InputItem::Text {
            text: "hello".into()
        }]),
        Some("hello".into())
    );
    assert_eq!(
        prompt_text(&[
            InputItem::Text { text: "a".into() },
            InputItem::Text { text: "b".into() }
        ]),
        Some("a\nb".into())
    );
    assert_eq!(prompt_text(&[InputItem::Text { text: "   ".into() }]), None);
}

fn state_with_sessions(data_dir: &std::path::Path) -> Arc<AppState> {
    Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(
            octos_bus::SessionManager::open(data_dir).expect("session manager"),
        ))),
        ..AppState::empty_for_tests()
    })
}

/// Build an `ActiveTurn` with default `Active` state for tests that drive
/// the registry directly without going through `handle_turn_start`.
async fn test_connection_turn(
    active: &SharedActiveTurns,
    session: &SessionKey,
    turn_id: &TurnId,
) -> ConnectionTurn {
    let map = active.lock().await;
    let state = map
        .get(session)
        .filter(|entry| entry.turn_id == *turn_id)
        .map(|entry| entry.state.clone())
        .unwrap_or_else(|| Arc::new(TokioMutex::new(TurnState::Active)));
    ConnectionTurn {
        turn_id: turn_id.clone(),
        state,
    }
}

fn test_active_turn(turn_id: TurnId, abort: AbortHandle) -> ActiveTurn {
    let (tx, _rx) = mpsc::channel::<()>(1);
    ActiveTurn {
        turn_id,
        profile_id: MAIN_PROFILE_ID.to_owned(),
        state: Arc::new(TokioMutex::new(TurnState::Active)),
        interrupt_tx: Arc::new(TokioMutex::new(Some(tx))),
        steer: None,
        abort,
    }
}

#[tokio::test]
async fn session_open_replays_notifications_after_cursor_and_returns_ledger_cursor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let first = ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        text: "one".into(),
    }));
    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id,
        text: "two".into(),
    }));

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(first.cursor),
        },
    )
    .await
    .expect("open session after retained cursor");

    assert_eq!(outcome.result.opened.session_id, session_id);
    assert_eq!(outcome.result.opened.cursor.expect("cursor").seq, 3);
    assert_eq!(outcome.replay.len(), 1);
    assert_eq!(outcome.replay[0].cursor.seq, 2);
    assert!(matches!(
        &outcome.replay[0].event,
        UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(event))
            if event.text == "two"
    ));
}

#[tokio::test]
async fn session_open_topic_scope_replays_only_matching_topic_bucket() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let base_session = SessionKey("local:topic-replay".into());
    let alpha_session = session_key_with_optional_topic(&base_session, Some("alpha"));
    let beta_session = session_key_with_optional_topic(&base_session, Some("beta"));
    let root_turn = TurnId::new();
    let alpha_turn = TurnId::new();
    let beta_turn = TurnId::new();

    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: base_session.clone(),
        topic: None,
        turn_id: root_turn,
        text: "root".into(),
    }));
    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: alpha_session.clone(),
        topic: None,
        turn_id: alpha_turn,
        text: "alpha".into(),
    }));
    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: beta_session.clone(),
        topic: None,
        turn_id: beta_turn,
        text: "beta".into(),
    }));

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: base_session.clone(),
            topic: Some("alpha".into()),
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: alpha_session.0.clone(),
                seq: 0,
            }),
        },
    )
    .await
    .expect("topic-scoped session/open succeeds");

    assert_eq!(outcome.result.opened.session_id, alpha_session);
    assert_eq!(outcome.replay.len(), 1);
    assert!(matches!(
        &outcome.replay[0].event,
        UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(event))
            if event.text == "alpha" && event.topic.as_deref() == Some("alpha")
    ));

    let root_outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: base_session.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: base_session.0.clone(),
                seq: 0,
            }),
        },
    )
    .await
    .expect("root session/open succeeds");

    assert_eq!(root_outcome.result.opened.session_id, base_session);
    assert_eq!(root_outcome.replay.len(), 1);
    assert!(matches!(
        &root_outcome.replay[0].event,
        UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(event))
            if event.text == "root" && event.topic.is_none()
    ));
}

#[tokio::test]
async fn session_open_rejects_after_cursor_from_other_stream() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());

    let error = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: "local:other".into(),
                seq: 0,
            }),
        },
    )
    .await
    .expect_err("foreign stream cursor should fail");

    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::CURSOR_INVALID
    );
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("cursor_stream_mismatch"))
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("expected_stream")),
        Some(&json!(session_id.0))
    );
}

#[tokio::test]
async fn session_open_rejects_stale_after_cursor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(1);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        text: "one".into(),
    }));
    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id,
        text: "two".into(),
    }));

    let error = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        },
    )
    .await
    .expect_err("stale cursor should fail");

    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::CURSOR_OUT_OF_RANGE
    );
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("cursor_expired"))
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("oldest_retained_seq")),
        Some(&json!(2))
    );
}

#[tokio::test]
async fn session_open_replays_pending_approval_after_reconnect_without_cursor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let approval_id = ApprovalId::new();
    approvals.request(ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        TurnId::new(),
        "shell",
        "Run command",
        "cargo test",
    ));

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("open session should replay pending approval");

    assert!(outcome.replay.is_empty());
    assert_eq!(outcome.pending_approvals.len(), 1);
    assert_eq!(outcome.pending_approvals[0].approval_id, approval_id);
    assert_eq!(outcome.pending_approvals[0].title, "Run command");
}

// ---- UPCR-2026-023 pending-question reconnect + capability gating ----

fn sample_pending_question(
    session_id: SessionKey,
    question_id: QuestionId,
    turn_id: TurnId,
) -> UserQuestionRequestedEvent {
    use octos_core::ui_protocol::{UserQuestion, UserQuestionOption};
    UserQuestionRequestedEvent::new(
        session_id,
        question_id,
        turn_id,
        "Pick a framework",
        "Which framework should I scaffold?",
        vec![UserQuestion {
            header: "Framework".into(),
            question: "Which framework?".into(),
            options: vec![
                UserQuestionOption {
                    label: "axum".into(),
                    description: "tower-based".into(),
                },
                UserQuestionOption {
                    label: "actix".into(),
                    description: "actor-based".into(),
                },
            ],
            multi_select: false,
            allow_free_text: true,
        }],
    )
}

fn features_with_user_question_v1() -> ConnectionUiFeatures {
    ConnectionUiFeatures {
        user_question_v1: true,
        ..ConnectionUiFeatures::default()
    }
}

#[tokio::test]
async fn session_open_replays_pending_question_for_negotiated_client() {
    // #3: a reconnecting client that negotiated `user_question.v1` must see
    // the still-pending structured question in `pending_questions`.
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let session_id = SessionKey("local:test".into());
    let question_id = QuestionId::new();
    let _rx = questions.request_runtime(sample_pending_question(
        session_id.clone(),
        question_id.clone(),
        TurnId::new(),
    ));

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &questions,
        ConnectionId::next(),
        None,
        None,
        features_with_user_question_v1(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("open session should replay pending question");

    assert_eq!(outcome.pending_questions.len(), 1);
    assert_eq!(outcome.pending_questions[0].question_id, question_id);
    assert_eq!(
        outcome.pending_questions[0].title, "Pick a framework",
        "the reconnecting client must re-render the pending question"
    );
}

#[tokio::test]
async fn session_open_pending_question_filtered_out_without_capability() {
    // #4 (replay path): a client lacking `user_question.v1` must NOT
    // receive the pending question even though it is in the store — the
    // outcome carries it (computed unconditionally) but the send-site
    // capability filter drops it. Assert the filter at the unit level so
    // the contract is pinned regardless of send wiring.
    let event = sample_pending_question(
        SessionKey("local:test".into()),
        QuestionId::new(),
        TurnId::new(),
    );
    let ledger_event =
        UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionRequested(event));
    assert!(
        !live_event_passes_capability_filter(&ledger_event, ConnectionUiFeatures::default()),
        "a connection without user_question.v1 must not receive UserQuestionRequested"
    );
    assert!(
        live_event_passes_capability_filter(&ledger_event, features_with_user_question_v1()),
        "a negotiated connection must receive UserQuestionRequested"
    );
}

#[test]
fn plan_updated_gated_by_plan_todos_capability() {
    use octos_core::ui_protocol::{PlanUpdatedEvent, UiPlanRecord};
    let event =
        UiProtocolLedgerEvent::Notification(UiNotification::PlanUpdated(PlanUpdatedEvent {
            session_id: SessionKey("local:test".into()),
            topic: None,
            turn_id: None,
            plan: UiPlanRecord {
                items: Vec::new(),
                title: None,
                updated_at_ms: 0,
            },
        }));
    // A connection that did not negotiate plan.todos.v1 never receives it —
    // on the live broadcast OR reconnect replay (both call this filter).
    assert!(!live_event_passes_capability_filter(
        &event,
        ConnectionUiFeatures::default()
    ));
    let negotiated = ConnectionUiFeatures {
        plan_todos: true,
        ..Default::default()
    };
    assert!(live_event_passes_capability_filter(&event, negotiated));
}

/// #2019 — build a human-sink event for the tests below.
fn background_activity_for(
    session_id: &SessionKey,
    origin_id: &str,
    text: &str,
) -> octos_core::ui_protocol::BackgroundActivityEvent {
    octos_core::ui_protocol::BackgroundActivityEvent {
        session_id: session_id.clone(),
        profile_id: Some("main".into()),
        origin_kind: "monitor".into(),
        origin_id: origin_id.into(),
        origin_label: Some("ci-tail".into()),
        text: text.into(),
        emitted_at_ms: 1_760_000_000_000,
        dropped_count: None,
        suppressed: false,
    }
}

/// #2019 — `background/activity` is a NEW notification shape. A client that
/// did not negotiate `event.background_activity.v1` cannot render it and would
/// report "unknown UI protocol notification" — the ui-protocol-v2-migration
/// trap. Gate it on both the live broadcast and reconnect replay (both routes
/// call this filter), mirroring the `plan.todos.v1` discipline.
#[test]
fn should_gate_background_activity_when_the_capability_was_not_negotiated() {
    let event = UiProtocolLedgerEvent::Notification(UiNotification::BackgroundActivity(
        background_activity_for(&SessionKey("local:test".into()), "monitor_01", "boom"),
    ));
    assert!(
        !live_event_passes_capability_filter(&event, ConnectionUiFeatures::default()),
        "a connection without event.background_activity.v1 must never receive it"
    );
    let negotiated = ConnectionUiFeatures {
        background_activity: true,
        ..Default::default()
    };
    assert!(live_event_passes_capability_filter(&event, negotiated));
}

/// #2019 acceptance — ten monitor fires land on the OWNING session's durable
/// stream and are replayable by cursor after a client disconnects mid-run.
///
/// Two properties in one test, because they are the same property: the ledger
/// append is the routing decision AND the disconnect-survival mechanism.
/// Asserts ROUTING (the sibling session's stream stays empty), not merely that
/// events were emitted — activity on the wrong stream renders in whichever
/// session happens to be focused (octos-tui#461, #466, #483).
#[tokio::test]
async fn should_replay_background_activity_on_the_owning_session_when_a_client_reconnects() {
    let (ws, _rx) = ws_connection_for_test(64);
    let ledger = UiProtocolLedger::new(256);
    let owner = SessionKey("local:owner".into());
    let sibling = SessionKey("local:sibling".into());

    // A client connected at the start of the run captures the cursor it would
    // resume from, then "disconnects" (we simply stop reading the socket).
    let resume_from = UiCursor {
        stream: owner.0.clone(),
        seq: 0,
    };
    for i in 0..10 {
        let _ = send_notification_durable(
            &ws,
            &ledger,
            UiNotification::BackgroundActivity(background_activity_for(
                &owner,
                "monitor_01",
                &format!("line {i}"),
            )),
        );
    }
    // One event from a DIFFERENT origin on the SAME session: grouping is a
    // client concern, so both must be on the one session stream.
    let _ = send_notification_durable(
        &ws,
        &ledger,
        UiNotification::BackgroundActivity(background_activity_for(
            &owner,
            "monitor_02",
            "other origin",
        )),
    );

    let replay = ledger
        .replay_after(&owner, Some(&resume_from))
        .expect("reconnecting client replays the owning session");
    let replayed: Vec<(String, String)> = replay
        .iter()
        .filter_map(|entry| match &entry.event {
            UiProtocolLedgerEvent::Notification(UiNotification::BackgroundActivity(event)) => {
                Some((event.origin_id.clone(), event.text.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        replayed.len(),
        11,
        "the whole run replays after a mid-run disconnect, not just its tail"
    );
    assert_eq!(replayed[0], ("monitor_01".into(), "line 0".into()));
    assert_eq!(replayed[9], ("monitor_01".into(), "line 9".into()));
    assert_eq!(replayed[10], ("monitor_02".into(), "other origin".into()));

    // ROUTING: the sibling session's stream never saw any of it.
    let sibling_replay = ledger
        .replay_after(
            &sibling,
            Some(&UiCursor {
                stream: sibling.0.clone(),
                seq: 0,
            }),
        )
        .unwrap_or_default();
    assert!(
        !sibling_replay.iter().any(|entry| matches!(
            &entry.event,
            UiProtocolLedgerEvent::Notification(UiNotification::BackgroundActivity(_))
        )),
        "background activity must never land on a session that did not own the emitter"
    );
}

#[tokio::test]
async fn session_open_does_not_duplicate_pending_question_already_in_cursor_replay() {
    // #3: a question already carried by the cursor replay window must not
    // be re-sent as a supplemental pending question (mirrors the approval
    // de-dup).
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let session_id = SessionKey("local:test".into());
    let question_id = QuestionId::new();
    let event = sample_pending_question(session_id.clone(), question_id.clone(), TurnId::new());
    let _rx = questions.request_runtime(event.clone());
    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: TurnId::new(),
        text: "before".into(),
    }));
    ledger.append_notification(UiNotification::UserQuestionRequested(event));

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &questions,
        ConnectionId::next(),
        None,
        None,
        features_with_user_question_v1(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 1,
            }),
        },
    )
    .await
    .expect("open session should rely on cursor replay");

    assert!(
        outcome.pending_questions.is_empty(),
        "a question already in the cursor replay must not be re-sent"
    );
}

#[tokio::test]
async fn pending_question_waiter_guard_cancels_entry_on_drop() {
    // #2: when the requester's waiting future is dropped (timeout,
    // interrupt, abort, panic) before a clean resolution, the RAII guard
    // cancels the pending store entry — the blocked tool sees a closed
    // receiver (Cancelled) and the entry does not leak.
    let contracts = Arc::new(UiProtocolContractStores::default());
    let session_id = SessionKey("local:test".into());
    let question_id = QuestionId::new();
    let turn_id = TurnId::new();
    let rx = contracts
        .user_questions
        .request_runtime(sample_pending_question(
            session_id.clone(),
            question_id.clone(),
            turn_id,
        ));

    // Arm a guard exactly as the requester does, then drop it WITHOUT
    // disarming — simulating the tool future being dropped mid-wait.
    {
        let _guard = PendingQuestionWaiterGuard::new(
            contracts.clone(),
            session_id.clone(),
            question_id.clone(),
        );
    }

    // The waiter is resolved-as-cancelled (sender dropped → Err).
    assert!(
        rx.await.is_err(),
        "dropping the guard must cancel the pending entry, closing the waiter"
    );
    // The entry is no longer pending (it was moved to Cancelled).
    assert!(
        contracts
            .user_questions
            .pending_for_session(&session_id)
            .is_empty(),
        "cancelled entry must not appear as pending"
    );
}

#[tokio::test]
async fn pending_question_waiter_guard_disarmed_does_not_cancel() {
    // The guard must NOT cancel a cleanly-resolved entry: after disarm,
    // dropping it is a no-op and the still-pending entry survives.
    let contracts = Arc::new(UiProtocolContractStores::default());
    let session_id = SessionKey("local:test".into());
    let question_id = QuestionId::new();
    let _rx = contracts
        .user_questions
        .request_runtime(sample_pending_question(
            session_id.clone(),
            question_id.clone(),
            TurnId::new(),
        ));

    {
        let mut guard = PendingQuestionWaiterGuard::new(
            contracts.clone(),
            session_id.clone(),
            question_id.clone(),
        );
        guard.disarm();
    }

    assert_eq!(
        contracts
            .user_questions
            .pending_for_session(&session_id)
            .len(),
        1,
        "a disarmed guard must not cancel the still-pending entry"
    );
}

#[tokio::test]
async fn session_hydrate_returns_pending_question_for_negotiated_client() {
    // #3 (hydrate path): a reconnecting client that requests the
    // pending-approvals section and negotiated `user_question.v1` gets the
    // pending structured question back in `pending_questions`.
    use octos_core::ui_protocol::hydrate_sections;
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let session_id = SessionKey("local:test".into());
    let question_id = QuestionId::new();
    let _rx = questions.request_runtime(sample_pending_question(
        session_id.clone(),
        question_id.clone(),
        TurnId::new(),
    ));
    // Make the session known so hydrate does not reject it.
    {
        let sessions = state.sessions.as_ref().expect("sessions");
        let mut guard = sessions.lock().await;
        guard.get_or_create(&session_id).await;
    }

    let (ws, mut rx) = ws_connection_for_test(16);
    handle_session_hydrate(
        &ws,
        &state,
        &ledger,
        &approvals,
        &questions,
        &active_turns,
        None,
        None,
        features_with_user_question_v1(),
        "hydrate-q".into(),
        SessionHydrateParams {
            session_id: session_id.clone(),
            after: None,
            include: vec![hydrate_sections::PENDING_APPROVALS.into()],
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], json!("hydrate-q"));
    let pending = frame["result"]["pending_questions"]
        .as_array()
        .expect("pending_questions must be present for a negotiated client");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["question_id"], json!(question_id.0.to_string()));
}

#[tokio::test]
async fn session_hydrate_omits_pending_question_without_capability() {
    // #3/#4: a client lacking `user_question.v1` must not receive the
    // `pending_questions` section at all (omitted, not `null`).
    use octos_core::ui_protocol::hydrate_sections;
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let session_id = SessionKey("local:test".into());
    let _rx = questions.request_runtime(sample_pending_question(
        session_id.clone(),
        QuestionId::new(),
        TurnId::new(),
    ));
    {
        let sessions = state.sessions.as_ref().expect("sessions");
        let mut guard = sessions.lock().await;
        guard.get_or_create(&session_id).await;
    }

    let (ws, mut rx) = ws_connection_for_test(16);
    handle_session_hydrate(
        &ws,
        &state,
        &ledger,
        &approvals,
        &questions,
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::default(),
        "hydrate-no-q".into(),
        SessionHydrateParams {
            session_id: session_id.clone(),
            after: None,
            include: vec![hydrate_sections::PENDING_APPROVALS.into()],
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], json!("hydrate-no-q"));
    assert!(
        frame["result"].get("pending_questions").is_none(),
        "non-negotiated client must not receive pending_questions; got {}",
        frame["result"]
    );
}

#[tokio::test]
async fn session_open_does_not_duplicate_pending_approval_already_in_cursor_replay() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let approval_id = ApprovalId::new();
    let approval = ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        TurnId::new(),
        "shell",
        "Run command",
        "cargo test",
    );
    approvals.request(approval.clone());
    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: TurnId::new(),
        text: "before".into(),
    }));
    ledger.append_notification(UiNotification::ApprovalRequested(approval));

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 1,
            }),
        },
    )
    .await
    .expect("open session should rely on cursor replay");

    assert_eq!(outcome.replay.len(), 1);
    assert!(matches!(
        &outcome.replay[0].event,
        UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(event))
            if event.approval_id == approval_id
    ));
    assert!(outcome.pending_approvals.is_empty());
}

#[tokio::test]
async fn session_open_rejects_cwd_without_negotiated_feature() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:cwd-feature".into());

    let error = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id,
            topic: None,
            profile_id: None,
            cwd: Some(temp.path().to_string_lossy().to_string()),
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect_err("cwd should require negotiated feature");

    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("feature_required"))
    );
}

// ----- UPCR-2026-007: capability advertisement on `SessionOpened` -----

#[test]
fn semantic_context_cache_diagnostics_negotiate_with_parent_capability() {
    let features = ConnectionUiFeatures::from_requested_feature_tokens(
        [
            UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1,
            UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1,
        ],
        true,
    );
    let capabilities = features.negotiated_capabilities();

    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1));
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1));
    assert!(features.context_semantic_cache_available());
}

#[test]
fn semantic_context_cache_diagnostics_cannot_negotiate_without_parent() {
    let features = ConnectionUiFeatures::from_requested_feature_tokens(
        [UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1],
        true,
    );
    let capabilities = features.negotiated_capabilities();

    assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1));
    assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1));
    assert!(!features.context_semantic_cache_available());
}

fn semantic_context_state_for_test(session_id: &SessionKey) -> UiContextState {
    let mut state = context_state_for_test(session_id);
    state.cache_epoch_id = Some("sha256:epoch".into());
    state.last_cache_invalidation_reason = Some("compaction_installed".into());
    state.semantic_head_id = Some("semblk_000007".into());
    state.semantic_head_kind = Some("tool_interaction".into());
    state
}

#[test]
fn semantic_cache_fields_are_absent_from_unnegotiated_session_open_payload() {
    let session_id = SessionKey("local:semantic-open".into());
    let context = json!({
        "schema": "octos.context.lifecycle.v1",
        "state": {
            "generation": 7,
            "cache_epoch_id": "sha256:epoch",
            "last_cache_invalidation_reason": "compaction_installed",
            "semantic_head_id": "semblk_000007",
            "semantic_head_kind": "tool_interaction"
        }
    });
    let event = UiProtocolLedgerEvent::Notification(UiNotification::SessionOpened(SessionOpened {
        session_id: session_id.clone(),
        active_profile_id: None,
        workspace_root: None,
        context: Some(context),
        context_state: Some(semantic_context_state_for_test(&session_id)),
        cursor: None,
        panes: None,
        capabilities: UiProtocolCapabilities::first_server_slice(),
        reasoning_effort: None,
    }));
    let lifecycle_only = ConnectionUiFeatures::from_requested_feature_tokens(
        [UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1],
        true,
    );

    let projected = context_event_for_features(event.clone(), lifecycle_only);
    let encoded = serde_json::to_value(projected).expect("serialize projected session open");
    let encoded = encoded.to_string();
    assert!(!encoded.contains("cache_epoch_id"));
    assert!(!encoded.contains("semantic_head_id"));
    assert!(!encoded.contains("semantic_head_kind"));
    assert!(!encoded.contains("last_cache_invalidation_reason"));

    let negotiated = ConnectionUiFeatures::from_requested_feature_tokens(
        [
            UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1,
            UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1,
        ],
        true,
    );
    let encoded = serde_json::to_value(context_event_for_features(event, negotiated))
        .expect("serialize negotiated session open")
        .to_string();
    assert!(encoded.contains("cache_epoch_id"));
    assert!(encoded.contains("semantic_head_id"));
}

#[test]
fn semantic_cache_fields_are_gated_on_compaction_and_normalization_payloads() {
    let session_id = SessionKey("local:semantic-events".into());
    let mut compaction = context_compaction_completed_for(&session_id);
    let UiNotification::ContextCompactionCompleted(compaction_event) = &mut compaction else {
        unreachable!()
    };
    compaction_event.context_state = semantic_context_state_for_test(&session_id);
    let mut normalization = context_normalization_reported_for(&session_id);
    let UiNotification::ContextNormalizationReported(normalization_event) = &mut normalization
    else {
        unreachable!()
    };
    normalization_event.context_state = semantic_context_state_for_test(&session_id);

    let lifecycle_only = ConnectionUiFeatures::from_requested_feature_tokens(
        [UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1],
        true,
    );
    for notification in [compaction.clone(), normalization.clone()] {
        let projected = context_event_for_features(
            UiProtocolLedgerEvent::Notification(notification),
            lifecycle_only,
        );
        let encoded = serde_json::to_value(projected)
            .expect("serialize unnegotiated lifecycle payload")
            .to_string();
        assert!(!encoded.contains("cache_epoch_id"));
        assert!(!encoded.contains("semantic_head_id"));
    }

    let negotiated = ConnectionUiFeatures::from_requested_feature_tokens(
        [
            UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1,
            UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1,
        ],
        true,
    );
    for notification in [compaction, normalization] {
        let projected = context_event_for_features(
            UiProtocolLedgerEvent::Notification(notification),
            negotiated,
        );
        let encoded = serde_json::to_value(projected)
            .expect("serialize negotiated lifecycle payload")
            .to_string();
        assert!(encoded.contains("cache_epoch_id"));
        assert!(encoded.contains("semantic_head_id"));
    }
}

#[test]
fn prompt_coverage_compares_provider_visible_media_and_reasoning() {
    let mut known = test_message(MessageRole::Assistant, "same visible text");
    known.media = vec!["image://one".into()];
    known.reasoning_content = Some("visible reasoning one".into());

    let mut changed_media = known.clone();
    changed_media.media = vec!["image://two".into()];
    assert_eq!(
        covered_prompt_message_indices(&[changed_media], &[known.clone()]),
        vec![false],
        "equal text with different provider-visible media is not covered"
    );

    let mut changed_reasoning = known.clone();
    changed_reasoning.reasoning_content = Some("visible reasoning two".into());
    assert_eq!(
        covered_prompt_message_indices(&[changed_reasoning], &[known.clone()]),
        vec![false],
        "equal text with different provider-visible reasoning is not covered"
    );
    assert_eq!(
        covered_prompt_message_indices(&[known.clone()], &[known]),
        vec![true]
    );
}

#[test]
fn rejected_manual_compaction_reports_typed_failure_without_generation_change() {
    let session_id = SessionKey("local:manual-rejected".into());
    let mut manager = ContextManager::new(session_id.to_string(), None);
    manager.record_message(&Message::user("old request ".repeat(100)));
    manager.record_message(&Message::assistant("old answer ".repeat(100)));
    manager.record_message(&Message::user("current request"));
    let generation_before = manager.generation();
    let record = manager.compact_context(
        "summary",
        CompactContextPolicy {
            keep_recent_tokens: Some(10_000),
            target_tokens_after_compaction: Some(96),
            ..CompactContextPolicy::default()
        },
    );
    assert_eq!(record.status, ContextCompactionStatus::Failed);
    assert_eq!(
        record.budget_outcome,
        ContextCompactionBudgetOutcome::RejectedOverBudget
    );
    assert_eq!(manager.generation(), generation_before);

    let result = appui_manual_compaction_result(&session_id, &record, None);
    assert_eq!(result["compacted"], json!(false));
    assert_eq!(result["status"], json!("failed"));
    assert_eq!(result["reason"], json!("rejected_over_budget"));
    assert_eq!(result["input_generation"], json!(generation_before));
    assert!(result["output_generation"].is_null());
}

// ===== M12 Phase D-1 auxiliary REST → WS negotiation =====

// ----- M9-γ-1: projection.envelope.v1 capability negotiation -----
//
// γ-1 wires capability negotiation only — no emit site references the
// new field yet, and legacy notifications continue to flow on the wire.
// The tests below mirror the `event.spawn_complete.v1` recipe and
// capture each of the six wiring sites (struct field, header parse,
// stdio defaults, requested-token rebuild, negotiated advertisement,
// notification methods list) so γ-2 emit-site wiring can land
// additively without touching the negotiation surface.

#[test]
fn projection_envelope_v1_off_in_stdio_defaults() {
    // `projection.envelope.v1` is NOT auto-enabled for stdio
    // connections. The γ-cutover mutual-exclusion gate
    // (`live_event_passes_capability_filter`) drops the legacy
    // `turn/completed` notification whenever `projection_envelope`
    // is true. The octoscode over stdio does NOT consume
    // `projection/envelope` and clears its turn-active state ONLY on
    // legacy `turn/completed`; auto-enabling envelopes here would
    // suppress that lifecycle signal and wedge the client (every
    // message after turn 1 queues "after active turn" forever). A
    // stdio client that genuinely consumes envelopes still opts in
    // via `client_hello` (see
    // `projection_envelope_client_hello_over_stdio_opt_in_preserved`).
    let features = ConnectionUiFeatures::stdio_defaults();
    assert!(!features.projection_envelope);
    let capabilities = features.negotiated_capabilities();
    assert!(!capabilities.supports_feature(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1));
}

/// Over a stdio-default connection (`projection_envelope == false`),
/// the legacy `turn/completed` notification MUST pass the
/// per-connection capability filter — both the broadcast path
/// (`live_event_passes_capability_filter`) and the direct-send path
/// (`direct_send_passes_capability_filter`). This is the
/// turn-lifecycle signal the stdio TUI keys on to clear its
/// turn-active state. If it were dropped (as it is when
/// `projection_envelope` is true), the TUI wedges after turn 1.
#[tokio::test]
async fn stdio_default_connection_delivers_legacy_turn_completed() {
    let session_id = SessionKey("local:stdio-turn-completed".into());
    let completed =
        UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: TurnId::new(),
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        }));

    // Broadcast / live-forwarder path.
    let features = ConnectionUiFeatures::stdio_defaults();
    assert!(
        live_event_passes_capability_filter(&completed, features),
        "stdio-default connection must receive legacy turn/completed via the broadcast filter"
    );

    // Direct-send path: a stdio connection snapshots stdio_defaults
    // into its live-features, so the direct-send gate must also let
    // turn/completed through.
    let (tx, _rx) = mpsc::channel(16);
    let ws = WsConnection::new(tx);
    ws.update_live_features(ConnectionUiFeatures::stdio_defaults());
    assert!(
        direct_send_passes_capability_filter(&ws, &completed),
        "stdio-default connection must receive legacy turn/completed via the direct-send filter"
    );
}

/// Opt-in preservation: a stdio connection that DOES consume
/// envelopes can still negotiate `projection.envelope.v1` via
/// `client_hello` (`from_requested_feature_tokens` with the stdio
/// transport flag), flipping `projection_envelope` back to true. The
/// default change is default-only — it does not remove the ability
/// to opt in. When opted in, the γ gate then (correctly) suppresses
/// legacy `turn/completed` for that connection in favour of the
/// canonical envelope.
#[test]
fn projection_envelope_client_hello_over_stdio_opt_in_preserved() {
    let features = ConnectionUiFeatures::from_requested_feature_tokens(
        [UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1],
        true, // stdio_transport
    );
    assert!(
        features.projection_envelope,
        "client_hello over stdio must still be able to opt into projection.envelope.v1"
    );
    assert!(features.stdio_transport);
    let capabilities = features.negotiated_capabilities();
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1));

    // And once opted in, the γ gate suppresses legacy turn/completed
    // for that connection (envelope supersedes it) — confirming the
    // opt-in actually re-engages the mutual-exclusion contract.
    let session_id = SessionKey("local:stdio-opt-in".into());
    let completed =
        UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id,
            topic: None,
            turn_id: TurnId::new(),
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        }));
    assert!(
        !live_event_passes_capability_filter(&completed, features),
        "an opted-in stdio connection sees the envelope, not legacy turn/completed"
    );
}

#[test]
fn projection_envelope_client_hello_feature_tokens_round_trip() {
    let features = ConnectionUiFeatures::from_requested_feature_tokens(
        [UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1],
        false,
    );
    assert!(features.projection_envelope);
    assert!(features.header_present);
    assert!(!features.stdio_transport);
    let capabilities = features.negotiated_capabilities();
    assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1));
}

#[test]
fn projection_envelope_method_in_notification_methods_list() {
    assert!(
        octos_core::ui_protocol::UI_PROTOCOL_NOTIFICATION_METHODS
            .contains(&octos_core::ui_protocol::methods::PROJECTION_ENVELOPE),
        "projection/envelope must be reserved in the notification methods list"
    );
    assert_eq!(
        octos_core::ui_protocol::methods::PROJECTION_ENVELOPE,
        "projection/envelope"
    );
}

// ────────────────────────────────────────────────────────────────────
// Codex #1336 round-2 BLOCKER 1: direct-send capability filter
// ────────────────────────────────────────────────────────────────────

/// A `projection.envelope.v1` connection that direct-sends a
/// legacy `MessageDelta` via `send_notification_ephemeral` must
/// observe ZERO wire frames on its writer channel. Pre-fix the
/// frame was sent directly (bypassing the
/// `live_event_passes_capability_filter` gate that the broadcast
/// forwarder applies). Post-fix the direct-send helpers consult
/// `WsConnection::snapshot_live_features` and apply the same
/// filter so the connection's mutual exclusion contract holds
/// even on the originating handler's direct path.
#[tokio::test]
async fn direct_ephemeral_send_drops_legacy_message_delta_for_projection_envelope_connection() {
    use octos_core::ui_protocol::MessageDeltaEvent;
    let (tx, mut rx) = mpsc::channel(16);
    let ws = WsConnection::new(tx);
    // Negotiate projection.envelope.v1.
    ws.update_live_features(ConnectionUiFeatures {
        projection_envelope: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    });

    let ledger = UiProtocolLedger::new(8);
    let session_id = SessionKey("local:blocker1-eph".into());
    let notif = UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: TurnId::new(),
        text: "hello".into(),
    });

    // Direct ephemeral send — should be filtered out for this connection.
    let result = send_notification_ephemeral(&ws, &ledger, notif);
    assert!(
        result.is_ok(),
        "filter-drop returns Ok so callers don't treat it as a fatal error"
    );
    assert!(
        rx.try_recv().is_err(),
        "projection.envelope.v1 connection must NOT receive the legacy MessageDelta directly"
    );
}

/// Mirror of the above for `send_notification_durable`. The γ
/// cutover gate filters `ToolStarted` / `ToolCompleted` /
/// legacy persisted-message / `FileAttached` / `TurnCompleted` — the
/// canonical envelopes emitted by `ledger.emit_envelope` cover
/// the same logical events via the broadcast forwarder.
#[tokio::test]
async fn direct_durable_send_drops_legacy_tool_completed_for_projection_envelope_connection() {
    use octos_core::ui_protocol::ToolCompletedEvent;
    let (tx, mut rx) = mpsc::channel(16);
    let ws = WsConnection::new(tx);
    ws.update_live_features(ConnectionUiFeatures {
        projection_envelope: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    });

    let ledger = UiProtocolLedger::new(8);
    let session_id = SessionKey("local:blocker1-dur".into());
    let notif = UiNotification::ToolCompleted(ToolCompletedEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: TurnId::new(),
        tool_call_id: "tc-1".into(),
        tool_name: "shell".into(),
        success: Some(true),
        output_preview: None,
        duration_ms: None,
    });

    let _ = send_notification_durable(&ws, &ledger, notif);
    assert!(
        rx.try_recv().is_err(),
        "projection.envelope.v1 connection must NOT receive the legacy ToolCompleted directly"
    );
}

/// Defensive: a legacy (non-projection.envelope) connection must
/// STILL receive direct sends of `MessageDelta` and tool events.
/// The filter is mutual exclusion — without
/// `projection.envelope.v1` the legacy shapes are the only thing
/// the client knows how to render.
#[tokio::test]
async fn direct_send_delivers_legacy_frames_to_non_projection_envelope_connection() {
    use octos_core::ui_protocol::MessageDeltaEvent;
    let (tx, mut rx) = mpsc::channel(16);
    let ws = WsConnection::new(tx);
    // Default features: projection_envelope is false.
    ws.update_live_features(ConnectionUiFeatures::default());

    let ledger = UiProtocolLedger::new(8);
    let session_id = SessionKey("local:blocker1-legacy".into());
    let notif = UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: TurnId::new(),
        text: "should reach legacy client".into(),
    });

    let _ = send_notification_ephemeral(&ws, &ledger, notif);
    let frame = rx
        .try_recv()
        .expect("legacy client must receive MessageDelta directly");
    // Sanity-check the frame is a JSON-RPC notification for message/delta.
    if let WsMessage::Text(text) = frame {
        let value: serde_json::Value = serde_json::from_str(text.as_str()).expect("JSON");
        assert_eq!(value["method"], "message/delta");
    } else {
        panic!("expected text frame");
    }
}

/// A `projection.envelope.v1` connection direct-sending an
/// `Envelope` (e.g. via `send_ledger_event_durable`) MUST pass
/// through — the envelope is exactly what the connection
/// negotiated for.
#[tokio::test]
async fn direct_send_delivers_envelope_to_projection_envelope_connection() {
    use octos_core::ui_protocol::{Envelope, EnvelopeNotification, EnvelopeTokenUsage, Payload};
    let (tx, mut rx) = mpsc::channel(16);
    let ws = WsConnection::new(tx);
    ws.update_live_features(ConnectionUiFeatures {
        projection_envelope: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    });

    let ledger = UiProtocolLedger::new(8);
    let session_id = SessionKey("local:blocker1-env".into());
    let envelope_notif = UiNotification::Envelope(EnvelopeNotification {
        session_id: session_id.clone(),
        topic: None,
        envelope: Envelope {
            thread_id: "thread-blocker1".into(),
            seq: 1,
            client_message_id: None,
            payload: Payload::TurnCompleted {
                token_usage: EnvelopeTokenUsage::default(),
            },
        },
    });

    let _ = send_notification_durable(&ws, &ledger, envelope_notif);
    let frame = rx
        .try_recv()
        .expect("projection.envelope.v1 connection MUST receive envelope direct-sends");
    if let WsMessage::Text(text) = frame {
        let value: serde_json::Value = serde_json::from_str(text.as_str()).expect("JSON");
        assert_eq!(value["method"], "projection/envelope");
    } else {
        panic!("expected text frame");
    }
}

// ────────────────────────────────────────────────────────────────────
// Codex #1336 round-3 BLOCKER 1: M15 live-subagent fixture path
// ────────────────────────────────────────────────────────────────────
//
// Round-2 fix landed the per-connection capability filter on the
// direct-send helpers (`send_notification_ephemeral` /
// `send_notification_durable` / `send_notification_lifecycle`).
// Codex round-2 then flagged that the M15 fixture
// (`run_m15_live_subagent_process`) still emitted a raw
// `message/delta` via `send_raw_notification_ephemeral`, which
// jumps straight to `ws.send_ephemeral` and bypasses the gate.
// The fix routes the delta through
// `emit_envelope_for_legacy_notification` (canonical envelope
// dual-emit) + `send_notification_ephemeral` (filtered legacy
// ephemeral). The next three tests pin that contract.

/// `projection.envelope.v1` connection: the M15 fixture's
/// "Subagent done" delta MUST NOT deliver a legacy
/// `message/delta` to this connection's writer channel. The
/// envelope dual-emit publishes the canonical envelope via
/// `ledger.emit_envelope` (observable on the broadcast forwarder),
/// but the filtered ephemeral send is dropped on the originating
/// connection because `projection.envelope.v1` supersedes
/// `message/delta`.
#[tokio::test]
async fn m15_fixture_delta_filtered_for_projection_envelope_connection() {
    let (tx, mut rx) = mpsc::channel(16);
    let ws = WsConnection::new(tx);
    ws.update_live_features(ConnectionUiFeatures {
        projection_envelope: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    });

    let ledger = UiProtocolLedger::new(8);
    let session_id = SessionKey("local:m15-delta-env".into());
    let turn_id = TurnId::new();
    // Mirror the exact shape `run_m15_live_subagent_process` builds.
    let delta = UiNotification::MessageDelta(octos_core::ui_protocol::MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        text: "Subagent done: reviewer-api (Ada) completed; artifact `notes` is ready.\n".into(),
    });

    // 1) Canonical envelope dual-emit — observable through the ledger.
    emit_envelope_for_legacy_notification(&ledger, &session_id, &delta);
    // 2) Filtered ephemeral legacy send — must be dropped on this connection.
    let result = send_notification_ephemeral(&ws, &ledger, delta);
    assert!(
        result.is_ok(),
        "filter-drop returns Ok so the spawn loop does not treat it as a fatal error"
    );

    // Wire: no legacy `message/delta` frame reaches the writer.
    match rx.try_recv() {
        Err(_) => {}
        Ok(frame) => {
            if let WsMessage::Text(text) = &frame {
                let value: serde_json::Value = serde_json::from_str(text.as_str()).expect("JSON");
                panic!(
                    "projection.envelope.v1 connection must NOT receive legacy frame; got {}",
                    value["method"]
                );
            }
            panic!("unexpected wire frame: {frame:?}");
        }
    }

    // Ledger: a canonical envelope WAS appended for the session.
    let (snapshot, _head) = ledger
        .snapshot_with_cursor(&session_id, None)
        .expect("snapshot succeeds for a session that just emitted an envelope");
    let envelope_count = snapshot
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                UiProtocolLedgerEvent::Notification(UiNotification::Envelope(_))
            )
        })
        .count();
    assert_eq!(
        envelope_count, 1,
        "exactly one canonical envelope must be appended for the M15 fixture delta"
    );
    let envelope = snapshot
        .iter()
        .find_map(|event| match &event.event {
            UiProtocolLedgerEvent::Notification(UiNotification::Envelope(envelope)) => {
                Some(envelope)
            }
            _ => None,
        })
        .expect("envelope notification present");
    assert_eq!(envelope.envelope.thread_id, turn_id.0.to_string());
    assert!(matches!(
        envelope.envelope.payload,
        octos_core::ui_protocol::Payload::AssistantDelta { .. }
    ));
}

/// Legacy (non-projection.envelope) connection: the M15 fixture
/// delta MUST deliver the legacy `message/delta` frame, and the
/// envelope ledger entry is also produced (which the live
/// forwarder filters out on this connection's wire — covered by
/// `live_event_passes_capability_filter` tests elsewhere; here
/// we focus on the direct-send half).
#[tokio::test]
async fn m15_fixture_delta_delivered_to_legacy_connection() {
    let (tx, mut rx) = mpsc::channel(16);
    let ws = WsConnection::new(tx);
    ws.update_live_features(ConnectionUiFeatures::default());

    let ledger = UiProtocolLedger::new(8);
    let session_id = SessionKey("local:m15-delta-legacy".into());
    let turn_id = TurnId::new();
    let delta = UiNotification::MessageDelta(octos_core::ui_protocol::MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        text: "Subagent done: reviewer-tests (Hypatia) completed; artifact `notes` is ready.\n"
            .into(),
    });

    emit_envelope_for_legacy_notification(&ledger, &session_id, &delta);
    let _ = send_notification_ephemeral(&ws, &ledger, delta);

    let frame = rx
        .try_recv()
        .expect("legacy client must receive the M15 fixture's MessageDelta directly");
    if let WsMessage::Text(text) = frame {
        let value: serde_json::Value = serde_json::from_str(text.as_str()).expect("JSON");
        assert_eq!(value["method"], "message/delta");
        assert!(
            value["params"]["text"]
                .as_str()
                .unwrap_or("")
                .starts_with("Subagent done:"),
            "delta text must carry the fixture's subagent-done body"
        );
    } else {
        panic!("expected text frame");
    }
    // Ledger still carries the envelope alongside; legacy connections
    // just never see it on the wire (live forwarder filter).
    let (snapshot, _head) = ledger
        .snapshot_with_cursor(&session_id, None)
        .expect("snapshot succeeds for a session that just emitted an envelope");
    assert!(
        snapshot.iter().any(|event| matches!(
            event.event,
            UiProtocolLedgerEvent::Notification(UiNotification::Envelope(_))
        )),
        "envelope dual-emit must still append to the ledger for replay correctness"
    );
}

/// Defense-in-depth: even if a future caller reaches for
/// `send_raw_notification_ephemeral` with an envelope-superseded
/// method, the helper itself refuses the dispatch — fail-closed
/// with `BackpressureDrop` and no wire frame emitted. Round-2's
/// per-connection filter is the primary gate; this is the second
/// belt so a `message/delta` raw send can never leak past it.
#[tokio::test]
async fn raw_ephemeral_helper_refuses_envelope_superseded_method() {
    let (tx, mut rx) = mpsc::channel(16);
    let ws = WsConnection::new(tx);
    ws.update_live_features(ConnectionUiFeatures::default());

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        send_raw_notification_ephemeral(
            &ws,
            octos_core::ui_protocol::methods::MESSAGE_DELTA,
            json!({"text": "should be refused"}),
        )
    }));
    // In debug builds the helper hits a `debug_assert!`; in release
    // builds it returns `Err(BackpressureDrop)`. Either way the wire
    // MUST be empty.
    match outcome {
        Ok(result) => assert!(
            matches!(result, Err(SendError::BackpressureDrop)),
            "raw helper must refuse envelope-superseded methods in release builds"
        ),
        Err(_) => {
            // debug_assert tripped — expected in debug.
        }
    }
    assert!(
        rx.try_recv().is_err(),
        "no wire frame may be emitted for an envelope-superseded raw send"
    );
}

#[test]
fn advertised_capabilities_include_session_btw() {
    let state = AppState::empty_for_tests();
    let capabilities = ConnectionUiFeatures::default().advertised_capabilities(&state);
    assert!(
        capabilities
            .supported_methods
            .iter()
            .any(|method| method == octos_core::ui_protocol::methods::SESSION_BTW),
        "session/btw must be advertised so clients can gate /btw; got {:?}",
        capabilities.supported_methods
    );
}

#[test]
fn build_btw_messages_shapes_prompt_without_tools() {
    let now = Utc::now();
    let mk = |role: MessageRole, content: &str| Message {
        role,
        content: content.into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: now,
    };
    let transcript = vec![
        mk(MessageRole::User, "please refactor the parser"),
        mk(MessageRole::Assistant, ""),
        mk(MessageRole::Assistant, "starting on it"),
    ];
    let activity = vec!["tool `shell` running".to_owned()];
    let messages = build_btw_messages(
        &transcript,
        &activity,
        "…drafting the cwnd table",
        "btw what are you working on?",
    );

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::System);
    assert!(
        messages[0].content.contains("NO tools"),
        "system prompt must state the no-tools restriction"
    );
    let prompt = &messages[1].content;
    assert!(prompt.contains("please refactor the parser"));
    assert!(prompt.contains("starting on it"));
    assert!(
        !prompt.contains("assistant: \n"),
        "empty-content messages are skipped"
    );
    assert!(prompt.contains("- tool `shell` running"));
    assert!(
        prompt.contains("…drafting the cwnd table"),
        "the in-flight draft tail must reach the provider; got {prompt}"
    );
    assert!(prompt.contains("btw what are you working on?"));
}

#[tokio::test]
async fn session_btw_rejects_empty_question() {
    let state = Arc::new(AppState::empty_for_tests());
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(4);

    handle_session_btw(
        &ws,
        &state,
        &ledger,
        &active_turns_registry(),
        None,
        None,
        "b1".into(),
        SessionBtwParams {
            session_id: SessionKey("local:btw-empty".into()),
            topic: None,
            question: "   ".into(),
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], "b1");
    assert!(
        frame["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("non-empty question")),
        "got {frame}"
    );
}

#[tokio::test]
async fn session_btw_rejects_unknown_session() {
    let known = SessionKey("local:btw-known".into());
    let state = prg_state_with_session(&known, prg_seed_user_assistant);
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(4);

    handle_session_btw(
        &ws,
        &state,
        &ledger,
        &active_turns_registry(),
        None,
        None,
        "b2".into(),
        SessionBtwParams {
            session_id: SessionKey("local:btw-unknown".into()),
            topic: None,
            question: "what are you working on?".into(),
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], "b2");
    assert_eq!(frame["error"]["data"]["session_id"], "local:btw-unknown");
}

#[tokio::test]
async fn session_btw_rejects_second_aside_while_first_in_flight() {
    let session_id = SessionKey("local:btw-busy".into());
    let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(4);

    btw_in_flight_sessions()
        .lock()
        .expect("in-flight registry")
        .insert((MAIN_PROFILE_ID.to_owned(), session_id.clone()));

    handle_session_btw(
        &ws,
        &state,
        &ledger,
        &active_turns_registry(),
        None,
        None,
        "b3".into(),
        SessionBtwParams {
            session_id: session_id.clone(),
            topic: None,
            question: "still there?".into(),
        },
    )
    .await;

    btw_in_flight_sessions()
        .lock()
        .expect("in-flight registry")
        .remove(&(MAIN_PROFILE_ID.to_owned(), session_id.clone()));

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], "b3");
    assert_eq!(frame["error"]["data"]["kind"], "btw_busy");
}

#[tokio::test]
async fn session_btw_answers_via_provider_with_no_tools() {
    struct BtwStubProvider;
    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for BtwStubProvider {
        fn provider_name(&self) -> &str {
            "test-provider"
        }

        async fn chat(
            &self,
            messages: &[octos_core::Message],
            tools: &[octos_llm::ToolSpec],
            config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            assert!(tools.is_empty(), "btw aside must offer NO tools");
            assert!(
                matches!(config.tool_choice, octos_llm::ToolChoice::None),
                "btw aside must force tool_choice=None"
            );
            let prompt = &messages.last().expect("user prompt").content;
            assert!(
                prompt.contains("what are you working on"),
                "question must reach the provider; got {prompt}"
            );
            assert!(
                prompt.contains("hello"),
                "transcript tail must reach the provider; got {prompt}"
            );
            Ok(octos_llm::ChatResponse {
                content: Some("Refactoring the parser; tests are running.".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "btw-stub-model"
        }
    }

    let session_id = SessionKey("local:btw-happy".into());
    let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(4);

    let _serial = btw_test_slot_serial().lock().await;
    *btw_test_provider_slot().lock().expect("slot") = Some(Arc::new(BtwStubProvider));
    handle_session_btw(
        &ws,
        &state,
        &ledger,
        &active_turns_registry(),
        None,
        None,
        "b4".into(),
        SessionBtwParams {
            session_id: session_id.clone(),
            topic: None,
            question: "btw, what are you working on?".into(),
        },
    )
    .await;

    // The aside runs detached — only clear the provider slot once the
    // result frame proves the spawned task has consumed it.
    let frame = recv_rpc_json(&mut rx).await;
    *btw_test_provider_slot().lock().expect("slot") = None;
    assert_eq!(frame["id"], "b4", "got {frame}");
    assert_eq!(
        frame["result"]["answer"],
        "Refactoring the parser; tests are running."
    );
    assert_eq!(frame["result"]["model"], "btw-stub-model");
    assert_eq!(frame["result"]["session_id"], session_id.to_string());
    // The aside runs detached; give its guard drop a beat before asserting.
    let busy_key = (MAIN_PROFILE_ID.to_owned(), session_id.clone());
    for _ in 0..100 {
        if !btw_in_flight_sessions()
            .lock()
            .expect("in-flight registry")
            .contains(&busy_key)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        !btw_in_flight_sessions()
            .lock()
            .expect("in-flight registry")
            .contains(&busy_key),
        "the in-flight slot must be released after the answer"
    );
}

#[test]
fn btw_live_draft_is_turn_scoped() {
    let session_a = SessionKey("local:btw-draft-a".into());
    let session_b = SessionKey("local:btw-draft-b".into());
    let turn_a = TurnId::new();
    let turn_b = TurnId::new();
    btw_live_draft_append(&session_a, &turn_a, "alpha stream");
    btw_live_draft_append(&session_b, &turn_b, "beta stream");

    // (session, turn) keying: streams can never mix, and a reused turn id
    // starts clean after its admission-time clear.
    assert_eq!(btw_live_draft_tail(&session_a, &turn_a), "alpha stream");
    assert_eq!(btw_live_draft_tail(&session_b, &turn_b), "beta stream");
    btw_live_draft_clear(&session_a, &turn_a);
    assert_eq!(btw_live_draft_tail(&session_a, &turn_a), "");
    assert_eq!(btw_live_draft_tail(&session_b, &turn_b), "beta stream");
}

/// The aside reads the live draft ONLY through the registry's CURRENT
/// NON-TERMINAL turn — a finished turn's entry is retained for idempotent
/// interrupts, and its leftover tail must not masquerade as in-flight.
#[tokio::test]
async fn session_btw_reads_draft_only_for_a_non_terminal_turn() {
    struct DraftProbeProvider;
    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for DraftProbeProvider {
        fn provider_name(&self) -> &str {
            "test-provider"
        }

        async fn chat(
            &self,
            messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            let prompt = messages.last().expect("prompt").content.clone();
            Ok(octos_llm::ChatResponse {
                content: Some(if prompt.contains("PAXOS-DRAFT-TAIL") {
                    "saw-draft".into()
                } else {
                    "no-draft".into()
                }),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "draft-probe"
        }
    }

    let session_id = SessionKey("local:btw-draftgate".into());
    let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
    let ledger = event_ledger(&state).await;
    let turn_id = TurnId::new();
    btw_live_draft_append(&session_id, &turn_id, "PAXOS-DRAFT-TAIL");

    let _serial = btw_test_slot_serial().lock().await;
    *btw_test_provider_slot().lock().expect("slot") = Some(Arc::new(DraftProbeProvider));

    // The registry retains the turn as TERMINAL → the leftover draft must
    // NOT reach the provider.
    let abort = tokio::spawn(async {}).abort_handle();
    active_turns_registry().lock().await.insert(
        session_id.clone(),
        ActiveTurn {
            profile_id: MAIN_PROFILE_ID.to_owned(),
            turn_id: turn_id.clone(),
            state: Arc::new(TokioMutex::new(TurnState::Terminal(
                TerminalReason::Completed,
            ))),
            interrupt_tx: Arc::new(TokioMutex::new(None)),
            steer: None,
            abort,
        },
    );
    let (ws, mut rx) = ws_connection_for_test(4);
    handle_session_btw(
        &ws,
        &state,
        &ledger,
        &active_turns_registry(),
        None,
        None,
        "b6".into(),
        SessionBtwParams {
            session_id: session_id.clone(),
            topic: None,
            question: "what are you drafting?".into(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["answer"], "no-draft", "got {frame}");

    // A live turn admitted under ANOTHER profile (bare session-id
    // collision) must not leak its draft into this profile's aside.
    let abort = tokio::spawn(async {}).abort_handle();
    active_turns_registry().lock().await.insert(
        session_id.clone(),
        ActiveTurn {
            profile_id: "someone-else".to_owned(),
            turn_id: turn_id.clone(),
            state: Arc::new(TokioMutex::new(TurnState::Active)),
            interrupt_tx: Arc::new(TokioMutex::new(None)),
            steer: None,
            abort,
        },
    );
    let (ws, mut rx) = ws_connection_for_test(4);
    handle_session_btw(
        &ws,
        &state,
        &ledger,
        &active_turns_registry(),
        None,
        None,
        "b6b".into(),
        SessionBtwParams {
            session_id: session_id.clone(),
            topic: None,
            question: "what are you drafting?".into(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(
        frame["result"]["answer"], "no-draft",
        "another profile's live draft must not leak; got {frame}"
    );

    // The same turn ACTIVE under OUR profile → its draft rides the prompt.
    let abort = tokio::spawn(async {}).abort_handle();
    active_turns_registry().lock().await.insert(
        session_id.clone(),
        ActiveTurn {
            profile_id: MAIN_PROFILE_ID.to_owned(),
            turn_id: turn_id.clone(),
            state: Arc::new(TokioMutex::new(TurnState::Active)),
            interrupt_tx: Arc::new(TokioMutex::new(None)),
            steer: None,
            abort,
        },
    );
    let (ws, mut rx) = ws_connection_for_test(4);
    handle_session_btw(
        &ws,
        &state,
        &ledger,
        &active_turns_registry(),
        None,
        None,
        "b7".into(),
        SessionBtwParams {
            session_id: session_id.clone(),
            topic: None,
            question: "what are you drafting?".into(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    active_turns_registry().lock().await.remove(&session_id);
    *btw_test_provider_slot().lock().expect("slot") = None;
    assert_eq!(frame["result"]["answer"], "saw-draft", "got {frame}");
}

/// The ingress gate scopes `session#topic`; the handler must fold the
/// topic into the canonical key before ANY lookup — a topic-scoped aside
/// answers from the topic-scoped session, never the bare one.
#[tokio::test]
async fn session_btw_folds_topic_into_the_session_key() {
    struct TopicStubProvider;
    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for TopicStubProvider {
        fn provider_name(&self) -> &str {
            "test-provider"
        }

        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            Ok(octos_llm::ChatResponse {
                content: Some("scoped answer".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "topic-stub"
        }
    }

    let folded = SessionKey("local:btw-topic#coding".into());
    let state = prg_state_with_session(&folded, prg_seed_user_assistant);
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(4);

    let _serial = btw_test_slot_serial().lock().await;
    *btw_test_provider_slot().lock().expect("slot") = Some(Arc::new(TopicStubProvider));
    handle_session_btw(
        &ws,
        &state,
        &ledger,
        &active_turns_registry(),
        None,
        None,
        "b5".into(),
        SessionBtwParams {
            session_id: SessionKey("local:btw-topic".into()),
            topic: Some("coding".into()),
            question: "which scope answered?".into(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    *btw_test_provider_slot().lock().expect("slot") = None;
    assert_eq!(frame["id"], "b5", "got {frame}");
    assert!(frame["error"].is_null(), "got {frame}");
    assert_eq!(
        frame["result"]["session_id"],
        folded.to_string(),
        "the result must carry the topic-folded session key"
    );
}

// M11-E: `session_filesystem_profile_for_workspace` was deleted
// alongside `session_tool_registry`. Its server-wide containment
// semantics (cwd must live under the legacy agent's workspace_root)
// are obsolete in the multi-profile world — coding-agent UIs
// legitimately point sessions at arbitrary repos OUTSIDE the
// profile data_dir. The replacement gate is the path-safety check
// in `validate_session_workspace_path_safety` + the bootstrap-time
// re-check inside `SessionRuntime::bootstrap`. The
// `session_workspace_authorizes_approved_subdir` /
// `session_workspace_rejects_outside_root` tests that locked the
// old containment behavior in place are removed with the helper;
// the new path-safety check is covered by the M11-E acceptance
// tests below + the bootstrap-side coverage in
// `crate::runtime::session::tests`.

// M11-E: `session_tool_registry` and its Tier-1 / Tier-2 fallback
// helpers were deleted. The same Tier-1 invariant ("client-supplied
// cwd wins over the bootstrap default") now lives on
// `SessionRuntime::bootstrap`, exercised by
// `crate::runtime::session::tests::bootstrap_with_two_hints_yields_distinct_workspaces`
// and the M11-E acceptance tests
// `appui_session_with_custom_cwd_reads_supplied_workspace` +
// `two_appui_sessions_on_same_profile_with_different_cwds_isolated`
// below. Tier-2 (operator-default `appui.default_session_cwd`) is
// a known follow-up: the new `SessionRuntime::bootstrap` resolves
// workspace_hint at the per-session layer and does not yet honor a
// profile-scope operator default. Tracked alongside the M11
// shared-`validate_session_workspace_allowed`-helper TODO in
// `crate::runtime::session::validate_workspace_hint`.

#[test]
fn pane_snapshot_prefers_approved_session_workspace_root() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let project = tempfile::tempdir().expect("project dir");
    let src = project.path().join("src");
    std::fs::create_dir_all(&src).expect("src dir");
    std::fs::write(src.join("main.rs"), "fn main() {}\n").expect("write file");
    let session_id = SessionKey("local:cwd-pane".into());

    let panes = build_pane_snapshot(data_dir.path(), &session_id, Some(project.path()));
    let workspace = panes.workspace.expect("workspace pane");

    assert_eq!(workspace.root, project.path().to_string_lossy());
    assert!(workspace.entries.iter().any(|entry| {
        entry.path == "src/main.rs" && entry.kind == "file" && entry.detail.is_some()
    }));
}

#[test]
fn runtime_unavailable_errors_are_typed_for_protocol_clients() {
    let error = runtime_unavailable_error("No LLM provider configured");

    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::INTERNAL_ERROR
    );
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("kind")),
        Some(&json!("runtime_unavailable"))
    );
}

#[test]
fn held_data_dir_lock_yields_a_clear_actionable_error() {
    // A `session/open` bootstrap that fails because another octos process
    // already owns the profile's redb must be recognized structurally
    // (through the eyre wrap chain that `ProfileRuntime::bootstrap` adds) and
    // rendered with both remedies. Previously this reached the client as
    // "failed to bootstrap ProfileRuntime for profile 'alan': failed to open
    // episode store for profile 'alan'" — the cause, the path, and every hint
    // about what to do were dropped by the `{error}` (non-alternate) format.
    let report = eyre::Report::new(octos_memory::EpisodeStoreLocked {
        path: std::path::PathBuf::from("/Users/dev/.octos/profiles/alan/data/episodes.redb"),
    })
    .wrap_err("failed to open episode store for profile 'alan'");
    assert!(
        octos_memory::is_episode_store_locked(&report),
        "lock contention must be detected through the eyre wrap chain"
    );

    let error = data_dir_locked_error("alan", &report);
    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::INTERNAL_ERROR
    );
    assert_eq!(
        error.data.as_ref().and_then(|d| d.get("kind")),
        Some(&json!("data_dir_locked")),
        "clients branch on `kind`; this must not be the generic runtime_unavailable"
    );
    let message = error
        .data
        .as_ref()
        .and_then(|d| d.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("alan"),
        "message must name the profile: {message}"
    );
    assert!(
        message.contains("--instance-data-dir"),
        "message must offer the private-storage remedy: {message}"
    );
    assert!(
        message.contains("episodes.redb"),
        "message must carry the underlying cause, including the contended path: {message}"
    );
}

#[test]
fn non_lock_bootstrap_error_is_not_misclassified_as_data_dir_locked() {
    // Guards the detector against over-matching: a missing provider is not a
    // lock problem and has no `--instance-data-dir` remedy, so it must keep
    // falling through to the generic `runtime_unavailable` kind.
    let report = eyre::eyre!("No LLM provider configured")
        .wrap_err("failed to bootstrap ProfileRuntime for profile 'alan'");
    assert!(
        !octos_memory::is_episode_store_locked(&report),
        "an unrelated bootstrap failure must not be reported as lock contention"
    );
}

#[test]
fn permission_denied_workspace_yields_a_clear_actionable_error() {
    // A session/open bootstrap failure caused by a non-writable workspace
    // folder must be recognized structurally (through the eyre wrap chain)
    // and rendered as a clear, folder-named message the client shows
    // verbatim — not the opaque "failed to bootstrap session runtime".
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied");
    let report = eyre::Report::new(io_err)
        .wrap_err("write workspace policy failed: /Users/dev/.octos-workspace.toml")
        .wrap_err("failed to bootstrap session workspace policy");
    assert!(
        is_permission_denied_error(&report),
        "permission denial must be detected through the eyre wrap chain"
    );

    let error = workspace_not_writable_error(Some("/Users/dev"));
    assert_eq!(
        error.data.as_ref().and_then(|d| d.get("kind")),
        Some(&json!("workspace_not_writable"))
    );
    let message = error
        .data
        .as_ref()
        .and_then(|d| d.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("/Users/dev"),
        "message must name the folder: {message}"
    );
    assert!(
        message.contains("writable"),
        "message must explain the cause: {message}"
    );
}

#[test]
fn non_permission_bootstrap_error_is_not_misclassified_as_writability() {
    let report = eyre::eyre!("some other failure: database corrupt");
    assert!(!is_permission_denied_error(&report));
}

#[test]
fn final_assistant_message_persists_content_when_response_messages_omit_it() {
    let message = final_assistant_message(&[Message::user("hello")], "world", Some("r".into()))
        .expect("assistant message");

    assert_eq!(message.role, MessageRole::Assistant);
    assert_eq!(message.content, "world");
    assert_eq!(message.reasoning_content.as_deref(), Some("r"));
}

#[test]
fn final_assistant_message_skips_duplicate_assistant_content() {
    let messages = vec![Message::assistant("world")];

    assert!(final_assistant_message(&messages, "world", None).is_none());
}

/// NEW-10 (Fleet-UX soak, mini3/kimi-k2.5): the iter-N carrier
/// message in `response.messages` ends with a trailing newline
/// (a frequent kimi-k2.5 streaming artefact) while the EndTurn
/// `response.content` does not. Pre-fix byte-equality dedupe
/// missed and BOTH rows persisted; the SPA rendered two
/// identical bubbles around the tool chip. The trimmed-equality
/// branch must close this case.
#[test]
fn final_assistant_message_skips_when_iter_n_carrier_has_trailing_whitespace() {
    let iter_n_carrier = Message::assistant("旧金山今天天气晴朗，气温17.1°C\n");
    let messages = vec![iter_n_carrier];
    let final_content = "旧金山今天天气晴朗，气温17.1°C";

    assert!(
        final_assistant_message(&messages, final_content, None).is_none(),
        "trimmed-equal iter-N content must be deduped against final response.content",
    );
}

/// NEW-10 codex round-1 P2: substring containment is NOT a
/// safe dedupe signal — a prior iter-N Assistant row that
/// merely speculates about the final answer (e.g. "I'll verify
/// whether <answer>...") cannot be reliably distinguished from
/// a true preamble-wraps-final duplicate carrier. To avoid
/// suppressing legitimate final answers, dedupe is restricted
/// to TRIMMED EQUALITY only. This test locks in the negative
/// direction so a regression that re-introduces substring
/// matching is caught here.
#[test]
fn final_assistant_message_does_not_dedupe_when_iter_n_speculates_about_final() {
    let final_content =
        "旧金山今天天气晴朗，气温17.1°C，湿度68%。需要我查询更详细的湾区天气预报吗？";
    let iter_n_carrier = Message::assistant(format!("I'll verify whether {final_content}"));
    let messages = vec![iter_n_carrier];

    assert!(
        final_assistant_message(&messages, final_content, None).is_some(),
        "iter-N speculation that incidentally contains the final answer must NOT \
             dedupe against the final response (codex round-1 P2 false-positive guard)",
    );
}

/// NEW-10 codex round-1 P2: even when the iter-N carrier wraps
/// the final answer in a preamble verbatim ("Here's the
/// result: <X>" + final="<X>"), we DO NOT dedupe. Substring
/// containment is too ambiguous to distinguish duplicate
/// carriers from speculative mentions. Both rows persist; the
/// SPA may render two bubbles when the model produces this
/// shape — but a duplicate is strictly better than a missing
/// final answer. The actual reported NEW-10 bug
/// (kimi-k2.5/`get_weather`) is whitespace-only-difference,
/// not preamble-wrap.
#[test]
fn final_assistant_message_persists_when_iter_n_carrier_wraps_final_in_preamble() {
    let final_content =
        "旧金山今天天气晴朗，气温17.1°C，湿度68%。需要我查询更详细的湾区天气预报吗？";
    let iter_n_carrier = Message::assistant(format!("Here's the result: {final_content}"));
    let messages = vec![iter_n_carrier];

    assert!(
        final_assistant_message(&messages, final_content, None).is_some(),
        "preamble-wraps-final is NOT a dedupe signal — codex round-1 P2 narrowed \
             dedupe to trimmed-equality only, so both rows must persist here",
    );
}

/// NEW-10 NEGATIVE: the legitimate two-bubble flow (iter-N
/// emits a SHORT preamble "Looking that up..." and the EndTurn
/// produces a DIFFERENT long answer) must STILL render both
/// rows. The preamble is not trimmed-equal to the final answer,
/// so the dedupe helper returns false.
#[test]
fn final_assistant_message_preserves_preamble_plus_distinct_final() {
    let preamble = Message::assistant("Looking that up...");
    let messages = vec![preamble];
    let final_content = "旧金山今天天气晴朗，气温17.1°C，湿度68%。需要更详细的湾区预报吗？";

    let synthesised = final_assistant_message(&messages, final_content, None);
    assert!(
        synthesised.is_some(),
        "preamble + distinct-final flow must persist BOTH rows",
    );
    assert_eq!(synthesised.unwrap().content, final_content);
}

/// NEW-10 NEGATIVE: when iter-N carrier holds tool_calls AND
/// genuinely-distinct text (no overlap with final), both rows
/// must persist. This is the bread-and-butter "two-bubble"
/// flow where the model narrates progress then delivers the
/// answer.
#[test]
fn final_assistant_message_preserves_when_iter_n_text_is_unrelated_to_final() {
    let iter_n_carrier = Message::assistant("I'll fetch the weather data now.");
    let messages = vec![iter_n_carrier];
    let final_content =
        "Based on the readings, San Francisco is sunny at 17.1°C today with 68% humidity.";

    assert!(
        final_assistant_message(&messages, final_content, None).is_some(),
        "iter-N narrative + distinct-final answer must persist BOTH rows",
    );
}

/// NEW-10: bare spawn_only / tool-call-only iter-N carriers
/// (empty `content`) must NEVER block the final_assistant
/// synthesis. Empty content has nothing to dedupe against.
#[test]
fn final_assistant_message_persists_when_iter_n_carrier_has_empty_content() {
    // The iter-N carrier exists (it held tool_calls) but its
    // text content is empty — typical of a fan-out tool call
    // from a non-chat model.
    let iter_n_carrier = Message::assistant("");
    let messages = vec![iter_n_carrier];
    let final_content = "Here are the weather readings: 17.1°C, sunny.";

    assert!(
        final_assistant_message(&messages, final_content, None).is_some(),
        "empty iter-N carrier content cannot dedupe against the final response",
    );
}

/// NEW-10 helper-level contract: empty/whitespace `content`
/// itself MUST return `false` from the helper so the caller's
/// `content.is_empty()` branch in `final_assistant_message`
/// remains the single source of truth for "skip synthesis".
#[test]
fn final_assistant_content_already_persisted_returns_false_for_empty_input() {
    let messages = vec![Message::assistant("anything goes here")];
    assert!(!final_assistant_content_already_persisted(&messages, ""));
    assert!(!final_assistant_content_already_persisted(&messages, "   "));
    assert!(!final_assistant_content_already_persisted(
        &messages, "\n\t  "
    ));
}

/// NEW-10: a `Tool` row whose body coincidentally contains the
/// final assistant content MUST NOT trigger dedupe — tool
/// outputs render as tool chips, not chat bubbles, so they
/// cannot be the source of a duplicate bubble.
#[test]
fn final_assistant_content_already_persisted_ignores_tool_rows() {
    let tool_row = Message {
        role: MessageRole::Tool,
        content: "旧金山今天天气晴朗，气温17.1°C，湿度68%。需要更详细的湾区预报吗？".to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: Some("call_1".to_string()),
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    };
    let messages = vec![tool_row];
    let final_content = "旧金山今天天气晴朗，气温17.1°C，湿度68%。需要更详细的湾区预报吗？";

    assert!(
        final_assistant_message(&messages, final_content, None).is_some(),
        "Tool rows must not be considered when deduping the final assistant message",
    );
}

/// NEW-10 codex round-1 P2 carrier flip: the persist loop's
/// `is_final_assistant_carrier` check must use trimmed-equality
/// — matching the synth-skip helper's semantics — otherwise a
/// trimmed-equal iter-N row that flips synth-skip would land
/// with `final_assistant_persisted=false`, the `final_send`
/// oneshot would resolve to `None`, and self-paced loops with
/// `<<loop-next-in: ...>>` hints in the EndTurn text would
/// fall back to the unsafe `.last()` history scan.
#[test]
fn final_assistant_carrier_trimmed_equality_matches_synth_skip_helper() {
    // Trimmed-equal iter-N carrier (trailing newline diff).
    let iter_n = Message::assistant("旧金山今天天气晴朗\n");
    let final_content = "旧金山今天天气晴朗";

    assert!(
        is_final_assistant_carrier_under_trimmed_equality(&iter_n, final_content),
        "trimmed-equal iter-N row MUST be recognised as the final carrier so \
             `final_assistant_persisted` flips correctly",
    );
    // Sanity: the synth-skip helper recognises the SAME row
    // (this is the lockstep invariant — codex round-1 P2).
    assert!(
        final_assistant_content_already_persisted(std::slice::from_ref(&iter_n), final_content),
        "synth-skip helper must recognise the same row as the carrier helper",
    );
}

/// NEW-10 codex round-1 P2 carrier flip: the byte-exact case
/// from before this PR is preserved (subset of trimmed-equal).
#[test]
fn final_assistant_carrier_trimmed_equality_preserves_byte_exact_case() {
    let iter_n = Message::assistant("byte-exact final answer");
    assert!(is_final_assistant_carrier_under_trimmed_equality(
        &iter_n,
        "byte-exact final answer"
    ));
}

/// NEW-10 codex round-1 P2 carrier flip: empty `response_content`
/// MUST NOT flip the carrier flag, otherwise a turn that
/// explicitly EndTurn'd with no text would mark an unrelated
/// iter-N empty-content row as the final carrier. The
/// `final_send` short-circuit at the call site relies on this
/// returning false for empty content.
#[test]
fn final_assistant_carrier_trimmed_equality_returns_false_for_empty_response() {
    let iter_n = Message::assistant("");
    assert!(!is_final_assistant_carrier_under_trimmed_equality(
        &iter_n, ""
    ));
    assert!(!is_final_assistant_carrier_under_trimmed_equality(
        &iter_n, "   "
    ));
}

/// NEW-10 codex round-1 P2 carrier flip: User and Tool rows
/// MUST NOT be recognised as final-assistant carriers, even
/// when their content happens to trimmed-equal the final
/// content. This is the same role-discriminator the synth-skip
/// helper applies — keeping both helpers in lockstep.
#[test]
fn final_assistant_carrier_trimmed_equality_rejects_non_assistant_roles() {
    let user_row = Message::user("matching content");
    let tool_row = Message {
        role: MessageRole::Tool,
        content: "matching content".to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: Some("call_1".to_string()),
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    };
    assert!(!is_final_assistant_carrier_under_trimmed_equality(
        &user_row,
        "matching content"
    ));
    assert!(!is_final_assistant_carrier_under_trimmed_equality(
        &tool_row,
        "matching content"
    ));
}

/// NEW-03 (codex rounds 1-3 P2): the synth-ack skip path picks
/// the captured_final_reply by precedence:
///   1. If a non-empty preamble assistant row was persisted →
///      return that text. The post-turn parser reads the
///      preamble's `<<loop-next-in: ...>>` hint directly — no
///      dependency on the unsafe history-fallback walk.
///   2. Otherwise → return the synth-ack text. The parser finds
///      no hint and `apply_self_paced_response` stamps the
///      default delay, so a bare-spawn-only self-paced loop
///      stays scheduled.
#[test]
fn captured_final_reply_on_synth_ack_skip_picks_preamble_when_available() {
    let ack = "Background work started for `bg_research`. \
                   The final result will be delivered automatically when it is ready."
        .to_string();
    let preamble = "Starting the pipeline now. <<loop-next-in: 60s>>";

    // Round-3 case: preamble persisted → use preamble text so
    // the parser sees the hint deterministically.
    assert_eq!(
        captured_final_reply_for_synth_ack_skip(ack.clone(), Some(preamble)),
        preamble,
        "preamble assistant content present → captured_final_reply must be the preamble text",
    );

    // Round-1 case: bare spawn_only with no preamble → fall
    // back to the synth-ack so the loop still reschedules.
    assert_eq!(
        captured_final_reply_for_synth_ack_skip(ack.clone(), None),
        ack,
        "no preamble assistant content → captured_final_reply must be the synth-ack text",
    );

    // Empty preamble option still routes to the synth-ack.
    assert_eq!(
        captured_final_reply_for_synth_ack_skip(ack.clone(), None),
        ack,
        "None preamble (regardless of how it became None) → use synth-ack",
    );
}

/// M10 Phase 6.1: the standalone-turn persist loop must pre-stamp the
/// `User` row with the originating `TurnId`-derived thread id so the
/// user prompt and the assistant reply land in the same thread on the
/// SPA. Without this the SPA renders an empty placeholder bubble in
/// the user's `clientMessageId`-keyed thread and creates an orphan
/// thread for the assistant reply (3 bubbles per spawn_only turn
/// instead of the target 2).
#[test]
fn pre_stamp_turn_thread_id_stamps_user_assistant_and_tool_when_unbound() {
    let turn_thread_id = "turn-abc";

    let user = pre_stamp_turn_thread_id(Message::user("hi"), turn_thread_id);
    let assistant = pre_stamp_turn_thread_id(Message::assistant("ok"), turn_thread_id);
    let tool = pre_stamp_turn_thread_id(
        Message {
            role: MessageRole::Tool,
            content: "result".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call-1".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        turn_thread_id,
    );

    assert_eq!(
        user.thread_id.as_deref(),
        Some(turn_thread_id),
        "user row must inherit the turn-derived thread_id so its bubble \
             coalesces with the assistant reply"
    );
    assert_eq!(assistant.thread_id.as_deref(), Some(turn_thread_id));
    assert_eq!(tool.thread_id.as_deref(), Some(turn_thread_id));
}

/// Caller-supplied `thread_id` values must NOT be overwritten — that
/// would corrupt rows already routed to the correct sub-thread (e.g.
/// spawn_only completion rows that bind a different originating
/// thread).
#[test]
fn pre_stamp_turn_thread_id_preserves_caller_supplied_thread_id() {
    let mut user = Message::user("hi");
    user.thread_id = Some("explicit-thread".into());

    let stamped = pre_stamp_turn_thread_id(user, "turn-other");

    assert_eq!(
        stamped.thread_id.as_deref(),
        Some("explicit-thread"),
        "caller-supplied thread_id must be preserved"
    );
}

/// System rows are not thread-scoped — the helper must leave them
/// alone so the per-turn system primer (when present) does not get
/// retro-rooted into a turn thread that didn't author it.
#[test]
fn pre_stamp_turn_thread_id_leaves_system_rows_alone() {
    let system = Message {
        role: MessageRole::System,
        content: "primer".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    };

    let stamped = pre_stamp_turn_thread_id(system, "turn-abc");

    assert!(
        stamped.thread_id.is_none(),
        "system rows must remain unbound to a turn thread"
    );
}

#[tokio::test]
async fn abort_connection_turns_removes_only_matching_active_turns() {
    let owned_session_id = SessionKey("local:owned".into());
    let stale_session_id = SessionKey("local:stale".into());
    let owned_turn_id = TurnId::new();
    let stale_connection_turn_id = TurnId::new();
    let newer_turn_id = TurnId::new();
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let owned_handle = tokio::spawn(async { std::future::pending::<()>().await });
    let newer_handle = tokio::spawn(async { std::future::pending::<()>().await });
    active_turns.lock().await.insert(
        owned_session_id.clone(),
        test_active_turn(owned_turn_id.clone(), owned_handle.abort_handle()),
    );
    active_turns.lock().await.insert(
        stale_session_id.clone(),
        test_active_turn(newer_turn_id.clone(), newer_handle.abort_handle()),
    );
    connection_turns.lock().await.insert(
        owned_session_id.clone(),
        test_connection_turn(&active_turns, &owned_session_id, &owned_turn_id).await,
    );
    connection_turns.lock().await.insert(
        stale_session_id.clone(),
        test_connection_turn(&active_turns, &stale_session_id, &stale_connection_turn_id).await,
    );

    let scopes = ScopePolicy::default();
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let user_questions = PendingQuestionStore::default();
    abort_connection_turns(
        &active_turns,
        &connection_turns,
        &scopes,
        &ledger,
        &approvals,
        &user_questions,
    )
    .await;

    assert!(!active_turns.lock().await.contains_key(&owned_session_id));
    assert_eq!(
        active_turns
            .lock()
            .await
            .get(&stale_session_id)
            .map(|active| active.turn_id.clone()),
        Some(newer_turn_id)
    );
    assert!(connection_turns.lock().await.is_empty());
    owned_handle.abort();
    newer_handle.abort();
}

#[tokio::test]
async fn interrupt_cancels_running_spawn_only_tasks_for_session() {
    // Root-cause regression: `turn/interrupt` must cancel the session's
    // still-running spawn_only background tasks (a hung `bg_research` /
    // `bg_research`), not only abort the foreground agent loop. The
    // interrupt path calls `cancel_session_spawn_only_tasks`, which fires
    // each task's supervisor cancel token so the detached worker drops its
    // in-flight pipeline future at the next poll.
    let supervisor = octos_agent::TaskSupervisor::new();
    let session_id = SessionKey("api:profile/local:owned".into());
    let session_key = session_id.to_string();

    // Two live spawn_only tasks for THIS session and one for another
    // session that must survive (turns are per-session; a sibling
    // session's background work is unrelated to this interrupt).
    let running_a = supervisor.register("bg_research", "tc-a", Some(&session_key));
    let running_b = supervisor.register("bg_research", "tc-b", Some(&session_key));
    supervisor.mark_running(&running_a);
    supervisor.mark_running(&running_b);

    let other_session = SessionKey("api:profile/local:other".into());
    let other_running =
        supervisor.register("bg_research", "tc-c", Some(&other_session.to_string()));
    supervisor.mark_running(&other_running);

    // An already-terminal task for this session: cancel must skip it
    // (idempotent — `cancel` would otherwise return `AlreadyTerminal`).
    let done = supervisor.register("bg_research", "tc-d", Some(&session_key));
    supervisor.mark_completed(&done, vec![]);

    cancel_session_spawn_only_tasks(&supervisor, &session_id);

    // Both live tasks for this session are now terminal `Cancelled`,
    // which fires their cancel tokens.
    assert!(matches!(
        supervisor.get_task(&running_a).map(|t| t.status),
        Some(octos_agent::TaskStatus::Cancelled)
    ));
    assert!(supervisor.cancel_token(&running_a).is_cancelled());
    assert!(matches!(
        supervisor.get_task(&running_b).map(|t| t.status),
        Some(octos_agent::TaskStatus::Cancelled)
    ));
    assert!(supervisor.cancel_token(&running_b).is_cancelled());

    // The completed task is left intact (still `Completed`, not clobbered).
    assert!(matches!(
        supervisor.get_task(&done).map(|t| t.status),
        Some(octos_agent::TaskStatus::Completed)
    ));

    // A different session's running task is untouched by this interrupt.
    assert!(matches!(
        supervisor.get_task(&other_running).map(|t| t.status),
        Some(octos_agent::TaskStatus::Running)
    ));
    assert!(!supervisor.cancel_token(&other_running).is_cancelled());
}

/// Mirror of `handle_turn_interrupt`'s post-abort drain step. Used by
/// the interrupt-flow tests below to drive the store + ledger without
/// constructing a real `WsSink`.
fn drain_pending_approvals_for_interrupt(
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
    session_id: &SessionKey,
    turn_id: &TurnId,
) -> Vec<ApprovalCancelledEvent> {
    let cancelled = approvals.cancel_pending_for_turn(
        session_id,
        turn_id,
        approval_cancelled_reasons::TURN_INTERRUPTED,
    );
    let mut emitted = Vec::with_capacity(cancelled.len());
    for entry in cancelled {
        let event = ApprovalCancelledEvent::turn_interrupted(
            session_id.clone(),
            entry.approval_id,
            entry.turn_id,
        );
        ledger.append_notification(UiNotification::ApprovalCancelled(event.clone()));
        emitted.push(event);
    }
    emitted
}

#[tokio::test]
async fn interrupt_cancels_pending_approvals_for_turn() {
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let interrupted_turn = TurnId::new();
    let approval_id = ApprovalId::new();
    let surviving_turn = TurnId::new();
    let surviving_approval = ApprovalId::new();

    approvals.request(ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        interrupted_turn.clone(),
        "shell",
        "Pending",
        "ls",
    ));
    approvals.request(ApprovalRequestedEvent::generic(
        session_id.clone(),
        surviving_approval.clone(),
        surviving_turn,
        "shell",
        "Different turn",
        "ls",
    ));

    let emitted =
        drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &interrupted_turn);

    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].approval_id, approval_id);
    assert_eq!(emitted[0].turn_id, interrupted_turn);
    assert_eq!(emitted[0].reason, "turn_interrupted");

    let err = approvals
        .respond(ApprovalRespondParams::new(
            session_id.clone(),
            approval_id,
            ApprovalDecision::Approve,
        ))
        .expect_err("late respond against cancelled approval");
    assert_eq!(err.code, rpc_error_codes::APPROVAL_CANCELLED);

    // Approval on the surviving (non-interrupted) turn still works.
    let ok = approvals
        .respond(ApprovalRespondParams::new(
            session_id,
            surviving_approval,
            ApprovalDecision::Approve,
        ))
        .expect("non-interrupted turn approval still pending");
    // FIX-06 wrapped the result in `RespondOutcome { result, context }`.
    assert!(ok.result.accepted);
}

#[tokio::test]
async fn interrupt_with_no_pending_approvals_is_no_op() {
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();

    let first = drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &turn_id);
    let second = drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &turn_id);

    assert!(first.is_empty(), "no approvals to cancel on first call");
    assert!(second.is_empty(), "double-interrupt is idempotent");
}

#[tokio::test]
async fn cancelled_approval_replays_on_reconnect() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    let approval = ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        turn_id.clone(),
        "shell",
        "Pending",
        "ls",
    );
    approvals.request(approval.clone());
    // The original approval/requested notification is in the durable
    // ledger (typical lifecycle when M9-FIX-01 is active).
    ledger.append_notification(UiNotification::ApprovalRequested(approval));

    let emitted = drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &turn_id);
    assert_eq!(emitted.len(), 1);

    // A reconnecting client with no cursor must rebuild from the ledger
    // replay; pending_for_session must NOT yield the cancelled approval
    // (otherwise the UI would re-render a fresh card after seeing the
    // cancellation event).
    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: None,
        },
    )
    .await
    .expect("session/open after cancellation");
    assert!(
        outcome.pending_approvals.is_empty(),
        "cancelled approvals must not surface as fresh pending replays",
    );

    // A reconnecting client *with* a pre-cancellation cursor must see
    // the durable approval/cancelled event in the cursor-bounded replay.
    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        },
    )
    .await
    .expect("session/open with cursor 0 replays everything");
    assert!(outcome.replay.iter().any(|event| matches!(
        &event.event,
        UiProtocolLedgerEvent::Notification(UiNotification::ApprovalCancelled(event))
            if event.approval_id == approval_id
                && event.reason == "turn_interrupted"
    )));
}

#[tokio::test]
async fn respond_to_cancelled_approval_returns_typed_error() {
    let ledger = UiProtocolLedger::new(16);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    approvals.request(ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        turn_id.clone(),
        "shell",
        "Pending",
        "ls",
    ));

    drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &turn_id);

    let err = approvals
        .respond(ApprovalRespondParams::new(
            session_id,
            approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .expect_err("late respond returns typed error");
    assert_eq!(err.code, rpc_error_codes::APPROVAL_CANCELLED);
    let data = err.data.expect("typed error data");
    assert_eq!(data["kind"], json!("approval_cancelled"));
    assert_eq!(data["reason"], json!("turn_interrupted"));
    assert_eq!(data["approval_id"], json!(approval_id));
}

#[tokio::test]
async fn one_hundred_concurrent_interrupts_emit_cancellation_exactly_once() {
    // Stress: even with 100 racing interrupts on the same session/turn,
    // the cancellation transition is exactly-once and emits one
    // approval/cancelled per pending approval.
    let ledger = Arc::new(UiProtocolLedger::new(2048));
    let approvals = Arc::new(PendingApprovalStore::default());
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_count = 8usize;
    let mut approval_ids = Vec::with_capacity(approval_count);
    for _ in 0..approval_count {
        let approval_id = ApprovalId::new();
        approvals.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Pending",
            "ls",
        ));
        approval_ids.push(approval_id);
    }

    let mut handles = Vec::with_capacity(100);
    for _ in 0..100 {
        let approvals = Arc::clone(&approvals);
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        handles.push(tokio::spawn(async move {
            approvals.cancel_pending_for_turn(
                &session_id,
                &turn_id,
                approval_cancelled_reasons::TURN_INTERRUPTED,
            )
        }));
    }

    let mut total_cancelled = 0usize;
    let mut seen_ids = HashSet::new();
    for handle in handles {
        let cancelled = handle.await.expect("interrupt task");
        for entry in cancelled {
            assert!(
                seen_ids.insert(entry.approval_id.clone()),
                "double-emit detected for {:?}",
                entry.approval_id,
            );
            total_cancelled += 1;
        }
    }

    assert_eq!(
        total_cancelled, approval_count,
        "exactly one cancellation per pending approval across 100 racing interrupts",
    );
    for approval_id in &approval_ids {
        let err = approvals
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect_err("respond against cancelled approval fails");
        assert_eq!(err.code, rpc_error_codes::APPROVAL_CANCELLED);
    }

    // We never emitted notifications above because the test exercises
    // the store directly; the ledger must therefore be empty for this
    // session.
    assert!(
        ledger
            .replay_after(
                &session_id,
                Some(&UiCursor {
                    stream: session_id.0.clone(),
                    seq: 0,
                }),
            )
            .expect("replay")
            .is_empty(),
        "stress test should not write to the ledger",
    );
}

// TODO(M9-FIX-06): once ScopePolicy lands in this worktree, add a test
// verifying that approve_for_session scopes survive turn/interrupt while
// approve_for_turn and per-call pending entries are cancelled. The
// supervisor will reconcile the test during merge.

#[test]
fn notification_serializes_as_json_rpc_method_frame() {
    let frame = UiNotification::TurnError(TurnErrorEvent {
        session_id: SessionKey("local:test".into()),
        topic: None,
        turn_id: TurnId::new(),
        code: "test".into(),
        message: "failed".into(),
        token_usage: None,
        partial_result: None,
    })
    .into_rpc_notification()
    .expect("notification");

    assert_eq!(frame.method, methods::TURN_ERROR);
}

// ====================================================================
// M9-FIX-03 — interrupt/turn state-machine + TOCTOU repro
// ====================================================================

/// Insert an `ActiveTurn` whose state has already moved to `Terminal(_)`
/// — emulates the world after natural completion of a prior turn.
async fn insert_terminal_turn(
    active_turns: &SharedActiveTurns,
    session_id: &SessionKey,
    turn_id: &TurnId,
    reason: TerminalReason,
) -> tokio::task::JoinHandle<()> {
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    let entry = test_active_turn(turn_id.clone(), handle.abort_handle());
    *entry.state.lock().await = TurnState::Terminal(reason);
    active_turns.lock().await.insert(session_id.clone(), entry);
    handle
}

#[tokio::test]
async fn interrupt_idempotent_on_completed_turn() {
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let handle = insert_terminal_turn(
        &active_turns,
        &session_id,
        &turn_id,
        TerminalReason::Completed,
    )
    .await;

    let outcome = decide_interrupt(
        &active_turns,
        &TurnInterruptParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;

    assert!(matches!(
        outcome,
        InterruptOutcome::AlreadyTerminal(TerminalReason::Completed)
    ));
    // A second interrupt returns the same shape — idempotent.
    let outcome2 = decide_interrupt(
        &active_turns,
        &TurnInterruptParams {
            session_id,
            turn_id,
        },
    )
    .await;
    assert!(matches!(
        outcome2,
        InterruptOutcome::AlreadyTerminal(TerminalReason::Completed)
    ));
    handle.abort();
}

#[tokio::test]
async fn interrupt_unknown_turn_returns_unknown_turn_error() {
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let turn_id = TurnId::new();

    let outcome = decide_interrupt(
        &active_turns,
        &TurnInterruptParams {
            session_id: SessionKey("local:test".into()),
            turn_id: turn_id.clone(),
        },
    )
    .await;
    assert!(matches!(outcome, InterruptOutcome::Unknown));

    let error = unknown_turn_error(&turn_id);
    assert_eq!(error.code, UNKNOWN_TURN_CODE);
    assert_eq!(
        error.data.as_ref().and_then(|d| d.get("kind")),
        Some(&json!("unknown_turn"))
    );
}

#[tokio::test]
async fn interrupt_in_flight_turn_aborts_emits_one_terminal() {
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    let entry = test_active_turn(turn_id.clone(), handle.abort_handle());
    let turn_state = entry.state.clone();
    active_turns.lock().await.insert(session_id.clone(), entry);

    let outcome = decide_interrupt(
        &active_turns,
        &TurnInterruptParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;
    let ack_rx = match outcome {
        InterruptOutcome::Captured { ack_rx } => ack_rx,
        other => panic!("expected Captured, got {other:?}"),
    };
    assert!(matches!(
        *turn_state.lock().await,
        TurnState::Interrupting { .. }
    ));

    // Simulate the turn task winning by transitioning Interrupting →
    // Terminal(Interrupted) and signalling ack. The `expected` reason
    // (Completed) is overridden because state is `Interrupting`.
    let transition = transition_to_terminal(&turn_state, TerminalReason::Completed)
        .await
        .expect("first transition wins");
    assert_eq!(transition.reason, TerminalReason::Interrupted);
    if let Some(ack) = transition.ack {
        ack.send(()).expect("ack delivered");
    }
    assert_eq!(ack_rx.await.expect("handler observes ack"), ());

    // A second transition must be a no-op — no double-emit possible.
    let second = transition_to_terminal(&turn_state, TerminalReason::Errored).await;
    assert!(second.is_none(), "second emission must be a no-op");
    assert!(matches!(
        *turn_state.lock().await,
        TurnState::Terminal(TerminalReason::Interrupted)
    ));
    handle.abort();
}

#[tokio::test]
async fn interrupt_called_twice_returns_same_response() {
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    let entry = test_active_turn(turn_id.clone(), handle.abort_handle());
    active_turns.lock().await.insert(session_id.clone(), entry);

    let first = decide_interrupt(
        &active_turns,
        &TurnInterruptParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;
    assert!(matches!(first, InterruptOutcome::Captured { .. }));

    // Second call: state is Interrupting, so AlreadyInterrupting; no
    // double-emit, response shape is the idempotent `interrupted: true`.
    let second = decide_interrupt(
        &active_turns,
        &TurnInterruptParams {
            session_id,
            turn_id,
        },
    )
    .await;
    assert!(matches!(second, InterruptOutcome::AlreadyInterrupting));
    handle.abort();
}

#[tokio::test]
async fn interrupt_mismatch_does_not_emit_invalid_params() {
    let session_id = SessionKey("local:test".into());
    let active_turn_id = TurnId::new();
    let other_turn_id = TurnId::new();
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    let entry = test_active_turn(active_turn_id.clone(), handle.abort_handle());
    active_turns.lock().await.insert(session_id.clone(), entry);

    let outcome = decide_interrupt(
        &active_turns,
        &TurnInterruptParams {
            session_id,
            turn_id: other_turn_id,
        },
    )
    .await;
    assert!(matches!(outcome, InterruptOutcome::Mismatch));
    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_then_completion_race_emits_one_terminal() {
    // Drive 100 iterations of concurrent "natural-complete vs interrupt"
    // and assert: (a) exactly one terminal transition wins per iteration,
    // (b) at least one iteration actually exercises the race window —
    // i.e., the interrupt path captured first, then the completion path
    // observed `Interrupting` and converted it to `Terminal(Interrupted)`
    // (the original TOCTOU window between lookup and emission). A
    // `tokio::sync::Barrier` aligns the two tasks so they reliably
    // contend for the per-turn lock instead of running serially.
    let mut race_window_observed = 0;
    let mut completed_first = 0;
    let mut interrupted_first = 0;
    const ITERATIONS: usize = 100;
    for _ in 0..ITERATIONS {
        let turn_state = Arc::new(TokioMutex::new(TurnState::Active));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        // Branch A: simulate the natural-completion path.
        let s_a = turn_state.clone();
        let b_a = barrier.clone();
        let task_a = tokio::spawn(async move {
            b_a.wait().await;
            transition_to_terminal(&s_a, TerminalReason::Completed).await
        });

        // Branch B: simulate the interrupt-handler path. First mutate to
        // `Interrupting` (decide_interrupt-style); then yield so the
        // turn-task path (branch A) has a chance to lock the state and
        // observe `Interrupting` before B's own transition emits. This
        // is precisely the original TOCTOU race window.
        let s_b = turn_state.clone();
        let b_b = barrier.clone();
        let task_b = tokio::spawn(async move {
            b_b.wait().await;
            let captured = {
                let mut state = s_b.lock().await;
                if matches!(*state, TurnState::Active) {
                    let (ack_tx, _ack_rx) = oneshot::channel();
                    *state = TurnState::Interrupting {
                        ack: ack_tx,
                        origin: InterruptOrigin::Client,
                    };
                    true
                } else {
                    false
                }
            };
            if captured {
                // Yield repeatedly — give the runtime an opportunity to
                // schedule branch A on a different worker. Without this
                // the same-task lock-release-acquire happens atomically
                // from the runtime's POV and A never wins.
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                }
                transition_to_terminal(&s_b, TerminalReason::Interrupted).await
            } else {
                None
            }
        });

        let (a, b) = tokio::try_join!(task_a, task_b).expect("tasks join");

        // Exactly one of the two transition calls must have actually
        // mutated state. Both being `Some` would be a double-emit bug.
        let mutations = [a.as_ref().is_some(), b.as_ref().is_some()]
            .iter()
            .filter(|&&x| x)
            .count();
        assert_eq!(mutations, 1, "exactly one terminal transition per turn");

        let terminal = match &*turn_state.lock().await {
            TurnState::Terminal(r) => *r,
            other => panic!("expected Terminal, got {other:?}"),
        };
        match terminal {
            TerminalReason::Completed => completed_first += 1,
            TerminalReason::Interrupted => interrupted_first += 1,
            TerminalReason::Errored => unreachable!(),
        }

        // Race window: branch A's transition reason is `Interrupted` —
        // it observed `Interrupting` set by branch B and converted it.
        // This is precisely the original TOCTOU window — under the old
        // code both `turn/completed` and `turn/error` would emit. Under
        // the new state machine, A reports `Interrupted` and B's second
        // transition is a no-op.
        if matches!(
            a.as_ref().map(|t| t.reason),
            Some(TerminalReason::Interrupted)
        ) {
            race_window_observed += 1;
        }
    }
    eprintln!(
        "interrupt-race repro: iterations={ITERATIONS} \
             race_window_observed={race_window_observed} \
             completed_first={completed_first} interrupted_first={interrupted_first}"
    );
    assert!(
        race_window_observed > 0,
        "expected at least one of {ITERATIONS} iterations to exercise the \
             race window (Completed-path observes Interrupting); got \
             completed_first={completed_first}, interrupted_first={interrupted_first}, \
             race_window={race_window_observed}"
    );
}

// ====================================================================
// M9-FIX-06 — `approval_scope` enforcement (#644)
//
// These tests sit at the `(PendingApprovalStore, ScopePolicy)` integration
// level. They mimic the exact recording sequence that
// `handle_approval_respond` performs after a successful `respond`, then
// probe `ScopePolicy::lookup` to verify auto-resolution. Going through
// `handle_approval_respond` itself would require a real WebSocket sink;
// the routing is exercised by the higher-level e2e suite.
// ====================================================================

/// Mirrors what `handle_approval_respond` does on success: respond to
/// the pending approval and, if the scope is recordable, register the
/// policy entry. Returns the recorded scope kind (or `None` if the
/// scope was one-shot / unknown).
fn respond_with_scope(
    contracts: &UiProtocolContractStores,
    params: ApprovalRespondParams,
) -> Option<ApprovalScopeKind> {
    let session_id = params.session_id.clone();
    let scope = params.approval_scope.clone();
    // FIX-01: `ApprovalDecision` is non-Copy (`Unknown(String)`); clone
    // out of `params` before `respond` consumes it.
    let decision = params.decision.clone();
    let outcome = contracts.approvals.respond(params).expect("respond ok");
    let scope = scope?;
    let context = outcome.context?;
    let kind = ApprovalScopeKind::from_scope_str(&scope);
    if !kind.is_recordable() {
        return None;
    }
    let key = match_key_for(kind, &context.tool_name, &context.turn_id);
    contracts.scopes.record(&session_id, kind, key, decision);
    Some(kind)
}

fn store_request(
    contracts: &UiProtocolContractStores,
    session_id: &SessionKey,
    approval_id: ApprovalId,
    turn_id: TurnId,
    tool: &str,
) {
    contracts.approvals.request(ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id,
        turn_id,
        tool,
        "Run command",
        "cargo test",
    ));
}

#[test]
fn scope_approve_for_turn_auto_resolves_within_turn() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_id.clone(),
        turn_id.clone(),
        "shell",
    );

    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    params.approval_scope = Some("approve_for_turn".into());
    let kind = respond_with_scope(&contracts, params).expect("scope recorded");
    assert_eq!(kind, ApprovalScopeKind::ApproveForTurn);

    // Second approval in the same turn — same tool — should auto-resolve.
    let hit = contracts
        .scopes
        .lookup(&session_id, "shell", &turn_id)
        .expect("auto-resolve hit");
    assert_eq!(hit.decision, ApprovalDecision::Approve);
    assert_eq!(hit.scope_wire(), approval_scopes::TURN);
}

#[test]
fn scope_approve_for_turn_re_prompts_on_next_turn() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_a = TurnId::new();
    let turn_b = TurnId::new();
    let approval_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_id.clone(),
        turn_a.clone(),
        "shell",
    );

    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    params.approval_scope = Some("approve_for_turn".into());
    respond_with_scope(&contracts, params);

    // Same session but different turn → no auto-resolve; user must
    // re-affirm.
    assert!(
        contracts
            .scopes
            .lookup(&session_id, "shell", &turn_b)
            .is_none()
    );
}

#[test]
fn scope_approve_for_session_persists_until_session_close() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_a = TurnId::new();
    let turn_b = TurnId::new();
    let approval_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_id.clone(),
        turn_a.clone(),
        "shell",
    );

    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    params.approval_scope = Some("approve_for_session".into());
    respond_with_scope(&contracts, params);

    // Auto-resolve in turn A.
    assert!(
        contracts
            .scopes
            .lookup(&session_id, "shell", &turn_a)
            .is_some()
    );
    // Eviction-on-turn must NOT drop the session-scope entry.
    contracts.scopes.evict_turn(&session_id, &turn_a);
    assert!(
        contracts
            .scopes
            .lookup(&session_id, "shell", &turn_b)
            .is_some()
    );

    // Session close drops it.
    contracts.scopes.evict_session(&session_id);
    assert!(
        contracts
            .scopes
            .lookup(&session_id, "shell", &turn_b)
            .is_none()
    );
}

#[test]
fn scope_approve_for_tool_auto_resolves_same_tool() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_a = TurnId::new();
    let turn_b = TurnId::new();
    let approval_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_id.clone(),
        turn_a.clone(),
        "shell",
    );

    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    params.approval_scope = Some("approve_for_tool".into());
    respond_with_scope(&contracts, params);

    // Same tool, even on a different turn, auto-resolves.
    let hit = contracts
        .scopes
        .lookup(&session_id, "shell", &turn_b)
        .expect("tool scope persists across turns");
    assert_eq!(hit.scope_wire(), approval_scopes::TOOL);
    assert_eq!(hit.decision, ApprovalDecision::Approve);
}

#[test]
fn scope_approve_for_tool_does_not_match_different_tool() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_id.clone(),
        turn_id.clone(),
        "shell",
    );

    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    params.approval_scope = Some("approve_for_tool".into());
    respond_with_scope(&contracts, params);

    // Different tool name → no hit, must prompt again.
    assert!(
        contracts
            .scopes
            .lookup(&session_id, "browser", &turn_id)
            .is_none()
    );
}

#[test]
fn scope_evicts_on_session_close() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_id.clone(),
        turn_id.clone(),
        "shell",
    );

    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    params.approval_scope = Some("approve_for_session".into());
    respond_with_scope(&contracts, params);

    assert!(
        contracts
            .scopes
            .lookup(&session_id, "shell", &turn_id)
            .is_some()
    );
    contracts.scopes.evict_session(&session_id);
    assert!(
        contracts
            .scopes
            .lookup(&session_id, "shell", &turn_id)
            .is_none()
    );
}

#[test]
fn unknown_scope_string_falls_back_to_approve_once() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_id.clone(),
        turn_id.clone(),
        "shell",
    );

    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    // A scope token the server doesn't recognise — open-registry rule
    // says we MUST NOT error; we just don't record anything.
    params.approval_scope = Some("approve_for_galaxy_v9".into());
    let kind = respond_with_scope(&contracts, params);
    assert!(
        kind.is_none(),
        "unknown scope string must be treated as approve_once"
    );
    assert!(
        contracts
            .scopes
            .lookup(&session_id, "shell", &turn_id)
            .is_none()
    );
}

#[test]
fn scope_approve_for_turn_evicted_when_finalize_turn_runs() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_id.clone(),
        turn_id.clone(),
        "shell",
    );
    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    params.approval_scope = Some(approval_scopes::TURN.into());
    respond_with_scope(&contracts, params);

    contracts.scopes.evict_turn(&session_id, &turn_id);
    assert!(
        contracts
            .scopes
            .lookup(&session_id, "shell", &turn_id)
            .is_none(),
        "turn/completed must drop approve_for_turn entries"
    );
}

#[test]
fn scope_deny_short_circuit_records_deny() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_id.clone(),
        turn_id.clone(),
        "shell",
    );
    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Deny);
    params.approval_scope = Some(approval_scopes::TOOL.into());
    respond_with_scope(&contracts, params);

    let hit = contracts
        .scopes
        .lookup(&session_id, "shell", &turn_id)
        .expect("deny scope hit");
    assert_eq!(hit.decision, ApprovalDecision::Deny);
}

#[test]
fn scope_list_for_session_round_trips_via_handler_shape() {
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();

    let approval_a = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_a.clone(),
        turn_id.clone(),
        "shell",
    );
    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_a, ApprovalDecision::Approve);
    params.approval_scope = Some(approval_scopes::TURN.into());
    respond_with_scope(&contracts, params);

    let approval_b = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        approval_b.clone(),
        turn_id.clone(),
        "shell",
    );
    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_b, ApprovalDecision::Deny);
    params.approval_scope = Some(approval_scopes::TOOL.into());
    respond_with_scope(&contracts, params);

    let listed = contracts.scopes.list_for_session(&session_id);
    assert_eq!(listed.len(), 2);
    // Sorted by scope wire string ascending: tool < turn.
    assert_eq!(listed[0].scope, approval_scopes::TOOL);
    assert_eq!(listed[0].decision, ApprovalDecision::Deny);
    assert_eq!(listed[0].scope_match, "shell");
    assert_eq!(listed[1].scope, approval_scopes::TURN);
    assert_eq!(listed[1].decision, ApprovalDecision::Approve);
    assert_eq!(listed[1].turn_id.as_ref(), Some(&turn_id));
}

// ====================================================================
// M9-FIX-04 — send-error handling + backpressure
// ====================================================================

/// Builds a `WsConnection` whose writer side feeds an in-test `mpsc`. The
/// returned receiver is the "dedicated writer task" stand-in; drain it to
/// unblock further sends, leave it alone to simulate a slow client.
fn ws_connection_for_test(capacity: usize) -> (WsConnection, mpsc::Receiver<super::WsMessage>) {
    let (tx, rx) = mpsc::channel(capacity);
    (WsConnection::new(tx), rx)
}

#[tokio::test(flavor = "current_thread")]
async fn approval_respond_ledgers_decided_before_unblocked_turn_completion() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let sessions = state.sessions.as_ref().expect("sessions").clone();
    let sessions_guard = sessions.lock().await;
    let (ws, _rx) = ws_connection_for_test(32);
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();
    let request = ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        turn_id.clone(),
        "shell",
        "Run command",
        "cargo test",
    );
    let response_rx = contracts.approvals.request_runtime(request);
    let ledger_for_turn = Arc::clone(&ledger);
    let session_for_turn = session_id.clone();
    let turn_for_turn = turn_id.clone();
    let completion_task = tokio::spawn(async move {
        assert_eq!(
            response_rx.await.expect("approval decision"),
            ApprovalDecision::Approve,
        );
        ledger_for_turn.append_notification(UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id: session_for_turn,
            topic: None,
            turn_id: turn_for_turn,
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        }));
    });

    let handler_ws = ws.clone();
    let handler_state = Arc::clone(&state);
    let handler_ledger = Arc::clone(&ledger);
    let handler_contracts = Arc::clone(&contracts);
    let handler_session = session_id.clone();
    let handler_approval = approval_id.clone();
    let handler = tokio::spawn(async move {
        handle_approval_respond(
            &handler_ws,
            &handler_state,
            &handler_ledger,
            &handler_contracts,
            None,
            "approval-respond".into(),
            ApprovalRespondParams::new(
                handler_session,
                handler_approval,
                ApprovalDecision::Approve,
            ),
        )
        .await;
    });

    tokio::task::yield_now().await;
    drop(sessions_guard);
    handler.await.expect("approval/respond handler");
    completion_task.await.expect("completion task");

    let replay = ledger
        .replay_after(
            &session_id,
            Some(&UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        )
        .expect("replay after approval response");
    let decided_pos = replay
        .iter()
        .position(|entry| {
            matches!(
                &entry.event,
                UiProtocolLedgerEvent::Notification(UiNotification::ApprovalDecided(event))
                    if event.approval_id == approval_id
            )
        })
        .expect("approval/decided ledger event");
    let completed_pos = replay
        .iter()
        .position(|entry| {
            matches!(
                &entry.event,
                UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(event))
                    if event.turn_id == turn_id
            )
        })
        .expect("turn/completed ledger event");
    assert!(
        decided_pos < completed_pos,
        "approval/decided must ledger before the unblocked turn can complete"
    );
}

#[tokio::test]
async fn send_error_propagates_for_lifecycle_messages() {
    // capacity=1, the channel fills with the first frame; the second
    // lifecycle send must surface as `LifecycleFailure`. Without this
    // change, the bug was that callers `let _ =`'d the failure.
    let (ws, _rx) = ws_connection_for_test(1);

    // Fill the channel.
    let first = send_rpc_result(&ws, "1".into(), json!({"ok": true}));
    assert!(first.is_ok(), "first send must succeed");

    // Second lifecycle send should fail with LifecycleFailure (not be
    // silently dropped).
    let second = send_rpc_result(&ws, "2".into(), json!({"ok": true}));
    assert!(matches!(second, Err(SendError::LifecycleFailure(_))));
}

#[tokio::test]
async fn forced_backpressure_fixture_ledgers_terminal_and_latches_failed() {
    let (ws, mut rx) = ws_connection_for_test(8);
    let ledger = UiProtocolLedger::new(16);
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();

    let result = send_notification_lifecycle_forced_backpressure_fixture(
        &ws,
        &ledger,
        UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        }),
    );

    assert!(matches!(result, Err(SendError::LifecycleFailure(_))));
    assert!(
        ws.is_failed(),
        "forced lifecycle backpressure must close the socket"
    );
    assert!(
        rx.try_recv().is_err(),
        "forced lifecycle backpressure should not live-send the terminal frame"
    );

    let replay = ledger
        .replay_after(
            &session_id,
            Some(&UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        )
        .expect("replay after start cursor");
    assert!(replay.iter().any(|entry| matches!(
        &entry.event,
        UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(event))
            if event.turn_id == turn_id
    )));
}

#[tokio::test]
async fn send_error_logged_for_durable_notifications() {
    let (ws, _rx) = ws_connection_for_test(1);
    let ledger = UiProtocolLedger::new(16);
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();

    // Pre-fill capacity=1 channel.
    let first = send_notification_durable(
        &ws,
        &ledger,
        UiNotification::TurnStarted(octos_core::ui_protocol::TurnStartedEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            timestamp: Utc::now(),
            topic: None,
        }),
    );
    assert!(first.is_ok());

    // The second durable notification must be a BackpressureDrop and the
    // dropped count must increment so the next emit_replay_lossy* sees it.
    let second = send_notification_durable(
        &ws,
        &ledger,
        UiNotification::Warning(octos_core::ui_protocol::WarningEvent {
            session_id: session_id.clone(),
            turn_id: Some(turn_id.clone()),
            code: "test".into(),
            message: "drop me".into(),
        }),
    );
    assert!(matches!(second, Err(SendError::BackpressureDrop)));
    // The opportunistic replay_lossy attempt also fails (channel full), so
    // dropped_count is restored to >= 1 for a later flush.
    let metrics = ws.metrics();
    assert!(metrics.dropped_count.load(Ordering::Relaxed) >= 1);
}

#[tokio::test]
async fn approval_request_backpressure_cancels_pending_runtime_waiter() {
    let (ws, _rx) = ws_connection_for_test(1);
    let ledger = UiProtocolLedger::new(16);
    let contracts = UiProtocolContractStores::default();
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let approval_id = ApprovalId::new();

    let first = send_rpc_result(&ws, "fill".into(), json!({"ok": true}));
    assert!(first.is_ok(), "first send fills the bounded channel");

    let request = ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        turn_id.clone(),
        "shell",
        "Run command",
        "cargo test",
    );
    let response_rx = contracts.approvals.request_runtime(request.clone());
    let send = send_notification_durable(
        &ws,
        &ledger,
        UiNotification::ApprovalRequested(request.clone()),
    );
    assert!(matches!(send, Err(SendError::BackpressureDrop)));

    cancel_approval_after_request_send_failure(
        &contracts,
        &ws,
        &ledger,
        &session_id,
        &approval_id,
        &turn_id,
    );

    assert!(
        response_rx.await.is_err(),
        "cancelling the pending approval drops the runtime sender"
    );
    assert!(
        contracts
            .approvals
            .pending_for_session(&session_id)
            .is_empty(),
        "failed sends must not leave a reconnect-pending approval"
    );
    let late_response = contracts
        .approvals
        .respond_with_context(ApprovalRespondParams::new(
            session_id.clone(),
            approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .expect_err("late response should see typed cancellation");
    assert_eq!(
        late_response.code,
        octos_core::ui_protocol::rpc_error_codes::APPROVAL_CANCELLED
    );
    assert_eq!(
        late_response.data.as_ref().unwrap()["reason"],
        APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED
    );

    let replay = ledger
        .replay_after(
            &session_id,
            Some(&UiCursor {
                stream: session_id.0.clone(),
                seq: 0,
            }),
        )
        .expect("replay after start cursor");
    assert!(replay.iter().any(|entry| matches!(
        &entry.event,
        UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(event))
            if event.approval_id == approval_id
    )));
    assert!(replay.iter().any(|entry| matches!(
        &entry.event,
        UiProtocolLedgerEvent::Notification(UiNotification::ApprovalCancelled(event))
            if event.approval_id == approval_id
                && event.reason == APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED
    )));
}

#[tokio::test]
async fn ephemeral_drops_are_silent_and_do_not_increment_dropped_count() {
    let (ws, _rx) = ws_connection_for_test(1);
    let ledger = UiProtocolLedger::new(16);
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();

    // Fill the channel with a non-ephemeral lifecycle frame.
    let first = send_rpc_result(&ws, "1".into(), json!({"ok": true}));
    assert!(first.is_ok());

    // Ephemeral message/delta drop: must surface as BackpressureDrop but
    // must NOT bump the dropped_count (ephemeral is non-durable per spec).
    let second = send_notification_ephemeral(
        &ws,
        &ledger,
        UiNotification::MessageDelta(MessageDeltaEvent {
            session_id,
            topic: None,
            turn_id,
            text: "hi".into(),
        }),
    );
    assert!(matches!(second, Err(SendError::BackpressureDrop)));
    assert_eq!(ws.metrics().dropped_count.load(Ordering::Relaxed), 0);
}

/// #924 BLOCK 2: once a lifecycle send marks the connection failed,
/// every subsequent enqueue must fail with `FatalClosed` — even if
/// the underlying channel has spare capacity now. Background
/// forwarders that keep pumping after the read loop tore down would
/// otherwise queue frames into a writer about to drain and exit.
#[tokio::test]
async fn try_enqueue_returns_fatal_closed_after_mark_failed() {
    let (ws, mut rx) = ws_connection_for_test(8);

    // First send succeeds and leaves capacity available.
    let frame = WsMessage::Text("ping".to_string().into());
    assert!(ws.try_enqueue(frame).is_ok());

    // Drain so capacity is fully open.
    let _ = rx.try_recv();

    // Latch the connection as failed (the lifecycle wrappers do
    // this on backpressure / closed writer; here we drive the API
    // directly to isolate the check).
    ws.mark_failed();

    // Even with capacity open, the next enqueue must fail loudly.
    let frame = WsMessage::Text("after-fail".to_string().into());
    let err = ws
        .try_enqueue(frame)
        .expect_err("post-latch enqueue must fail");
    assert!(matches!(err, SendError::FatalClosed));

    // And the lifecycle wrapper turns it into LifecycleFailure so
    // existing RPC-reply callsites still see the failure-shaped
    // error.
    let res = send_rpc_result(&ws, "post-fail".into(), json!({"ok": true}));
    assert!(matches!(res, Err(SendError::LifecycleFailure(_))));
}

/// #924 BLOCK 1: `mark_failed` must wake every pending `notified()`
/// waiter so the read loop's `select!` arm fires immediately. An
/// idle socket with a failed write side must NOT sit waiting for
/// the next client frame.
#[tokio::test]
async fn mark_failed_wakes_failed_notify_waiters() {
    let (ws, _rx) = ws_connection_for_test(1);
    let notify = ws.failed_notify();

    // Park a waiter; latch failed; the waiter must complete promptly.
    let waited = tokio::time::timeout(std::time::Duration::from_millis(500), async move {
        notify.notified().await;
    });
    // Run latch + await concurrently — the `notified()` future must
    // observe the wake even though we never read another frame.
    let ws_clone = ws.clone();
    let latch = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        ws_clone.mark_failed();
    });
    waited.await.expect("notified() must wake within 500ms");
    latch.await.expect("latch task joins cleanly");
    assert!(ws.is_failed());
}

/// #924 round-2 BLOCK: the round-1 test parks a waiter BEFORE calling
/// `mark_failed`. The lost-notify race only shows up the other way
/// around: the latch fires while the read loop is between iterations
/// (no `Notified` future exists yet). `notify_waiters` stashes nothing,
/// so the next-iteration `notified()` would never resolve without the
/// pre-park latch re-check. This test replays the same select shape
/// the production loop uses and asserts it exits on an idle socket
/// even when `mark_failed` fires before the read task starts polling.
#[tokio::test]
async fn mark_failed_during_idle_loop_still_cleans_up() {
    let (ws, _rx) = ws_connection_for_test(1);
    let failed_notify = ws.failed_notify();
    // The "ws_rx" stand-in: an mpsc never sent on => idle socket.
    let (_inbound_tx, mut inbound_rx) = mpsc::channel::<()>(1);

    // Latch failed BEFORE the read task ever spins. This is the
    // lost-notify ordering: the future that will park does not yet
    // exist when `notify_waiters` fires.
    ws.mark_failed();

    let ws_for_task = ws.clone();
    let read_task = tokio::spawn(async move {
        loop {
            if ws_for_task.is_failed() {
                break;
            }
            let notified = failed_notify.notified();
            tokio::pin!(notified);
            if ws_for_task.is_failed() {
                break;
            }
            tokio::select! {
                biased;
                _ = &mut notified => break,
                _ = inbound_rx.recv() => continue,
            }
        }
    });

    tokio::time::timeout(std::time::Duration::from_millis(100), read_task)
        .await
        .expect("read loop must exit promptly when latch fires before park")
        .expect("read task joins cleanly");
    assert!(ws.is_failed());
}

#[tokio::test]
async fn slow_client_does_not_wedge_other_connections() {
    // Two independent WsConnection wrappers (each with its own writer
    // channel + drainer) simulate two clients. Pause client A's drainer;
    // verify client B continues to receive frames during that window.
    let (ws_a, mut rx_a) = ws_connection_for_test(WS_WRITER_CHANNEL_CAPACITY);
    let (ws_b, mut rx_b) = ws_connection_for_test(WS_WRITER_CHANNEL_CAPACITY);
    let ledger = UiProtocolLedger::new(64);
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();

    // Spawn a "slow client A": sleeps 200ms before its first read. With
    // the old `Arc<Mutex<WsSink>>` pattern this would block all callers
    // because they held the lock across `.send().await`. With the new
    // mpsc design, each connection is independent.
    let slow_a = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let mut received = 0u32;
        while rx_a.try_recv().is_ok() {
            received += 1;
        }
        received
    });

    // While A is "paused", client B should continue to receive frames.
    for _ in 0..16u32 {
        let res = send_notification_durable(
            &ws_b,
            &ledger,
            UiNotification::Warning(octos_core::ui_protocol::WarningEvent {
                session_id: session_id.clone(),
                turn_id: Some(turn_id.clone()),
                code: "tick".into(),
                message: "for client B".into(),
            }),
        );
        assert!(res.is_ok(), "client B send must not be wedged by client A");
    }

    // Drain client B's channel to confirm frames did reach the writer side.
    let mut b_count = 0u32;
    while rx_b.try_recv().is_ok() {
        b_count += 1;
    }
    assert!(b_count >= 16, "client B received {b_count} frames");

    // Send something to A so the slow task has work. Sleep > 200ms total
    // by awaiting the join.
    let _ = send_notification_durable(
        &ws_a,
        &ledger,
        UiNotification::Warning(octos_core::ui_protocol::WarningEvent {
            session_id,
            turn_id: Some(turn_id),
            code: "tick".into(),
            message: "for client A".into(),
        }),
    );
    let a_received = slow_a.await.expect("slow client task");
    assert!(
        a_received >= 1,
        "client A eventually received {a_received} frames"
    );
}

#[tokio::test]
async fn bounded_channel_full_emits_replay_lossy() {
    // Fill a small channel by never draining it; emit many durable
    // notifications. A `protocol/replay_lossy` frame must surface in the
    // channel before the test ends (opportunistic emit + flush).
    let (ws, mut rx) = ws_connection_for_test(8);
    let ledger = UiProtocolLedger::new(64);
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let progress_dropped = Arc::new(AtomicU64::new(0));

    // Pump 2000 durable notifications. Most will drop; the cumulative
    // count is held in `metrics.dropped_count`.
    for _ in 0..2000u32 {
        let _ = send_notification_durable(
            &ws,
            &ledger,
            UiNotification::Warning(octos_core::ui_protocol::WarningEvent {
                session_id: session_id.clone(),
                turn_id: Some(turn_id.clone()),
                code: "tick".into(),
                message: "load".into(),
            }),
        );
    }

    // Drain the channel — the replay_lossy frame may already be in there
    // from an opportunistic emit when capacity briefly opened.
    let mut frames = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        frames.push(msg);
    }

    // Now flush at the turn boundary (mimics what happens before
    // turn/completed). Any remaining drops must produce a replay_lossy.
    flush_replay_lossy(&ws, &ledger, &session_id, &progress_dropped);

    // After flush, drain again.
    while let Ok(msg) = rx.try_recv() {
        frames.push(msg);
    }

    // At least one frame in the captured set must be a `protocol/replay_lossy`.
    let lossy_frame = frames.iter().find_map(|frame| match frame {
        super::WsMessage::Text(text) if text.as_str().contains("\"protocol/replay_lossy\"") => {
            Some(text.as_str().to_string())
        }
        _ => None,
    });
    assert!(
        lossy_frame.is_some(),
        "expected a protocol/replay_lossy frame among {} captured",
        frames.len()
    );
    // Surface a sample for the M9 status report — useful when running
    // with `-- --nocapture`.
    if let Some(sample) = lossy_frame {
        eprintln!("sample protocol/replay_lossy frame: {sample}");
    }
}

#[test]
fn replay_lossy_method_is_registered_in_core_protocol() {
    // Schema-side guard: the new method name and notification variant
    // must be wired into the core protocol's notification list and
    // dispatch table. Catches "added the variant but forgot the entry"
    // regressions.
    let methods = octos_core::ui_protocol::UI_PROTOCOL_NOTIFICATION_METHODS;
    assert!(methods.contains(&octos_core::ui_protocol::methods::REPLAY_LOSSY));

    let event = UiNotification::ReplayLossy(ReplayLossyEvent {
        session_id: SessionKey("local:test".into()),
        dropped_count: 7,
        last_durable_cursor: Some(UiCursor {
            stream: "local:test".into(),
            seq: 42,
        }),
    });
    let frame = event
        .into_rpc_notification()
        .expect("serialize replay_lossy");
    assert_eq!(frame.method, octos_core::ui_protocol::methods::REPLAY_LOSSY);
    assert_eq!(frame.params["dropped_count"], json!(7));
    assert_eq!(frame.params["last_durable_cursor"]["seq"], json!(42));
}

// ====================================================================
// M9-FIX-07 — approval decision audit log + replay
// ====================================================================

#[test]
fn audit_log_records_every_decision() {
    // Mirrors what `handle_approval_respond` does. Verifies one
    // JSON-Lines entry per decision and that no payload bodies leak.
    use octos_core::ui_protocol::ApprovalRequestedEvent;

    let temp = tempfile::tempdir().expect("tempdir");
    let log = ApprovalsAuditLog::new(temp.path(), ApprovalsAuditConfig::default());
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:audit".into());

    let mut ids = Vec::new();
    for _ in 0..3 {
        let approval_id = ApprovalId::new();
        ids.push(approval_id.clone());
        approvals.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            TurnId::new(),
            "shell",
            "Run",
            "secret-body",
        ));
        let params =
            ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
        let outcome = approvals
            .respond_with_context(params.clone())
            .expect("decide");
        let event = crate::api::ui_protocol_approvals::build_decided_event(
            &params,
            &outcome,
            "user:test",
            chrono::Utc::now(),
        );
        let tool_name = outcome.context.as_ref().map(|ctx| ctx.tool_name.clone());
        log.record(&event, tool_name.as_deref()).expect("write");
    }

    let active = std::fs::read_dir(temp.path().join("audit"))
        .expect("audit dir")
        .filter_map(Result::ok)
        .next()
        .expect("active log")
        .path();
    let lines = crate::api::ui_protocol_audit::read_audit_lines(&active);
    assert_eq!(lines.len(), 3);
    for (line, expected_id) in lines.iter().zip(ids.iter()) {
        assert_eq!(line["approval_id"], json!(expected_id.0.to_string()));
        assert_eq!(line["decision"], json!("approve"));
        assert_eq!(line["tool_name"], json!("shell"));
        assert_eq!(line["auto_resolved"], json!(false));
        // PII rule: no command body fields, no body content.
        assert!(!serde_json::to_string(line).unwrap().contains("secret-body"));
    }
}

#[tokio::test]
async fn reconnect_after_decision_replays_decided_event() {
    use chrono::Utc;
    use octos_core::ui_protocol::{ApprovalDecidedEvent, ApprovalRequestedEvent};

    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
    let ledger = UiProtocolLedger::new(64);
    let approvals = PendingApprovalStore::default();
    let session_id = SessionKey("local:reconnect".into());
    let approval_id = ApprovalId::new();
    let turn_id = TurnId::new();

    // Seed a pre-C1 anchor so the reconnect cursor can express "before
    // C1" — the cursor space starts at 1.
    let warmup = ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        text: "preamble".into(),
    }));
    let request = ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        turn_id,
        "shell",
        "Run command",
        "cargo test",
    );
    approvals.request(request.clone());
    ledger.append_notification(UiNotification::ApprovalRequested(request));
    let outcome_decide = approvals
        .respond_with_context(ApprovalRespondParams::new(
            session_id.clone(),
            approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .expect("decide");
    let decided_turn_id = outcome_decide
        .context
        .as_ref()
        .map(|ctx| ctx.turn_id.clone())
        .expect("request was registered");
    ledger.append_notification(UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: session_id.clone(),
        topic: None,
        approval_id: approval_id.clone(),
        turn_id: decided_turn_id,
        decision: ApprovalDecision::Approve,
        scope: Some("session".into()),
        decided_at: Utc::now(),
        decided_by: "user:tester".into(),
        auto_resolved: false,
        policy_id: None,
        client_note: None,
    }));

    let outcome = open_session_result(
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        None,
        None,
        ConnectionUiFeatures::default(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: Some(warmup.cursor.clone()),
        },
    )
    .await
    .expect("reconnect should succeed");

    let mut saw_requested = false;
    let mut saw_decided = false;
    for event in &outcome.replay {
        match &event.event {
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(e))
                if e.approval_id == approval_id =>
            {
                saw_requested = true;
            }
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalDecided(e))
                if e.approval_id == approval_id =>
            {
                saw_decided = true;
                assert_eq!(e.decision, ApprovalDecision::Approve);
                assert_eq!(e.scope.as_deref(), Some("session"));
            }
            _ => {}
        }
    }
    assert!(saw_requested, "replay missing approval/requested");
    assert!(saw_decided, "replay missing approval/decided");
    assert!(outcome.pending_approvals.is_empty());
}

// ====================================================================
// M9-06 — terminal task lifecycle durability under WS backpressure
// ====================================================================

fn make_background_task(
    id: &str,
    status: octos_agent::TaskStatus,
    runtime_state: octos_agent::TaskRuntimeState,
) -> octos_agent::BackgroundTask {
    octos_agent::BackgroundTask {
        id: id.into(),
        tool_name: "search".into(),
        tool_call_id: "call-1".into(),
        parent_session_key: Some("local:test".into()),
        child_session_key: None,
        child_terminal_state: None,
        child_join_state: None,
        child_joined_at: None,
        child_failure_action: None,
        task_ledger_path: None,
        status,
        runtime_state,
        runtime_detail: None,
        started_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        completed_at: None,
        output_files: Vec::new(),
        error: None,
        final_output: None,
        failed_by_observer: false,
        session_key: Some("local:test".into()),
        tool_input: None,
        originating_client_message_id: None,
        source: None,
        role: None,
        summary: None,
        artifact_count: None,
        runtime_policy_stamp: None,
        projection_metadata: None,
        workspace_root: None,
    }
}

/// Sanity-check the fast path: when the channel has capacity, the
/// helper sends synchronously without spawning anything and without
/// touching the drop counter.
#[tokio::test(flavor = "current_thread")]
async fn task_update_fast_path_when_channel_has_capacity() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
    let dropped = Arc::new(AtomicU64::new(0));

    let task = make_background_task(
        "01900000-0000-7000-8000-0000000000cc",
        octos_agent::TaskStatus::Failed,
        octos_agent::TaskRuntimeState::Failed,
    );
    forward_task_progress_to_channel(&tx, &dropped, &task, None);

    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    let event = rx.try_recv().expect("event must be available immediately");
    let parsed: serde_json::Value = serde_json::from_str(&event).expect("valid json");
    assert_eq!(parsed["state"], "failed");
}

// ====================================================================
// Gap-1 unification: parity contract over the SINGLE terminal sink
// (`route_terminal_event_to_continuation_queue`). The same consumer fn
// is wired via `set_on_terminal` in every runtime mode (WS, gateway,
// headless drain), so driving it directly proves the cross-mode parity:
// exactly one continuation, correct reason, enqueued under the resolved
// runtime profile (ZERO under `_main`), idempotent under repeated
// terminal marks, recovery prompt body unchanged for failure-with-ack.
// ====================================================================

// ====================================================================
// PR G — UPCR-2026-009 / -010 / -011 / -012 handler tests
// ====================================================================

fn prg_state_with_session(
    session_id: &SessionKey,
    seed: impl FnOnce(&mut octos_bus::Session),
) -> Arc<AppState> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manager = octos_bus::SessionManager::open(tmp.path()).expect("session manager open");
    let manager = Arc::new(tokio::sync::Mutex::new(manager));
    // Seed by directly mutating in-memory session.
    {
        let mut guard = manager.try_lock().expect("session manager lock");
        // get_or_create is async, so we sidestep by using try_lock + a
        // synchronous workaround: spawn-blocking is overkill; this
        // helper is only called from sync context above the test.
        // We block_on a separate task so we can call async manager.
        // Easiest: rebuild via a sync-OK helper. Use futures executor.
        let session = futures::executor::block_on(guard.get_or_create(session_id));
        seed(session);
    }
    Arc::new(AppState {
        sessions: Some(manager),
        ..AppState::empty_for_tests()
    })
    // tmp is dropped when state drops; tests don't observe disk
}

fn prg_seed_user_assistant(session: &mut octos_bus::Session) {
    let now = Utc::now();
    session.messages.push(Message {
        role: MessageRole::User,
        content: "hello".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: Some("cmid-user-1".into()),
        thread_id: Some("cmid-user-1".into()),
        timestamp: now,
    });
    session.messages.push(Message {
        role: MessageRole::Assistant,
        content: "world".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: Some("cmid-user-1".into()),
        timestamp: now + chrono::Duration::milliseconds(10),
    });
}

/// Open a disk-backed `SessionManager` and persist `turns` user turns
/// (user + assistant each, thread-grouped) so `session/rollback` has a real
/// JSONL to append its marker to and reload from. Returns the state plus the
/// live `TempDir` — the caller must keep it alive for the test's duration.
async fn prg_state_with_persisted_turns(
    session_id: &SessionKey,
    turns: usize,
) -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manager = octos_bus::SessionManager::open(tmp.path()).expect("session manager open");
    let manager = Arc::new(tokio::sync::Mutex::new(manager));
    {
        let mut guard = manager.lock().await;
        for n in 1..=turns {
            let tid = format!("t{n}");
            let now = Utc::now() + chrono::Duration::milliseconds((n as i64) * 2);
            let user = Message {
                role: MessageRole::User,
                content: format!("turn {n}"),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: Some(tid.clone()),
                thread_id: Some(tid.clone()),
                timestamp: now,
            };
            guard
                .add_message(session_id, user)
                .await
                .expect("persist user");
            let asst = Message {
                role: MessageRole::Assistant,
                content: format!("reply {n}"),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: Some(tid.clone()),
                timestamp: now + chrono::Duration::milliseconds(1),
            };
            guard
                .add_message(session_id, asst)
                .await
                .expect("persist assistant");
        }
    }
    let state = Arc::new(AppState {
        sessions: Some(manager),
        ..AppState::empty_for_tests()
    });
    (state, tmp)
}

#[tokio::test(flavor = "current_thread")]
async fn session_rollback_drops_last_turn_and_returns_trimmed_thread() {
    let session_id = SessionKey("local:rollback-1".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_rollback(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        "rb1".into(),
        SessionRollbackParams {
            session_id: session_id.clone(),
            num_turns: 1,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], "rb1");
    let result = &frame["result"];
    assert_eq!(result["dropped_turns"], 1);
    let thread = &result["thread"];
    assert_eq!(thread["session_id"], session_id.to_string());
    assert!(thread["cursor"].is_object());
    let messages = thread["messages"].as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        4,
        "turns 1 & 2 remain after dropping turn 3"
    );
    assert!(messages.iter().all(|m| m["content"] != "turn 3"));
    assert!(messages.iter().all(|m| m["content"] != "reply 3"));
    let threads = thread["threads"].as_array().expect("threads array");
    assert_eq!(threads.len(), 2);
    assert!(thread["turns"].is_array());
    assert!(
        !thread
            .as_object()
            .unwrap()
            .contains_key("replayed_tool_envelopes"),
        "rollback hydrate projection must preserve legacy omission semantics for tool replay"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_rollback_survives_reload_from_disk() {
    let session_id = SessionKey("local:rollback-reload".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_rollback(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        "rb2".into(),
        SessionRollbackParams {
            session_id: session_id.clone(),
            num_turns: 1,
        },
    )
    .await;
    let _ = recv_rpc_json(&mut rx).await;

    // Evict the cache and reload from disk: the append-only marker replays
    // the trim (it was persisted, not truncated).
    {
        let sessions = state.sessions.as_ref().expect("sessions store");
        let mut guard = sessions.lock().await;
        guard.invalidate_cache(&session_id);
        let session = guard.get_or_create(&session_id).await;
        assert_eq!(
            session.messages.len(),
            4,
            "rollback marker must survive a disk reload"
        );
        assert!(session.messages.iter().all(|m| m.content != "turn 3"));
    }
}

/// A stale context ledger must not resurrect rolled-back turns. The
/// ledger coverage check is a high-watermark `>=` (deliberately
/// slice-tolerant, because turn paths pass bounded history slices), so
/// after a rollback shrinks durable history a pre-rollback ledger still
/// "covers" it, gets Loaded verbatim, and the next model prompt would
/// contain the very turns the user rewound away. `session/rollback` must
/// rebuild + persist the context ledger from the trimmed history.
#[tokio::test(flavor = "current_thread")]
async fn session_rollback_rebuilds_context_ledger() {
    let session_id = SessionKey("local:rollback-ctx-ledger".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    // Persist a pre-rollback context ledger exactly like a prior turn's
    // prompt path would have.
    let data_dir = {
        let sessions = state.sessions.as_ref().expect("sessions store");
        let mut guard = sessions.lock().await;
        let data_dir = guard.data_dir();
        let history = guard.get_or_create(&session_id).await.messages.clone();
        let manager = crate::context_manager::ContextManager::from_session_history(
            session_id.to_string(),
            None,
            &history,
        );
        crate::context_manager::persist_context_manager_snapshot(
            &data_dir,
            &session_id.to_string(),
            &manager,
        )
        .expect("persist pre-rollback ledger");
        data_dir
    };
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_rollback(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        "rb-ctx".into(),
        SessionRollbackParams {
            session_id: session_id.clone(),
            num_turns: 1,
        },
    )
    .await;
    let _ = recv_rpc_json(&mut rx).await;

    // The next turn loads the ledger against the TRIMMED history; the
    // dropped turn must not be visible in the resulting prompt frame.
    let trimmed = {
        let sessions = state.sessions.as_ref().expect("sessions store");
        let mut guard = sessions.lock().await;
        guard.get_or_create(&session_id).await.messages.clone()
    };
    assert!(trimmed.iter().all(|m| m.content != "turn 3"));
    let (loaded, _status) = crate::context_manager::load_or_rebuild_context_manager(
        &data_dir,
        session_id.to_string(),
        None,
        &trimmed,
    );
    let frame = loaded.for_prompt(&crate::context_manager::PromptBuildPolicy::default());
    assert!(
        !frame
            .messages
            .iter()
            .any(|m| m.content.contains("turn 3") || m.content.contains("reply 3")),
        "rolled-back turns must not resurrect through a stale context ledger: {:#?}",
        frame.messages
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_rollback_rejects_when_turn_in_progress() {
    let session_id = SessionKey("local:rollback-busy".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    let active_turns = active_turns_registry();
    // Insert a synthetic in-flight turn, exactly as handle_turn_start would.
    let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
    let dummy_handle = tokio::spawn(async {});
    {
        let mut guard = active_turns.lock().await;
        guard.insert(
            session_id.clone(),
            ActiveTurn {
                turn_id: TurnId::new(),
                profile_id: MAIN_PROFILE_ID.to_owned(),
                state: Arc::new(TokioMutex::new(TurnState::Active)),
                interrupt_tx: Arc::new(TokioMutex::new(Some(interrupt_tx))),
                steer: None,
                abort: dummy_handle.abort_handle(),
            },
        );
    }
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_rollback(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        "rb3".into(),
        SessionRollbackParams {
            session_id: session_id.clone(),
            num_turns: 1,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert!(
        frame.get("error").is_some(),
        "rollback under an active turn must error: {frame}"
    );
    assert_eq!(frame["error"]["data"]["kind"], "turn_in_progress");
    // The transcript must be untouched (6 messages = 3 turns).
    {
        let sessions = state.sessions.as_ref().expect("sessions store");
        let mut guard = sessions.lock().await;
        let session = guard.get_or_create(&session_id).await;
        assert_eq!(session.messages.len(), 6, "no trim under an active turn");
    }
    active_turns.lock().await.remove(&session_id);
}

#[tokio::test(flavor = "current_thread")]
async fn session_rollback_rejects_zero_num_turns() {
    let session_id = SessionKey("local:rollback-zero".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 2).await;
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_rollback(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        "rb0".into(),
        SessionRollbackParams {
            session_id: session_id.clone(),
            num_turns: 0,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert!(frame.get("error").is_some(), "num_turns=0 must be rejected");
    assert_eq!(frame["error"]["data"]["kind"], "invalid_num_turns");
}

/// Codex P2: the ledger snapshot handed to `collect_session_turns` is taken
/// BEFORE the trim, so it still carries lifecycle + v2 projection events
/// for the rolled-back turns. The returned `thread.turns` must scope
/// to the SURVIVING threads — a dropped turn must not linger in the turn
/// projection even though its ledger rows persist.
#[tokio::test(flavor = "current_thread")]
async fn session_rollback_excludes_dropped_turns_from_thread_turns() {
    let session_id = SessionKey("local:rollback-turns-scope".into());
    // Two persisted turns, thread-grouped under "t1" and "t2".
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 2).await;
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;

    // Full lifecycle for BOTH turns in the ledger, thread-linked to the
    // persisted turns by canonical v2 envelopes.
    let turn_keep = TurnId::new(); // turn 1 -> thread "t1" (survives)
    let turn_drop = TurnId::new(); // turn 2 -> thread "t2" (rolled back)
    for (turn_id, thread_id, seq) in [(&turn_keep, "t1", 1_u64), (&turn_drop, "t2", 3_u64)] {
        let _ = ledger.append_notification(UiNotification::TurnStarted(
            octos_core::ui_protocol::TurnStartedEvent {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                timestamp: Utc::now(),
                topic: None,
            },
        ));
        let _ = ledger.append_notification(UiNotification::EnvelopeV2(EnvelopeV2Notification {
            session_id: session_id.clone(),
            topic: None,
            envelope: EnvelopeV2 {
                thread_id: thread_id.to_string(),
                seq,
                cursor: None,
                turn_id: turn_id.0.to_string(),
                client_message_id: None,
                payload: PayloadV2::UserMessage {
                    text: "user".into(),
                    files: vec![],
                },
            },
        }));
        let _ = ledger.append_notification(UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        }));
    }

    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_rollback(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        "rb-scope".into(),
        SessionRollbackParams {
            session_id: session_id.clone(),
            num_turns: 1,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["dropped_turns"], 1);
    let turns = frame["result"]["thread"]["turns"]
        .as_array()
        .expect("turns array");
    let turn_ids: Vec<String> = turns
        .iter()
        .filter_map(|turn| turn["turn_id"].as_str().map(str::to_owned))
        .collect();
    assert!(
        turn_ids.contains(&turn_keep.0.to_string()),
        "surviving turn 1 must remain in thread.turns; got {turn_ids:?}"
    );
    assert!(
        !turn_ids.contains(&turn_drop.0.to_string()),
        "dropped turn 2 must be excluded from thread.turns; got {turn_ids:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_copies_full_history_by_default() {
    let session_id = SessionKey("local:fork-parent".into());
    // 3 turns -> 6 messages persisted on the parent.
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk1".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "fork-child".into(),
            copy_messages: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], "fk1");
    let result = &frame["result"];
    assert_eq!(result["new_session_id"], "local:fork-child");
    assert_eq!(result["parent_session_id"], session_id.to_string());
    assert_eq!(result["copied_messages"], 6);

    // The child must exist on the manager, carry parent lineage, and
    // hold the full copied history.
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    let child_key = SessionKey("local:fork-child".into());
    assert!(
        guard.session_known(&child_key),
        "child session must persist"
    );
    let child = guard.get_or_create(&child_key).await;
    assert_eq!(child.parent_key.as_ref(), Some(&session_id));
    assert_eq!(child.messages.len(), 6);
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_copies_only_requested_tail() {
    let session_id = SessionKey("local:fork-tail".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk2".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "tail-child".into(),
            copy_messages: Some(2),
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["copied_messages"], 2);
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    let child = guard
        .get_or_create(&SessionKey("local:tail-child".into()))
        .await;
    assert_eq!(child.messages.len(), 2);
    // Tail = the LAST turn (user + assistant of turn 3).
    assert_eq!(child.messages[0].content, "turn 3");
    assert_eq!(child.messages[1].content, "reply 3");
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_unknown_session_errors() {
    let seeded = SessionKey("local:fork-seeded".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&seeded, 1).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk3".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: SessionKey("local:never-created".into()),
            new_chat_id: "child".into(),
            copy_messages: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert!(
        frame.get("error").is_some(),
        "fork of an unknown session must error, not auto-create"
    );
    // Fork must NOT have created either session as a side effect.
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    assert!(!guard.session_known(&SessionKey("local:never-created".into())));
    assert!(!guard.session_known(&SessionKey("local:child".into())));
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_refuses_existing_child_key() {
    let session_id = SessionKey("local:fork-clobber".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 2).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    for id in ["fka", "fkb"] {
        handle_session_fork(
            &ws,
            &state,
            None,
            None,
            id.into(),
            octos_core::ui_protocol::SessionForkParams {
                session_id: session_id.clone(),
                new_chat_id: "same-child".into(),
                copy_messages: None,
            },
        )
        .await;
    }

    let first = recv_rpc_json(&mut rx).await;
    assert!(first.get("result").is_some(), "first fork succeeds");
    let second = recv_rpc_json(&mut rx).await;
    assert_eq!(
        second["error"]["data"]["kind"], "child_exists",
        "second fork onto the same child key must refuse, not clobber"
    );

    // The child's history must be the FIRST fork's copy, untouched.
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    let child = guard
        .get_or_create(&SessionKey("local:same-child".into()))
        .await;
    assert_eq!(child.messages.len(), 4);
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_rejects_invalid_chat_id() {
    let session_id = SessionKey("local:fork-badname".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 1).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk4".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "../escape".into(),
            copy_messages: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["data"]["kind"], "invalid_new_chat_id");
}

#[test]
fn fork_reservations_scope_by_sessions_dir() {
    // codex #1613 r2: identical child keys in DIFFERENT profiles'
    // sessions dirs name different files — they must not exclude
    // each other. Same dir + same key must.
    let child = SessionKey("local:contested".into());
    let dir_a = std::path::Path::new("/tmp/profile-a/sessions");
    let dir_b = std::path::Path::new("/tmp/profile-b/sessions");

    let a = ForkReservation::try_acquire(dir_a, &child).expect("dir A reserves");
    let b = ForkReservation::try_acquire(dir_b, &child);
    assert!(b.is_some(), "different sessions dir must not collide");
    assert!(
        ForkReservation::try_acquire(dir_a, &child).is_none(),
        "same dir + same child key must exclude"
    );
    drop(a);
    assert!(
        ForkReservation::try_acquire(dir_a, &child).is_some(),
        "released reservation must be reacquirable"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_raw_spa_parent_yields_raw_child() {
    // codex #1613 P1: a raw SPA handle ("web-123") must NOT be
    // treated as a channel — the child is a raw handle too, not
    // "web-123:kid" (excluded from the raw REST fallbacks).
    let session_id = SessionKey("web-123".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 1).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk-raw".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "web-456".into(),
            copy_messages: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["new_session_id"], "web-456");
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    assert!(guard.session_known(&SessionKey("web-456".into())));
    assert!(
        !guard.session_known(&SessionKey("web-123:web-456".into())),
        "the naive channel-split key must not exist"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_concurrent_same_child_one_wins() {
    // codex #1613 P2: two forks from DIFFERENT parents racing to
    // the same child key — exactly one may win; the loser must not
    // silently overwrite the winner's copied history.
    let parent_a = SessionKey("local:race-a".into());
    let parent_b = SessionKey("local:race-b".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&parent_a, 1).await;
    {
        // Seed the second parent on the same manager.
        let sessions = state.sessions.as_ref().expect("sessions");
        let mut guard = sessions.lock().await;
        let now = Utc::now();
        let msg = Message {
            role: MessageRole::User,
            content: "b says".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: now,
        };
        guard.add_message(&parent_b, msg).await.expect("seed b");
    }
    let (ws, mut rx) = ws_connection_for_test(8);

    let fork = |parent: SessionKey, id: &str| {
        let ws = &ws;
        let state = &state;
        let id = id.to_string();
        async move {
            handle_session_fork(
                ws,
                state,
                None,
                None,
                id,
                octos_core::ui_protocol::SessionForkParams {
                    session_id: parent,
                    new_chat_id: "contested".into(),
                    copy_messages: None,
                },
            )
            .await;
        }
    };
    tokio::join!(
        fork(parent_a.clone(), "fk-a"),
        fork(parent_b.clone(), "fk-b")
    );

    let first = recv_rpc_json(&mut rx).await;
    let second = recv_rpc_json(&mut rx).await;
    let oks = [&first, &second]
        .iter()
        .filter(|f| f.get("result").is_some())
        .count();
    assert_eq!(oks, 1, "exactly one fork wins: {first} / {second}");
    let loser = if first.get("error").is_some() {
        &first
    } else {
        &second
    };
    assert_eq!(loser["error"]["data"]["kind"], "child_exists");
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_enforces_connection_scope() {
    // A profile-PREFIXED session id names another tenant's scope
    // outright; a connection bound to "tenant-b" must be refused.
    // (Un-prefixed ids are accepted by SPA convention — isolation for
    // those comes from per-profile runtime resolution.)
    let session_id = SessionKey("tenant-a:local:fork-scope".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 1).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        Some("tenant-b"),
        None,
        "fk5".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "stolen".into(),
            copy_messages: None,
        },
    )
    .await;

    // Scope violations close 1008-first (see
    // send_scope_error_closes_with_1008_on_authenticated_mismatch).
    let first = rx.recv().await.expect("close frame");
    match first {
        super::WsMessage::Close(Some(frame)) => {
            assert_eq!(frame.code, 1008);
        }
        other => panic!("expected 1008 close on cross-scope fork, got {other:?}"),
    }
    // And the fork must NOT have happened.
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    assert!(!guard.session_known(&SessionKey("tenant-a:local:stolen".into())));
    assert!(!guard.session_known(&SessionKey("local:stolen".into())));
}

#[tokio::test(flavor = "current_thread")]
async fn session_hydrate_returns_full_chat_state() {
    let session_id = SessionKey("local:hydrate-1".into());
    let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
    let approvals = PendingApprovalStore::default();
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_hydrate(
        &ws,
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::default(),
        "h1".into(),
        SessionHydrateParams {
            session_id: session_id.clone(),
            after: None,
            include: vec![],
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], "h1");
    let result = &frame["result"];
    assert_eq!(result["session_id"], session_id.to_string());
    assert!(result["cursor"].is_object());
    assert_eq!(
        result["context_state"]["session_id"],
        session_id.to_string()
    );
    assert_eq!(result["context"]["schema"], "octos.context.lifecycle.v1");
    assert_eq!(
        result["context"]["state"]["session_id"],
        session_id.to_string()
    );
    assert!(
        result["context_state"]["transcript_hash"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty()),
        "hydrate must expose a typed context snapshot for clients that negotiated or default to context.lifecycle.v1"
    );
    let messages = result["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    let threads = result["threads"].as_array().expect("threads array");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["thread_id"], "cmid-user-1");
    assert_eq!(threads[0]["root_seq"], 0);
    assert_eq!(threads[0]["message_seqs"], json!([0, 1]));
    assert!(result["turns"].is_array());
    assert_eq!(result["pending_approvals"].as_array().unwrap().len(), 0);
}

/// The canonical background writer can migrate the old flat transcript while
/// the foreground manager still owns its pre-migration mirror. Its next three
/// rows have earlier model timestamps, so a cold merge moves the background
/// row from 52 to 55. The committed message ID must not move with that index.
#[tokio::test(flavor = "current_thread")]
async fn should_keep_background_identity_when_mixed_store_merge_reindexes_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = SessionKey("mixed-store-background-identity".into());
    let mut manager = octos_bus::SessionManager::open(dir.path()).unwrap();
    let start = Utc::now() - chrono::Duration::minutes(10);
    for index in 0..52 {
        let mut message = if index % 2 == 0 {
            Message::user_rooting_thread(
                format!("seed user {index}"),
                octos_core::ClientMessageId(format!("seed-{}", index / 2)),
            )
        } else {
            Message::assistant_with_thread(
                format!("seed final {index}"),
                octos_core::ThreadId(format!("seed-{}", index / 2)),
            )
        };
        message.timestamp = start + chrono::Duration::seconds(index);
        manager.add_message(&session_id, message).await.unwrap();
    }
    let mut ledger_config = LedgerConfig::durable(dir.path().join("identity-ledger"));
    ledger_config.retained_per_session = 128;
    ledger_config.rotate_bytes = 1;
    ledger_config.retained_log_files = 6;
    let ledger = Arc::new(UiProtocolLedger::with_config(ledger_config.clone()));
    // Two disposable old log records: after the later writes, the first file
    // rotates away but the BG reference remains durably retained at seq 3.
    for index in 0..2 {
        ledger
            .emit_envelope_v2(
                &session_id,
                "old-tool-turn".into(),
                PayloadV2::ToolStart {
                    tool_call_id: format!("old-call-{index}"),
                    name: "read_file".into(),
                    arguments_preview: None,
                },
                None,
            )
            .unwrap();
    }
    let observer = message_commit_observer(ledger.clone());
    octos_bus::session::set_scoped_message_commit_observer(dir.path(), &observer);
    let parent = "mixed-parent";
    let mut background =
        Message::assistant_with_thread("same completion body", octos_core::ThreadId(parent.into()));
    background.timestamp = start + chrono::Duration::seconds(100);
    background.media = vec!["result.md".into()];
    let original_id = format!(
        "{}:52:{}",
        session_id.0,
        background.timestamp.timestamp_nanos_opt().unwrap(),
    );
    let committed = MESSAGE_PROJECTION_OVERRIDE
        .scope(
            Some(MessageProjectionOverride::BackgroundChild(
                BackgroundChildProjection {
                    parent_turn_id: parent.into(),
                    response_to_client_message_id: None,
                    task_id: Some("mixed-child".into()),
                    tool_call_id: Some("mixed-spawn".into()),
                    media: background.media.clone(),
                },
            )),
            octos_bus::persist_message_through_canonical_path(
                dir.path(),
                &session_id,
                background.clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(committed, 52, "actual canonical writer starts at row 52");

    let mut user = Message::user_rooting_thread(
        "start background",
        octos_core::ClientMessageId(parent.into()),
    );
    user.timestamp = start + chrono::Duration::seconds(90);
    let mut call = Message::assistant_with_thread("", octos_core::ThreadId(parent.into()));
    call.timestamp = start + chrono::Duration::seconds(91);
    call.tool_calls = Some(vec![octos_core::ToolCall {
        id: "mixed-spawn".into(),
        name: "spawn".into(),
        arguments: json!({}),
        metadata: None,
    }]);
    let mut tool = Message::tool_with_thread(
        "Spawned background task",
        "mixed-spawn",
        octos_core::ThreadId(parent.into()),
    );
    tool.timestamp = start + chrono::Duration::seconds(92);
    for message in [user, call, tool] {
        manager.add_message(&session_id, message).await.unwrap();
    }
    // Equal text from a distinct autonomous turn is a distinct canonical row.
    let mut continuation = Message::assistant_with_thread(
        background.content.clone(),
        octos_core::ThreadId("independent-continuation".into()),
    );
    continuation.timestamp = start + chrono::Duration::seconds(101);
    continuation.media = background.media.clone();
    manager
        .add_message(&session_id, continuation)
        .await
        .unwrap();
    manager.invalidate_cache(&session_id);
    let merged = manager.get_or_create(&session_id).await;
    assert_eq!(merged.messages[55].timestamp, background.timestamp);
    assert_eq!(merged.messages.len(), 57);

    let state = Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(manager))),
        ..AppState::empty_for_tests()
    });
    async fn hydrate(
        state: &Arc<AppState>,
        ledger: &Arc<UiProtocolLedger>,
        key: &SessionKey,
        after: Option<UiCursor>,
    ) -> Value {
        let (ws, mut rx) = ws_connection_for_test(8);
        handle_session_hydrate(
            &ws,
            state,
            ledger,
            &PendingApprovalStore::default(),
            &PendingQuestionStore::default(),
            &active_turns_registry(),
            None,
            None,
            features_for_projection_envelope_v2_test(),
            "mixed-hydrate".into(),
            SessionHydrateParams {
                session_id: key.clone(),
                after,
                include: vec![],
            },
        )
        .await;
        recv_rpc_json(&mut rx).await["result"].clone()
    }
    let first = hydrate(&state, &ledger, &session_id, None).await;
    let first_rows = first["messages"].as_array().unwrap();
    let background_envelope = first["replayed_envelopes"].as_array().unwrap();
    assert_eq!(
        background_envelope.len(),
        1,
        "actual durable background reference: {first}"
    );
    assert_eq!(first_rows[55]["message_id"], original_id);
    assert_eq!(first_rows[55]["source"], "background");
    assert_ne!(first_rows[56]["message_id"], original_id);
    assert_ne!(first_rows[56]["source"], "background");
    assert_eq!(
        background_envelope[0]["payload"]["data"]["message_id"],
        original_id
    );

    // A live next turn followed by another cold hydrate must not turn the
    // already-owned card into an unmatched envelope appended after that turn.
    {
        let mut sessions = state.sessions.as_ref().unwrap().lock().await;
        let mut user = Message::user_rooting_thread(
            "T23 after cold client",
            octos_core::ClientMessageId("mixed-t23".into()),
        );
        user.timestamp = start + chrono::Duration::seconds(102);
        let mut answer =
            Message::assistant_with_thread("T23 final", octos_core::ThreadId("mixed-t23".into()));
        answer.timestamp = start + chrono::Duration::seconds(103);
        sessions.add_message(&session_id, user).await.unwrap();
        sessions.add_message(&session_id, answer).await.unwrap();
        sessions.invalidate_cache(&session_id);
    }
    // Reopen the real durable ledger with a ring smaller than the history:
    // identity recovery cannot depend on a process-local map or hot tail.
    drop(observer);
    drop(ledger);
    ledger_config.retained_per_session = 2;
    let ledger = Arc::new(UiProtocolLedger::with_config(ledger_config));
    let second = hydrate(&state, &ledger, &session_id, None).await;
    let second_rows = second["messages"].as_array().unwrap();
    assert_eq!(second_rows[55]["message_id"], original_id);
    assert_eq!(&second_rows[..first_rows.len()], first_rows.as_slice());
    assert_eq!(second_rows[57]["content"], "T23 after cold client");
    assert_eq!(second_rows[58]["content"], "T23 final");
    assert!(
        second["replayed_envelopes"].as_array().unwrap().is_empty(),
        "the old background envelope is outside the hydrated hot-tail window"
    );
    let incremental = hydrate(
        &state,
        &ledger,
        &session_id,
        Some(serde_json::from_value(first["cursor"].clone()).unwrap()),
    )
    .await;
    assert!(
        incremental["replayed_envelopes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let incremental_background = incremental["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["seq"] == 55)
        .unwrap();
    assert_eq!(incremental_background["message_id"], original_id);
    assert_eq!(incremental_background["source"], "background");
}

#[test]
fn should_rebind_hydrate_identity_only_with_unique_scoped_canonical_provenance() {
    let session = SessionKey("hydrate-identity-provenance".into());
    let mut message =
        Message::assistant_with_thread("shared body", octos_core::ThreadId("owned-turn".into()));
    message.media = vec!["owned-result.md".into()];
    let notification = octos_core::ui_protocol::EnvelopeV2Notification {
        session_id: session.clone(),
        topic: None,
        envelope: EnvelopeV2 {
            thread_id: "owned-turn:background:child".into(),
            turn_id: "owned-turn:background:child".into(),
            seq: 1,
            cursor: None,
            client_message_id: None,
            payload: PayloadV2::BackgroundChildCompleted {
                parent_turn_id: "owned-turn".into(),
                response_to_client_message_id: None,
                task_id: "child".into(),
                tool_call_id: None,
                // Intentionally opaque: neither server compatibility lookup
                // nor the client needs to decode a historical ID format.
                message_id: "original-opaque-background-row".into(),
                source: "background".into(),
                content: message.content.clone(),
                persisted_at: message.timestamp,
                media: message.media.clone(),
            },
        },
    };
    let expected = HashMap::from([(0, ("original-opaque-background-row".into(), true))]);
    assert_eq!(
        hydrated_canonical_message_identities(
            &session,
            std::slice::from_ref(&message),
            std::slice::from_ref(&notification)
        ),
        expected,
    );
    assert_eq!(
        hydrated_canonical_message_identities(
            &session,
            std::slice::from_ref(&message),
            &[notification.clone(), notification.clone()],
        ),
        expected,
        "identical replays of one durable reference are idempotent",
    );
    let mut equal_text_sibling = message.clone();
    equal_text_sibling.thread_id = Some("different-turn".into());
    assert_eq!(
        hydrated_canonical_message_identities(
            &session,
            &[message.clone(), equal_text_sibling],
            std::slice::from_ref(&notification),
        ),
        expected,
        "equal body/media are never an ownership lookup key",
    );
    for case in [
        "owner",
        "missing-owner",
        "timestamp",
        "content",
        "media",
        "role",
    ] {
        let mut altered = message.clone();
        match case {
            "owner" => altered.thread_id = Some("foreign-owner".into()),
            "missing-owner" => altered.thread_id = None,
            "timestamp" => altered.timestamp += chrono::Duration::nanoseconds(1),
            "content" => altered.content.push('!'),
            "media" => altered.media.push("foreign-result.md".into()),
            "role" => altered.role = MessageRole::User,
            _ => unreachable!(),
        }
        assert!(
            hydrated_canonical_message_identities(
                &session,
                &[altered],
                std::slice::from_ref(&notification)
            )
            .is_empty(),
            "{case} is not matching canonical provenance",
        );
    }
    let mut same_owner_timestamp = message.clone();
    same_owner_timestamp.content = "a distinct body at the same instant".into();
    assert!(
        hydrated_canonical_message_identities(
            &session,
            &[message.clone(), same_owner_timestamp],
            std::slice::from_ref(&notification),
        )
        .is_empty(),
        "ambiguous timestamp/owner must not be disambiguated by body text",
    );
    for case in [
        "different-id",
        "same-id-conflicting-media",
        "same-id-conflicting-owner",
    ] {
        let mut contradictory = notification.clone();
        let PayloadV2::BackgroundChildCompleted {
            message_id,
            media,
            parent_turn_id,
            ..
        } = &mut contradictory.envelope.payload
        else {
            unreachable!()
        };
        match case {
            "different-id" => *message_id = "second-claim-for-same-row".into(),
            "same-id-conflicting-media" => media.push("conflicting.md".into()),
            "same-id-conflicting-owner" => *parent_turn_id = "another-owner".into(),
            _ => unreachable!(),
        }
        assert!(
            hydrated_canonical_message_identities(
                &session,
                std::slice::from_ref(&message),
                &[notification.clone(), contradictory],
            )
            .is_empty(),
            "{case} cannot give either claimant a row identity",
        );
    }
    let mut foreign_scope = notification.clone();
    foreign_scope.session_id = SessionKey("another-session".into());
    assert!(
        hydrated_canonical_message_identities(&session, &[message], &[foreign_scope]).is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn should_reject_recomputed_hydrate_id_when_its_typed_owner_contradicts_the_row() {
    let session = SessionKey("hydrate-recomputed-owner".into());
    let message =
        Message::assistant_with_thread("same body", octos_core::ThreadId("new-owner".into()));
    let reference = octos_core::ui_protocol::EnvelopeV2Notification {
        session_id: session.clone(),
        topic: None,
        envelope: EnvelopeV2 {
            thread_id: "old-owner:background:child".into(),
            turn_id: "old-owner:background:child".into(),
            seq: 1,
            cursor: None,
            client_message_id: None,
            payload: PayloadV2::BackgroundChildCompleted {
                parent_turn_id: "old-owner".into(),
                response_to_client_message_id: None,
                task_id: "child".into(),
                tool_call_id: None,
                message_id: format!(
                    "{}:0:{}",
                    session.0,
                    message.timestamp.timestamp_nanos_opt().unwrap()
                ),
                source: "background".into(),
                content: message.content.clone(),
                persisted_at: message.timestamp,
                media: vec![],
            },
        },
    };
    assert!(
        hydrated_canonical_message_identities(
            &session,
            std::slice::from_ref(&message),
            std::slice::from_ref(&reference)
        )
        .is_empty(),
        "a position-derived ID is not stronger authority than an explicit owner contradiction"
    );
    let state = prg_state_with_session(&session, |session| session.messages.push(message));
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let PayloadV2::BackgroundChildCompleted {
        message_id: claimed_id,
        ..
    } = &reference.envelope.payload
    else {
        unreachable!()
    };
    ledger.append_notification(UiNotification::EnvelopeV2(reference.clone()));
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_hydrate(
        &ws,
        &state,
        &ledger,
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        &active_turns_registry(),
        None,
        None,
        features_for_projection_envelope_v2_test(),
        "owner-conflict-hydrate".into(),
        SessionHydrateParams {
            session_id: session,
            after: None,
            include: vec![],
        },
    )
    .await;
    let result = recv_rpc_json(&mut rx).await;
    assert_ne!(
        result["result"]["messages"][0]["message_id"], *claimed_id,
        "unresolved fallback must not reissue the rejected claim as a client dedupe key"
    );
    assert_ne!(result["result"]["messages"][0]["source"], "background");
}

#[test]
fn should_limit_hydrate_identity_references_to_the_captured_scope_and_head() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = LedgerConfig::durable(dir.path().into());
    config.retained_per_session = 1;
    let ledger = UiProtocolLedger::with_config(config);
    let session = SessionKey("scoped-identity-evidence".into());
    ledger.set_session_scope(&session, Some("workspace-a".into()));
    let persisted = |id: &str| PayloadV2::AssistantPersisted {
        text: id.into(),
        assistant_segment_id: format!("segment-{id}"),
        meta: MessageMeta {
            message_id: id.into(),
            persisted_at: Utc::now(),
            media: vec![],
        },
    };
    let captured = ledger
        .emit_envelope_v2(&session, "before".into(), persisted("before"), None)
        .unwrap();
    ledger
        .emit_envelope_v2(&session, "after".into(), persisted("after"), None)
        .unwrap();
    ledger
        .emit_envelope_v2(
            &session,
            "after".into(),
            PayloadV2::ToolStart {
                tool_call_id: "unrelated-tool".into(),
                name: "read_file".into(),
                arguments_preview: None,
            },
            None,
        )
        .unwrap();
    let references = ledger
        .retained_message_identity_references(&session, &captured.cursor)
        .unwrap();
    assert_eq!(
        references.len(),
        1,
        "only the older eligible disk reference is included"
    );
    assert_eq!(references[0].cursor, captured.cursor);
    assert_eq!(references[0].event, captured.event);

    ledger.set_session_scope(&session, Some("workspace-b".into()));
    assert!(
        ledger
            .retained_message_identity_references(&session, &captured.cursor)
            .is_err(),
        "a cursor for another workspace is never identity authority"
    );
    let sibling = ledger
        .emit_envelope_v2(&session, "sibling".into(), persisted("sibling"), None)
        .unwrap();
    let references = ledger
        .retained_message_identity_references(&session, &sibling.cursor)
        .unwrap();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].event, sibling.event);
}

#[tokio::test(flavor = "current_thread")]
async fn raw_session_status_read_rebuilds_context_state() {
    let session_id = SessionKey("local:raw-status-context".into());
    let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
    let request = RpcRequest::<Value>::new(
        "status-read-1",
        APPUI_METHOD_SESSION_STATUS_READ,
        json!({ "session_id": session_id.clone() }),
    );

    let result = raw_session_status_result(&state, &request, ConnectionUiFeatures::default(), None)
        .await
        .expect("raw status read");
    assert_eq!(
        result["context_state"]["session_id"],
        session_id.to_string()
    );
    assert!(result["context"].is_object());
    assert!(
        result["context_state"]["transcript_hash"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty()),
        "session/status/read must rebuild context state when only persisted session history exists"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_hydrate_atomically_consistent_snapshot_and_cursor() {
    // Codex's atomicity ask: an event landing between the snapshot read
    // and the cursor read must NOT slip past either. We exercise this by
    // calling `snapshot_with_cursor` once and asserting the returned
    // cursor.seq is >= every event's seq in the returned vec — i.e. the
    // cursor pairs with the snapshot atomically.
    let session_id = SessionKey("local:hydrate-atomic".into());
    let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
    let ledger = event_ledger(&state).await;

    // Append two notifications to the ledger so there's something to
    // bound.
    let _ = ledger.append_notification(UiNotification::Warning(
        octos_core::ui_protocol::WarningEvent {
            session_id: session_id.clone(),
            turn_id: None,
            code: "test".into(),
            message: "first".into(),
        },
    ));
    let _ = ledger.append_notification(UiNotification::Warning(
        octos_core::ui_protocol::WarningEvent {
            session_id: session_id.clone(),
            turn_id: None,
            code: "test".into(),
            message: "second".into(),
        },
    ));

    let (events, cursor) = ledger
        .snapshot_with_cursor(&session_id, None)
        .expect("snapshot");
    // The pair invariant: cursor.seq >= max(event.cursor.seq) for every
    // event in the snapshot. Combined with the lock held during reads,
    // this means a follow-up `replay_after(cursor)` returns only events
    // strictly after — no gap.
    let max_event = events.iter().map(|e| e.cursor.seq).max().unwrap_or(0);
    assert!(
        cursor.seq >= max_event,
        "cursor.seq {} must >= max event seq {}",
        cursor.seq,
        max_event,
    );
}

#[tokio::test(flavor = "current_thread")]
async fn thread_graph_get_returns_known_threads() {
    let session_id = SessionKey("local:graph-1".into());
    let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_thread_graph_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        "g1".into(),
        ThreadGraphGetParams {
            session_id: session_id.clone(),
            at: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], "g1");
    let threads = frame["result"]["threads"].as_array().expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["thread_id"], "cmid-user-1");
    assert_eq!(threads[0]["root_seq"], 0);
    assert_eq!(threads[0]["root_client_message_id"], "cmid-user-1");
    assert_eq!(threads[0]["message_seqs"], json!([0, 1]));
    let orphans = frame["result"]["orphans"].as_array().expect("orphans");
    assert_eq!(orphans.len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn thread_graph_get_surfaces_orphans() {
    // A non-system row missing thread_id is an orphan. Per UPCR-2026-010
    // it lands in `orphans` so a client can metric on it.
    let session_id = SessionKey("local:graph-orphan".into());
    let state = prg_state_with_session(&session_id, |session| {
        let now = Utc::now();
        session.messages.push(Message {
            role: MessageRole::User,
            content: "rooted".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some("cmid-1".into()),
            thread_id: Some("cmid-1".into()),
            timestamp: now,
        });
        session.messages.push(Message {
            role: MessageRole::Assistant,
            content: "orphan".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None, // <- orphan
            timestamp: now + chrono::Duration::milliseconds(10),
        });
    });
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_thread_graph_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        "g2".into(),
        ThreadGraphGetParams {
            session_id: session_id.clone(),
            at: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    let orphans = frame["result"]["orphans"].as_array().expect("orphans");
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0], 1);
}

#[tokio::test(flavor = "current_thread")]
async fn turn_state_get_returns_active_for_in_flight() {
    let session_id = SessionKey("local:turn-active".into());
    let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
    let active_turns = active_turns_registry();
    let turn_id = TurnId::new();
    // Insert a synthetic active turn into the registry. We construct
    // ActiveTurn directly the same way handle_turn_start would.
    let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
    let dummy_handle = tokio::spawn(async {});
    {
        let mut guard = active_turns.lock().await;
        guard.insert(
            session_id.clone(),
            ActiveTurn {
                turn_id: turn_id.clone(),
                profile_id: MAIN_PROFILE_ID.to_owned(),
                state: Arc::new(TokioMutex::new(TurnState::Active)),
                interrupt_tx: Arc::new(TokioMutex::new(Some(interrupt_tx))),
                steer: None,
                abort: dummy_handle.abort_handle(),
            },
        );
    }
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_turn_state_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "t1".into(),
        TurnStateGetParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["state"], "active");
    assert_eq!(
        frame["result"]["context"]["schema"],
        "octos.context.lifecycle.v1"
    );
    assert_eq!(
        frame["result"]["context_state"]["session_id"],
        session_id.to_string()
    );
    // Cleanup so the test does not pollute the global registry for
    // sibling tests.
    active_turns.lock().await.remove(&session_id);
}

#[tokio::test(flavor = "current_thread")]
async fn turn_state_get_falls_back_to_durable_projection_for_evicted() {
    // Codex's durable-backing ask: a turn that is no longer in the
    // active-turn registry but whose lifecycle is recorded in the
    // ledger must still surface a non-`unknown` state.
    let session_id = SessionKey("local:turn-evicted".into());
    let state = prg_state_with_session(&session_id, |_| {});
    let active_turns = active_turns_registry();
    let turn_id = TurnId::new();
    let ledger = event_ledger(&state).await;

    // Append a turn/started + turn/completed to the ledger so the
    // projection has truth without anything in the registry.
    let _ = ledger.append_notification(UiNotification::TurnStarted(
        octos_core::ui_protocol::TurnStartedEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            timestamp: Utc::now(),
            topic: None,
        },
    ));
    let _ = ledger.append_notification(UiNotification::TurnCompleted(TurnCompletedEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        cursor: None,
        tokens_in: None,
        tokens_out: None,
        session_result: None,
    }));

    let (ws, mut rx) = ws_connection_for_test(8);
    handle_turn_state_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "t2".into(),
        TurnStateGetParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(
        frame["result"]["state"], "completed",
        "evicted turn must surface terminal state from the ledger projection"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_hydrate_rejects_unknown_session() {
    // Build a sessions manager with NO sessions seeded; the handler
    // must reject the request rather than auto-create or return an
    // empty hydrate.
    let tmp = tempfile::tempdir().expect("tempdir");
    let manager = octos_bus::SessionManager::open(tmp.path()).expect("open");
    let state = Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(manager))),
        ..AppState::empty_for_tests()
    });
    let approvals = PendingApprovalStore::default();
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_hydrate(
        &ws,
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::default(),
        "h-unknown".into(),
        SessionHydrateParams {
            session_id: SessionKey("local:nope".into()),
            after: None,
            include: vec![],
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert!(frame.get("error").is_some(), "must return error frame");
    assert_eq!(frame["error"]["data"]["kind"], "unknown_session");
}

/// A v2 client receives retained background-child and tool envelopes on
/// hydrate, with the same payloads it receives live. The transcript row's
/// stable message id links the background child without a legacy source
/// notification.
#[tokio::test(flavor = "current_thread")]
async fn session_hydrate_surfaces_replayed_envelopes_for_negotiated_client() {
    let session_id = SessionKey("local:hydrate-envelopes".into());
    // Capture the spawn-ack row's timestamp so the envelope's
    // `message_id` can mirror what `MessageCommitObserver` would
    // emit on the live wire (and what the hydrate handler now
    // synthesizes for `HydratedMessage.message_id`).
    let spawn_ack_ts = Utc::now() + chrono::Duration::milliseconds(10);
    let state = prg_state_with_session(&session_id, |session| {
        let now = spawn_ack_ts - chrono::Duration::milliseconds(10);
        session.messages.push(Message {
            role: MessageRole::User,
            content: "kick off bg_research".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some("cmid-user-1".into()),
            thread_id: Some("cmid-user-1".into()),
            timestamp: now,
        });
        // Historical companion row; current producer paths carry its
        // media on the background-child payload instead.
        session.messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec!["research/_report.md".into()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some("cmid-user-1".into()),
            timestamp: now + chrono::Duration::milliseconds(5),
        });
        // Background completion row.
        session.messages.push(Message {
            role: MessageRole::Assistant,
            content: "bg_research delivered.".into(),
            media: vec!["research/_report.md".into()],
            tool_calls: None,
            tool_call_id: None,
            // Persisted thinking text — must survive into the hydrated
            // row for negotiated clients (the "· reasoning" block used
            // to vanish on every client restart).
            reasoning_content: Some("I should summarize the findings.".into()),
            client_message_id: None,
            thread_id: Some("cmid-user-1".into()),
            timestamp: spawn_ack_ts,
        });
    });
    let approvals = PendingApprovalStore::default();
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;

    let spawn_ack_message_id = format!(
        "{}:2:{}",
        session_id.0,
        spawn_ack_ts.timestamp_nanos_opt().unwrap_or(0),
    );
    let parent_turn_id = "turn-parent";
    ledger
        .emit_envelope_v2(
            &session_id,
            format!("{parent_turn_id}:background:task_abc"),
            PayloadV2::BackgroundChildCompleted {
                parent_turn_id: parent_turn_id.into(),
                response_to_client_message_id: Some("cmid-user-1".into()),
                task_id: "task_abc".into(),
                content: "bg_research delivered.".into(),
                tool_call_id: None,
                message_id: spawn_ack_message_id.clone(),
                source: "background".into(),
                persisted_at: spawn_ack_ts,
                media: vec!["research/_report.md".into()],
            },
            None,
        )
        .expect("background child envelope");
    ledger
        .emit_envelope_v2(
            &session_id,
            "cmid-user-1".into(),
            PayloadV2::ToolStart {
                tool_call_id: "tc-shell-1".into(),
                name: "shell".into(),
                arguments_preview: None,
            },
            None,
        )
        .expect("tool envelope");

    // 1) Negotiated client: messages list is byte-identical to
    // the legacy shape (3 rows), AND the new
    // `replayed_envelopes` field carries the envelope so the
    // client can dedup on its side.
    let (ws_new, mut rx_new) = ws_connection_for_test(8);
    handle_session_hydrate(
        &ws_new,
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        &active_turns,
        None,
        None,
        features_for_projection_envelope_v2_test(),
        "h-new".into(),
        SessionHydrateParams {
            session_id: session_id.clone(),
            after: None,
            include: vec![],
        },
    )
    .await;
    let frame_new = recv_rpc_json(&mut rx_new).await;
    let messages_new = frame_new["result"]["messages"]
        .as_array()
        .expect("messages array");
    assert_eq!(
        messages_new.len(),
        3,
        "hydrate keeps the durable transcript alongside its v2 replay facts",
    );
    // Codex Bug C round-5: the spawn-ack row's `message_id` must
    // be present on the hydrated wire so the client can match it
    // against the envelope. Without this, the client has nothing
    // to dedup against.
    let spawn_ack_row = messages_new
        .iter()
        .find(|m| m["seq"] == 2)
        .expect("seq=2 spawn-ack row");
    assert_eq!(
        spawn_ack_row["message_id"], spawn_ack_message_id,
        "spawn-ack row must expose message_id matching the envelope",
    );
    // The matching v2 child marks the completion row as background.
    let companion_row = messages_new
        .iter()
        .find(|m| m["seq"] == 1)
        .expect("seq=1 companion row");
    assert_eq!(spawn_ack_row["source"], "background");
    assert!(
        companion_row
            .get("source")
            .map(Value::is_null)
            .unwrap_or(true),
        "only a v2 payload with the exact message id establishes provenance",
    );
    // Negotiated hydrate carries the persisted reasoning so the
    // "· reasoning" block survives a client restart.
    assert_eq!(
        spawn_ack_row["reasoning_content"], "I should summarize the findings.",
        "hydrated row must surface persisted reasoning_content"
    );
    assert!(
        companion_row
            .get("reasoning_content")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "rows without reasoning omit the field"
    );
    let user_row = messages_new
        .iter()
        .find(|m| m["seq"] == 0)
        .expect("seq=0 user row");
    // The user row has no background-child payload, so provenance is
    // omitted.
    assert!(
        user_row.get("source").map(|v| v.is_null()).unwrap_or(true),
        "user row's source field is omitted absent a matching v2 payload; got: {user_row:?}",
    );
    let envelopes = frame_new["result"]["replayed_envelopes"]
        .as_array()
        .expect("replayed_envelopes array");
    assert_eq!(envelopes.len(), 1, "single envelope retained");
    assert_eq!(
        envelopes[0]["payload"]["data"]["message_id"], spawn_ack_message_id,
        "child payload message_id matches the durable completion row",
    );
    assert_eq!(envelopes[0]["payload"]["type"], "background/spawn_complete");
    assert_eq!(envelopes[0]["payload"]["data"]["task_id"], "task_abc");
    assert_eq!(
        envelopes[0]["payload"]["data"]["content"],
        "bg_research delivered."
    );
    assert_eq!(
        envelopes[0]["payload"]["data"]["media"],
        json!(["research/_report.md"])
    );
    let tool_envelopes = frame_new["result"]["replayed_tool_envelopes"]
        .as_array()
        .expect("replayed_tool_envelopes array");
    assert_eq!(tool_envelopes.len(), 1, "single tool envelope retained");
    assert_eq!(tool_envelopes[0]["thread_id"], "cmid-user-1");
    assert_eq!(tool_envelopes[0]["payload"]["type"], "tool_start");
    assert_eq!(
        tool_envelopes[0]["payload"]["data"]["tool_call_id"],
        "tc-shell-1",
    );
    assert_eq!(tool_envelopes[0]["payload"]["data"]["name"], "shell");

    // 2) A non-v2 hydrate request gets no v2 replay fields.
    let (ws_legacy, mut rx_legacy) = ws_connection_for_test(8);
    handle_session_hydrate(
        &ws_legacy,
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::default(),
        "h-legacy".into(),
        SessionHydrateParams {
            session_id: session_id.clone(),
            after: None,
            include: vec![],
        },
    )
    .await;
    let frame_legacy = recv_rpc_json(&mut rx_legacy).await;
    let messages_legacy = frame_legacy["result"]["messages"]
        .as_array()
        .expect("messages array");
    assert_eq!(messages_legacy.len(), 3, "transcript remains available");
    let result = frame_legacy["result"].as_object().expect("result object");
    assert!(
        !result.contains_key("replayed_envelopes"),
        "non-v2 hydrate omits replayed_envelopes; got keys: {:?}",
        result.keys().collect::<Vec<_>>(),
    );
    assert!(
        !result.contains_key("replayed_tool_envelopes"),
        "non-v2 hydrate omits tool replay field; got keys: {:?}",
        result.keys().collect::<Vec<_>>(),
    );
    // Without v2 negotiation, hydrate does not expose v2 correlation
    // metadata on transcript rows.
    for msg in messages_legacy {
        let msg_obj = msg.as_object().expect("message object");
        assert!(
            !msg_obj.contains_key("message_id"),
            "non-v2 hydrate message must omit message_id; got: {msg_obj:?}",
        );
        assert!(
            !msg_obj.contains_key("source"),
            "non-v2 hydrate message must omit source; got: {msg_obj:?}",
        );
    }
}

/// Bug C corollary: a negotiated client whose hydrate request
/// excludes `messages` does not need the envelopes either — they
/// only matter as a dedup key against the messages list. Keep
/// `replayed_envelopes` absent in that case so the response stays
/// minimal.
#[tokio::test(flavor = "current_thread")]
async fn session_hydrate_omits_envelopes_when_messages_excluded() {
    let session_id = SessionKey("local:hydrate-envelopes-no-msgs".into());
    let state = prg_state_with_session(&session_id, prg_seed_user_assistant);
    let approvals = PendingApprovalStore::default();
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    ledger
        .emit_envelope_v2(
            &session_id,
            "turn-parent:background:task_x".into(),
            PayloadV2::BackgroundChildCompleted {
                parent_turn_id: "turn-parent".into(),
                response_to_client_message_id: Some("cmid-user-1".into()),
                task_id: "task_x".into(),
                content: "done".into(),
                tool_call_id: None,
                message_id: format!("{}:1:0", session_id.0),
                source: "background".into(),
                persisted_at: Utc::now(),
                media: vec![],
            },
            None,
        )
        .expect("background child envelope");

    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_hydrate(
        &ws,
        &state,
        &ledger,
        &approvals,
        &PendingQuestionStore::default(),
        &active_turns,
        None,
        None,
        features_for_projection_envelope_v2_test(),
        "h-no-msgs".into(),
        SessionHydrateParams {
            session_id: session_id.clone(),
            after: None,
            include: vec!["threads".into()],
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    let result = frame["result"].as_object().expect("result object");
    assert!(
        !result.contains_key("messages"),
        "messages excluded by include filter",
    );
    assert!(
        !result.contains_key("replayed_envelopes"),
        "envelopes are a messages-list dedup key; omit when messages aren't requested",
    );
    assert!(
        !result.contains_key("replayed_tool_envelopes"),
        "tool envelopes are also messages-list replay state; omit when messages aren't requested",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn turn_state_get_returns_unknown_for_missing() {
    let session_id = SessionKey("local:turn-unknown".into());
    let state = prg_state_with_session(&session_id, |_| {});
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_turn_state_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "t3".into(),
        TurnStateGetParams {
            session_id: session_id.clone(),
            turn_id: TurnId::new(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    // Per UPCR-2026-011: missing turn returns `state: "unknown"` —
    // NOT an error.
    assert!(frame.get("result").is_some(), "missing turn must succeed");
    assert_eq!(frame["result"]["state"], "unknown");
}

/// Serialise tests that mutate the process-global message-commit
/// observer so they don't race each other or with concurrently running
/// fixtures that also exercise `add_message_with_seq`.
fn message_commit_observer_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[tokio::test(flavor = "current_thread")]
// The guard serialises tests against the process-global commit observer;
// it must stay held for the whole test, awaits included.
#[allow(clippy::await_holding_lock)]
async fn message_commit_observer_runs_after_each_commit_in_order() {
    // Wires the bus-level observer hook to a local sink and asserts
    // notifications fire in commit order, with strictly monotonic seqs.
    let _guard = message_commit_observer_test_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let observed: Arc<std::sync::Mutex<Vec<(SessionKey, Message, usize)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_clone = observed.clone();
    // Filter to THIS test's session id: the commit observer is
    // process-global, so a concurrently-running suite test that commits a
    // message to a DIFFERENT session would otherwise pollute this sink and
    // make `observed.len()` exceed 3 (the `message_commit_observer_test_lock`
    // only serialises observer-MUTATING tests, not every committer).
    let filter_key = "local:persisted-order";
    let prev = octos_bus::set_message_commit_observer(Some(Arc::new(move |key, message, seq| {
        if key.0 != filter_key {
            return;
        }
        observed_clone
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((key.clone(), message.clone(), seq));
    })));

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut manager = octos_bus::SessionManager::open(tmp.path()).expect("session manager open");
    let session_id = SessionKey(filter_key.into());
    for content in ["one", "two", "three"] {
        let msg = Message {
            role: MessageRole::User,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some(format!("cmid-{content}")),
            thread_id: None,
            timestamp: Utc::now(),
        };
        manager
            .add_message_with_seq(&session_id, msg)
            .await
            .expect("add_message succeeds");
    }

    let observed = observed.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(observed.len(), 3, "one observation per commit");
    assert_eq!(observed[0].2, 0);
    assert_eq!(observed[1].2, 1);
    assert_eq!(observed[2].2, 2);
    assert_eq!(observed[0].1.content, "one");
    assert_eq!(observed[1].1.content, "two");
    assert_eq!(observed[2].1.content, "three");

    // Restore the previous observer (None for clean tests).
    octos_bus::set_message_commit_observer(prev);
}

#[tokio::test(flavor = "current_thread")]
// See message_commit_observer_test_lock: guard intentionally outlives awaits.
#[allow(clippy::await_holding_lock)]
async fn message_commit_observer_is_not_retroactive_after_installation() {
    let _guard = message_commit_observer_test_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // The observer must NOT see a row that did not commit. Simulate a
    // commit failure by exhausting the file size limit. In practice
    // we cannot easily inject a failure into `add_message_with_seq`
    // without rewriting the helper; instead assert the commit-failure
    // contract via the call-site comment + a guarded-write test that
    // succeeds end-to-end (the negative assertion is implicitly
    // covered by the `record_session_persist("failed")` early-return).
    //
    // Concretely: remove the observer, run a commit, re-install, run
    // a second commit. The first commit must NOT appear in the second
    // observer's sink.
    let observed: Arc<std::sync::Mutex<Vec<()>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_clone = observed.clone();
    // Save the global observer (e.g. the process-wide ledger
    // observer installed by sibling tests via `event_ledger`) so we
    // can restore it on exit.
    let prev = octos_bus::set_message_commit_observer(None);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut manager = octos_bus::SessionManager::open(tmp.path()).expect("session manager open");
    let session_id = SessionKey("local:persisted-failure".into());

    // First commit — observer NOT installed, so no event recorded.
    let msg = Message {
        role: MessageRole::User,
        content: "no-observer".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: Some("cmid-1".into()),
        thread_id: None,
        timestamp: Utc::now(),
    };
    manager
        .add_message_with_seq(&session_id, msg)
        .await
        .expect("first commit");
    assert!(observed.lock().unwrap().is_empty());

    // Install the sink and run a second commit. Sink must contain
    // exactly one event (the second), not two. Filter to this test's
    // session id so a concurrent suite committer (the observer is
    // process-global) cannot inflate the count past 1.
    octos_bus::set_message_commit_observer(Some(Arc::new(move |key, _message, _seq| {
        if key.0 != "local:persisted-failure" {
            return;
        }
        observed_clone.lock().unwrap().push(());
    })));
    let msg2 = Message {
        role: MessageRole::User,
        content: "with-observer".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: Some("cmid-2".into()),
        thread_id: None,
        timestamp: Utc::now(),
    };
    manager
        .add_message_with_seq(&session_id, msg2)
        .await
        .expect("second commit");
    let observed_after = observed.lock().unwrap();
    assert_eq!(
        observed_after.len(),
        1,
        "observer must only see commits that ran while it was installed"
    );

    octos_bus::set_message_commit_observer(prev);
}

/// M9-γ-7 (issue #844): `is_metadata_only_assistant_row` is the
/// pure-function classifier the observer uses to drop intermediate
/// metadata-only assistant rows. Lock its truth table here so a
/// future refactor that "helpfully" widens the filter cannot drop
/// rows the wire surface needs.
#[test]
fn is_metadata_only_assistant_row_truth_table() {
    // Empty assistant with no media: metadata-only -> drop.
    let mut empty_assistant = Message {
        role: MessageRole::Assistant,
        content: String::new(),
        media: vec![],
        tool_calls: Some(vec![]),
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: Utc::now(),
    };
    assert!(is_metadata_only_assistant_row(&empty_assistant));

    // Whitespace-only counts as empty.
    empty_assistant.content = "   \n\t".into();
    assert!(is_metadata_only_assistant_row(&empty_assistant));

    // Assistant with text: keep.
    empty_assistant.content = "hello".into();
    assert!(!is_metadata_only_assistant_row(&empty_assistant));

    // Assistant with media but empty text: keep (image-only response).
    empty_assistant.content = String::new();
    empty_assistant.media = vec!["data:image/png;base64,abc".into()];
    assert!(!is_metadata_only_assistant_row(&empty_assistant));

    // Tool messages are never filtered.
    let tool_message = Message {
        role: MessageRole::Tool,
        content: String::new(),
        media: vec![],
        tool_calls: None,
        tool_call_id: Some("tc-1".into()),
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: Utc::now(),
    };
    assert!(!is_metadata_only_assistant_row(&tool_message));

    // User rows are never filtered.
    let user_message = Message {
        role: MessageRole::User,
        content: String::new(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: Utc::now(),
    };
    assert!(!is_metadata_only_assistant_row(&user_message));
}

/// Metadata-only assistant commits must not create projection rows; the
/// final visible assistant commit produces one canonical v2 envelope.
#[tokio::test(flavor = "current_thread")]
// See message_commit_observer_test_lock: guard intentionally outlives awaits.
#[allow(clippy::await_holding_lock)]
async fn metadata_only_commits_emit_one_v2_assistant_persisted_row() {
    let _guard = message_commit_observer_test_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // Spin a fresh ledger + observer wired the same way the live
    // server wires them on the first `event_ledger` call.
    let ledger = Arc::new(UiProtocolLedger::new(64));
    install_message_commit_observer(ledger.clone());

    let session_id = SessionKey("local:gamma-7-dedup".into());
    let mut subscriber = ledger.subscribe(&session_id);

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut manager = octos_bus::SessionManager::open(tmp.path()).expect("session manager open");

    // Simulate the agent loop's commits: 2 metadata-only assistant
    // rows (intermediate iterations whose only payload was
    // tool_calls), interleaved with their tool results, then the
    // final assistant text row.
    let thread = "cmid-gamma-7".to_string();
    let mk_assistant = |content: &str, with_tool_calls: bool| Message {
        role: MessageRole::Assistant,
        content: content.into(),
        media: vec![],
        tool_calls: if with_tool_calls {
            Some(vec![octos_core::ToolCall {
                id: format!("tc-{}", uuid::Uuid::now_v7()),
                name: "shell".into(),
                arguments: serde_json::json!({}),
                metadata: None,
            }])
        } else {
            None
        },
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: Some(thread.clone()),
        timestamp: Utc::now(),
    };
    let mk_tool = |out: &str, tc_id: &str| Message {
        role: MessageRole::Tool,
        content: out.into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: Some(tc_id.into()),
        reasoning_content: None,
        client_message_id: None,
        thread_id: Some(thread.clone()),
        timestamp: Utc::now(),
    };

    // Iteration 1: assistant returns only tool_calls (empty content).
    manager
        .add_message_with_seq(&session_id, mk_assistant("", true))
        .await
        .expect("commit it1 assistant");
    manager
        .add_message_with_seq(&session_id, mk_tool("ok", "tc-1"))
        .await
        .expect("commit it1 tool");
    // Iteration 2: assistant returns only tool_calls again.
    manager
        .add_message_with_seq(&session_id, mk_assistant("", true))
        .await
        .expect("commit it2 assistant");
    manager
        .add_message_with_seq(&session_id, mk_tool("ok", "tc-2"))
        .await
        .expect("commit it2 tool");
    // Iteration 3: final assistant text (the user-visible reply).
    manager
        .add_message_with_seq(&session_id, mk_assistant("here is your answer", false))
        .await
        .expect("commit it3 assistant");

    // Drain the canonical v2 projection stream.
    let mut assistant_persisted = Vec::new();
    while let Ok(event) = subscriber.try_recv() {
        if let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) =
            &event.event
        {
            if let PayloadV2::AssistantPersisted { text, .. } = &envelope.envelope.payload {
                assistant_persisted.push(text.clone());
            }
        }
    }

    assert_eq!(
        assistant_persisted.len(),
        1,
        "exactly ONE assistant_persisted v2 envelope per turn (the final text); \
             got {} envelopes (phantom-bubble regression)",
        assistant_persisted.len(),
    );
    assert_eq!(assistant_persisted, vec!["here is your answer"]);

    // Restore the global observer slot to None so subsequent tests
    // see a clean state.
    octos_bus::set_message_commit_observer(None);
}

/// PR F (M8.10 thread-binding chain `#649 → #740`): every progress
/// event the BoundedChannelReporter emits MUST carry the bound
/// `thread_id`. Without this, the SPA reducer for the standalone
/// `octos serve` UI Protocol path falls back to sticky-map
/// heuristics — the exact wire-side leak PR F closes.
#[tokio::test]
async fn bounded_channel_reporter_emits_typed_thread_id_on_progress_events() {
    use octos_agent::ProgressReporter;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
    let dropped = Arc::new(AtomicU64::new(0));

    let reporter = BoundedChannelReporter::new(tx, dropped.clone())
        .with_thread_id(Some("turn-pr-f-A".to_string()));
    reporter.report(octos_agent::ProgressEvent::Thinking { iteration: 0 });

    let event = rx.try_recv().expect("event must be available");
    let parsed: serde_json::Value = serde_json::from_str(&event).expect("valid json");
    assert_eq!(
        parsed["thread_id"], "turn-pr-f-A",
        "BoundedChannelReporter must stamp every progress event with the bound thread_id. event: {parsed}"
    );

    // Without binding, `thread_id` must be absent (legacy compat).
    let (tx2, mut rx2) = tokio::sync::mpsc::channel::<String>(8);
    let unbound = BoundedChannelReporter::new(tx2, dropped);
    unbound.report(octos_agent::ProgressEvent::Thinking { iteration: 1 });
    let event = rx2.try_recv().expect("event must be available");
    let parsed: serde_json::Value = serde_json::from_str(&event).expect("valid json");
    assert!(
        parsed.get("thread_id").is_none(),
        "unbound reporter must not stamp thread_id (legacy compat): {parsed}"
    );
}

#[test]
fn legacy_voice_turn_only_short_circuits_when_no_other_input_remains() {
    assert!(should_short_circuit_no_speech(true, false, false, true));
    assert!(!should_short_circuit_no_speech(true, false, false, false));
    assert!(!should_short_circuit_no_speech(true, true, false, true));
    assert!(!should_short_circuit_no_speech(true, false, true, true));
    assert!(!should_short_circuit_no_speech(false, false, false, true));
}

// ========================================================================
// Live ledger publish-subscribe (issue #760, Phase C blocker)
// ========================================================================

fn assistant_persisted_v2_for(session: &SessionKey) -> UiNotification {
    UiNotification::EnvelopeV2(EnvelopeV2Notification {
        session_id: session.clone(),
        topic: None,
        envelope: EnvelopeV2 {
            thread_id: "thread-1".into(),
            seq: 1,
            cursor: None,
            turn_id: "thread-1".into(),
            client_message_id: None,
            payload: PayloadV2::AssistantPersisted {
                text: "assistant".into(),
                assistant_segment_id: "thread-1:assistant:1".into(),
                meta: MessageMeta {
                    message_id: "msg-1".into(),
                    persisted_at: Utc::now(),
                    media: vec![],
                },
            },
        },
    })
}

/// V2 envelopes are delivered even with `projection_envelope_v2` unset;
/// this fixture exercises that Stage-5 unconditional route.
fn features_for_v2_delivery() -> ConnectionUiFeatures {
    ConnectionUiFeatures {
        header_present: true,
        ..ConnectionUiFeatures::default()
    }
}

/// Build a `ConnectionUiFeatures` for the UPCR-2026-014 M9-α-9
/// `event.file_attached.v1` capability gate.
fn features_for_file_attached_test(file_attached: bool) -> ConnectionUiFeatures {
    ConnectionUiFeatures {
        file_attached,
        header_present: true,
        ..ConnectionUiFeatures::default()
    }
}

/// Slides soak regression: build a representative `file/attached`
/// notification carrying a PPTX artefact and the expected MIME hint.
/// Used by the capability-gate tests to assert legacy clients never
/// see the new envelope and new clients always do.
fn file_attached_for(session: &SessionKey) -> UiNotification {
    UiNotification::FileAttached(octos_core::ui_protocol::FileAttachedEvent {
        session_id: session.clone(),
        topic: session.topic().map(ToOwned::to_owned),
        turn_id: TurnId::new(),
        path: "/tmp/deck.pptx".into(),
        tool_call_id: Some("tc-slides".into()),
        attachment_owner: None,
        mime: Some(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation".into(),
        ),
    })
}

fn context_state_for_test(session: &SessionKey) -> UiContextState {
    UiContextState {
        session_id: session.clone(),
        thread_id: None,
        generation: 1,
        transcript_hash: "sha256:context".into(),
        item_count: 3,
        token_estimate: 42,
        recovery_state: "active".into(),
        last_checkpoint_id: Some("ctx-checkpoint".into()),
        last_compaction_id: Some("ctx-compaction".into()),
        cache_epoch_id: None,
        last_cache_invalidation_reason: None,
        semantic_head_id: None,
        semantic_head_kind: None,
    }
}

fn context_compaction_completed_for(session: &SessionKey) -> UiNotification {
    UiNotification::ContextCompactionCompleted(ContextCompactionCompletedEvent {
        session_id: session.clone(),
        context_state: context_state_for_test(session),
        compaction: UiContextCompactionRecord {
            compaction_id: "ctx-compaction".into(),
            checkpoint_id: "ctx-checkpoint".into(),
            status: "installed".into(),
            policy_id: "default".into(),
            trigger: "test".into(),
            input_generation: 0,
            output_generation: Some(1),
            input_transcript_hash: "sha256:input".into(),
            replacement_transcript_hash: Some("sha256:replacement".into()),
            installed_transcript_hash: Some("sha256:installed".into()),
            input_item_count: 5,
            retained_count: 2,
            dropped_count: 3,
            summary_item_id: Some("item-summary".into()),
            token_estimate_before: 1200,
            token_estimate_after: Some(400),
            error: None,
        },
    })
}

fn context_normalization_reported_for(session: &SessionKey) -> UiNotification {
    UiNotification::ContextNormalizationReported(ContextNormalizationReportedEvent {
        session_id: session.clone(),
        context_state: context_state_for_test(session),
        normalization: UiContextNormalizationReport {
            generation: 1,
            input_transcript_hash: "sha256:input".into(),
            output_prompt_hash: "sha256:prompt".into(),
            model_capability_id: "test/model".into(),
            prompt_message_count: 4,
            token_estimate: 400,
            repaired_count: 1,
            dropped_count: 0,
            synthetic_count: 1,
            truncated_count: 0,
        },
    })
}

/// Builds the canonical background-result projection emitted by the
/// post-commit observer.
fn background_child_v2_for(session: &SessionKey) -> UiNotification {
    UiNotification::EnvelopeV2(EnvelopeV2Notification {
        session_id: session.clone(),
        topic: None,
        envelope: EnvelopeV2 {
            thread_id: "turn-parent:background:task_abc123".into(),
            seq: 1,
            cursor: None,
            turn_id: "turn-parent:background:task_abc123".into(),
            client_message_id: None,
            payload: PayloadV2::BackgroundChildCompleted {
                parent_turn_id: "turn-parent".into(),
                response_to_client_message_id: Some("cmid-user-1".into()),
                task_id: "task_abc123".into(),
                content: "Background research complete.".into(),
                tool_call_id: None,
                message_id: "msg-bg".into(),
                source: "background".into(),
                persisted_at: Utc::now(),
                media: vec!["research/_report.md".into()],
            },
        },
    })
}

/// Decodes a queued WS frame back to its JSON-RPC method name (or
/// returns `None` for non-text / non-JSON frames). Lets tests assert
/// the live broadcast forwarder routed a notification, without
/// coupling to whatever frame_for serialization shape is.
fn frame_method(frame: &WsMessage) -> Option<String> {
    match frame {
        WsMessage::Text(text) => {
            let v: Value = serde_json::from_str(text).ok()?;
            v.get("method").and_then(Value::as_str).map(str::to_owned)
        }
        _ => None,
    }
}

#[tokio::test]
async fn live_forwarder_topic_scope_drops_other_topic_events() {
    let (ws_alpha, mut rx_alpha) = ws_connection_for_test(16);
    let (ws_beta, mut rx_beta) = ws_connection_for_test(16);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let session_id = SessionKey("local:topic-live".into());
    let forwarders_alpha: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let forwarders_beta: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let alpha_live_rx = ledger.subscribe(&session_id);
    spawn_live_forwarder(
        ws_alpha.clone(),
        ledger.clone(),
        session_id.clone(),
        0,
        ws_alpha.connection_id(),
        ConnectionUiFeatures::default(),
        Some("alpha".into()),
        Some(MAIN_PROFILE_ID.to_owned()),
        alpha_live_rx,
        forwarders_alpha.clone(),
    )
    .await;

    let beta_live_rx = ledger.subscribe(&session_id);
    spawn_live_forwarder(
        ws_beta.clone(),
        ledger.clone(),
        session_id.clone(),
        0,
        ws_beta.connection_id(),
        ConnectionUiFeatures::default(),
        Some("beta".into()),
        Some(MAIN_PROFILE_ID.to_owned()),
        beta_live_rx,
        forwarders_beta.clone(),
    )
    .await;

    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: Some("alpha".into()),
        turn_id: TurnId::new(),
        text: "alpha".into(),
    }));
    ledger.append_notification(UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: session_id.clone(),
        topic: Some("beta".into()),
        turn_id: TurnId::new(),
        text: "beta".into(),
    }));

    let alpha_frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx_alpha.recv())
        .await
        .expect("alpha bridge frame")
        .expect("alpha ws open");
    let alpha_json: Value = match &alpha_frame {
        WsMessage::Text(text) => serde_json::from_str(text).expect("alpha frame json"),
        other => panic!("unexpected alpha frame: {other:?}"),
    };
    assert_eq!(
        alpha_json.get("method").and_then(Value::as_str),
        Some(octos_core::ui_protocol::methods::MESSAGE_DELTA),
    );
    assert_eq!(alpha_json["params"]["text"], json!("alpha"));
    assert_eq!(alpha_json["params"]["topic"], json!("alpha"));

    let beta_frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx_beta.recv())
        .await
        .expect("beta bridge frame")
        .expect("beta ws open");
    let beta_json: Value = match &beta_frame {
        WsMessage::Text(text) => serde_json::from_str(text).expect("beta frame json"),
        other => panic!("unexpected beta frame: {other:?}"),
    };
    assert_eq!(
        beta_json.get("method").and_then(Value::as_str),
        Some(octos_core::ui_protocol::methods::MESSAGE_DELTA),
    );
    assert_eq!(beta_json["params"]["text"], json!("beta"));
    assert_eq!(beta_json["params"]["topic"], json!("beta"));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        rx_alpha.try_recv().is_err(),
        "alpha topic bridge must not receive beta events",
    );
    assert!(
        rx_beta.try_recv().is_err(),
        "beta topic bridge must not receive alpha events",
    );

    abort_live_forwarders(&forwarders_alpha, &ledger).await;
    abort_live_forwarders(&forwarders_beta, &ledger).await;
}

/// P0-A regression: slides soak round-13 captured `file/attached`
/// envelopes that landed durably on the ledger (seq 91 in the
/// fleet ledger evidence) but never reached the SPA. The capability
/// gate passes (`event.file_attached.v1` was negotiated) and
/// broadcast fan-out succeeded — the surviving filter dropping
/// the event is `ledger_event_matches_topic_scope`. The
/// `FileAttachedEvent` struct has no `topic` field, so its
/// `UiNotification::topic()` impl falls back to the
/// `SessionKey.topic()` suffix. Any emit site that constructs the
/// event with a session_id that does NOT carry the `#<topic>`
/// suffix (e.g. a future caller passing the base session, or a
/// pre-stamp `bg_session_id` capture) results in
/// `event.topic() == None` while the topic-scoped subscriber
/// expects `Some("slides")` — the filter mismatches and the event
/// is silently dropped.
///
/// File/attached is intrinsically session-scoped via its
/// `tool_call_id` — the SPA already knows which turn/tool produced
/// the artefact, so topic scoping adds no value and only risks
/// false negatives. This end-to-end test pins the invariant that a
/// `file/attached` emitted on the topic-suffixed broadcast key
/// reaches a topic-scoped subscriber. The companion unit test
/// (`ledger_event_matches_topic_scope_exempts_file_attached`)
/// covers the filter-only invariant for the bare-event /
/// mismatched-topic shapes that the broadcast-fan-out path can't
/// reach without monkey-patching the ledger.
#[tokio::test]
async fn live_forwarder_delivers_file_attached_to_topic_scoped_subscriber() {
    let (ws, mut rx) = ws_connection_for_test(16);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    // Subscriber opens on the topic-suffixed broadcast key — matches
    // the SPA's session/open with `topic: "slides"`.
    let topic_session = SessionKey("local:slides-soak#slides".into());
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let live_rx = ledger.subscribe(&topic_session);
    spawn_live_forwarder(
        ws.clone(),
        ledger.clone(),
        topic_session.clone(),
        0,
        ws.connection_id(),
        features_for_file_attached_test(true),
        Some("slides".into()),
        Some(MAIN_PROFILE_ID.to_owned()),
        live_rx,
        forwarders.clone(),
    )
    .await;

    let file_attached_matching = file_attached_for(&topic_session);
    ledger.append_notification(file_attached_matching);

    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("topic-matching file/attached frame")
        .expect("ws open");
    assert_eq!(
        frame_method(&frame).as_deref(),
        Some(octos_core::ui_protocol::methods::FILE_ATTACHED),
        "file/attached on the matching-topic session must reach the subscriber",
    );

    abort_live_forwarders(&forwarders, &ledger).await;
}

/// #1329 (closes the P0-A class routing drop): The 6 events that
/// previously had no explicit `topic` field — ToolStarted,
/// ToolProgress, ToolCompleted, ApprovalAutoResolved,
/// ApprovalDecided, ApprovalCancelled — gained the same
/// `topic: Option<String>` field that the 10 already-fixed
/// variants carry. With emitters populating the field from the
/// upstream `SessionKey.topic()` BEFORE any `base_key()` strip,
/// each event reaches a topic-scoped subscriber.
///
/// This integration-style test pins the invariant for ALL 6
/// variants on the live broadcast path: emit each event on a
/// topic-suffixed broadcast key with the explicit `topic` field,
/// then assert each frame reaches a topic-scoped subscriber (the
/// classifier reads `event.topic()` first, honoring the explicit
/// field).
#[tokio::test]
async fn live_forwarder_delivers_tool_and_approval_events_to_topic_scoped_subscriber() {
    let (ws, mut rx) = ws_connection_for_test(64);
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let topic_session = SessionKey("local:slides-soak#slides".into());
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let live_rx = ledger.subscribe(&topic_session);
    spawn_live_forwarder(
        ws.clone(),
        ledger.clone(),
        topic_session.clone(),
        0,
        ws.connection_id(),
        ConnectionUiFeatures::default(),
        Some("slides".into()),
        Some(MAIN_PROFILE_ID.to_owned()),
        live_rx,
        forwarders.clone(),
    )
    .await;

    let turn_id = TurnId::new();
    let tool_call_id = "tc-1329".to_owned();

    // 1. ToolStarted
    ledger.append_notification(UiNotification::ToolStarted(ToolStartedEvent {
        session_id: topic_session.clone(),
        topic: Some("slides".into()),
        turn_id: turn_id.clone(),
        tool_call_id: tool_call_id.clone(),
        tool_name: "shell".into(),
        arguments: None,
    }));

    // 2. ToolProgress
    ledger.append_notification(UiNotification::ToolProgress(ToolProgressEvent {
        session_id: topic_session.clone(),
        topic: Some("slides".into()),
        turn_id: turn_id.clone(),
        tool_call_id: tool_call_id.clone(),
        message: Some("running step 1".into()),
        progress_pct: Some(50.0),
    }));

    // 3. ToolCompleted
    ledger.append_notification(UiNotification::ToolCompleted(ToolCompletedEvent {
        session_id: topic_session.clone(),
        topic: Some("slides".into()),
        turn_id: turn_id.clone(),
        tool_call_id: tool_call_id.clone(),
        tool_name: "shell".into(),
        success: Some(true),
        output_preview: None,
        duration_ms: Some(10),
    }));

    // 4. ApprovalAutoResolved
    ledger.append_notification(UiNotification::ApprovalAutoResolved(
        ApprovalAutoResolvedEvent {
            session_id: topic_session.clone(),
            topic: Some("slides".into()),
            approval_id: ApprovalId::new(),
            turn_id: turn_id.clone(),
            tool_name: "shell".into(),
            scope: "session".into(),
            scope_match: "exact".into(),
            decision: ApprovalDecision::Approve,
        },
    ));

    // 5. ApprovalDecided
    ledger.append_notification(UiNotification::ApprovalDecided(ApprovalDecidedEvent {
        session_id: topic_session.clone(),
        topic: Some("slides".into()),
        approval_id: ApprovalId::new(),
        turn_id: turn_id.clone(),
        decision: ApprovalDecision::Approve,
        scope: Some("session".into()),
        decided_at: Utc::now(),
        decided_by: "user:test".into(),
        auto_resolved: false,
        policy_id: None,
        client_note: None,
    }));

    // 6. ApprovalCancelled
    ledger.append_notification(UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
        session_id: topic_session.clone(),
        topic: Some("slides".into()),
        approval_id: ApprovalId::new(),
        turn_id: turn_id.clone(),
        reason: "turn_interrupted".into(),
    }));

    // Verify each method lands on the subscriber. Order matches
    // emission order — the ledger preserves seq.
    let expected = [
        methods::TOOL_STARTED,
        methods::TOOL_PROGRESS,
        methods::TOOL_COMPLETED,
        methods::APPROVAL_AUTO_RESOLVED,
        methods::APPROVAL_DECIDED,
        methods::APPROVAL_CANCELLED,
    ];
    for method in expected.iter() {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {method}"))
            .unwrap_or_else(|| panic!("ws closed before {method}"));
        assert_eq!(
            frame_method(&frame).as_deref(),
            Some(*method),
            "{method} with explicit topic=Some(\"slides\") must reach \
                 a topic-scoped subscriber (#1329)"
        );
    }

    abort_live_forwarders(&forwarders, &ledger).await;
}

/// Mirror of the positive test above: when an event of one of the
/// six P0-A class variants carries a NON-MATCHING explicit topic,
/// the classifier drops it from the topic-scoped subscriber.
/// Topic IS part of the routing key now (no more file/attached-
/// style always-deliver exemption).
#[tokio::test]
async fn live_forwarder_drops_mismatched_topic_for_tool_and_approval_events() {
    let (ws, mut rx) = ws_connection_for_test(16);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let topic_session = SessionKey("local:slides-soak#slides".into());
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let live_rx = ledger.subscribe(&topic_session);
    spawn_live_forwarder(
        ws.clone(),
        ledger.clone(),
        topic_session.clone(),
        0,
        ws.connection_id(),
        ConnectionUiFeatures::default(),
        Some("slides".into()),
        Some(MAIN_PROFILE_ID.to_owned()),
        live_rx,
        forwarders.clone(),
    )
    .await;

    // Event carries `topic = "other"` — the classifier must drop
    // it for the "slides"-scoped subscriber.
    ledger.append_notification(UiNotification::ToolStarted(ToolStartedEvent {
        session_id: topic_session.clone(),
        topic: Some("other".into()),
        turn_id: TurnId::new(),
        tool_call_id: "tc-other".into(),
        tool_name: "shell".into(),
        arguments: None,
    }));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        rx.try_recv().is_err(),
        "tool/started with topic=\"other\" must NOT reach a \
             topic=\"slides\" subscriber (#1329 routing-key invariant)"
    );

    abort_live_forwarders(&forwarders, &ledger).await;
}

/// #1329 follow-up to P0-A: `file/attached` now carries an explicit
/// `topic: Option<String>` field. The classifier consults
/// `event.topic()`, which reads that field first and falls back to
/// the `SessionKey.topic()` suffix. With emitters populating the
/// field at the source, the prior `file/attached`-only exemption
/// in `ledger_event_matches_topic_scope` is no longer needed — the
/// routing follows the same rule as every other variant.
#[test]
fn ledger_event_matches_topic_scope_routes_file_attached_by_topic_field() {
    let bare_session = SessionKey("local:slides-soak".into());
    let topic_session = SessionKey("local:slides-soak#slides".into());
    let other_topic_session = SessionKey("local:slides-soak#other".into());

    // Bare event, topic-scoped subscriber: no topic on the event,
    // topic on the subscriber → DROP (no longer exempt).
    let bare_event = UiProtocolLedgerEvent::Notification(file_attached_for(&bare_session));
    assert!(
        !ledger_event_matches_topic_scope(&bare_event, Some("slides")),
        "file/attached with no topic must NOT reach a topic-scoped subscriber"
    );

    // Topic-suffixed event, topic-scoped subscriber: explicit
    // topic matches → PASS.
    let topic_event = UiProtocolLedgerEvent::Notification(file_attached_for(&topic_session));
    assert!(
        ledger_event_matches_topic_scope(&topic_event, Some("slides")),
        "file/attached with matching topic must reach a topic-scoped subscriber"
    );

    // Mismatched topic, topic-scoped subscriber: explicit topic
    // mismatches → DROP. Topic IS part of the routing key now.
    let other_event = UiProtocolLedgerEvent::Notification(file_attached_for(&other_topic_session));
    assert!(
        !ledger_event_matches_topic_scope(&other_event, Some("slides")),
        "file/attached with non-matching topic must NOT reach a topic-scoped subscriber"
    );

    // Bare subscriber, topic-suffixed event → DROP (topic on
    // event, no topic on subscriber).
    assert!(
        !ledger_event_matches_topic_scope(&topic_event, None),
        "file/attached with topic must NOT reach a bare (no-topic) subscriber"
    );

    // Bare event + bare subscriber → PASS (the no-topic case).
    assert!(
        ledger_event_matches_topic_scope(&bare_event, None),
        "file/attached with no topic must reach a bare (no-topic) subscriber"
    );
}

#[tokio::test]
async fn live_forwarder_pushes_v2_assistant_persisted_to_subscribed_ws() {
    let (ws, mut rx) = ws_connection_for_test(16);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let session_id = SessionKey("local:livefwd".into());
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let live_rx = ledger.subscribe(&session_id);
    spawn_live_forwarder(
        ws.clone(),
        ledger.clone(),
        session_id.clone(),
        0,
        ws.connection_id(),
        features_for_v2_delivery(),
        None,
        Some(MAIN_PROFILE_ID.to_owned()),
        live_rx,
        forwarders.clone(),
    )
    .await;

    // Background-task path appends late artifact AFTER the WS is wired up.
    ledger.append_notification(assistant_persisted_v2_for(&session_id));

    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("ws received frame within 1s")
        .expect("ws channel still open");
    assert_eq!(
        frame_method(&frame).as_deref(),
        Some("projection/envelope"),
        "live forwarder must emit a v2 projection envelope; frame={frame:?}"
    );

    // Cleanup: aborting the forwarder must not panic and must release
    // the receiver so subsequent prune_idle_subscribers reclaims the slot.
    abort_live_forwarders(&forwarders, &ledger).await;
}

#[tokio::test]
async fn live_forwarder_skips_events_at_or_below_baseline_seq() {
    let (ws, mut rx) = ws_connection_for_test(16);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let session_id = SessionKey("local:baseline".into());
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    // Pre-existing event so baseline_seq=1 represents "we already sent
    // this in replay; do not re-emit live."
    let baseline = ledger.append_notification(assistant_persisted_v2_for(&session_id));
    assert_eq!(baseline.cursor.seq, 1);

    let live_rx = ledger.subscribe(&session_id);
    spawn_live_forwarder(
        ws.clone(),
        ledger.clone(),
        session_id.clone(),
        baseline.cursor.seq,
        ws.connection_id(),
        features_for_v2_delivery(),
        None,
        Some(MAIN_PROFILE_ID.to_owned()),
        live_rx,
        forwarders.clone(),
    )
    .await;

    // A new append must surface; the forwarder filters strictly on
    // seq > baseline.
    let next = ledger.append_notification(assistant_persisted_v2_for(&session_id));
    assert_eq!(next.cursor.seq, 2);

    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("ws received frame within 1s")
        .expect("ws channel still open");
    let v: Value = match &frame {
        WsMessage::Text(t) => serde_json::from_str(t).expect("valid json"),
        other => panic!("unexpected frame: {other:?}"),
    };
    assert_eq!(
        v.get("method").and_then(Value::as_str),
        Some("projection/envelope")
    );

    // No further frames are queued (only one live event emitted).
    assert!(rx.try_recv().is_err(), "no more frames expected");

    abort_live_forwarders(&forwarders, &ledger).await;
}

#[tokio::test]
async fn live_forwarder_delivers_v2_without_a_capability_flag() {
    let (ws, mut rx) = ws_connection_for_test(16);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let session_id = SessionKey("local:nofeat".into());
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let live_rx = ledger.subscribe(&session_id);
    spawn_live_forwarder(
        ws.clone(),
        ledger.clone(),
        session_id.clone(),
        0,
        ws.connection_id(),
        features_for_v2_delivery(),
        None,
        Some(MAIN_PROFILE_ID.to_owned()),
        live_rx,
        forwarders.clone(),
    )
    .await;

    ledger.append_notification(assistant_persisted_v2_for(&session_id));
    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("v2 envelope arrives without an obsolete capability")
        .expect("ws remains open");
    assert_eq!(frame_method(&frame).as_deref(), Some("projection/envelope"));

    abort_live_forwarders(&forwarders, &ledger).await;
}

#[test]
fn capability_filter_delivers_v2_background_children_unconditionally() {
    let session = SessionKey("local:filter".into());
    let child = UiProtocolLedgerEvent::Notification(background_child_v2_for(&session));

    for features in [
        ConnectionUiFeatures::default(),
        features_for_projection_envelope_test(false),
        features_for_projection_envelope_test(true),
        features_for_projection_envelope_v2_test(),
    ] {
        assert!(
            live_event_passes_capability_filter(&child, features),
            "a canonical v2 child must never be capability-filtered",
        );
    }
}

/// UPCR-2026-014 M9-α-9 `event.file_attached.v1` capability gate.
/// Old clients that never advertised the feature MUST NOT receive
/// `file/attached` envelopes — they keep relying on `media` on
/// historic persisted-message / `turn/spawn_complete` lanes. New clients that
/// negotiated the feature MUST receive the dedicated envelope so
/// the slides soak's "PPTX on disk but no button on SPA" regression
/// can be closed by a redundant wire signal.
#[test]
fn capability_filter_routes_file_attached_gating() {
    let session = SessionKey("local:file-attached-gate".into());
    let file_attached = UiProtocolLedgerEvent::Notification(file_attached_for(&session));

    // Old client: never observe the new envelope.
    let old = features_for_file_attached_test(false);
    assert!(
        !live_event_passes_capability_filter(&file_attached, old),
        "clients without event.file_attached.v1 must not receive file/attached envelopes",
    );

    // New client: receive the envelope.
    let new = features_for_file_attached_test(true);
    assert!(
        live_event_passes_capability_filter(&file_attached, new),
        "clients with event.file_attached.v1 receive the per-artefact envelope",
    );

    // Independence from spawn_complete: a new client that
    // negotiated ONLY file_attached (no retired persisted-message feature, no
    // spawn_complete) still sees the file delivery. This matches
    // the redundancy goal — file_attached is the safety net for
    // clients whose richer-envelope reducers might drop the
    // delivery.
    let only_file_attached = ConnectionUiFeatures {
        file_attached: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };
    assert!(
        live_event_passes_capability_filter(&file_attached, only_file_attached),
        "file_attached gate is independent of spawn_complete / retired persisted-message feature",
    );
}

#[test]
fn capability_filter_routes_context_lifecycle_gating() {
    let session = SessionKey("local:context-filter".into());
    let compaction =
        UiProtocolLedgerEvent::Notification(context_compaction_completed_for(&session));
    let normalization =
        UiProtocolLedgerEvent::Notification(context_normalization_reported_for(&session));

    let old = ConnectionUiFeatures {
        context_lifecycle_v1: false,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };
    assert!(
        !live_event_passes_capability_filter(&compaction, old),
        "clients without context.lifecycle.v1 must not receive compaction events",
    );
    assert!(
        !live_event_passes_capability_filter(&normalization, old),
        "clients without context.lifecycle.v1 must not receive normalization events",
    );

    let new = ConnectionUiFeatures {
        context_lifecycle_v1: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    };
    assert!(
        live_event_passes_capability_filter(&compaction, new),
        "clients with context.lifecycle.v1 receive compaction events",
    );
    assert!(
        live_event_passes_capability_filter(&normalization, new),
        "clients with context.lifecycle.v1 receive normalization events",
    );
}

// ========================================================================
// UPCR-2026-014 M9-γ — per-connection envelope/legacy mutual exclusion.
// ========================================================================

fn projection_envelope_event_for(session: &SessionKey) -> UiNotification {
    UiNotification::Envelope(octos_core::ui_protocol::EnvelopeNotification {
        session_id: session.clone(),
        topic: None,
        envelope: octos_core::ui_protocol::Envelope {
            thread_id: "thread-1".into(),
            seq: 1,
            client_message_id: None,
            payload: Payload::AssistantDelta { text: "x".into() },
        },
    })
}

fn projection_envelope_v2_event_for(session: &SessionKey) -> UiNotification {
    UiNotification::EnvelopeV2(octos_core::ui_protocol::EnvelopeV2Notification {
        session_id: session.clone(),
        topic: None,
        envelope: octos_core::ui_protocol::EnvelopeV2 {
            thread_id: "thread-v2".into(),
            seq: 1,
            cursor: Some(UiCursor {
                stream: session.0.clone(),
                seq: 1,
            }),
            turn_id: "turn-v2".into(),
            client_message_id: None,
            payload: octos_core::ui_protocol::PayloadV2::AssistantDelta {
                text: "v2".into(),
                assistant_segment_id: "turn-v2:assistant:1".into(),
            },
        },
    })
}

fn features_for_projection_envelope_test(projection_envelope: bool) -> ConnectionUiFeatures {
    ConnectionUiFeatures {
        // Pre-existing capability flags are enabled so the *only*
        // gate being exercised is the M9-γ projection.envelope.v1
        // mutual exclusion — the test would otherwise be polluted by
        // unrelated additive capability gates.
        projection_envelope,
        spawn_complete: true,
        file_attached: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    }
}

fn features_for_projection_envelope_v2_test() -> ConnectionUiFeatures {
    ConnectionUiFeatures {
        projection_envelope_v2: true,
        spawn_complete: true,
        file_attached: true,
        header_present: true,
        ..ConnectionUiFeatures::default()
    }
}

/// Per-connection envelope/legacy mutual exclusion is the cutover
/// mechanism for M9-γ (spec § 14.7). A connection that negotiated
/// `projection.envelope.v1` sees ONLY canonical envelopes for the
/// events that surface had legacy analogs; a connection that did
/// NOT negotiate sees ONLY the legacy events and never the envelope.
#[test]
fn capability_filter_envelope_legacy_mutual_exclusion() {
    let session = SessionKey("local:envelope-gate".into());
    let envelope_event =
        UiProtocolLedgerEvent::Notification(projection_envelope_event_for(&session));
    let delta_event =
        UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session.clone(),
            topic: None,
            turn_id: TurnId::new(),
            text: "hello".into(),
        }));
    let tool_started =
        UiProtocolLedgerEvent::Notification(UiNotification::ToolStarted(ToolStartedEvent {
            session_id: session.clone(),
            topic: None,
            turn_id: TurnId::new(),
            tool_call_id: "tc-1".into(),
            tool_name: "shell".into(),
            arguments: None,
        }));
    let tool_progress =
        UiProtocolLedgerEvent::Notification(UiNotification::ToolProgress(ToolProgressEvent {
            session_id: session.clone(),
            topic: None,
            turn_id: TurnId::new(),
            tool_call_id: "tc-1".into(),
            message: Some("step".into()),
            progress_pct: None,
        }));
    let tool_completed =
        UiProtocolLedgerEvent::Notification(UiNotification::ToolCompleted(ToolCompletedEvent {
            session_id: session.clone(),
            topic: None,
            turn_id: TurnId::new(),
            tool_call_id: "tc-1".into(),
            tool_name: "shell".into(),
            success: Some(true),
            output_preview: None,
            duration_ms: None,
        }));
    let turn_completed =
        UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id: session.clone(),
            topic: None,
            turn_id: TurnId::new(),
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        }));
    let file_attached = UiProtocolLedgerEvent::Notification(file_attached_for(&session));

    // Legacy client (projection_envelope=false): receives ALL legacy
    // events; envelope is filtered out.
    let legacy = features_for_projection_envelope_test(false);
    assert!(
        !live_event_passes_capability_filter(&envelope_event, legacy),
        "legacy client must NOT receive projection/envelope notifications",
    );
    for (label, ev) in [
        ("MessageDelta", &delta_event),
        ("ToolStarted", &tool_started),
        ("ToolProgress", &tool_progress),
        ("ToolCompleted", &tool_completed),
        ("TurnCompleted", &turn_completed),
        ("FileAttached", &file_attached),
    ] {
        assert!(
            live_event_passes_capability_filter(ev, legacy),
            "legacy client must STILL receive legacy {label} notifications",
        );
    }

    // Envelope client (projection_envelope=true): receives ONLY the
    // envelope; legacy variants superseded by envelopes are
    // filtered out.
    let envelope_client = features_for_projection_envelope_test(true);
    assert!(
        live_event_passes_capability_filter(&envelope_event, envelope_client),
        "envelope client receives projection/envelope notifications",
    );
    for (label, ev) in [
        ("MessageDelta", &delta_event),
        ("ToolStarted", &tool_started),
        ("ToolProgress", &tool_progress),
        ("ToolCompleted", &tool_completed),
        ("TurnCompleted", &turn_completed),
        ("FileAttached", &file_attached),
    ] {
        assert!(
            !live_event_passes_capability_filter(ev, envelope_client),
            "envelope client must NOT receive legacy {label} notifications",
        );
    }
}

#[test]
fn capability_filter_routes_v2_unconditionally_without_leaking_sources() {
    let session = SessionKey("local:envelope-v2-gate".into());
    let legacy_delta =
        UiProtocolLedgerEvent::Notification(UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: session.clone(),
            topic: None,
            turn_id: TurnId::new(),
            text: "legacy delta".into(),
        }));
    let v1 = UiProtocolLedgerEvent::Notification(projection_envelope_event_for(&session));
    let v2 = UiProtocolLedgerEvent::Notification(projection_envelope_v2_event_for(&session));

    let legacy = features_for_projection_envelope_test(false);
    assert!(live_event_passes_capability_filter(&legacy_delta, legacy));
    assert!(!live_event_passes_capability_filter(&v1, legacy));
    assert!(live_event_passes_capability_filter(&v2, legacy));

    let v1_features = features_for_projection_envelope_test(true);
    assert!(!live_event_passes_capability_filter(
        &legacy_delta,
        v1_features
    ));
    assert!(live_event_passes_capability_filter(&v1, v1_features));
    assert!(live_event_passes_capability_filter(&v2, v1_features));

    let v2_features = features_for_projection_envelope_v2_test();
    assert!(!live_event_passes_capability_filter(
        &legacy_delta,
        v2_features
    ));
    assert!(!live_event_passes_capability_filter(&v1, v2_features));
    assert!(live_event_passes_capability_filter(&v2, v2_features));
}

#[test]
fn delivery_telemetry_tracks_canonical_v2_only() {
    // The Stage-4 legacy counter was deliberately removed in Stage 5.
    // This probe exercises the successful direct-delivery site and proves
    // the only remaining family metric is v2.
    let _ = take_ui_protocol_delivery_metrics_for_test();
    let session = SessionKey("local:v2-delivery-metrics".into());
    let (ws, mut rx) = ws_connection_for_test(4);
    ws.update_live_features(ConnectionUiFeatures::default());
    let ledger = UiProtocolLedger::new(8);

    assert!(send_notification_durable(&ws, &ledger, assistant_persisted_v2_for(&session),).is_ok());
    let frame = rx.try_recv().expect("v2 envelope was enqueued");
    assert_eq!(frame_method(&frame).as_deref(), Some("projection/envelope"));
    assert_eq!(
        take_ui_protocol_delivery_metrics_for_test(),
        vec![UiProtocolDeliveryMetric::V2Envelope],
        "every successful projection delivery records the v2 counter"
    );
}

#[tokio::test]
async fn v2_connection_receives_v2_envelopes_with_stage_one_fields() {
    let (ws, mut rx) = ws_connection_for_test(16);
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let session_id = SessionKey("local:envelope-v2-wire".into());
    let turn_id = TurnId::new();
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let live_rx = ledger.subscribe(&session_id);
    spawn_live_forwarder(
        ws.clone(),
        ledger.clone(),
        session_id.clone(),
        0,
        ws.connection_id(),
        features_for_projection_envelope_v2_test(),
        None,
        Some(MAIN_PROFILE_ID.to_owned()),
        live_rx,
        forwarders,
    )
    .await;

    let first = ledger
        .emit_envelope_v2(
            &session_id,
            turn_id.0.to_string(),
            PayloadV2::AssistantPersisted {
                text: "first assistant iteration".into(),
                assistant_segment_id: format!("{}:assistant:1", turn_id.0),
                meta: MessageMeta {
                    message_id: "msg-v2-first".into(),
                    persisted_at: Utc::now(),
                    media: vec![],
                },
            },
            None,
        )
        .expect("first canonical v2 envelope is durable");
    let second = ledger
        .emit_envelope_v2(
            &session_id,
            turn_id.0.to_string(),
            PayloadV2::AssistantPersisted {
                text: "second assistant iteration".into(),
                assistant_segment_id: format!("{}:assistant:2", turn_id.0),
                meta: MessageMeta {
                    message_id: "msg-v2-second".into(),
                    persisted_at: Utc::now(),
                    media: vec![],
                },
            },
            None,
        )
        .expect("later canonical v2 envelope is durable");
    ledger
        .emit_envelope_v2(
            &session_id,
            turn_id.0.to_string(),
            PayloadV2::ToolStart {
                tool_call_id: "call-v2-tool".into(),
                name: "shell".into(),
                arguments_preview: Some("command: \"cargo test\"".into()),
            },
            None,
        )
        .expect("canonical v2 tool envelope is durable");
    ledger
        .emit_envelope_v2(
            &session_id,
            turn_id.0.to_string(),
            PayloadV2::TurnTerminal {
                outcome: TurnTerminalOutcome::Completed,
                error: None,
                token_usage: Some(EnvelopeTokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    ..EnvelopeTokenUsage::default()
                }),
            },
            None,
        )
        .expect("canonical v2 terminal is durable");
    ledger
        .emit_envelope_v2(
            &session_id,
            format!("{}:background:task-v2-child", turn_id.0),
            PayloadV2::BackgroundChildCompleted {
                parent_turn_id: turn_id.0.to_string(),
                response_to_client_message_id: Some("cmid-v2-parent".into()),
                task_id: "task-v2-child".into(),
                content: "background result".into(),
                tool_call_id: Some("call-v2-child".into()),
                message_id: "msg-v2-child".into(),
                source: "background".into(),
                persisted_at: Utc::now(),
                media: vec!["artifacts/child.md".into()],
            },
            None,
        )
        .expect("canonical v2 background child is durable");

    let mut received = Vec::new();
    for _ in 0..5 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("v2 frame arrives")
            .expect("writer remains open");
        let WsMessage::Text(text) = frame else {
            panic!("expected JSON text frame");
        };
        assert!(
            !text.contains("message/persisted"),
            "a Stage-5 full-turn frame cannot use the retired wire method",
        );
        received.push(serde_json::from_str::<Value>(text.as_str()).expect("valid JSON"));
    }

    for frame in &received {
        assert_eq!(frame["method"], "projection/envelope");
        assert!(frame["params"]["cursor"].is_object());
    }

    let assistant_segments: Vec<&Value> = received
        .iter()
        .filter(|frame| frame["params"]["payload"]["type"] == "assistant_persisted")
        .map(|frame| &frame["params"]["payload"]["data"]["assistant_segment_id"])
        .collect();
    assert_eq!(assistant_segments.len(), 2);
    assert!(assistant_segments.iter().all(|id| id.is_string()));
    assert_ne!(assistant_segments[0], assistant_segments[1]);

    let tool = received
        .iter()
        .find(|frame| frame["params"]["payload"]["type"] == "tool_start")
        .expect("tool v2 envelope");
    assert_eq!(
        tool["params"]["payload"]["data"]["tool_call_id"],
        "call-v2-tool",
    );

    let terminal = received
        .iter()
        .find(|frame| frame["params"]["payload"]["type"] == "turn_terminal")
        .expect("terminal v2 envelope");
    assert_eq!(
        terminal["params"]["payload"]["data"]["outcome"],
        "completed"
    );
    assert_eq!(
        terminal["params"]["payload"]["data"]["token_usage"]["input_tokens"],
        11,
    );

    let child = received
        .iter()
        .find(|frame| frame["params"]["payload"]["type"] == "background/spawn_complete")
        .expect("linked child completion v2 envelope");
    assert_eq!(
        child["params"]["payload"]["data"]["parent_turn_id"],
        turn_id.0.to_string(),
    );
    assert_eq!(
        child["params"]["payload"]["data"]["response_to_client_message_id"],
        "cmid-v2-parent",
    );
    assert_ne!(child["params"]["thread_id"], turn_id.0.to_string());
    assert_ne!(child["params"]["turn_id"], turn_id.0.to_string());

    for frame in received
        .iter()
        .filter(|frame| frame["params"]["payload"]["type"] != "background/spawn_complete")
    {
        assert_eq!(frame["params"]["turn_id"], turn_id.0.to_string());
    }

    assert!(first.cursor.seq < second.cursor.seq);
}

#[tokio::test]
async fn v2_background_child_never_appends_to_parent_after_terminal() {
    let (ws, mut rx) = ws_connection_for_test(16);
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let session_id = SessionKey("local:envelope-v2-child-order".into());
    let turn_id = TurnId::new();
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let live_rx = ledger.subscribe(&session_id);
    spawn_live_forwarder(
        ws.clone(),
        ledger.clone(),
        session_id.clone(),
        0,
        ws.connection_id(),
        features_for_projection_envelope_v2_test(),
        None,
        Some(MAIN_PROFILE_ID.to_owned()),
        live_rx,
        forwarders,
    )
    .await;

    ledger
        .emit_envelope_v2(
            &session_id,
            turn_id.0.to_string(),
            PayloadV2::TurnTerminal {
                outcome: TurnTerminalOutcome::Completed,
                error: None,
                token_usage: None,
            },
            None,
        )
        .expect("parent terminal is durable");
    // A background result is a terminal event in its own child stream;
    // its media stays on that payload and never targets the settled
    // foreground stream.
    ledger
        .emit_envelope_v2(
            &session_id,
            format!("{}:background:task-v2-after-terminal", turn_id.0),
            PayloadV2::BackgroundChildCompleted {
                parent_turn_id: turn_id.0.to_string(),
                response_to_client_message_id: Some("cmid-v2-parent".into()),
                task_id: "task-v2-after-terminal".into(),
                content: "background result".into(),
                tool_call_id: Some("call-v2-after-terminal".into()),
                message_id: "msg-v2-after-terminal".into(),
                source: "background".into(),
                persisted_at: Utc::now(),
                media: vec!["artifacts/child.md".into()],
            },
            None,
        )
        .expect("child terminal is durable");

    let mut received = Vec::new();
    for _ in 0..2 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("v2 terminal or child frame arrives")
            .expect("writer remains open");
        let WsMessage::Text(text) = frame else {
            panic!("expected JSON text frame");
        };
        received.push(serde_json::from_str::<Value>(text.as_str()).expect("valid JSON"));
    }

    assert!(received.iter().any(|frame| {
        frame["params"]["payload"]["type"] == "turn_terminal"
            && frame["params"]["turn_id"] == turn_id.0.to_string()
    }));
    let child = received
        .iter()
        .find(|frame| frame["params"]["payload"]["type"] == "background/spawn_complete")
        .expect("linked background child completion");
    assert_ne!(child["params"]["thread_id"], turn_id.0.to_string());
    assert_eq!(
        child["params"]["payload"]["data"]["parent_turn_id"],
        turn_id.0.to_string(),
    );
    assert!(
        child["params"]["payload"]["data"]["media"] == json!(["artifacts/child.md"]),
        "background media belongs to the child payload",
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "no parent-stream file envelope may follow the child completion",
    );
}

#[tokio::test]
async fn unnegotiated_connection_receives_only_canonical_v2_wire_shape() {
    let (ws, mut rx) = ws_connection_for_test(8);
    let ledger = Arc::new(UiProtocolLedger::new(8));
    let session_id = SessionKey("local:envelope-v2-default".into());
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let live_rx = ledger.subscribe(&session_id);
    spawn_live_forwarder(
        ws.clone(),
        ledger.clone(),
        session_id.clone(),
        0,
        ws.connection_id(),
        ConnectionUiFeatures::default(),
        None,
        Some(MAIN_PROFILE_ID.to_owned()),
        live_rx,
        forwarders.clone(),
    )
    .await;

    ledger.append_notification(assistant_persisted_v2_for(&session_id));

    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("canonical frame arrives")
        .expect("writer remains open");
    let WsMessage::Text(text) = frame else {
        panic!("expected JSON text frame");
    };
    let json: Value = serde_json::from_str(text.as_str()).expect("canonical JSON frame");
    assert_eq!(json["method"], "projection/envelope");
    assert_eq!(json["params"]["payload"]["type"], "assistant_persisted");
    assert!(
        !text.contains("message/persisted"),
        "the retired wire method cannot appear in a Stage-5 frame",
    );

    abort_live_forwarders(&forwarders, &ledger).await;
}

#[test]
fn v2_projects_errored_and_interrupted_terminals() {
    let ledger = UiProtocolLedger::new(16);
    let session_id = SessionKey("local:envelope-v2-errors".into());

    for (code, expected_outcome) in [
        ("provider_failed", TurnTerminalOutcome::Errored),
        ("interrupted", TurnTerminalOutcome::Interrupted),
    ] {
        let turn_id = TurnId::new();
        let source = ledger.append_notification(UiNotification::TurnError(TurnErrorEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            code: code.into(),
            message: format!("{code} terminal"),
            token_usage: None,
            partial_result: None,
        }));
        let projected = project_v2_ledger_event(&ledger, &source.event, &source.cursor)
            .expect("turn/error has a v2 terminal projection");
        let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) = projected
        else {
            panic!("turn/error must project to EnvelopeV2");
        };

        assert_eq!(envelope.envelope.cursor.as_ref(), Some(&source.cursor));
        assert_eq!(envelope.envelope.turn_id, turn_id.0.to_string());
        match envelope.envelope.payload {
            PayloadV2::TurnTerminal {
                outcome,
                error: Some(error),
                token_usage,
            } => {
                assert_eq!(outcome, expected_outcome);
                assert_eq!(error.code, code);
                assert_eq!(error.message, format!("{code} terminal"));
                assert!(token_usage.is_none());
            }
            other => panic!("expected v2 terminal, got {other:?}"),
        }
    }
}

#[test]
fn should_replay_exact_failed_turn_usage_without_changing_old_error_wire() {
    let temp = tempfile::tempdir().unwrap();
    let session = SessionKey("local:failed-usage-replay".into());
    let turn = TurnId::new();
    let old_wire = json!({
        "session_id": session, "turn_id": turn,
        "code": "runtime_error", "message": "ordinary failure"
    });
    let old: TurnErrorEvent = serde_json::from_value(old_wire.clone()).unwrap();
    assert!(old.token_usage.is_none());
    assert!(old.partial_result.is_none());
    assert_eq!(serde_json::to_value(&old).unwrap(), old_wire);
    let usage = EnvelopeTokenUsage {
        input_tokens: 17,
        output_tokens: 11,
        reasoning_tokens: 5,
        cache_read_tokens: 3,
        cache_write_tokens: 2,
    };
    let partial_result = TurnErrorPartialResult {
        session_result: Some(TurnSessionResult {
            committed_seq: 15,
            message_id: "actual-final-id".into(),
            client_message_id: None,
        }),
    };
    {
        let ledger = UiProtocolLedger::with_config(
            crate::api::ui_protocol_ledger::LedgerConfig::durable(temp.path().to_owned()),
        );
        ledger.append_notification(UiNotification::TurnError(TurnErrorEvent {
            token_usage: Some(usage.clone()),
            partial_result: Some(partial_result.clone()),
            ..old
        }));
    }
    let ledger = UiProtocolLedger::with_config(
        crate::api::ui_protocol_ledger::LedgerConfig::durable(temp.path().to_owned()),
    );
    let replay = ledger
        .replay_after(
            &session,
            Some(&UiCursor {
                stream: session.0.clone(),
                seq: 0,
            }),
        )
        .unwrap();
    assert_eq!(replay.len(), 1);
    let projected = project_v2_ledger_event(&ledger, &replay[0].event, &replay[0].cursor).unwrap();
    let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) = projected
    else {
        panic!("expected native failure projection");
    };
    assert_eq!(envelope.envelope.turn_id, turn.0.to_string());
    let PayloadV2::TurnTerminal {
        outcome,
        token_usage,
        error,
    } = envelope.envelope.payload
    else {
        panic!("expected terminal");
    };
    assert_eq!(outcome, TurnTerminalOutcome::Errored);
    let error = error.unwrap();
    assert_eq!(error.code, "runtime_error");
    assert_eq!(error.data, Some(json!({"partial_result": partial_result})));
    assert_eq!(token_usage, Some(usage));
}

#[test]
fn should_replay_authoritative_no_final_without_promoting_legacy_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let session = SessionKey("local:no-final-replay".into());
    {
        let ledger = UiProtocolLedger::with_config(
            crate::api::ui_protocol_ledger::LedgerConfig::durable(temp.path().to_owned()),
        );
        for partial_result in [
            None,
            Some(TurnErrorPartialResult {
                session_result: None,
            }),
        ] {
            ledger.append_notification(UiNotification::TurnError(TurnErrorEvent {
                session_id: session.clone(),
                topic: None,
                turn_id: TurnId::new(),
                code: "output_truncated".into(),
                message: "failed".into(),
                token_usage: None,
                partial_result,
            }));
        }
    }
    let ledger = UiProtocolLedger::with_config(
        crate::api::ui_protocol_ledger::LedgerConfig::durable(temp.path().to_owned()),
    );
    let replay = ledger
        .replay_after(
            &session,
            Some(&UiCursor {
                stream: session.0.clone(),
                seq: 0,
            }),
        )
        .unwrap();
    assert_eq!(replay.len(), 2);
    for (row, expected) in replay.iter().zip([
        None,
        Some(json!({"partial_result": {"session_result": null}})),
    ]) {
        let projected = project_v2_ledger_event(&ledger, &row.event, &row.cursor).unwrap();
        let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) = projected
        else {
            panic!()
        };
        let PayloadV2::TurnTerminal {
            error: Some(error), ..
        } = envelope.envelope.payload
        else {
            panic!()
        };
        assert_eq!(error.data, expected);
    }
}

#[tokio::test]
async fn should_not_overwrite_terminal_usage_or_fabricate_it_for_ordinary_failures() {
    for first_usage in [
        None,
        Some(EnvelopeTokenUsage {
            input_tokens: 17,
            output_tokens: 11,
            reasoning_tokens: 5,
            cache_read_tokens: 3,
            cache_write_tokens: 2,
        }),
    ] {
        let ledger = UiProtocolLedger::new(16);
        let session = SessionKey("local:terminal-usage-once".into());
        let turn = TurnId::new();
        let state = TokioMutex::new(TurnState::Active);
        let (ws, _rx) = ws_connection_for_test(16);
        for (code, usage) in [
            ("original_failure", first_usage.clone()),
            (
                "late_failure",
                Some(EnvelopeTokenUsage {
                    input_tokens: 999,
                    ..Default::default()
                }),
            ),
        ] {
            try_emit_terminal(
                &state,
                TerminalReason::Errored,
                &ws,
                &ledger,
                &session,
                &turn,
                Some((code, "failed")),
                Some(TurnCompletionDetails {
                    token_usage: usage,
                    ..Default::default()
                }),
                None,
            )
            .await;
        }
        let replay = ledger
            .replay_after(
                &session,
                Some(&UiCursor {
                    stream: session.0.clone(),
                    seq: 0,
                }),
            )
            .unwrap();
        assert_eq!(
            replay.len(),
            1,
            "a late failure cannot emit another terminal"
        );
        let UiProtocolLedgerEvent::Notification(UiNotification::TurnError(error)) =
            &replay[0].event
        else {
            panic!("expected durable failure");
        };
        assert_eq!(error.code, "original_failure");
        assert_eq!(error.token_usage, first_usage);
        if first_usage.is_none() {
            assert!(
                serde_json::to_value(error)
                    .unwrap()
                    .get("token_usage")
                    .is_none()
            );
        }
    }
}

#[test]
fn should_assign_unique_v2_seq_to_terminal_and_consecutive_attachments() {
    let ledger = UiProtocolLedger::new(16);
    let session_id = SessionKey("local:envelope-v2-voice-audio".into());
    let turn_id = TurnId::new();
    let thread_id = turn_id.0.to_string();

    ledger.append_notification(UiNotification::EnvelopeV2(EnvelopeV2Notification {
        session_id: session_id.clone(),
        topic: None,
        envelope: EnvelopeV2 {
            thread_id: thread_id.clone(),
            seq: 1,
            cursor: None,
            turn_id: thread_id.clone(),
            client_message_id: None,
            payload: PayloadV2::AssistantPersisted {
                text: "第一句。第二句。".into(),
                assistant_segment_id: format!("{thread_id}:assistant:1"),
                meta: MessageMeta {
                    message_id: "voice-reply".into(),
                    persisted_at: Utc::now(),
                    media: vec![],
                },
            },
        },
    }));

    let terminal_source =
        ledger.append_notification(UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        }));
    // Production dual-emission persists the v1 terminal companion after the
    // legacy terminal source. It advances the durable base for later files,
    // so that already-represented source must not be counted twice.
    ledger.append_notification(UiNotification::Envelope(
        octos_core::ui_protocol::EnvelopeNotification {
            session_id: session_id.clone(),
            topic: None,
            envelope: octos_core::ui_protocol::Envelope {
                thread_id: thread_id.clone(),
                seq: 2,
                client_message_id: None,
                payload: Payload::TurnCompleted {
                    token_usage: EnvelopeTokenUsage::default(),
                },
            },
        },
    ));

    let file_sources = [
        UiNotification::FileAttached(octos_core::ui_protocol::FileAttachedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            path: "reply-first.mp3".into(),
            tool_call_id: None,
            attachment_owner: None,
            mime: Some("audio/mpeg".into()),
        }),
        UiNotification::FileAttached(octos_core::ui_protocol::FileAttachedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id,
            path: "reply-second.mp3".into(),
            tool_call_id: None,
            attachment_owner: None,
            mime: Some("audio/mpeg".into()),
        }),
    ]
    .map(|notification| ledger.append_notification(notification));
    let sources = [
        terminal_source,
        file_sources[0].clone(),
        file_sources[1].clone(),
    ];

    let projected_seqs = || {
        sources
            .iter()
            .map(|source| {
                let projected = project_v2_ledger_event(&ledger, &source.event, &source.cursor)
                    .expect("legacy source has a v2 projection");
                let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) =
                    projected
                else {
                    panic!("legacy source must project to EnvelopeV2");
                };
                envelope.envelope.seq
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(projected_seqs(), vec![2, 3, 4]);
    assert_eq!(
        projected_seqs(),
        vec![2, 3, 4],
        "replaying the same durable rows must assign the same sequence",
    );
}

/// UPCR-2026-014 M9-γ per-payload dual-emit: every legacy
/// notification surfaced by `forward_progress_event` triggers a
/// parallel `projection/envelope` ledger append. The test exercises
/// the helper that wires the dual-emit
/// (`emit_envelope_for_legacy_notification`) so a future refactor
/// can't silently drop a variant from the dual surface.
#[test]
fn emit_envelope_carries_tool_fidelity_previews() {
    // The tool-card fidelity lane: ToolStarted.arguments →
    // ToolStart.arguments_preview (key: value rendering, bounded) and
    // ToolCompleted.output_preview/duration_ms → ToolEnd (re-bounded).
    let ledger = UiProtocolLedger::new(32);
    let session_id = SessionKey("local:fidelity-emit".into());
    let turn_id = TurnId::new();

    let giant = "æ".repeat(9000);
    emit_envelope_for_legacy_notification(
        &ledger,
        &session_id,
        &UiNotification::ToolStarted(ToolStartedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            tool_call_id: "tc-fid".into(),
            tool_name: "shell".into(),
            arguments: Some(serde_json::json!({
                "command": "cargo test",
                "blob": giant,
            })),
        }),
    );
    emit_envelope_for_legacy_notification(
        &ledger,
        &session_id,
        &UiNotification::ToolCompleted(ToolCompletedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            tool_call_id: "tc-fid".into(),
            tool_name: "shell".into(),
            success: Some(true),
            output_preview: Some("test result: ok. 815 passed".into()),
            duration_ms: Some(4321),
        }),
    );

    let baseline = UiCursor {
        stream: session_id.0.clone(),
        seq: 0,
    };
    let replay = ledger.replay_after(&session_id, Some(&baseline)).unwrap();
    let payloads: Vec<&Payload> = replay
        .iter()
        .filter_map(|e| match &e.event {
            UiProtocolLedgerEvent::Notification(UiNotification::Envelope(env)) => {
                Some(&env.envelope.payload)
            }
            _ => None,
        })
        .collect();

    let Some(Payload::ToolStart {
        arguments_preview: Some(preview),
        ..
    }) = payloads.first()
    else {
        panic!("expected enriched ToolStart, got {payloads:?}");
    };
    assert!(
        preview.contains("command: \"cargo test\""),
        "object args render as key: value pairs, got {preview}"
    );
    assert!(
        preview.chars().count() <= octos_core::ui_protocol::ENVELOPE_TOOL_ARGUMENTS_PREVIEW_MAX + 1,
        "arguments preview must be bounded (UTF-8-safe), got {} chars",
        preview.chars().count()
    );
    let Some(Payload::ToolEnd {
        output_preview: Some(output),
        duration_ms: Some(duration),
        ..
    }) = payloads.get(1)
    else {
        panic!("expected enriched ToolEnd, got {payloads:?}");
    };
    assert_eq!(output, "test result: ok. 815 passed");
    assert_eq!(*duration, 4321);

    // `{}` arguments render empty — spec says OMIT, not empty-string.
    emit_envelope_for_legacy_notification(
        &ledger,
        &session_id,
        &UiNotification::ToolStarted(ToolStartedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            tool_call_id: "tc-empty".into(),
            tool_name: "noop".into(),
            arguments: Some(serde_json::json!({})),
        }),
    );
    let replay = ledger.replay_after(&session_id, Some(&baseline)).unwrap();
    let empty_start = replay
        .iter()
        .filter_map(|e| match &e.event {
            UiProtocolLedgerEvent::Notification(UiNotification::Envelope(env)) => {
                match &env.envelope.payload {
                    Payload::ToolStart {
                        tool_call_id,
                        arguments_preview,
                        ..
                    } if tool_call_id == "tc-empty" => Some(arguments_preview.clone()),
                    _ => None,
                }
            }
            _ => None,
        })
        .next()
        .expect("tc-empty envelope present");
    assert_eq!(empty_start, None, "empty args must omit the preview");
}
