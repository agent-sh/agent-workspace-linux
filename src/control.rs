use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const CONTROL_FILE: &str = "mcp-control.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpControlMode {
    #[default]
    Active,
    ReadOnly,
    Paused,
}

impl McpControlMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "active" | "run" | "running" => Ok(Self::Active),
            "read_only" | "read-only" | "readonly" | "ro" => Ok(Self::ReadOnly),
            "paused" | "pause" => Ok(Self::Paused),
            other => {
                bail!("unknown MCP control mode {other:?}. Expected active, read_only, or paused")
            }
        }
    }

    pub fn button_label(self) -> &'static str {
        match self {
            Self::Active => "Act",
            Self::ReadOnly => "RO",
            Self::Paused => "Pause",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReadOnly => "read-only",
            Self::Paused => "paused",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReadOnly => "read_only",
            Self::Paused => "paused",
        }
    }

    pub fn allows_agent_mutation(self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpControlState {
    #[serde(default)]
    pub mode: McpControlMode,
    #[serde(default)]
    pub updated_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Default for McpControlState {
    fn default() -> Self {
        Self {
            mode: McpControlMode::Active,
            updated_at_unix: 0,
            updated_by: None,
            reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct McpControlStatus {
    pub path: PathBuf,
    pub state: McpControlState,
}

pub fn control_status() -> Result<McpControlStatus> {
    let path = control_state_path();
    let state = load_control_state_from_path(&path)?;
    Ok(McpControlStatus { path, state })
}

pub fn strict_control_status() -> Result<McpControlStatus> {
    let path = control_state_path();
    let state = load_existing_control_state_from_path(&path)?;
    Ok(McpControlStatus { path, state })
}

pub fn ensure_control_state_initialized(
    updated_by: impl Into<String>,
    reason: Option<String>,
) -> Result<()> {
    let path = control_state_path();
    ensure_control_state_initialized_at_path(&path, updated_by, reason)
}

pub fn set_control_mode(
    mode: McpControlMode,
    updated_by: impl Into<String>,
    reason: Option<String>,
) -> Result<McpControlStatus> {
    let path = control_state_path();
    let state = McpControlState {
        mode,
        updated_at_unix: wall_clock_seconds(),
        updated_by: Some(updated_by.into()),
        reason: reason.filter(|reason| !reason.trim().is_empty()),
    };
    save_control_state_to_path(&path, &state)?;
    Ok(McpControlStatus { path, state })
}

fn load_control_state_from_path(path: &Path) -> Result<McpControlState> {
    if !path.exists() {
        return Ok(McpControlState::default());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(McpControlState::default());
    }
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn load_existing_control_state_from_path(path: &Path) -> Result<McpControlState> {
    if !path.exists() {
        bail!("MCP control state missing at {}", path.display());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        bail!("MCP control state empty at {}", path.display());
    }
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn ensure_control_state_initialized_at_path(
    path: &Path,
    updated_by: impl Into<String>,
    reason: Option<String>,
) -> Result<()> {
    if load_existing_control_state_from_path(path).is_ok() {
        return Ok(());
    }
    let state = McpControlState {
        mode: McpControlMode::Active,
        updated_at_unix: wall_clock_seconds(),
        updated_by: Some(updated_by.into()),
        reason: reason.filter(|reason| !reason.trim().is_empty()),
    };
    save_control_state_to_path(path, &state)
}

fn save_control_state_to_path(path: &Path, state: &McpControlState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temp_path = path.with_extension("json.tmp");
    let content =
        serde_json::to_string_pretty(state).context("failed to serialize MCP control state")?;
    fs::write(&temp_path, format!("{content}\n"))
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to move {} to {}",
            temp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn control_state_path() -> PathBuf {
    runtime_dir_from_env(env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)).join(CONTROL_FILE)
}

fn runtime_dir_from_env(xdg_runtime_dir: Option<PathBuf>) -> PathBuf {
    xdg_runtime_dir
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(env::temp_dir)
        .join("agent-workspace-linux")
}

fn wall_clock_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_control_path(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "agent-workspace-control-test-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn missing_control_state_defaults_to_active() {
        let path = temp_control_path("missing");
        let _ = fs::remove_file(&path);
        let state = load_control_state_from_path(&path).expect("missing state loads");
        assert_eq!(state.mode, McpControlMode::Active);
    }

    #[test]
    fn strict_missing_control_state_fails_closed() {
        let path = temp_control_path("strict-missing");
        let _ = fs::remove_file(&path);
        let error =
            load_existing_control_state_from_path(&path).expect_err("missing state should fail");
        assert!(error.to_string().contains("MCP control state missing"));
    }

    #[test]
    fn strict_empty_control_state_fails_closed() {
        let path = temp_control_path("strict-empty");
        fs::write(&path, "\n").expect("write empty state");
        let error =
            load_existing_control_state_from_path(&path).expect_err("empty state should fail");
        assert!(error.to_string().contains("MCP control state empty"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn control_state_round_trips() {
        let path = temp_control_path("round-trip");
        let _ = fs::remove_file(&path);
        let state = McpControlState {
            mode: McpControlMode::ReadOnly,
            updated_at_unix: 42,
            updated_by: Some("test".to_string()),
            reason: Some("smoke".to_string()),
        };
        save_control_state_to_path(&path, &state).expect("save state");
        let loaded = load_control_state_from_path(&path).expect("load state");
        assert_eq!(loaded.mode, McpControlMode::ReadOnly);
        assert_eq!(loaded.updated_at_unix, 42);
        assert_eq!(loaded.updated_by.as_deref(), Some("test"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn invalid_control_state_fails_closed() {
        let path = temp_control_path("invalid");
        fs::write(&path, "{not json").expect("write invalid state");
        let error = load_control_state_from_path(&path).expect_err("invalid state should fail");
        assert!(error.to_string().contains("failed to parse"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn initialization_repairs_corrupt_control_state() {
        let path = temp_control_path("repair-corrupt");
        fs::write(&path, "{not json").expect("write invalid state");

        ensure_control_state_initialized_at_path(
            &path,
            "test",
            Some("repair corrupt state".to_string()),
        )
        .expect("repair corrupt state");

        let loaded = load_existing_control_state_from_path(&path).expect("load repaired state");
        assert_eq!(loaded.mode, McpControlMode::Active);
        assert_eq!(loaded.updated_by.as_deref(), Some("test"));
        assert_eq!(loaded.reason.as_deref(), Some("repair corrupt state"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn initialization_preserves_valid_control_state() {
        let path = temp_control_path("preserve-valid");
        let state = McpControlState {
            mode: McpControlMode::ReadOnly,
            updated_at_unix: 42,
            updated_by: Some("existing".to_string()),
            reason: Some("user paused".to_string()),
        };
        save_control_state_to_path(&path, &state).expect("save state");

        ensure_control_state_initialized_at_path(&path, "test", None)
            .expect("preserve valid state");

        let loaded = load_existing_control_state_from_path(&path).expect("load state");
        assert_eq!(loaded.mode, McpControlMode::ReadOnly);
        assert_eq!(loaded.updated_at_unix, 42);
        assert_eq!(loaded.updated_by.as_deref(), Some("existing"));
        assert_eq!(loaded.reason.as_deref(), Some("user paused"));
        let _ = fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn strict_unreadable_control_state_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_control_path("strict-unreadable");
        let _ = fs::remove_file(&path);
        fs::write(&path, r#"{"mode":"active"}"#).expect("write state");
        let original_permissions = fs::metadata(&path).expect("state metadata").permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
            .expect("remove state permissions");

        let result = load_existing_control_state_from_path(&path);

        fs::set_permissions(&path, original_permissions).expect("restore state permissions");
        let _ = fs::remove_file(&path);

        if result.is_ok() {
            eprintln!("skipping unreadable-file assertion for privileged test user");
            return;
        }
        let error = result.expect_err("unreadable state should fail");
        assert!(error.to_string().contains("failed to read"));
    }

    #[test]
    fn control_mode_parses_aliases() {
        assert_eq!(
            McpControlMode::parse("ro").unwrap(),
            McpControlMode::ReadOnly
        );
        assert_eq!(
            McpControlMode::parse("pause").unwrap(),
            McpControlMode::Paused
        );
    }
}
