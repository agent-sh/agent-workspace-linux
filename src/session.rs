pub(crate) mod x11;

use crate::desktop::DesktopControl;
use crate::workspace::{DaemonOptions, WorkspaceStatus};
use anyhow::{bail, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path, process::Command, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSpec {
    #[serde(default)]
    pub backend: SessionBackend,
}

impl SessionSpec {
    pub fn resolve(explicit: Option<Self>, profile: Option<Self>) -> Self {
        explicit.or(profile).unwrap_or_default()
    }

    pub(crate) fn start(self, options: &DaemonOptions) -> Result<x11::X11Session> {
        match self.backend {
            SessionBackend::X11 => x11::X11Session::start(options),
        }
    }

    pub(crate) fn desktop_control<'a>(
        self,
        status: &'a WorkspaceStatus,
    ) -> Box<dyn DesktopControl + 'a> {
        match self.backend {
            SessionBackend::X11 => Box::new(crate::desktop::x11::X11DesktopControl::new(status)),
        }
    }

    pub(crate) fn configure_process_environment(
        self,
        command: &mut Command,
        display: &str,
        xauthority: &Path,
    ) {
        match self.backend {
            SessionBackend::X11 => {
                x11::configure_process_environment(command, display, xauthority);
            }
        }
    }

    pub(crate) fn sandbox_resources(
        self,
        xauthority_path: &Path,
    ) -> crate::launch::SessionSandboxResources {
        match self.backend {
            SessionBackend::X11 => x11::sandbox_resources(xauthority_path),
        }
    }

    pub(crate) fn chrome_ozone_platform(self) -> &'static str {
        match self.backend {
            SessionBackend::X11 => x11::CHROME_OZONE_PLATFORM,
        }
    }

    pub(crate) fn cleanup_stale_processes(
        self,
        window_manager_pid: Option<u32>,
        x_server_pid: Option<u32>,
        dry_run: bool,
        actions: &mut Vec<String>,
    ) {
        match self.backend {
            SessionBackend::X11 => {
                x11::cleanup_stale_processes(window_manager_pid, x_server_pid, dry_run, actions);
            }
        }
    }
}

impl Default for SessionSpec {
    fn default() -> Self {
        Self {
            backend: SessionBackend::X11,
        }
    }
}

impl FromStr for SessionSpec {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Ok(Self {
            backend: value.parse()?,
        })
    }
}

impl fmt::Display for SessionSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.backend.as_str())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionBackend {
    #[default]
    X11,
}

impl SessionBackend {
    fn as_str(self) -> &'static str {
        match self {
            Self::X11 => "x11",
        }
    }
}

impl FromStr for SessionBackend {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "x11" => Ok(Self::X11),
            _ => bail!("session backend must be x11 (Xvfb with Openbox)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_session_fields_default_to_x11() {
        let empty: SessionSpec = serde_json::from_str("{}").expect("empty session should parse");
        let missing_backend: SessionSpec =
            serde_json::from_str(r#"{"backend":"x11"}"#).expect("x11 session should parse");

        assert_eq!(empty, SessionSpec::default());
        assert_eq!(missing_backend.backend, SessionBackend::X11);
        assert_eq!(SessionSpec::resolve(None, None), SessionSpec::default());
    }

    #[test]
    fn explicit_session_precedes_profile_and_default() {
        let explicit = SessionSpec::default();
        let profile = SessionSpec::default();

        assert_eq!(
            SessionSpec::resolve(Some(explicit), Some(profile)),
            explicit
        );
        assert_eq!(SessionSpec::resolve(None, Some(profile)), profile);
        assert_eq!(SessionSpec::resolve(Some(explicit), None), explicit);
    }

    #[test]
    fn unimplemented_session_backends_are_rejected() {
        assert!("kwin".parse::<SessionSpec>().is_err());
        assert!(serde_json::from_str::<SessionSpec>(r#"{"backend":"gnome"}"#).is_err());
    }
}
