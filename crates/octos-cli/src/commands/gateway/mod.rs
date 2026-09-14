//! Library surface kept from the retired `octos gateway` command:
//! the session-key builder shared by the OUP dispatcher.

pub mod profile_factory;
pub use profile_factory::profile_plugin_env;
pub use prompt::build_system_prompt;
pub mod prompt;
pub mod session_ui;
