use super::*;

// `UiProtocolContractStores`'s audit writer moved with the contract stores to
// `crate::approvals_audit`; the parent module no longer imports these two names
// directly, so name them here.

use crate::approvals_audit::{ApprovalsAuditConfig, ApprovalsAuditLog};

use octos_core::ui_protocol::{
    ApprovalDecision, ApprovalId, ApprovalRespondParams, ApprovalRespondStatus, approval_scopes,
    methods, rpc_error_codes,
};

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

/// Small reports are inlined verbatim; oversized ones collapse to a preview
/// that must carry the `read_task_output` recovery pointer (Mini4 regression:
/// the old bare 300-char preview made the parent conclude the report was lost).
#[test]
fn spawn_report_announcement_inlines_small_and_previews_large_reports() {
    let body = "Status: SUCCESS\n\nshort review body";
    let out = format_spawn_report_announcement("review-octos-web", body, Some("task-1"));
    assert!(out.contains(body), "small report must be inlined verbatim");
    assert!(!out.contains("preview truncated"));

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

/// Provider stub with a window LARGE relative to the compaction floor (64K →
/// ~45K threshold; 60×4000-char messages ≈ 60K estimate); `chat` is unreachable
/// because the deterministic summarizer never calls the model.
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

/// Field report 2026-08-07: a session whose ledger is REBUILT from long raw
/// history at `session/open` published an over-window estimate. Open must run
/// the SAME threshold compaction the pre-turn path would run.
#[tokio::test]
async fn session_open_snapshot_compacts_oversized_context() {
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

    // The pass must return lifecycle events for the caller to append.
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

    // The compacted manager — not the oversized rebuild — must be persisted.
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

#[test]
fn post_terminal_drain_skips_late_tokens_but_keeps_background_progress() {
    // Drain must drop late `token`/`reasoning_chunk` deltas (they resurrect
    // the client's input gate) alongside emitted terminal signals...
    assert!(drain_should_skip_event(Some("done")));
    assert!(drain_should_skip_event(Some("error")));
    assert!(drain_should_skip_event(Some("token")));
    assert!(drain_should_skip_event(Some("reasoning_chunk")));

    // ...while still forwarding the background task's real progress (#961).
    for keep in [
        "task_started",
        "task_updated",
        "tool_progress",
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

/// The runtime-unavailable message must distinguish a missing profile (not-found
/// explanation) from an unconfigured one (API-key guidance) and never blame a
/// missing API key for a profile that does not exist.
#[test]
fn profile_runtime_unavailable_message_distinguishes_missing_profile_from_no_llm() {
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
/// `profile/sub_providers/{list,upsert,remove}`: add/replace-by-key/remove the
/// named provider lanes; same-key upsert REPLACES, removing a missing key
/// reports `applied:false`, and everything persists.
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

    // api_key with no api_key_env must be REJECTED (was silently dropped).
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
}

/// Unknown fields on `profile/llm/upsert` must be rejected with EVERY field
/// named by its dotted path, and the prior configuration left untouched.
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

/// #2164 acceptance — an endpoint edit on the primary must evict the cached
/// ProfileRuntime and rebuild from the committed file for the next turn.
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
    // A weak handle keeps the allocation identity reserved without keeping
    // the runtime's episode store alive during reload.
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

#[test]
fn catalog_result_sourced_from_registry_and_canonical_catalog() {
    // No runtime data-dir catalog → fully populated from the compiled-in
    // canonical model_catalog.json (the SSOT) with registry key-envs.
    let dir = tempfile::tempdir().unwrap();
    let state = local_profile_state(dir.path());
    let catalog = raw_catalog_result(&state, None).expect("catalog");
    let families = catalog["families"].as_object().expect("families object");

    // Key-env comes from the registry, not a hand-maintained env map.
    assert_eq!(families["zai"]["env"], "ZAI_API_KEY");
    let ids = |fam: &str| -> Vec<String> {
        families[fam]["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect()
    };
    // Curation: glm-5.3 + k3 present, deepseek-chat curated out.
    assert!(ids("zai").contains(&"glm-5.3".to_owned()));
    assert!(ids("moonshot-coding").contains(&"k3".to_owned()));
    assert!(!ids("deepseek").contains(&"deepseek-chat".to_owned()));

    // Alternative provisioning endpoints survive into the onboarding catalog.
    let v4pro = families["deepseek"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "deepseek-v4-pro")
        .unwrap();
    assert!(
        v4pro["endpoints"]
            .as_array()
            .expect("v4-pro has endpoints")
            .iter()
            .any(|e| e["id"] == "autodl" && e["api_key_env"] == "AUTODL_API_KEY"),
    );
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

#[test]
fn stdio_session_open_candidate_profile_is_last_success_candidate_only() {
    let base = |session_id: SessionKey, profile_id: Option<String>| SessionOpenParams {
        session_id,
        topic: None,
        profile_id,
        cwd: None,
        sandbox: None,
        after: None,
    };
    let candidate = |session_id, profile_id| {
        stdio_session_open_candidate_profile(&base(session_id, profile_id), Some("previous"))
    };
    assert_eq!(
        candidate(SessionKey("coding:local:test".into()), None).as_deref(),
        Some("coding")
    );
    assert_eq!(
        candidate(SessionKey("local:test".into()), Some("explicit".into())).as_deref(),
        Some("explicit")
    );
    assert_eq!(
        candidate(SessionKey("local:test".into()), None).as_deref(),
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
            .filter(|message| message.role == MessageRole::User && message.content == "old request")
            .count(),
        1,
        "known history should not be duplicated while adding the current turn"
    );
    assert!(
        crate::context_manager::context_ledger_path(dir.path(), &session_id.to_string()).exists()
    );
}

/// UPCR-2026-026: the in-loop (mid-turn) compaction pass must emit
/// Started → Completed through the bridge's notify hook (it previously
/// compacted SILENTLY, so mid-turn fills never showed compaction UX).
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
    assert!(started < completed, "started must precede completed");
    let UiNotification::ContextCompactionStarted(event) = &events[started] else {
        unreachable!()
    };
    assert_eq!(event.trigger, "agent_loop:turn_start");
    let UiNotification::ContextCompactionCompleted(done) = &events[completed] else {
        unreachable!()
    };
    assert!(done.context_state.semantic_head_id.is_some());
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
    // Single-identity model: wire email is a synthesized placeholder.
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
    assert_eq!(profile.name, "Ada Lovelace");
    let profile_json: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("profiles/ada.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(profile_json["username"], json!("ada"));
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

    // codex #1613 r5: a reserved channel-name username must be rejected
    // BEFORE any record persists.
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

/// #1057 / M22 — the backend workspace probe reports canonical path,
/// existence, writability, and absent workspace_policy.toml for a plausible
/// onboarding pick, and rejects roots escaping into banned system paths
/// (`/etc`, `/usr`, ...) regardless of existence.
#[test]
fn workspace_probe_reports_writability_and_flags_banned_root_escape() {
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
    assert_eq!(result["runtime_mode"], json!("solo"));
    assert_eq!(
        result["canonical_path"].as_str().unwrap(),
        canonical.to_string_lossy()
    );
    assert_eq!(result["workspace_policy"]["present"], json!(false));

    let result = onboarding_workspace_probe_result(&state, "/etc/octos-test-not-a-real-path-1057")
        .expect("probe an /etc candidate");
    assert_eq!(result["root_escape"], json!(true));
    assert_eq!(result["banned_root"], json!("etc"));
}
#[test]
fn permission_profile_handlers_are_server_owned_and_reject_danger_outside_local() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSetParams, PermissionProfileUpdate,
    };

    // yolo GAP #1: Local + the explicit `--solo` opt-in is what permits danger.
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

    // An explicit `runtime_mode: "tenant"` in the request tightens the gate
    // even on a Local server (M12 soak negative-probe contract, #951).
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
}

/// GAP #1: `effective_permissions_for_session` maps Local → Solo ONLY with the
/// `--solo` opt-in; without it a persisted danger selection still resolves
/// through RuntimeMode::Local and is rejected.
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
}

/// #1162 / #1167 — defense-in-depth: a session key carrying a tenant/cloud
/// scope marker in a structural slot (profile slot or the exact
/// `{profile}:tenant:` channel-slot literal) rejects `danger_full_access` on a
/// Local server even when the client omits the `runtime_mode` override.
#[test]
fn danger_full_access_rejected_for_non_cloud_tenant_per_1162() {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSetParams, PermissionProfileUpdate,
    };

    // Enable the `--solo` opt-in so a denial is attributable to the
    // tenant/cloud SCOPE marker, not the missing opt-in.
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
        // M12-G soak shape: `tenant`/`cloud` sits in the channel slot and
        // `SessionKey::profile_id()` returns None (not a registered channel).
        (
            "channel-slot=tenant (soak shape)",
            SessionKey("coding:tenant:m12-negative".into()),
        ),
        // #1167 — profiled tenant sessions on channels core's
        // `is_channel_name` doesn't include; the gate can't rely solely on
        // `SessionKey::profile_id()`.
        (
            "profile=tenant-a + channel=line",
            SessionKey("tenant-a:line:room-1".into()),
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

/// The persisted `default-profile` pointer drives a bare launch even when it is
/// not the first-sorted profile; a stale pointer is ignored.
#[tokio::test]
async fn launch_resolve_prefers_persisted_default_profile() {
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveParams};

    let tmp = tempfile::tempdir().unwrap();
    // Each profile bootstraps in a separate data dir (single-writer redb).
    let home = tmp.path().join("home");

    let make_profile = |id: &str| crate::profiles::UserProfile {
        id: id.to_string(),
        name: id.to_string(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        // bootstrap requires a primary provider; the key never leaves env.
        config: crate::profiles::ProfileConfig {
            llm: Some(crate::profiles::LlmProfileConfig {
                primary: Some(crate::profiles::LlmModelSelectionConfig {
                    family_id: Some("openai".to_string()),
                    model_id: Some("gpt-4o-mini".to_string()),
                    route: Some(crate::profiles::LlmRouteConfig {
                        api_key_env: Some("LAUNCH_DEFAULT_TEST_KEY".to_string()),
                        ..Default::default()
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
    // Bare launch: the persisted default pointer must win over BOTH the
    // connection profile ("alpha") and sort order.
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
    // Process-wide caches: keep this profile identity distinct.
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
    // `open_session_result` bootstraps before validating cwd — mirror that.
    assert!(
        ensure_session_profile_runtime(&state, Some(&profile_id))
            .await
            .expect("configured profile runtime bootstraps")
            .is_some(),
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
    assert_eq!(stamp["approval_policy"], json!("never"));
    assert_eq!(stamp["sandbox_mode"], json!("danger-full-access"));
    assert_eq!(stamp["permission_profile"], json!("danger_full_access"));
    assert_eq!(stamp["filesystem_scope"], json!("host"));
    assert_eq!(stamp["network"], json!("allowed"));
}

#[test]
fn parses_turn_start_rpc_request_and_keeps_legacy_shape_back_compat() {
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

    // UPCR-2026-015 (M9-β-1): bare turn/start (no β-1 fields) still decodes
    // with the new defaults.
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

/// Issue #1332: `turn/completed` must surface the `done` event's token totals +
/// cursor + final-assistant message_id instead of the dormant `None` triple.
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
    assert!(frame.contains("\"tokens_in\":123"), "{frame}");
    assert!(frame.contains("\"tokens_out\":456"), "{frame}");
    assert!(frame.contains("\"session_result\""), "{frame}");
    assert!(frame.contains("\"committed_seq\":17"), "{frame}");
    assert!(
        frame.contains("\"client_message_id\":\"cmid-user-1\""),
        "{frame}"
    );
    assert!(frame.contains("\"cursor\""), "{frame}");
}

/// Issue #1337: in the trimmed-dedupe path the helper must source
/// `committed_seq` from `final_assistant_committed_seq` (the assistant
/// carrier row), not from the loop's last-row cursor.
#[test]
fn build_turn_session_result_from_done_pins_seq_to_assistant_carrier_in_trimmed_dedupe() {
    // done JSON: assistant carrier at seq 10 (+ tool rows 11, 12), loop
    // cursor ended at 12, final_assistant_committed_seq = 10.
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

    assert_eq!(
        session_result.committed_seq, assistant_seq,
        "committed_seq must pin to assistant carrier seq, not last tool-row cursor"
    );
    assert_ne!(session_result.committed_seq, last_tool_seq);
    assert_eq!(session_result.message_id, assistant_message_id);
    assert_eq!(session_result.client_message_id, None);
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

    assert_eq!(first.task_id, task_id);
    assert_eq!(first.text, "collecting\n");
    assert_eq!(second.cursor.offset, first.text.len() as u64);
    assert_eq!(second.text, "done\n");
}

async fn recv_rpc_json(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
    match rx.recv().await.expect("rpc frame") {
        WsMessage::Text(text) => serde_json::from_str(text.as_str()).expect("json frame"),
        other => panic!("expected text frame, got {other:?}"),
    }
}

#[test]
fn malformed_approval_params_return_invalid_params_not_unsupported() {
    // Unknown decision STRINGS are forward-compat (`Unknown` → Deny);
    // INVALID_PARAMS requires structurally malformed params.
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

/// Progress-mapped `approval_requested` events are stored for respond, and a
/// successful respond serializes as a typed JSON-RPC result frame.
#[test]
fn progress_approval_request_is_stored_and_responds_with_typed_result() {
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

    let frame = RpcResponse::success(
        "approval-1",
        serde_json::to_value(outcome.result).expect("serialize result"),
    );
    assert_eq!(frame.jsonrpc, octos_core::ui_protocol::JSON_RPC_VERSION);
    assert_eq!(frame.result["accepted"], json!(true));
    assert_eq!(
        frame.result["status"],
        json!(ApprovalRespondStatus::Accepted)
    );
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
}

#[test]
fn rejects_invalid_rpc_request_json() {
    let error = parse_rpc_request("{").expect_err("parse error");
    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::PARSE_ERROR
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

#[test]
fn session_scope_allows_matching_profile_and_rejects_cross_profile() {
    let session_id = SessionKey::with_profile("profile-a", "api", "chat-1");
    let active_profile_id =
        validate_session_scope(&session_id, Some("profile-a"), Some("profile-a"))
            .expect("valid scope");
    assert_eq!(active_profile_id.as_deref(), Some("profile-a"));

    // Cross-profile session ids are rejected with the expected/actual ids.
    let session_id = SessionKey::with_profile("profile-b", "api", "chat-1");
    let error =
        validate_session_scope(&session_id, None, Some("profile-a")).expect_err("scope error");
    assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
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

/// #2040: a stdio connection must NEVER receive the 1008 auth-expiry close —
/// the Close frame ends the stdio writer loop, killing the transport before
/// the error reply is written. The FIRST frame must be the error envelope.
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
}

/// Build a #2019 human-sink background-activity event.
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

/// Capability-gated notifications (`plan.todos.v1`, `event.background_activity.v1`)
/// never reach a connection that did not negotiate them — on the live broadcast
/// OR reconnect replay (both call this filter); #2019 migration trap.
#[test]
fn capability_gated_notifications_require_negotiation() {
    use octos_core::ui_protocol::{PlanUpdatedEvent, UiPlanRecord};
    let plan = UiProtocolLedgerEvent::Notification(UiNotification::PlanUpdated(PlanUpdatedEvent {
        session_id: SessionKey("local:test".into()),
        topic: None,
        turn_id: None,
        plan: UiPlanRecord {
            items: Vec::new(),
            title: None,
            updated_at_ms: 0,
        },
    }));
    assert!(!live_event_passes_capability_filter(
        &plan,
        ConnectionUiFeatures::default()
    ));
    let negotiated = ConnectionUiFeatures {
        plan_todos: true,
        ..Default::default()
    };
    assert!(live_event_passes_capability_filter(&plan, negotiated));

    let activity = UiProtocolLedgerEvent::Notification(UiNotification::BackgroundActivity(
        background_activity_for(&SessionKey("local:test".into()), "monitor_01", "boom"),
    ));
    assert!(
        !live_event_passes_capability_filter(&activity, ConnectionUiFeatures::default()),
        "a connection without event.background_activity.v1 must never receive it"
    );
    let negotiated = ConnectionUiFeatures {
        background_activity: true,
        ..Default::default()
    };
    assert!(live_event_passes_capability_filter(&activity, negotiated));
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

/// Over a stdio-default connection (`projection_envelope == false`) the legacy
/// `turn/completed` notification must pass BOTH capability filters (broadcast +
/// direct-send) — it is the turn-lifecycle signal the stdio TUI keys on. A
/// connection that opts into `projection.envelope.v1` via `client_hello` flips
/// the gate back and (correctly) suppresses the legacy notification.
#[tokio::test]
async fn stdio_delivers_legacy_turn_completed_until_projection_envelope_opt_in() {
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

    let features = ConnectionUiFeatures::stdio_defaults();
    assert!(
        live_event_passes_capability_filter(&completed, features),
        "stdio-default connection must receive legacy turn/completed via the broadcast filter"
    );

    let (tx, _rx) = mpsc::channel(16);
    let ws = WsConnection::new(tx);
    ws.update_live_features(ConnectionUiFeatures::stdio_defaults());
    assert!(
        direct_send_passes_capability_filter(&ws, &completed),
        "stdio-default connection must receive legacy turn/completed via the direct-send filter"
    );

    // Opt-in via client_hello over stdio must be preserved, and once opted in
    // the γ gate suppresses legacy turn/completed (envelope supersedes it).
    let features = ConnectionUiFeatures::from_requested_feature_tokens(
        [UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1],
        true, // stdio_transport
    );
    assert!(
        features.projection_envelope,
        "client_hello over stdio must still opt into projection.envelope.v1"
    );
    assert!(features.stdio_transport);
    assert!(
        features
            .negotiated_capabilities()
            .supports_feature(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1)
    );
    assert!(
        !live_event_passes_capability_filter(&completed, features),
        "an opted-in stdio connection sees the envelope, not legacy turn/completed"
    );
}
#[tokio::test]
async fn session_btw_rejects_unknown_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = state_with_sessions(temp.path());
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
    // Lock contention must be recognized structurally through the eyre wrap
    // chain and rendered with cause + remedies intact.
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
    );
    let message = error
        .data
        .as_ref()
        .and_then(|d| d.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    assert!(message.contains("alan"), "{message}");
    assert!(message.contains("--instance-data-dir"), "{message}");
    assert!(message.contains("episodes.redb"), "{message}");
}

#[test]
fn final_assistant_message_persists_omitted_dedupes_and_preserves_distinct_final() {
    // Response messages without an assistant row → persist the final content.
    let message = final_assistant_message(&[Message::user("hello")], "world", Some("r".into()))
        .expect("assistant message");
    assert_eq!(message.role, MessageRole::Assistant);
    assert_eq!(message.content, "world");
    assert_eq!(message.reasoning_content.as_deref(), Some("r"));

    // A trailing assistant row equal to the final content is deduped.
    let messages = vec![Message::assistant("world")];
    assert!(final_assistant_message(&messages, "world", None).is_none());

    // NEW-10 NEGATIVE: a short preamble + a DISTINCT final answer keeps both
    // rows (the preamble is not trimmed-equal to the final).
    let messages = vec![Message::assistant("Looking that up...")];
    let final_content = "旧金山今天天气晴朗，气温17.1°C，湿度68%。需要更详细的湾区预报吗？";
    let synthesised = final_assistant_message(&messages, final_content, None);
    assert!(
        synthesised.is_some(),
        "preamble + distinct-final flow must persist BOTH rows",
    );
    assert_eq!(synthesised.unwrap().content, final_content);
}
/// M10 Phase 6.1: pre-stamp rows with the turn-derived thread id so user +
/// assistant land in the same SPA thread (else: 3 bubbles per turn).
#[test]
fn pre_stamp_turn_thread_id_stamps_user_assistant_and_tool_when_unbound() {
    let turn_thread_id = "turn-abc";

    let user = pre_stamp_turn_thread_id(Message::user("hi"), turn_thread_id);
    let assistant = pre_stamp_turn_thread_id(Message::assistant("ok"), turn_thread_id);
    let mut tool = test_message(MessageRole::Tool, "result");
    tool.tool_call_id = Some("call-1".into());
    let tool = pre_stamp_turn_thread_id(tool, turn_thread_id);

    assert_eq!(user.thread_id.as_deref(), Some(turn_thread_id));
    assert_eq!(assistant.thread_id.as_deref(), Some(turn_thread_id));
    assert_eq!(tool.thread_id.as_deref(), Some(turn_thread_id));
}

#[tokio::test]
async fn interrupt_cancels_running_spawn_only_tasks_for_session() {
    // `turn/interrupt` must cancel the session's still-running spawn_only
    // background tasks, not only abort the foreground agent loop.
    let supervisor = octos_agent::TaskSupervisor::new();
    let session_id = SessionKey("api:profile/local:owned".into());
    let session_key = session_id.to_string();

    // Two live tasks for THIS session; one for another session survives.
    let running_a = supervisor.register("bg_research", "tc-a", Some(&session_key));
    let running_b = supervisor.register("bg_research", "tc-b", Some(&session_key));
    supervisor.mark_running(&running_a);
    supervisor.mark_running(&running_b);

    let other_session = SessionKey("api:profile/local:other".into());
    let other_running =
        supervisor.register("bg_research", "tc-c", Some(&other_session.to_string()));
    supervisor.mark_running(&other_running);

    // An already-terminal task for this session is skipped.
    let done = supervisor.register("bg_research", "tc-d", Some(&session_key));
    supervisor.mark_completed(&done, vec![]);

    cancel_session_spawn_only_tasks(&supervisor, &session_id);

    // Both live tasks for this session are now terminal `Cancelled`.
    assert!(matches!(
        supervisor.get_task(&running_a).map(|t| t.status),
        Some(octos_agent::TaskStatus::Cancelled)
    ));
    assert!(supervisor.cancel_token(&running_b).is_cancelled());
    // The completed task is intact; the sibling session's task is untouched.
    assert!(matches!(
        supervisor.get_task(&done).map(|t| t.status),
        Some(octos_agent::TaskStatus::Completed)
    ));
    assert!(!supervisor.cancel_token(&other_running).is_cancelled());
}

/// Mirror of `handle_turn_interrupt`'s post-abort drain step.
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

    approvals.request(ApprovalRequestedEvent::generic(
        session_id.clone(),
        approval_id.clone(),
        interrupted_turn.clone(),
        "shell",
        "Pending",
        "ls",
    ));

    let emitted =
        drain_pending_approvals_for_interrupt(&ledger, &approvals, &session_id, &interrupted_turn);

    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].approval_id, approval_id);
    assert_eq!(emitted[0].reason, "turn_interrupted");

    let err = approvals
        .respond(ApprovalRespondParams::new(
            session_id.clone(),
            approval_id,
            ApprovalDecision::Approve,
        ))
        .expect_err("late respond against cancelled approval");
    assert_eq!(err.code, rpc_error_codes::APPROVAL_CANCELLED);
    let data = err.data.expect("typed error data");
    assert_eq!(data["kind"], json!("approval_cancelled"));
    assert_eq!(data["reason"], json!("turn_interrupted"));
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
    handle.abort();
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

/// Mirrors `handle_approval_respond` on success: respond, then record a
/// recordable scope. Returns the recorded scope kind (or `None`).
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
fn scope_approve_auto_resolves_and_deny_short_circuits() {
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

    // approve_for_turn: a second approval for the same tool in the same turn
    // auto-resolves with the recorded decision.
    let mut params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    params.approval_scope = Some("approve_for_turn".into());
    let kind = respond_with_scope(&contracts, params).expect("scope recorded");
    assert_eq!(kind, ApprovalScopeKind::ApproveForTurn);
    let hit = contracts
        .scopes
        .lookup(&session_id, "shell", &turn_id)
        .expect("auto-resolve hit");
    assert_eq!(hit.decision, ApprovalDecision::Approve);
    assert_eq!(hit.scope_wire(), approval_scopes::TURN);

    // Deny with a tool scope records a deny decision.
    let turn_b = TurnId::new();
    let deny_id = ApprovalId::new();
    store_request(
        &contracts,
        &session_id,
        deny_id.clone(),
        turn_b.clone(),
        "shell",
    );
    let mut params =
        ApprovalRespondParams::new(session_id.clone(), deny_id, ApprovalDecision::Deny);
    params.approval_scope = Some(approval_scopes::TOOL.into());
    respond_with_scope(&contracts, params);
    let hit = contracts
        .scopes
        .lookup(&session_id, "shell", &turn_b)
        .expect("deny scope hit");
    assert_eq!(hit.decision, ApprovalDecision::Deny);
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

// ====================================================================
// M9-FIX-07 — approval decision audit log + replay
// ====================================================================

/// One JSON-Lines entry per decision, and no payload bodies leak.
#[test]
fn audit_log_records_every_decision() {
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

// ====================================================================
// PR G — UPCR-2026-009 / -010 / -011 / -012 handler tests
// ====================================================================

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

/// A stale pre-rollback context ledger (high-watermark coverage) must not
/// resurrect rolled-back turns: `session/rollback` rebuilds + persists the
/// ledger from the trimmed history.
#[tokio::test(flavor = "current_thread")]
async fn session_rollback_rebuilds_context_ledger() {
    let session_id = SessionKey("local:rollback-ctx-ledger".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    // Persist a pre-rollback context ledger like a prior turn would.
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

#[test]
fn fork_reservations_scope_by_sessions_dir() {
    // codex #1613 r2: identical keys in different sessions dirs don't collide.
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

    // Aborting must release the receiver so the slot can be reclaimed.
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
                assert!(token_usage.is_none());
            }
            other => panic!("expected v2 terminal, got {other:?}"),
        }
    }
}
