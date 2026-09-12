//! Message bus, channels, and session management for octos gateway.

pub mod bus;
pub mod channel;
pub mod cli_channel;
pub mod coalesce;
pub mod cron_service;
pub mod cron_types;
pub mod dedup;
pub mod file_handle;
pub mod heartbeat;
pub mod markdown_html;
pub mod media;
pub mod resume_policy;
pub mod api_channel;
pub mod session;


pub use bus::{AgentHandle, BusPublisher, create_bus};
pub use channel::{Channel, ChannelHealth, ChannelManager};
pub use cli_channel::CliChannel;
pub use cron_service::{CronService, write_cron_json_atomic};
pub use cron_types::{CronJob, CronMode, CronOrigin, CronPayload, CronSchedule, CronStore};
pub use dedup::MessageDedup;
pub use heartbeat::HeartbeatService;
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

#[cfg(feature = "api")]
pub use api_channel::{ApiChannel, TaskCancelOutcome, TaskRelaunchOutcome};
#[cfg(feature = "dingtalk")]
pub use dingtalk_channel::DingTalkChannel;
#[cfg(feature = "discord")]
pub use discord_channel::DiscordChannel;
#[cfg(feature = "email")]
pub use email_channel::EmailChannel;
#[cfg(feature = "feishu")]
pub use feishu_channel::FeishuChannel;
#[cfg(feature = "line")]
pub use line_channel::LineChannel;
#[cfg(feature = "matrix")]
pub use matrix_channel::{
    BotEntry, BotManager, BotRouter, BotVisibility, MatrixChannel, MatrixEventId, MatrixRoomId,
    MatrixUserId, SWARM_SUPERVISOR_EVENT_SCHEMA_V1, SteeringInput, SwarmHarnessEvent,
    SwarmSupervisorParams,
};
#[cfg(feature = "matrix")]
pub use matrix_user_channel::{
    MatrixAutoJoin, MatrixGroupPolicy, MatrixInviteStore, MatrixPendingInvite, MatrixUserChannel,
};
#[cfg(feature = "qq-bot")]
pub use qq_bot_channel::QQBotChannel;
#[cfg(feature = "slack")]
pub use slack_channel::SlackChannel;
#[cfg(feature = "telegram")]
pub use telegram_channel::TelegramChannel;
#[cfg(feature = "twilio")]
pub use twilio_channel::TwilioChannel;
#[cfg(feature = "wechat")]
pub use wechat_channel::WeChatChannel;
#[cfg(feature = "wecom-bot")]
pub use wecom_bot_channel::WeComBotChannel;
#[cfg(feature = "wecom")]
pub use wecom_channel::WeComChannel;
#[cfg(feature = "whatsapp")]
pub use whatsapp_channel::WhatsAppChannel;
