use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AgentModeSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_mode: Option<String>,
    pub allows_agent_mutation: bool,
    pub headless: bool,
    pub ready_for_x11_workspace: bool,
    pub ready_for_kwin_workspace: bool,
    pub ready_for_host_viewer: bool,
    pub viewer_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer_unavailable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exact_reactivation_parameters: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AgentTargetHandles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub app_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub window_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub viewer_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub browser_target_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminal_ids: Vec<String>,
}

impl AgentTargetHandles {
    pub fn is_empty(&self) -> bool {
        self.workspace_id.is_none()
            && self.app_ids.is_empty()
            && self.window_ids.is_empty()
            && self.viewer_ids.is_empty()
            && self.browser_target_ids.is_empty()
            && self.terminal_ids.is_empty()
    }
}
