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
    pub ready_for_gnome_workspace: bool,
    pub ready_for_i3_workspace: bool,
    pub ready_for_hyprland_workspace: bool,
    pub ready_for_cosmic_workspace: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_mode_summary_exposes_native_backend_readiness() {
        let schema = serde_json::to_value(schemars::schema_for!(AgentModeSummary))
            .expect("agent mode schema should serialize");
        for field in [
            "ready_for_i3_workspace",
            "ready_for_hyprland_workspace",
            "ready_for_cosmic_workspace",
        ] {
            assert!(schema["properties"].get(field).is_some(), "{field}");
        }

        let response = serde_json::to_value(AgentModeSummary {
            ready_for_i3_workspace: true,
            ready_for_hyprland_workspace: true,
            ready_for_cosmic_workspace: true,
            ..AgentModeSummary::default()
        })
        .expect("agent mode response should serialize");
        assert_eq!(response["ready_for_i3_workspace"], true);
        assert_eq!(response["ready_for_hyprland_workspace"], true);
        assert_eq!(response["ready_for_cosmic_workspace"], true);
    }
}
