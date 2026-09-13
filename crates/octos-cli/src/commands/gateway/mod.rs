//! Library surface kept from the retired `octos gateway` command:
//! the session-key builder shared by the OUP dispatcher.

use octos_core::{MAIN_PROFILE_ID, SessionKey};

pub mod profile_factory;
pub use profile_factory::{profile_plugin_env, profile_search_provider_keys};
pub use prompt::build_system_prompt;
pub mod prompt;
pub mod session_ui;

pub(crate) fn build_profiled_session_key(
    profile_id: Option<&str>,
    channel: &str,
    chat_id: &str,
    topic: &str,
) -> SessionKey {
    let effective_profile_id = profile_id.unwrap_or(MAIN_PROFILE_ID);
    SessionKey::with_profile_topic(effective_profile_id, channel, chat_id, topic)
}
