//! RingingV2 的唯一业务命令目录。

use qaqh_domain::ContentRef;
use qaqh_ringing_contract::commands::{
    AskAnswer, ImageBlock, InterAgentEnvelope, SubagentTerminalNotification,
};
use qaqh_ringing_contract::fact::InputPurpose;
use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::{ProviderUpsertDto, SessionSettingsPatch};

/// 完整会话输入 payload。会话 owner 和 SDK 消费同一个值，不再投影成内部
/// `LoopInput` 或 channel-specific command。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ConversationSendMessage {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageBlock>,
    /// Electron main 上传后的会话附件引用；命令中不允许出现本地路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<ContentRef>>,
    /// Stable inter-agent message identity. User/UI messages may omit it and
    /// fall back to the command id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Trigger, queue or merge this message into a running turn at a safe point.
    #[serde(default)]
    pub input_purpose: InputPurpose,
    /// Set by trusted runtime invocation context, never accepted from public JSON.
    #[serde(default)]
    pub as_system: bool,
    /// Set by trusted runtime invocation context, never accepted from public JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inter_agent: Option<InterAgentEnvelope>,
    /// Set by trusted runtime invocation context, never accepted from public JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_terminal: Option<SubagentTerminalNotification>,
}

/// RingingV2 业务命令根。target、身份、revision 和 driver epoch 只由提交信封表达。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum RingingCommand {
    #[serde(rename = "session.create")]
    SessionCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_selection: Option<qaqh_profile::ModelSelection>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_tools: Option<Vec<String>>,
    },
    #[serde(rename = "session.settings.patch")]
    SessionSettingsPatch { patch: SessionSettingsPatch },
    #[serde(rename = "session.resume")]
    SessionResume {},
    #[serde(rename = "session.attach")]
    SessionAttach {},
    #[serde(rename = "session.close")]
    SessionClose {},
    #[serde(rename = "session.rename")]
    SessionRename { title: String },
    #[serde(rename = "session.archive")]
    SessionArchive {},
    #[serde(rename = "session.unarchive")]
    SessionUnarchive {},
    #[serde(rename = "session.delete")]
    SessionDelete {},
    #[serde(rename = "session.shutdown")]
    SessionShutdown,
    #[serde(rename = "agent.reload_config")]
    AgentReloadConfig,
    #[serde(rename = "conversation.send_message")]
    ConversationSendMessage(ConversationSendMessage),
    #[serde(rename = "conversation.cancel")]
    ConversationCancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    #[serde(rename = "interaction.ask.respond")]
    InteractionAskRespond {
        interaction_id: String,
        answers: Vec<AskAnswer>,
    },
    #[serde(rename = "interaction.ask.dismiss")]
    InteractionAskDismiss { interaction_id: String },
    #[serde(rename = "driver.claim")]
    DriverClaim {
        client_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stale_holder: Option<String>,
    },
    #[serde(rename = "driver.release")]
    DriverRelease {
        client_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_epoch: Option<u64>,
    },
    #[serde(rename = "plan.review.respond")]
    PlanReviewRespond {
        interaction_id: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default)]
        autonomous: bool,
    },
    #[serde(rename = "tool.permission.respond")]
    ToolPermissionRespond {
        tool_call_id: String,
        approved: bool,
        #[serde(default)]
        trust_folder: bool,
    },
    #[serde(rename = "provider.list")]
    ProviderList {},
    #[serde(rename = "provider.upsert")]
    ProviderUpsert {
        id: String,
        provider: ProviderUpsertDto,
    },
    #[serde(rename = "provider.delete")]
    ProviderDelete { id: String },
    #[serde(rename = "provider.model.upsert")]
    ProviderModelUpsert {
        provider_id: String,
        model_id: String,
        model: qaqh_profile::ModelDef,
    },
    #[serde(rename = "provider.model.delete")]
    ProviderModelDelete {
        provider_id: String,
        model_id: String,
    },
    #[serde(rename = "model.selection.set_default")]
    ModelSelectionSetDefault {
        selection: qaqh_profile::ModelSelection,
    },
    #[serde(rename = "config.save")]
    ConfigSave { patch: serde_json::Value },
    #[serde(rename = "config.set_permission_level")]
    ConfigSetPermissionLevel { level: u8 },
    #[serde(rename = "profile.create")]
    ProfileCreate {
        name: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wire: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_length: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_tokens: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
    #[serde(rename = "profile.set_default")]
    ProfileSetDefault {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    #[serde(rename = "profile.apply")]
    ProfileApply { name: String },
    #[serde(rename = "profile.save_current")]
    ProfileSaveCurrent { name: String },
    #[serde(rename = "profile.delete")]
    ProfileDelete { name: String },
    #[serde(rename = "workspace.set")]
    WorkspaceSet { path: String },
    #[serde(rename = "workspace.create")]
    WorkspaceCreate { path: String },
    #[serde(rename = "workspace.rename")]
    WorkspaceRename { title: String },
    #[serde(rename = "workspace.delete")]
    WorkspaceDelete {},
    #[serde(rename = "workspace.move_session")]
    WorkspaceMoveSession { workspace_id: String },
    #[serde(rename = "workspace.detach")]
    WorkspaceDetach {},
    #[serde(rename = "subagent.spawn")]
    SubagentSpawn {
        #[serde(default)]
        tools: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_tokens: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    #[serde(rename = "todo.set")]
    TodoSet { args: serde_json::Value },
    #[serde(rename = "todo.cancel")]
    TodoCancel { id: String },
    #[serde(rename = "plan.action")]
    PlanAction {
        item_id: String,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_comment: Option<String>,
    },
    #[serde(rename = "diagnostics.memory.start")]
    DiagnosticsMemoryStart {},
    #[serde(rename = "diagnostics.memory.stop")]
    DiagnosticsMemoryStop {},
}

impl RingingCommand {
    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionCreate { .. } => "session.create",
            Self::SessionSettingsPatch { .. } => "session.settings.patch",
            Self::SessionResume {} => "session.resume",
            Self::SessionAttach {} => "session.attach",
            Self::SessionClose {} => "session.close",
            Self::SessionRename { .. } => "session.rename",
            Self::SessionArchive {} => "session.archive",
            Self::SessionUnarchive {} => "session.unarchive",
            Self::SessionDelete {} => "session.delete",
            Self::SessionShutdown => "session.shutdown",
            Self::AgentReloadConfig => "agent.reload_config",
            Self::ConversationSendMessage(_) => "conversation.send_message",
            Self::ConversationCancel { .. } => "conversation.cancel",
            Self::InteractionAskRespond { .. } => "interaction.ask.respond",
            Self::InteractionAskDismiss { .. } => "interaction.ask.dismiss",
            Self::DriverClaim { .. } => "driver.claim",
            Self::DriverRelease { .. } => "driver.release",
            Self::PlanReviewRespond { .. } => "plan.review.respond",
            Self::ToolPermissionRespond { .. } => "tool.permission.respond",
            Self::ProviderList { .. } => "provider.list",
            Self::ProviderUpsert { .. } => "provider.upsert",
            Self::ProviderDelete { .. } => "provider.delete",
            Self::ProviderModelUpsert { .. } => "provider.model.upsert",
            Self::ProviderModelDelete { .. } => "provider.model.delete",
            Self::ModelSelectionSetDefault { .. } => "model.selection.set_default",
            Self::ConfigSave { .. } => "config.save",
            Self::ConfigSetPermissionLevel { .. } => "config.set_permission_level",
            Self::ProfileCreate { .. } => "profile.create",
            Self::ProfileSetDefault { .. } => "profile.set_default",
            Self::ProfileApply { .. } => "profile.apply",
            Self::ProfileSaveCurrent { .. } => "profile.save_current",
            Self::ProfileDelete { .. } => "profile.delete",
            Self::WorkspaceSet { .. } => "workspace.set",
            Self::WorkspaceCreate { .. } => "workspace.create",
            Self::WorkspaceRename { .. } => "workspace.rename",
            Self::WorkspaceDelete { .. } => "workspace.delete",
            Self::WorkspaceMoveSession { .. } => "workspace.move_session",
            Self::WorkspaceDetach { .. } => "workspace.detach",
            Self::SubagentSpawn { .. } => "subagent.spawn",
            Self::TodoSet { .. } => "todo.set",
            Self::TodoCancel { .. } => "todo.cancel",
            Self::PlanAction { .. } => "plan.action",
            Self::DiagnosticsMemoryStart { .. } => "diagnostics.memory.start",
            Self::DiagnosticsMemoryStop { .. } => "diagnostics.memory.stop",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConversationSendMessage, RingingCommand};
    use qaqh_ringing_contract::fact::InputPurpose;

    #[test]
    fn conversation_send_message_is_a_flat_ringing_command() {
        let command = RingingCommand::ConversationSendMessage(ConversationSendMessage {
            text: "hello".into(),
            images: Vec::new(),
            attachments: None,
            message_id: None,
            input_purpose: InputPurpose::default(),
            as_system: false,
            inter_agent: None,
            subagent_terminal: None,
        });
        let value = serde_json::to_value(&command).expect("serialize command");
        assert_eq!(value["type"], "conversation.send_message");
        assert_eq!(value["text"], "hello");
        assert!(value.get("message").is_none());

        let decoded: RingingCommand =
            serde_json::from_value(value).expect("deserialize flat command");
        assert!(matches!(
            decoded,
            RingingCommand::ConversationSendMessage(ConversationSendMessage { text, .. })
                if text == "hello"
        ));
    }
}
