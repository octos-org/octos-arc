//! Session persistence, resume sanitization, and file-handle resolution for
//! the octos chat/serve runtime.

pub mod file_handle;
pub mod session;

pub use session::{
    ActiveSessionStore, AnalysisFile, AnalysisSession, MessageCommitObserver, Session,
    SessionHandle, SessionListEntry, SessionManager, persist_message_through_canonical_path,
    set_message_commit_observer, set_scoped_message_commit_observer, validate_topic_name,
};
