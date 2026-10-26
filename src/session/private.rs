//! Owned private-session processes and buses shared by native compositors.
use anyhow::{bail, Context, Result};
use std::{
    fs,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
pub(crate) fn bus_address(runtime: &Path) -> String {
    socket_address(&runtime.join("session-bus"))
}

pub(crate) fn socket_address(path: &Path) -> String {
    use std::{fmt::Write as _, os::unix::ffi::OsStrExt};
    let mut address = String::from("unix:path=");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"_-/.\\".contains(byte) {
            address.push(*byte as char);
        } else {
            write!(address, "%{byte:02X}").expect("write to String");
        }
    }
    address
}

struct AccessibilityBus {
    address: String,
}
#[zbus::interface(name = "org.a11y.Bus")]
impl AccessibilityBus {
    fn get_address(&self) -> &str {
        &self.address
    }
}
struct AccessibilityStatus {
    enabled: bool,
    screen_reader_enabled: bool,
}
#[zbus::interface(name = "org.a11y.Status")]
impl AccessibilityStatus {
    #[zbus(property)]
    fn is_enabled(&self) -> bool {
        self.enabled
    }
    #[zbus(property)]
    fn set_is_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
    #[zbus(property)]
    fn screen_reader_enabled(&self) -> bool {
        self.screen_reader_enabled
    }
    #[zbus(property)]
    fn set_screen_reader_enabled(&mut self, enabled: bool) {
        self.screen_reader_enabled = enabled;
    }
}

pub(crate) struct PrivateSession {
    pub(crate) compositor: Option<Child>,
    bus: Option<Child>,
    system_bus: Option<Child>,
    accessibility_bus: Option<Child>,
    accessibility_services: Vec<zbus::blocking::Connection>,
    user_services: Vec<super::host_services::Relay>,
}
impl PrivateSession {
    pub(crate) fn start(
        runtime: &Path,
        configure: impl Fn(&mut Command),
        extra_policy: &str,
    ) -> Result<Self> {
        let address_file = runtime.join("atspi-address");
        if address_file.exists() {
            fs::remove_file(&address_file)?;
        }
        let mut session = Self {
            compositor: None,
            bus: None,
            system_bus: None,
            accessibility_bus: None,
            accessibility_services: Vec::new(),
            user_services: Vec::new(),
        };
        session.bus = Some(start_private_bus(
            runtime,
            "session",
            &bus_address(runtime),
            &[],
            extra_policy,
            &configure,
        )?);
        session.user_services = user_services(runtime);
        let registry =
            Path::new("/usr/share/dbus-1/accessibility-services/org.a11y.atspi.Registry.service");
        if registry.is_file() {
            fs::create_dir_all(runtime.join("at-spi"))?;
            let address = socket_address(&runtime.join("at-spi/bus"));
            session.accessibility_bus = Some(start_private_bus(
                runtime,
                "accessibility",
                &address,
                &[registry],
                "",
                &configure,
            )?);
            session.accessibility_services.push(
                zbus::blocking::connection::Builder::address(bus_address(runtime).as_str())?
                    .method_timeout(Duration::from_secs(5))
                    .name("org.a11y.Bus")?
                    .serve_at(
                        "/org/a11y/bus",
                        AccessibilityBus {
                            address: address.clone(),
                        },
                    )?
                    .serve_at(
                        "/org/a11y/bus",
                        AccessibilityStatus {
                            enabled: true,
                            screen_reader_enabled: false,
                        },
                    )?
                    .build()?,
            );
            fs::write(address_file, address)?;
        }
        Ok(session)
    }
    /// Some desktop shells require a reachable system bus even when no system
    /// services exist. This empty bus never activates host system services.
    #[allow(dead_code)]
    pub(crate) fn start_system_bus(
        &mut self,
        runtime: &Path,
        configure: impl Fn(&mut Command),
    ) -> Result<()> {
        if self.system_bus.is_some() {
            bail!("private system bus already running");
        }
        self.system_bus = Some(start_private_bus(
            runtime,
            "system",
            &socket_address(&runtime.join("system-bus")),
            &[],
            "",
            &configure,
        )?);
        Ok(())
    }
    pub(crate) fn pid(&self) -> Option<u32> {
        self.compositor.as_ref().map(Child::id)
    }
    pub(crate) fn shutdown(&mut self) {
        self.user_services.clear();
        self.accessibility_services.clear();
        for slot in [
            &mut self.compositor,
            &mut self.accessibility_bus,
            &mut self.bus,
            &mut self.system_bus,
        ] {
            if let Some(mut child) = slot.take() {
                // Allow the compositor to unlink its display sockets before
                // reaping all remaining private-session services.
                let group = -(child.id() as i32);
                unsafe {
                    libc::kill(group, libc::SIGTERM);
                }
                // Observe the exit without reaping: the unreaped leader keeps
                // its PID, so the group SIGKILL below cannot reach a recycled
                // process group.
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && !exited_unreaped(child.id()) {
                    thread::sleep(Duration::from_millis(20));
                }
                unsafe {
                    libc::kill(group, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}
fn exited_unreaped(pid: u32) -> bool {
    !matches!(unreaped_exit(pid), UnreapedExit::Running)
}

pub(crate) enum UnreapedExit {
    Running,
    Exited(String),
    /// Already reaped or not our child.
    Unknown,
}

/// Observe a child's exit without reaping it. Startup checks use this so a
/// dead compositor stays a zombie: its PID, and therefore its process group
/// number, cannot be recycled before the group is signalled on shutdown.
pub(crate) fn unreaped_exit(pid: u32) -> UnreapedExit {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return UnreapedExit::Unknown;
    }
    if unsafe { info.si_pid() } == 0 {
        return UnreapedExit::Running;
    }
    let status = unsafe { info.si_status() };
    UnreapedExit::Exited(if info.si_code == libc::CLD_EXITED {
        format!("exit status: {status}")
    } else {
        format!("signal: {status}")
    })
}
impl Drop for PrivateSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}
/// A workspace session bus on its own, for backends without a private session.
pub(crate) fn start_session_bus(runtime: &Path, configure: impl Fn(&mut Command)) -> Result<Child> {
    start_private_bus(
        runtime,
        "session",
        &bus_address(runtime),
        &[],
        "",
        &configure,
    )
}

/// The user's keyring on the workspace session bus. A workspace without it
/// still starts: apps then cannot reach saved logins, which the log says.
pub(crate) fn user_services(runtime: &Path) -> Vec<super::host_services::Relay> {
    super::host_services::start(&bus_address(runtime)).unwrap_or_else(|error| {
        eprintln!("agent workspace: keyring not relayed into the workspace: {error:#}");
        Vec::new()
    })
}

fn start_private_bus(
    runtime: &Path,
    kind: &str,
    address: &str,
    services: &[&Path],
    extra_policy: &str,
    configure: &impl Fn(&mut Command),
) -> Result<Child> {
    let service_dir = runtime.join(format!("{kind}-services"));
    if service_dir.symlink_metadata().is_ok() {
        if service_dir.symlink_metadata()?.file_type().is_symlink() {
            fs::remove_file(&service_dir)?;
        } else {
            fs::remove_dir_all(&service_dir)?;
        }
    }
    fs::create_dir_all(&service_dir)?;
    for service in services {
        fs::copy(
            service,
            service_dir.join(service.file_name().context("service file name")?),
        )?;
    }
    let escape = |value: &str| {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let config = runtime.join(format!("{kind}-bus.conf"));
    fs::write(
        &config,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig><type>{bus_type}</type><auth>EXTERNAL</auth><listen>{address}</listen>
<servicedir>{services}</servicedir>
<policy context="default">
<allow user="{uid}"/><allow own="*"/><allow send_destination="*"/><allow receive_sender="*"/>
<deny send_destination="org.freedesktop.DBus" send_interface="org.freedesktop.DBus" send_member="UpdateActivationEnvironment"/>
<deny send_destination="org.freedesktop.DBus" send_interface="org.freedesktop.DBus" send_member="ReloadConfig"/>
{extra_policy}
</policy></busconfig>"#,
            bus_type = kind,
            address = escape(address),
            services = escape(&service_dir.to_string_lossy()),
            uid = unsafe { libc::geteuid() }
        ),
    )?;
    let socket = if kind == "session" {
        runtime.join("session-bus")
    } else if kind == "accessibility" {
        runtime.join("at-spi/bus")
    } else {
        runtime.join("system-bus")
    };
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let mut command = Command::new("dbus-daemon");
    configure(&mut command);
    command
        .env("HOME", runtime.join("session-home"))
        .env("GSETTINGS_BACKEND", "memory")
        .args(["--nofork", "--nopidfile", "--config-file"])
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::from(fs::File::create(
            runtime.join(format!("{kind}-bus.log")),
        )?));
    if kind == "accessibility" {
        // Registry activation must connect to this bus before atspi-address is
        // published for app launches. Never discover it through a session bus.
        command.env("AT_SPI_BUS_ADDRESS", address);
    }
    prepare_child(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("start private {kind} bus"))?;
    if let Err(error) = wait_for(
        Duration::from_secs(5),
        || {
            if let Some(status) = child.try_wait()? {
                bail!("private {kind} bus exited with {status}");
            }
            Ok(socket.exists())
        },
        &format!("private {kind} bus"),
    ) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

/// Restrict EGL to Mesa for a software-rendered process. glvnd otherwise
/// tries the NVIDIA vendor first, which fails or crashes without its devices.
/// Hosts without Mesa's vendor file keep the default vendor list.
pub(crate) fn use_mesa_egl(command: &mut Command) {
    if let Some(vendor) = [
        "/usr/share/glvnd/egl_vendor.d/50_mesa.json",
        "/etc/glvnd/egl_vendor.d/50_mesa.json",
    ]
    .into_iter()
    .map(Path::new)
    .find(|path| path.is_file())
    {
        command.env("__EGL_VENDOR_LIBRARY_FILENAMES", vendor);
    }
}

pub(crate) fn prepare_child(command: &mut Command) {
    let parent = std::process::id();
    command.stdin(Stdio::null()).process_group(0);
    // Only async-signal-safe syscalls in the post-fork child. Kill the owned
    // server if its workspace daemon crashes, including the fork/prctl race.
    unsafe {
        command.pre_exec(move || {
            // Ignore executable file capabilities in this private session.
            // This also keeps the parent-death signal across compositor exec.
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }

            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent as i32 {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

pub(crate) fn wait_for(
    timeout: Duration,
    mut ready: impl FnMut() -> Result<bool>,
    name: &str,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready()? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out waiting for {name}")
}
