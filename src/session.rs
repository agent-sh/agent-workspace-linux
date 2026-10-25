pub(crate) mod gnome;
pub(crate) mod hyprland;
pub(crate) mod i3;
pub(crate) mod kwin;
pub(crate) mod private;
pub(crate) mod x11;

use crate::desktop::DesktopControl;
use crate::workspace::{DaemonOptions, WorkspaceStatus};
use anyhow::{bail, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path, process::Command, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSpec {
    #[serde(default)]
    pub backend: SessionBackend,
    #[serde(default = "default_scale", skip_serializing_if = "is_default_scale")]
    pub scale: f64,
}

impl SessionSpec {
    pub fn resolve(explicit: Option<Self>, profile: Option<Self>) -> Self {
        explicit.or(profile).unwrap_or_default()
    }

    pub fn describe_policy(self, policy: &mut crate::policy::AppliedWorkspacePolicy) {
        let (backend, detail) = match self.backend {
            SessionBackend::Gnome => (
                "gnome_wayland_runtime",
                "workspace apps use a private GNOME Wayland display and session bus",
            ),
            SessionBackend::Kwin => (
                "kwin_wayland_runtime",
                "workspace apps use a private KWin Wayland display and session bus",
            ),
            SessionBackend::Hyprland => (
                "hyprland_wayland_runtime",
                "workspace apps use private Hyprland Wayland and XWayland displays and a private session bus",
            ),
            SessionBackend::I3 => (
                "i3_xauth_display",
                "workspace apps run on a private X11 display managed by i3 with scoped authority",
            ),
            SessionBackend::X11 => (
                "xvfb_xauth_display",
                "workspace apps run on a private X11 DISPLAY with a scoped XAUTHORITY",
            ),
        };
        policy.enforcement.display_isolation.backend = Some(backend.to_owned());
        policy.enforcement.display_isolation.detail = detail.to_owned();
    }
    pub fn matches(self, other: Self) -> bool {
        self.backend == other.backend
            && (self.scale - other.scale).abs()
                <= match self.backend {
                    SessionBackend::Kwin | SessionBackend::Hyprland => 0.5 / 256.0,
                    // Mutter exposes monitor scale as a single-precision float.
                    SessionBackend::Gnome => 1e-6,
                    SessionBackend::X11 | SessionBackend::I3 => 0.0,
                }
    }
    pub fn validate(self) -> Result<()> {
        if !self.scale.is_finite() || self.scale <= 0.0 {
            bail!("session scale must be finite and positive");
        }
        if matches!(self.backend, SessionBackend::X11 | SessionBackend::I3) && self.scale != 1.0 {
            bail!("X11 sessions use scale 1; choose a native Wayland backend for output scaling");
        }
        Ok(())
    }

    pub fn blockers(self) -> Vec<String> {
        match self.backend {
            SessionBackend::X11 => crate::workspace::doctor_report().blockers,
            SessionBackend::Kwin => kwin::blockers(),
            SessionBackend::Gnome => gnome::blockers(),
            SessionBackend::Hyprland => hyprland::blockers(),
            SessionBackend::I3 => i3::blockers(),
        }
    }

    pub(crate) fn start(self, options: &mut DaemonOptions) -> Result<Session> {
        self.validate()?;
        match self.backend {
            SessionBackend::X11 => x11::X11Session::start(options).map(Session::X11),
            SessionBackend::Kwin => kwin::KwinSession::start(options).map(Session::Kwin),
            SessionBackend::Gnome => gnome::GnomeSession::start(options).map(Session::Gnome),
            SessionBackend::Hyprland => {
                hyprland::HyprlandSession::start(options).map(Session::Hyprland)
            }
            SessionBackend::I3 => x11::X11Session::start(options).map(Session::X11),
        }
    }

    pub(crate) fn desktop_control<'a>(
        self,
        status: &'a WorkspaceStatus,
    ) -> Box<dyn DesktopControl + 'a> {
        match self.backend {
            SessionBackend::X11 | SessionBackend::I3 => {
                Box::new(crate::desktop::x11::X11DesktopControl::new(status))
            }
            SessionBackend::Gnome => {
                Box::new(crate::desktop::gnome::GnomeDesktopControl::new(status))
            }
            SessionBackend::Kwin => Box::new(crate::desktop::kwin::KwinDesktopControl::new(status)),
            SessionBackend::Hyprland => Box::new(
                crate::desktop::hyprland::HyprlandDesktopControl::new(status),
            ),
        }
    }

    pub(crate) fn configure_process_environment(
        self,
        command: &mut Command,
        display: &str,
        xauthority: &Path,
        runtime_dir: &Path,
    ) {
        match self.backend {
            SessionBackend::X11 | SessionBackend::I3 => {
                x11::configure_process_environment(command, display, xauthority);
            }
            SessionBackend::Gnome => {
                gnome::configure_process_environment(command, runtime_dir, display, xauthority)
            }
            SessionBackend::Hyprland => {
                hyprland::configure_process_environment(command, runtime_dir, display, xauthority)
            }
            SessionBackend::Kwin => {
                kwin::configure_process_environment(command, runtime_dir, display, xauthority)
            }
        }
    }

    pub(crate) fn sandbox_resources(
        self,
        xauthority_path: &Path,
        runtime_dir: &Path,
        display: &str,
    ) -> crate::launch::SessionSandboxResources {
        match self.backend {
            SessionBackend::X11 | SessionBackend::I3 => {
                x11::sandbox_resources(xauthority_path, display)
            }
            SessionBackend::Gnome => {
                gnome::sandbox_resources(runtime_dir, xauthority_path, display)
            }
            SessionBackend::Kwin => kwin::sandbox_resources(runtime_dir, xauthority_path, display),
            SessionBackend::Hyprland => {
                hyprland::sandbox_resources(runtime_dir, xauthority_path, display)
            }
        }
    }

    pub(crate) fn chrome_ozone_platform(self) -> &'static str {
        match self.backend {
            SessionBackend::X11 | SessionBackend::I3 => x11::CHROME_OZONE_PLATFORM,
            SessionBackend::Kwin | SessionBackend::Gnome | SessionBackend::Hyprland => "wayland",
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
            SessionBackend::Gnome => {
                gnome::cleanup_stale_processes(window_manager_pid, dry_run, actions)
            }
            SessionBackend::Hyprland => {
                hyprland::cleanup_stale_processes(window_manager_pid, dry_run, actions)
            }
            SessionBackend::I3 => {
                i3::cleanup_stale_processes(window_manager_pid, x_server_pid, dry_run, actions)
            }
            SessionBackend::Kwin => {
                kwin::cleanup_stale_processes(window_manager_pid, dry_run, actions)
            }
        }
    }
}

impl Default for SessionSpec {
    fn default() -> Self {
        Self {
            backend: SessionBackend::X11,
            scale: default_scale(),
        }
    }
}

impl FromStr for SessionSpec {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Ok(Self {
            backend: value.parse()?,
            scale: default_scale(),
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
    Kwin,
    Gnome,
    Hyprland,
    I3,
}

impl SessionBackend {
    pub(crate) fn requires_client_application_bus(self) -> bool {
        matches!(self, Self::Kwin | Self::Gnome | Self::Hyprland)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::X11 => "x11",
            Self::Kwin => "kwin",
            Self::Gnome => "gnome",
            Self::Hyprland => "hyprland",
            Self::I3 => "i3",
        }
    }
}

impl FromStr for SessionBackend {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "x11" => Ok(Self::X11),
            "kwin" => Ok(Self::Kwin),
            "gnome" => Ok(Self::Gnome),
            "hyprland" => Ok(Self::Hyprland),
            "i3" => Ok(Self::I3),
            _ => bail!(
                "session backend must be x11 (Xvfb with Openbox), i3 (Xvfb with i3), kwin, gnome or hyprland (native Wayland)"
            ),
        }
    }
}

fn default_scale() -> f64 {
    1.0
}

pub(crate) enum Session {
    X11(x11::X11Session),
    Kwin(kwin::KwinSession),
    Gnome(gnome::GnomeSession),
    Hyprland(hyprland::HyprlandSession),
}
impl Session {
    pub fn x_server_pid(&self) -> u32 {
        match self {
            Self::X11(s) => s.x_server_pid(),
            Self::Kwin(_) | Self::Gnome(_) | Self::Hyprland(_) => 0,
        }
    }
    pub fn window_manager_pid(&self) -> Option<u32> {
        match self {
            Self::X11(s) => s.window_manager_pid(),
            Self::Kwin(s) => s.pid(),
            Self::Gnome(s) => s.pid(),
            Self::Hyprland(s) => s.pid(),
        }
    }
    pub fn shutdown(&mut self) {
        match self {
            Self::X11(s) => s.shutdown(),
            Self::Kwin(s) => s.shutdown(),
            Self::Gnome(s) => s.shutdown(),
            Self::Hyprland(s) => s.shutdown(),
        }
    }
}

fn is_default_scale(scale: &f64) -> bool {
    *scale == 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_scale_identity_allows_only_fixed_point_rounding() {
        let requested = SessionSpec {
            backend: SessionBackend::Kwin,
            scale: 1.3,
        };
        assert!(requested.matches(SessionSpec {
            scale: 1.30078125,
            ..requested
        }));
        assert!(!requested.matches(SessionSpec {
            scale: 1.5,
            ..requested
        }));
        assert!(!requested.matches(SessionSpec::default()));
        let gnome = SessionSpec {
            backend: SessionBackend::Gnome,
            scale: 4.0 / 3.0,
        };
        assert!(gnome.matches(SessionSpec {
            scale: 1.3333333730697632,
            ..gnome
        }));
        assert!(!gnome.matches(SessionSpec {
            scale: 1.5,
            ..gnome
        }));
    }

    #[test]
    fn scale_requires_a_positive_finite_native_output() {
        let mut session: SessionSpec = "kwin".parse().unwrap();
        session.scale = 1.5;
        assert!(session.validate().is_ok());
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            session.scale = scale;
            assert!(session.validate().is_err());
        }
        session.backend = SessionBackend::X11;
        session.scale = 1.5;
        assert!(session.validate().is_err());
    }

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
        let profile = "kwin".parse::<SessionSpec>().expect("native profile");

        assert_eq!(
            SessionSpec::resolve(Some(explicit), Some(profile)),
            explicit
        );
        assert_eq!(SessionSpec::resolve(None, Some(profile)), profile);
        assert_eq!(SessionSpec::resolve(Some(explicit), None), explicit);
    }

    #[test]
    fn unimplemented_session_backends_are_rejected() {
        assert!("kwin".parse::<SessionSpec>().is_ok());
        assert!(serde_json::from_str::<SessionSpec>(r#"{"backend":"gnome"}"#).is_ok());
        assert!(serde_json::from_str::<SessionSpec>(r#"{"backend":"unknown"}"#).is_err());
    }
}
