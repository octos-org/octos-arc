//! Session persistence, resume sanitization, file-handle resolution, and cron
//! scheduling for the octos chat/serve runtime.

pub mod cron_service;
pub mod cron_types;
pub mod file_handle;
pub mod media;
pub mod resume_policy;
pub mod session;

pub use cron_service::{CronService, write_cron_json_atomic};
pub use cron_types::{CronJob, CronMode, CronOrigin, CronPayload, CronSchedule, CronStore};
pub use resume_policy::{
    RESUME_MTIME_MARKER, ReplacementStateRef, ResumePolicy, RetryStateView, SanitizeError,
    SanitizeOutcome, SessionSanitizeReport, filter_orphaned_thinking_only_messages,
    filter_unresolved_tool_uses, filter_whitespace_only_assistant_messages,
    reconstruct_content_replacement_state,
};
pub use session::{
    ActiveSessionStore, AnalysisFile, AnalysisSession, MessageCommitObserver, Session,
    SessionHandle, SessionListEntry, SessionManager, persist_message_through_canonical_path,
    set_message_commit_observer, set_scoped_message_commit_observer, validate_topic_name,
};
