use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalBundle {
    pub action: String,
    pub subject: String,
    pub approved: bool,
    pub blocked: bool,
    pub would_execute: bool,
    pub requires_user_approval: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_acknowledgements: Vec<ApprovalRequirement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_acknowledgements: Vec<ApprovalRequirement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approve_cli_flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approve_mcp_parameters: Vec<ApprovalMcpParameter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequirement {
    pub id: String,
    pub label: String,
    pub description: String,
    pub acknowledged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_flag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_parameter: Option<ApprovalMcpParameter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalMcpParameter {
    pub name: String,
    pub value: bool,
}

impl Default for ApprovalBundle {
    fn default() -> Self {
        Self::new("unknown", "unknown", false)
    }
}

impl ApprovalBundle {
    pub fn new(action: impl Into<String>, subject: impl Into<String>, would_execute: bool) -> Self {
        let mut bundle = Self {
            action: action.into(),
            subject: subject.into(),
            approved: true,
            blocked: false,
            would_execute,
            requires_user_approval: false,
            required_acknowledgements: Vec::new(),
            missing_acknowledgements: Vec::new(),
            blockers: Vec::new(),
            approve_cli_flags: Vec::new(),
            approve_mcp_parameters: Vec::new(),
        };
        bundle.refresh();
        bundle
    }

    pub fn require_acknowledgement(
        mut self,
        required: bool,
        requirement: ApprovalRequirement,
    ) -> Self {
        if required {
            self.push_requirement(requirement);
        }
        self.refresh();
        self
    }

    pub fn add_blocker(mut self, blocker: impl Into<String>) -> Self {
        let blocker = blocker.into();
        if !blocker.is_empty() && !self.blockers.iter().any(|existing| existing == &blocker) {
            self.blockers.push(blocker);
        }
        self.refresh();
        self
    }

    pub fn add_blockers(mut self, blockers: impl IntoIterator<Item = String>) -> Self {
        for blocker in blockers {
            if !blocker.is_empty() && !self.blockers.iter().any(|existing| existing == &blocker) {
                self.blockers.push(blocker);
            }
        }
        self.refresh();
        self
    }

    pub fn merge_child(mut self, child: &ApprovalBundle) -> Self {
        for requirement in &child.required_acknowledgements {
            self.push_requirement(requirement.clone());
        }
        for blocker in &child.blockers {
            if !self.blockers.iter().any(|existing| existing == blocker) {
                self.blockers.push(blocker.clone());
            }
        }
        self.refresh();
        self
    }

    pub fn retarget(
        mut self,
        action: impl Into<String>,
        subject: impl Into<String>,
        would_execute: bool,
    ) -> Self {
        self.action = action.into();
        self.subject = subject.into();
        self.would_execute = would_execute;
        self.refresh();
        self
    }

    fn push_requirement(&mut self, requirement: ApprovalRequirement) {
        if let Some(existing) = self
            .required_acknowledgements
            .iter_mut()
            .find(|existing| existing.id == requirement.id)
        {
            existing.acknowledged = existing.acknowledged && requirement.acknowledged;
        } else if !self
            .required_acknowledgements
            .iter()
            .any(|existing| existing.id == requirement.id)
        {
            self.required_acknowledgements.push(requirement.clone());
        }
        if !requirement.acknowledged
            && !self
                .missing_acknowledgements
                .iter()
                .any(|existing| existing.id == requirement.id)
        {
            if let Some(flag) = requirement.cli_flag.clone() {
                if !self
                    .approve_cli_flags
                    .iter()
                    .any(|existing| existing == &flag)
                {
                    self.approve_cli_flags.push(flag);
                }
            }
            if let Some(parameter) = requirement.mcp_parameter.clone() {
                if !self
                    .approve_mcp_parameters
                    .iter()
                    .any(|existing| existing.name == parameter.name)
                {
                    self.approve_mcp_parameters.push(parameter);
                }
            }
            self.missing_acknowledgements.push(requirement);
        }
    }

    fn refresh(&mut self) {
        self.requires_user_approval = !self.missing_acknowledgements.is_empty();
        self.blocked = !self.blockers.is_empty();
        self.approved = !self.requires_user_approval && !self.blocked;
    }
}

pub fn hidden_workspace_acknowledgement(acknowledged: bool) -> ApprovalRequirement {
    ApprovalRequirement {
        id: "hidden_workspace".to_string(),
        label: "Hidden workspace".to_string(),
        description:
            "User acknowledges that the agent will run in a separate workspace environment."
                .to_string(),
        acknowledged,
        cli_flag: Some("--ack-hidden-workspace".to_string()),
        mcp_parameter: Some(ApprovalMcpParameter {
            name: "acknowledge_hidden_workspace".to_string(),
            value: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_bundle_deduplicates_missing_acknowledgements() {
        let bundle = ApprovalBundle::new("workspace_open_profile", "profile qa", false)
            .require_acknowledgement(false, hidden_workspace_acknowledgement(true))
            .require_acknowledgement(true, hidden_workspace_acknowledgement(false))
            .require_acknowledgement(true, hidden_workspace_acknowledgement(false));

        assert!(!bundle.approved);
        assert!(bundle.requires_user_approval);
        assert_eq!(bundle.required_acknowledgements.len(), 1);
        assert_eq!(bundle.missing_acknowledgements.len(), 1);
        assert_eq!(bundle.missing_acknowledgements[0].id, "hidden_workspace");
        assert_eq!(bundle.approve_cli_flags, vec!["--ack-hidden-workspace"]);
        assert_eq!(bundle.approve_mcp_parameters.len(), 1);
        assert_eq!(
            bundle.approve_mcp_parameters[0].name,
            "acknowledge_hidden_workspace"
        );
        assert!(bundle.approve_mcp_parameters[0].value);
    }
}
