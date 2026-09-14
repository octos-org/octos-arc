//! Test-support agent factory shared by the OUP session tests.
//!
//! Extracted from the retired `octos acp` command surface: the ACP stdio
//! transport is gone, but the in-process `TestAgentFactory` remains the
//! seam tests use to bootstrap a full profile runtime around a mock
//! `LlmProvider`. Test-only: mounted behind `#[cfg(all(test, feature = "api"))]`.

use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;

use crate::config::Config as OctosConfig;

/// Supplies the shared OUP runtime to in-process embedders and tests.
///
/// Requires the default `api` feature.
#[cfg(feature = "api")]
#[async_trait::async_trait]
pub trait SessionAgentFactory: Send + Sync {
    /// Canonical runtime used by the session transports.
    async fn oup_state(&self) -> Result<Arc<crate::api::AppState>> {
        eyre::bail!("this embedding factory does not provide an OUP runtime")
    }

    /// Profile dimension for this factory's session keys. Sessions elsewhere are
    /// profile-scoped; an unscoped key would collide across profiles running
    /// the same session id and would not isolate them.
    fn session_profile_id(&self) -> String {
        octos_core::MAIN_PROFILE_ID.to_string()
    }
}

/// Test-support factory: hands every session an agent backed by an injected
/// [`octos_llm::LlmProvider`] with episodic memory in an isolated temporary
/// directory.
///
/// Not for production use.
#[cfg(feature = "api")]
#[doc(hidden)]
pub struct TestAgentFactory {
    llm: Arc<dyn octos_llm::LlmProvider>,
    memory_dir: PathBuf,
    oup_state: tokio::sync::OnceCell<Arc<crate::api::AppState>>,
}

#[cfg(feature = "api")]
#[doc(hidden)]
impl TestAgentFactory {
    /// Build a factory that hands every session an agent backed by `llm`, with
    /// episodic memory in `memory_dir`.
    pub fn new(llm: Arc<dyn octos_llm::LlmProvider>, memory_dir: PathBuf) -> Self {
        Self {
            llm,
            memory_dir,
            oup_state: tokio::sync::OnceCell::new(),
        }
    }
}

#[cfg(feature = "api")]
#[async_trait::async_trait]
impl SessionAgentFactory for TestAgentFactory {
    async fn oup_state(&self) -> Result<Arc<crate::api::AppState>> {
        self.oup_state
            .get_or_try_init(|| async {
                use crate::runtime::local_oup::{LocalOupOptions, bootstrap, local_profile};
                let data_dir = self.memory_dir.clone();
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
}
