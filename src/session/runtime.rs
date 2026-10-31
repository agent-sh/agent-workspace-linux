//! Prepared compositor userspaces, entered only by the workspace daemon.
use super::{SessionBackend, SessionSpec};
use crate::workspace::{command_path_check, DaemonOptions};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fmt, fs,
    path::PathBuf,
    process::{Command, Stdio},
    str::FromStr,
};

const MARKER: &str = "AGENT_WORKSPACE_SESSION_RUNTIME";
const OWNER_FD: i32 = 4;
const DEFINITION: &str = include_str!("../../runtimes/gnome-46/Dockerfile");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version {
    numbers: [u32; 3],
    length: usize,
}

impl FromStr for Version {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let mut result = Self {
            numbers: [0; 3],
            length: 0,
        };
        for (index, part) in value.split('.').enumerate() {
            if index >= 3
                || part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
            {
                bail!("version must have one to three numeric components, for example 46 or 46.0");
            }
            result.numbers[index] = part.parse().context("version component is too large")?;
            result.length += 1;
        }
        Ok(result)
    }
}
impl TryFrom<String> for Version {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}
impl From<Version> for String {
    fn from(value: Version) -> Self {
        value.to_string()
    }
}
impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for index in 0..self.length {
            if index != 0 {
                f.write_str(".")?;
            }
            write!(f, "{}", self.numbers[index])?;
        }
        Ok(())
    }
}
impl Version {
    pub fn matches(self, actual: &str) -> bool {
        actual.parse::<Self>().is_ok_and(|actual| {
            actual.length >= self.length
                && self.numbers[..self.length] == actual.numbers[..self.length]
        })
    }
}

pub fn validate_selection(session: SessionSpec) -> Result<()> {
    if let Some(version) = session.version {
        if session.backend != SessionBackend::Gnome || version.numbers[0] != 46 {
            bail!("no prepared runtime for {session}; available: gnome@46. Omit version to use the host runtime");
        }
    }
    Ok(())
}

fn key(session: SessionSpec) -> String {
    format!("{session}")
}
pub fn inside(session: SessionSpec) -> bool {
    session.version.is_some() && env::var(MARKER).is_ok_and(|value| value == key(session))
}

fn sha256_hex(content: &[u8]) -> String {
    Sha256::digest(content)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cache_dir() -> Result<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .context("HOME or XDG_CACHE_HOME is required for prepared runtimes")?;
    if !base.is_absolute() {
        bail!("runtime cache must be an absolute path");
    }
    Ok(base.join("agent-workspace-linux/runtimes").join(format!(
        "gnome-46-{}-{}",
        env::consts::ARCH,
        sha256_hex(DEFINITION.as_bytes())
    )))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub backend: String,
    pub version: String,
    pub actual_version: String,
    pub image_id: String,
    pub path: PathBuf,
    pub definition: String,
}

impl RuntimeInfo {
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({"backend":self.backend,"version":self.version,"actual_version":self.actual_version,"image_id":self.image_id,"path":self.path})
    }
}

fn installed() -> Result<RuntimeInfo> {
    let path = cache_dir()?;
    let info: RuntimeInfo = serde_json::from_slice(&fs::read(path.join("runtime.json"))?)?;
    if info.backend != "gnome"
        || info.version != "46"
        || info.path != path
        || info.definition != DEFINITION
        || !path.join("rootfs/usr/bin/gnome-shell").is_file()
        || !path.join("rootfs/usr/bin/patchelf").is_file()
        || !"46".parse::<Version>()?.matches(&info.actual_version)
    {
        bail!("prepared GNOME 46 runtime does not match its definition");
    }
    Ok(info)
}

pub fn catalog() -> serde_json::Value {
    match installed() {
        Ok(info) => {
            serde_json::json!({"host_default":true,"runtimes":[{"backend":"gnome","version":"46","installed":true,"runtime":info.summary()}]})
        }
        Err(error) => {
            serde_json::json!({"host_default":true,"runtimes":[{"backend":"gnome","version":"46","installed":false,"reason":error.to_string(),"prepare":"agent-workspace-linux runtime prepare --backend gnome --version 46"}]})
        }
    }
}

pub fn blockers(session: SessionSpec) -> Vec<String> {
    let mut blockers = Vec::new();
    if let Err(error) = validate_selection(session) {
        return vec![error.to_string()];
    }
    if !command_path_check("bwrap").ok {
        blockers.push("Install bubblewrap with overlay support for prepared runtimes".into());
    } else if !Command::new("timeout")
        .args(["5s", "bwrap", "--help"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("--overlay ")
        })
    {
        blockers.push("Prepared runtimes require bubblewrap with --overlay support".into());
    }
    match installed() {
        Ok(info) if session.version.is_some_and(|version| !version.matches(&info.actual_version)) =>
            blockers.push(format!("prepared GNOME version {} does not match {session}", info.actual_version)),
        Ok(_) => (),
        Err(error) => blockers.push(format!("GNOME 46 runtime is not prepared: {error}; run `agent-workspace-linux runtime prepare --backend gnome --version 46`")),
    }
    blockers
}

fn bounded(program: &str, args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new("timeout")
        .args(["--signal=TERM", "10m", program])
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

/// Prepare explicitly. Starting a workspace never downloads a runtime.
pub fn prepare(session: SessionSpec) -> Result<RuntimeInfo> {
    validate_selection(session)?;
    if session.version.is_none() {
        bail!("runtime prepare requires --version");
    }
    if let Ok(info) = installed() {
        if !session.version.unwrap().matches(&info.actual_version) {
            bail!(
                "prepared GNOME version {} does not match {session}",
                info.actual_version
            );
        }
        return Ok(info);
    }
    let destination = cache_dir()?;
    if destination.exists() {
        bail!(
            "runtime cache {} exists but is invalid; preserve and inspect it before replacing it",
            destination.display()
        );
    }
    let parent = destination
        .parent()
        .context("runtime cache parent missing")?;
    fs::create_dir_all(parent)?;
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let stage = parent.join(format!(".gnome-46-{}-{suffix}", std::process::id()));
    fs::create_dir(&stage)?;
    let mut container = None;
    let platform = match env::consts::ARCH {
        "x86_64" => "linux/amd64",
        "aarch64" => "linux/arm64",
        architecture => bail!("GNOME 46 runtime is not available for {architecture}"),
    };
    let image_tag = format!(
        "agent-workspace-linux-runtime:gnome-46-{}",
        env::consts::ARCH
    );
    let result = (|| {
        fs::write(stage.join("Dockerfile"), DEFINITION)?;
        let status = Command::new("timeout")
            .args([
                "--signal=TERM",
                "10m",
                "docker",
                "build",
                "--tag",
                &image_tag,
                "--file",
            ])
            .arg(stage.join("Dockerfile"))
            .args(["--platform", platform])
            .arg(&stage)
            .stdout(Stdio::from(std::io::stderr()))
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            bail!("GNOME 46 runtime image build exited with {status}");
        }
        let image = String::from_utf8(
            bounded(
                "docker",
                &["image", "inspect", &image_tag, "--format", "{{.Id}}"],
            )?
            .stdout,
        )?
        .trim()
        .to_string();
        let version = String::from_utf8(
            bounded(
                "docker",
                &[
                    "run",
                    "--rm",
                    "--network",
                    "none",
                    "--cap-drop",
                    "ALL",
                    "--user",
                    "65534:65534",
                    &image,
                    "gnome-shell",
                    "--version",
                ],
            )?
            .stdout,
        )?;
        let actual_version = version
            .split_whitespace()
            .last()
            .context("GNOME version missing")?
            .to_string();
        if !session.version.unwrap().matches(&actual_version) {
            bail!("runtime provides GNOME {actual_version}, requested {session}");
        }
        let id = String::from_utf8(bounded("docker", &["create", &image])?.stdout)?
            .trim()
            .to_string();
        container = Some(id.clone());
        let archive = stage.join("rootfs.tar");
        bounded(
            "docker",
            &[
                "export",
                "--output",
                archive
                    .to_str()
                    .context("runtime cache path is not UTF-8")?,
                &id,
            ],
        )?;
        fs::create_dir(stage.join("rootfs"))?;
        let status = Command::new("timeout")
            .args([
                "--signal=TERM",
                "10m",
                "tar",
                "--no-same-owner",
                "--no-same-permissions",
                "-xf",
            ])
            .arg(&archive)
            .arg("-C")
            .arg(stage.join("rootfs"))
            .status()?;
        if !status.success() {
            bail!("runtime extraction exited with {status}");
        }
        fs::remove_file(archive)?;
        let info = RuntimeInfo {
            backend: "gnome".into(),
            version: "46".into(),
            actual_version,
            image_id: image,
            path: destination.clone(),
            definition: DEFINITION.into(),
        };
        fs::write(
            stage.join("runtime.json"),
            serde_json::to_vec_pretty(&info)?,
        )?;
        fs::rename(&stage, &destination)?;
        Ok(info)
    })();
    if let Some(id) = container {
        let _ = bounded("docker", &["rm", &id]);
    }
    if stage.exists() {
        let _ = fs::remove_dir_all(stage);
    }
    result
}

/// Only the daemon enters this userspace. Keep the host PID namespace so
/// process ownership, stale-process checks and stop retain their identities.
pub fn daemon_prefix(options: &DaemonOptions) -> Result<Vec<std::ffi::OsString>> {
    use std::ffi::OsString;
    let info = installed()?;
    let home = PathBuf::from(env::var_os("HOME").context("HOME missing")?);
    if !home.is_absolute() || home == std::path::Path::new("/") {
        bail!("HOME must be an absolute user directory");
    }
    cleanup_stale_userspaces(&options.id, false)?;
    let overlay = userspace_dir(options)?;
    if overlay.exists() {
        bail!("session userspace {} already exists", overlay.display());
    }
    for name in ["upper", "work", "tmp"] {
        fs::create_dir_all(overlay.join(name))?;
    }
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&overlay, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(overlay.join("tmp"), fs::Permissions::from_mode(0o1777))?;
        let sockets = overlay.join("tmp/.X11-unix");
        fs::create_dir(&sockets)?;
        fs::set_permissions(sockets, fs::Permissions::from_mode(0o1777))?;
    }
    fs::write(
        overlay.join("owner.json"),
        serde_json::to_vec(&UserspaceOwner {
            session_id: options.session_id.clone(),
            pid: std::process::id(),
            start_ticks: crate::workspace::process_start_ticks(std::process::id())
                .context("supervisor process identity missing")?,
            root_device: None,
            mount_signature: None,
        })?,
    )?;
    // The control daemon keeps its own ABI. Only its private executable gets
    // this RPATH; compositor and app children use the selected image's loader
    // and libraries, with no LD_LIBRARY_PATH inherited from the controller.
    let executable = crate::workspace::daemon_executable_path()?;
    let copied = overlay.join("agent-workspace-linux");
    fs::copy(&executable, &copied)?;
    let interpreter = patcher_command(&info)?
        .arg("--print-interpreter")
        .arg(&executable)
        .output()?;
    if !interpreter.status.success() {
        bail!("cannot inspect workspace daemon interpreter");
    }
    let interpreter = PathBuf::from(String::from_utf8(interpreter.stdout)?.trim());
    let patched = patcher_command(&info)?
        .args([
            "--set-interpreter",
            "/opt/awl-controller-libs/ld.so",
            "--force-rpath",
            "--set-rpath",
            "/opt/awl-controller-libs",
        ])
        .arg(&copied)
        .status()?;
    if !patched.success() {
        bail!("could not prepare controller ABI inside selected runtime");
    }
    let libraries = Command::new("timeout")
        .args(["20s", "ldd"])
        .arg(&executable)
        .output()?;
    if !libraries.status.success() {
        bail!("cannot inspect workspace daemon libraries");
    }
    let library_paths = String::from_utf8(libraries.stdout)?
        .lines()
        .filter_map(|line| {
            line.split_once("=>")
                .and_then(|(_, path)| path.split_whitespace().next())
                .filter(|path| path.starts_with('/'))
                .map(PathBuf::from)
        })
        .collect::<Vec<_>>();
    let mut args: Vec<OsString> = [
        "bwrap",
        "--unshare-user",
        "--die-with-parent",
        "--overlay-src",
    ]
    .map(OsString::from)
    .to_vec();
    args.push(info.path.join("rootfs").into());
    args.push("--overlay".into());
    args.push(overlay.join("upper").into());
    args.push(overlay.join("work").into());
    args.push("/".into());
    args.extend(
        [
            "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/run", "--tmpfs", "/home", "--tmpfs",
            "/opt",
        ]
        .map(OsString::from),
    );
    let mut bind = |source: PathBuf, target: PathBuf, readonly: bool| {
        args.push(if readonly { "--ro-bind" } else { "--bind" }.into());
        args.push(source.into());
        args.push(target.into());
    };
    bind(overlay.join("tmp"), "/tmp".into(), false);
    bind(home.clone(), home.clone(), false);
    bind(
        options.runtime_dir.clone(),
        options.runtime_dir.clone(),
        false,
    );
    bind(copied, "/opt/agent-workspace-linux".into(), true);
    bind(interpreter, "/opt/awl-controller-libs/ld.so".into(), true);
    for path in library_paths {
        let name = path
            .file_name()
            .context("controller library name missing")?;
        bind(
            path.clone(),
            PathBuf::from("/opt/awl-controller-libs").join(name),
            true,
        );
    }
    for path in [
        "/etc/passwd",
        "/etc/group",
        "/etc/resolv.conf",
        "/etc/hosts",
    ] {
        bind(path.into(), path.into(), true);
    }
    if let Some(path) = &options.permissions_source {
        if !path.starts_with(&home) {
            bind(path.clone(), path.clone(), true);
        }
    }
    if let Some(path) = &options.profile_cwd {
        let source = fs::canonicalize(path)
            .with_context(|| format!("resolve profile directory {}", path.display()))?;
        // System paths belong to the selected image. In particular, never
        // expose the host's display sockets through /tmp or /run.
        let system_path = [
            "/", "/bin", "/sbin", "/usr", "/lib", "/lib64", "/etc", "/dev", "/proc", "/sys",
            "/run", "/tmp", "/opt",
        ]
        .iter()
        .any(|root| {
            if *root == "/" {
                path == std::path::Path::new("/") || source == std::path::Path::new("/")
            } else {
                path.starts_with(root) || source.starts_with(root)
            }
        });
        if !path.starts_with(&home) && !system_path {
            bind(source, path.clone(), false);
        }
    }
    args.extend(["--clearenv", "--setenv", "HOME"].map(OsString::from));
    args.push(home.into());
    args.extend(
        [
            "--setenv",
            "PATH",
            "/usr/local/bin:/usr/bin:/bin",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            MARKER,
        ]
        .map(OsString::from),
    );
    args.push(key(options.session).into());
    args.extend(["--setenv", crate::control::SHARED_CONTROL_ENV, "1"].map(OsString::from));
    for name in [
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
    ] {
        if let Some(value) = env::var_os(name) {
            args.push("--setenv".into());
            args.push(name.into());
            args.push(value);
        }
    }
    for (name, value) in [
        ("AGENT_WORKSPACE_ID", std::ffi::OsString::from(&options.id)),
        (
            "AGENT_WORKSPACE_RUNTIME_DIR",
            options.runtime_dir.clone().into_os_string(),
        ),
        (
            "AGENT_WORKSPACE_SOCKET",
            options.socket_path.clone().into_os_string(),
        ),
    ] {
        args.push("--setenv".into());
        args.push(name.into());
        args.push(value);
    }
    args.push("/opt/agent-workspace-linux".into());
    Ok(args)
}

fn patcher_command(info: &RuntimeInfo) -> Result<Command> {
    let (multiarch, loader) = match env::consts::ARCH {
        "x86_64" => ("x86_64-linux-gnu", "ld-linux-x86-64.so.2"),
        "aarch64" => ("aarch64-linux-gnu", "ld-linux-aarch64.so.1"),
        architecture => bail!("no runtime loader for {architecture}"),
    };
    let libraries = info.path.join("rootfs/usr/lib").join(multiarch);
    let mut command = Command::new("timeout");
    command
        .arg("20s")
        .arg(libraries.join(loader))
        .args(["--inhibit-cache", "--library-path"])
        .arg(libraries)
        .arg(info.path.join("rootfs/usr/bin/patchelf"));
    Ok(command)
}

pub fn share_runtime_descriptors(command: &mut Command, options: &DaemonOptions) -> Result<()> {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    let path = crate::control::control_state_path();
    let parent = path.parent().context("control directory missing")?;
    fs::create_dir_all(parent)?;
    let directory = high_descriptor(fs::File::open(parent)?)?;
    let owner = high_descriptor(
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(userspace_dir(options)?.join("owner.json"))?,
    )?;
    // SAFETY: only async-signal-safe descriptor operations run after fork.
    // Command owns the directory until spawning; the parent keeps CLOEXEC.
    unsafe {
        command.pre_exec(move || {
            for (source, target) in [
                (directory.as_raw_fd(), crate::control::SHARED_CONTROL_FD),
                (owner.as_raw_fd(), OWNER_FD),
            ] {
                if libc::dup2(source, target) < 0 || libc::fcntl(target, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

fn high_descriptor(file: fs::File) -> Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // Reserve descriptors 3 and 4 without source/target collisions after fork.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, OWNER_FD + 1) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fcntl returned a new descriptor owned by this File.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[derive(Serialize, Deserialize)]
struct UserspaceOwner {
    pid: u32,
    start_ticks: u64,
    session_id: String,
    #[serde(default)]
    root_device: Option<String>,
    #[serde(default)]
    mount_signature: Option<String>,
}

pub fn record_inner_userspace(options: &DaemonOptions) -> Result<()> {
    use std::{
        io::{Read, Seek, Write},
        os::{fd::FromRawFd, unix::fs::MetadataExt},
    };
    // SAFETY: the supervisor passed this owner marker at descriptor 4.
    let mut file = unsafe { fs::File::from_raw_fd(OWNER_FD) };
    if !file.metadata()?.is_file() {
        bail!("runtime owner descriptor is not a file");
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let mut owner: UserspaceOwner = serde_json::from_str(&text)?;
    if owner.session_id != options.session_id {
        bail!("runtime owner belongs to another session");
    }
    // bubblewrap without a private PID namespace can leave its payload alive
    // when its monitor dies. Chain parent death into the controller itself.
    let parent = unsafe { libc::getppid() };
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { libc::getppid() } != parent
        || crate::workspace::process_start_ticks(owner.pid) != Some(owner.start_ticks)
    {
        bail!("runtime supervisor exited before controller startup");
    }
    let device = fs::metadata("/")?.dev();
    owner.root_device = Some(format!("{}:{}", libc::major(device), libc::minor(device)));
    owner.mount_signature = fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .find(|line| line.split_whitespace().nth(4) == Some("/"))
        .and_then(|line| line.split_once(" - "))
        .map(|(_, signature)| signature.to_owned());
    if !owner
        .mount_signature
        .as_ref()
        .is_some_and(|signature| signature.starts_with("overlay "))
    {
        bail!("prepared runtime root is not the expected overlay filesystem");
    }
    file.rewind()?;
    file.set_len(0)?;
    file.write_all(&serde_json::to_vec(&owner)?)?;
    // Drop closes the capability before any app can inherit it.
    Ok(())
}

fn userspace_dir(options: &DaemonOptions) -> Result<PathBuf> {
    let cache = cache_dir()?;
    let base = cache
        .parent()
        .and_then(|path| path.parent())
        .context("runtime cache parent missing")?;
    Ok(base
        .join("runtime-sessions")
        .join(&options.id)
        .join(&options.session_id))
}

/// Called by the host supervisor after bubblewrap exits and releases mounts.
pub fn cleanup_userspace(options: &DaemonOptions) -> Result<()> {
    let path = userspace_dir(options)?;
    if !path.exists() {
        return Ok(());
    }
    let owner: UserspaceOwner = serde_json::from_slice(&fs::read(path.join("owner.json"))?)?;
    if owner.session_id != options.session_id || owner.pid != std::process::id() {
        bail!(
            "refusing to remove userspace with a different owner: {}",
            path.display()
        );
    }
    if userspace_in_use(&path, &owner)? {
        bail!("userspace is still in use; run workspace cleanup after its processes stop");
    }
    remove_userspace(&path)
}

fn remove_userspace(path: &std::path::Path) -> Result<()> {
    make_directories_removable(path)?;
    fs::remove_dir_all(path)?;
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(())
}

/// Reap this workspace's abandoned layers only after their supervisor and
/// all processes mounting the writable filesystem have stopped.
pub fn cleanup_stale_userspaces(id: &str, dry_run: bool) -> Result<Vec<String>> {
    let id = crate::workspace::sanitize_workspace_id(id)?;
    let cache = cache_dir()?;
    let directory = cache
        .parent()
        .and_then(|path| path.parent())
        .context("cache parent missing")?
        .join("runtime-sessions")
        .join(id);
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        if !fs::symlink_metadata(&path)?.is_dir() {
            bail!("unexpected userspace entry {}", path.display());
        }
        let owner: UserspaceOwner = serde_json::from_slice(&fs::read(path.join("owner.json"))?)?;
        if path.file_name().and_then(|name| name.to_str()) != Some(owner.session_id.as_str()) {
            bail!("userspace identity mismatch");
        }
        if crate::workspace::process_start_ticks(owner.pid) == Some(owner.start_ticks) {
            let stat = fs::read_to_string(format!("/proc/{}/stat", owner.pid)).unwrap_or_default();
            if !stat
                .rsplit_once(") ")
                .is_some_and(|(_, state)| state.starts_with('Z') || state.starts_with('X'))
            {
                bail!("userspace supervisor {} is still active", owner.pid);
            }
        }
        if !dry_run && userspace_in_use(&path, &owner)? {
            bail!(
                "userspace {} is still mounted by an active process",
                path.display()
            );
        }
        candidates.push(path);
    }
    let mut actions = Vec::new();
    for path in candidates {
        actions.push(format!(
            "{} prepared userspace {}",
            if dry_run {
                "would verify and remove"
            } else {
                "removed"
            },
            path.display()
        ));
        if !dry_run {
            remove_userspace(&path)?;
        }
    }
    Ok(actions)
}

fn userspace_in_use(path: &std::path::Path, owner: &UserspaceOwner) -> Result<bool> {
    use std::{collections::BTreeSet, os::unix::ffi::OsStrExt};
    let upper = path.join("upper");
    let uid = unsafe { libc::geteuid() };
    let mut namespaces = BTreeSet::new();
    for entry in fs::read_dir("/proc")? {
        let process = entry?.path();
        if process
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        let Ok(status) = fs::read_to_string(process.join("status")) else {
            continue;
        };
        let effective_uid = status
            .lines()
            .find(|line| line.starts_with("Uid:"))
            .and_then(|line| line.split_whitespace().nth(2))
            .and_then(|value| value.parse::<u32>().ok());
        if effective_uid != Some(uid) {
            continue;
        }
        let tasks = match fs::read_dir(process.join("task")) {
            Ok(tasks) => tasks,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("cannot enumerate userspace task holders"),
        };
        // A dead thread-group leader can still have live workers. Each task
        // owns its own namespace reference, including after unshare/setns.
        for task in tasks {
            let task = task?.path();
            if process_is_dead(&task) {
                continue;
            }
            if let Some(device) = &owner.root_device {
                let namespace = fs::read_link(task.join("ns/mnt")).ok();
                if namespace.as_ref().is_some_and(|ns| namespaces.contains(ns)) {
                    continue;
                }
                match fs::read_to_string(task.join("mountinfo")) {
                    Ok(mounts) => {
                        if let Some(namespace) = namespace {
                            namespaces.insert(namespace);
                        }
                        if mounts.lines().any(|line| {
                            line.split_whitespace().nth(2) == Some(device.as_str())
                                && line.split_once(" - ").is_some_and(|(_, signature)| {
                                    owner.mount_signature.as_ref().map_or_else(
                                        || signature.starts_with("overlay "),
                                        |expected| signature == expected,
                                    )
                                })
                        }) {
                            return Ok(true);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    // A process may become a zombie between status and mountinfo.
                    // It no longer owns a namespace. Do not mark its namespace
                    // checked until a live representative was actually read.
                    Err(_) if process_is_dead(&task) => (),
                    Err(error) => {
                        return Err(error).context("cannot verify userspace mount lifetime")
                    }
                }
            } else if let Ok(command) = fs::read(task.join("cmdline")) {
                if command
                    .split(|byte| *byte == 0)
                    .any(|argument| argument == upper.as_os_str().as_bytes())
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn process_is_dead(process: &std::path::Path) -> bool {
    match fs::read_to_string(process.join("stat")) {
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, state)| state.starts_with('Z') || state.starts_with('X')),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

fn make_directories_removable(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(path)? {
        make_directories_removable(&entry?.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_cache_hash_uses_stable_lowercase_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn version_roundtrip_and_component_matching() {
        let value: Version = "46.0".parse().unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), "\"46.0\"");
        assert!(value.matches("46.0.2"));
        assert!(!value.matches("46.1"));
        for invalid in ["", "host", "../46", "46..0", "046", "46.0.0.1"] {
            assert!(invalid.parse::<Version>().is_err());
        }
    }
    #[test]
    fn selected_version_is_part_of_workspace_identity() {
        let host: SessionSpec = "gnome".parse().unwrap();
        let pinned = SessionSpec {
            version: Some("46".parse().unwrap()),
            ..host
        };
        assert!(!host.matches(pinned));
        assert_eq!(pinned.to_string(), "gnome@46");
        assert!(pinned.validate().is_ok());
        assert!(SessionSpec {
            backend: SessionBackend::Kwin,
            ..pinned
        }
        .validate()
        .is_err());
    }
}
