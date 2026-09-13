//! Test-support agent factory shared by the OUP session and transport tests.
//!
//! Extracted from the retired `octos acp` command surface: the ACP stdio
//! transport is gone, but the in-process `TestAgentFactory` remains the
//! seam tests use to bootstrap a full profile runtime around a mock
//! `LlmProvider`.

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use eyre::Result;

use octos_agent::Agent;
use octos_bus::session::SessionManager;
use octos_core::SessionKey;

use crate::config::Config as OctosConfig;

/// Supplies the shared OUP runtime to in-process embedders and tests.
///
/// Requires the default `api` feature.
#[cfg(feature = "api")]
#[async_trait::async_trait]
pub trait SessionAgentFactory: Send + Sync {
    /// The cwd to fall back to when the client sends an empty `session/new` cwd.
    fn default_cwd(&self) -> &std::path::Path;

    /// Build a runnable agent rooted at `cwd`, returning it plus the shared
    /// shutdown flag wired into it.
    async fn build(
        &self,
        cwd: PathBuf,
    ) -> Result<(Arc<Agent>, Arc<std::sync::atomic::AtomicBool>)> {
        let state = self.oup_state().await?;
        let profile_id = self.session_profile_id();
        let profile = state
            .profiles
            .get(&profile_id)
            .ok_or_else(|| eyre::eyre!("OUP profile runtime unavailable: {profile_id}"))?;
        let key = SessionKey::with_profile(&profile_id, "acp", &uuid::Uuid::now_v7().to_string());
        let runtime = state
            .session_cache
            .get_or_init(profile, key, Some(cwd))
            .await?;
        Ok((runtime.agent.clone(), runtime.agent.shutdown_signal()))
    }

    /// Canonical runtime used by the session transports.
    async fn oup_state(&self) -> Result<Arc<crate::api::AppState>> {
        eyre::bail!("this embedding factory does not provide an OUP runtime")
    }

    /// Where conversations are persisted, if anywhere. `None` means sessions
    /// will not survive a restart — the test factory, or a store that failed to open.
    fn session_store(&self) -> Option<Arc<Mutex<SessionManager>>> {
        None
    }

    /// Profile dimension for this factory's session keys. Sessions elsewhere are
    /// profile-scoped; an unscoped key would collide across profiles running
    /// the same session id and would not isolate them.
    fn session_profile_id(&self) -> String {
        octos_core::MAIN_PROFILE_ID.to_string()
    }
}

/// Test-support factory: hands every session an agent backed by an injected
/// [`LlmProvider`] with episodic memory in an isolated temporary directory.
///
/// Not for production use.
#[cfg(feature = "api")]
#[doc(hidden)]
pub struct TestAgentFactory {
    llm: Arc<dyn octos_llm::LlmProvider>,
    memory_dir: PathBuf,
    default_cwd: PathBuf,
    /// Optional alternate persistence root. Otherwise the mock runtime uses
    /// memory_dir, normally an isolated temporary directory.
    session_store: Option<Arc<Mutex<SessionManager>>>,
    oup_state: tokio::sync::OnceCell<Arc<crate::api::AppState>>,
}

#[cfg(feature = "api")]
#[doc(hidden)]
impl TestAgentFactory {
    /// Build a factory that hands every session an agent backed by `llm`, with
    /// episodic memory in `memory_dir` and tools rooted at `default_cwd`.
    pub fn new(
        llm: Arc<dyn octos_llm::LlmProvider>,
        memory_dir: PathBuf,
        default_cwd: PathBuf,
    ) -> Self {
        Self {
            llm,
            memory_dir,
            default_cwd,
            session_store: None,
            oup_state: tokio::sync::OnceCell::new(),
        }
    }

    /// Persist this factory's sessions under `dir`, so a test can prove a
    /// conversation survives being reloaded into a fresh transport.
    pub fn with_session_store(mut self, dir: &std::path::Path) -> Self {
        self.session_store = SessionManager::open(dir)
            .ok()
            .map(|store| Arc::new(Mutex::new(store)));
        self
    }
}

#[cfg(feature = "api")]
#[async_trait::async_trait]
impl SessionAgentFactory for TestAgentFactory {
    async fn oup_state(&self) -> Result<Arc<crate::api::AppState>> {
        self.oup_state
            .get_or_try_init(|| async {
                use crate::runtime::local_oup::{LocalOupOptions, bootstrap, local_profile};
                let data_dir = match &self.session_store {
                    Some(store) => store.lock().await.data_dir(),
                    None => self.memory_dir.clone(),
                };
                let config = OctosConfig {
                    provider: Some(self.llm.provider_name().to_owned()),
                    model: Some(self.llm.model_id().to_owned()),
                    memory: Some(crate::config::MemoryConfig {
                        refresh: Some(crate::config::MemoryRefreshConfig {
                            enabled: Some(false),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                bootstrap(LocalOupOptions {
                    profile: local_profile(&self.session_profile_id(), &config),
                    config,
                    data_dir: data_dir.clone(),
                    config_home: data_dir,
                    no_retry: true,
                    provider: Some(self.llm.clone()),
                    tool_profile: None,
                    save_episodes: false,
                })
                .await
            })
            .await
            .cloned()
    }
    fn session_store(&self) -> Option<Arc<Mutex<SessionManager>>> {
        self.session_store.clone()
    }

    fn default_cwd(&self) -> &std::path::Path {
        &self.default_cwd
    }
}
