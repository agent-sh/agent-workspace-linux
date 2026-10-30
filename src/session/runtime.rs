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

fn cache_dir() -> Result<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .context("HOME or XDG_CACHE_HOME is required for prepared runtimes")?;
    if !base.is_absolute() {
        bail!("runtime cache must be an absolute path");
    }
    Ok(base.join("agent-workspace-linux/runtimes").join(format!(
        "gnome-46-{}-{:x}",
        env::consts::ARCH,
        Sha256::digest(DEFINITION.as_bytes())
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
        serde_json::to_vec(
            &serde_json::json!({"session_id":options.session_id,"pid":std::process::id()}),
        )?,
    )?;
    // The control daemon keeps its own ABI. Only its private executable gets
    // this RPATH; compositor and app children use the selected image's loader
    // and libraries, with no LD_LIBRARY_PATH inherited from the controller.
    let executable = crate::workspace::daemon_executable_path()?;
    let copied = overlay.join("agent-workspace-linux");
    fs::copy(&executable, &copied)?;
    let patcher = info.path.join("rootfs/usr/bin/patchelf");
    let interpreter = Command::new("timeout")
        .arg("20s")
        .arg(&patcher)
        .arg("--print-interpreter")
        .arg(&executable)
        .output()?;
    if !interpreter.status.success() {
        bail!("cannot inspect workspace daemon interpreter");
    }
    let interpreter = PathBuf::from(String::from_utf8(interpreter.stdout)?.trim());
    let patched = Command::new("timeout")
        .arg("20s")
        .arg(&patcher)
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
    let mut args: Vec<OsString> = ["bwrap", "--unshare-user", "--overlay-src"]
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
    bind(home.clone(), home.clone(), false);
    bind(
        options.runtime_dir.clone(),
        options.runtime_dir.clone(),
        false,
    );
    bind(overlay.join("tmp"), "/tmp".into(), false);
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
    let owner: serde_json::Value = serde_json::from_slice(&fs::read(path.join("owner.json"))?)?;
    if owner["session_id"] != options.session_id || owner["pid"] != std::process::id() {
        bail!(
            "refusing to remove userspace with a different owner: {}",
            path.display()
        );
    }
    make_directories_removable(&path)?;
    fs::remove_dir_all(&path)?;
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(())
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
