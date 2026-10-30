use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const CONTROL_FILE: &str = "mcp-control.json";
pub(crate) const SHARED_CONTROL_ENV: &str = "AGENT_WORKSPACE_SHARED_CONTROL";
pub(crate) const SHARED_CONTROL_FD: i32 = 3;
const STAGING_ATTEMPTS: u32 = 64;
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

/// Staging file owned by one publication attempt. Dropping it before
/// [`StagingFile::publish`] succeeds removes the file, so a failed attempt never
/// leaves a partial state behind.
// Drop handles normal failures. SIGKILL or power loss may leave residue;
// deliberately do not delete files that a concurrent process could still own.
struct StagingFile {
    path: PathBuf,
    published: bool,
}

impl StagingFile {
    /// Allocates a staging file next to `path` that no other writer can hold.
    /// `create_new` makes the allocation exclusive, so a name collision with a
    /// concurrent writer retries instead of clobbering their file.
    fn allocate(path: &Path, parent: &Path) -> Result<(Self, fs::File)> {
        let stem = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| CONTROL_FILE.to_string());
        let pid = std::process::id();
        for _ in 0..STAGING_ATTEMPTS {
            let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!("{stem}.{pid}.{sequence}.tmp"));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => {
                    return Ok((
                        Self {
                            path: candidate,
                            published: false,
                        },
                        file,
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to write {}", candidate.display()))
                }
            }
        }
        bail!(
            "failed to allocate a staging file for {} after {STAGING_ATTEMPTS} attempts",
            path.display()
        )
    }

    fn publish(mut self, path: &Path) -> Result<()> {
        fs::rename(&self.path, path).with_context(|| {
            format!(
                "failed to move {} to {}",
                self.path.display(),
                path.display()
            )
        })?;
        self.published = true;
        Ok(())
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn save_control_state_to_path(path: &Path, state: &McpControlState) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let content =
        serde_json::to_string_pretty(state).context("failed to serialize MCP control state")?;
    let (staging, mut file) = StagingFile::allocate(path, parent)?;
    file.write_all(format!("{content}\n").as_bytes())
        .with_context(|| format!("failed to write {}", staging.path.display()))?;
    drop(file);
    staging.publish(path)
}

pub(crate) fn control_state_path() -> PathBuf {
    if env::var_os(SHARED_CONTROL_ENV).is_some() {
        return PathBuf::from(format!("/proc/self/fd/{SHARED_CONTROL_FD}")).join(CONTROL_FILE);
    }
    runtime_dir_from_env(env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)).join(CONTROL_FILE)
}

/// Retain the host control directory in the daemon, not in executed apps.
pub(crate) fn claim_shared_control_directory() -> Result<()> {
    let directory = PathBuf::from(format!("/proc/self/fd/{SHARED_CONTROL_FD}"));
    if env::var(SHARED_CONTROL_ENV).as_deref() != Ok("1") || !fs::metadata(&directory)?.is_dir() {
        bail!("prepared runtime is missing its shared control directory");
    }
    // SAFETY: the launcher passed an owned directory at this descriptor. Set
    // a descriptor flag only; the directory stays open in this daemon.
    let result = unsafe { libc::fcntl(SHARED_CONTROL_FD, libc::F_SETFD, libc::FD_CLOEXEC) };
    if result < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
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

    #[test]
    fn directory_descriptor_observes_atomic_control_replacements() {
        use std::os::fd::AsRawFd;
        let directory = temp_control_dir("descriptor");
        let descriptor = fs::File::open(&directory).unwrap();
        let through_fd =
            PathBuf::from(format!("/proc/self/fd/{}", descriptor.as_raw_fd())).join(CONTROL_FILE);
        let direct = directory.join(CONTROL_FILE);
        let paused = McpControlState {
            mode: McpControlMode::Paused,
            ..Default::default()
        };
        save_control_state_to_path(&direct, &paused).unwrap();
        assert_eq!(
            load_existing_control_state_from_path(&through_fd)
                .unwrap()
                .mode,
            McpControlMode::Paused
        );
        save_control_state_to_path(&direct, &McpControlState::default()).unwrap();
        assert_eq!(
            load_existing_control_state_from_path(&through_fd)
                .unwrap()
                .mode,
            McpControlMode::Active
        );
        fs::remove_dir_all(directory).unwrap();
    }

    fn temp_control_path(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "agent-workspace-control-test-{}-{name}.json",
            std::process::id()
        ))
    }

    /// Disposable directory owned by a single test, so staging leftovers can be
    /// observed without interference from the flat-file tests above.
    fn temp_control_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "agent-workspace-control-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Everything in the test directory that is not the published control file.
    fn staging_leftovers(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .expect("read test dir")
            .map(|entry| entry.expect("read dir entry").path())
            .filter(|entry| entry.file_name().and_then(|name| name.to_str()) != Some(CONTROL_FILE))
            .collect()
    }

    #[test]
    fn concurrent_setters_all_publish_a_complete_state() {
        const WRITERS: usize = 8;
        const ROUNDS: u64 = 40;

        let dir = temp_control_dir("concurrent-setters");
        let path = dir.join(CONTROL_FILE);

        let start = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));
        let writers: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let path = path.clone();
                let start = start.clone();
                std::thread::spawn(move || -> Result<()> {
                    start.wait();
                    for round in 1..=ROUNDS {
                        let state = McpControlState {
                            mode: if writer % 2 == 0 {
                                McpControlMode::ReadOnly
                            } else {
                                McpControlMode::Paused
                            },
                            updated_at_unix: round,
                            updated_by: Some(format!("writer-{writer}")),
                            reason: Some(format!("round {round}")),
                        };
                        save_control_state_to_path(&path, &state)?;
                    }
                    Ok(())
                })
            })
            .collect();

        let results: Vec<_> = writers
            .into_iter()
            .map(|writer| writer.join().expect("writer thread finishes"))
            .collect();
        assert!(
            results.iter().all(Result::is_ok),
            "setter failures: {results:?}"
        );

        let loaded = load_existing_control_state_from_path(&path).expect("final state is complete");
        let attribution = loaded
            .updated_by
            .clone()
            .expect("final state is attributed");
        let expected: Vec<String> = (0..WRITERS).map(|w| format!("writer-{w}")).collect();
        assert!(
            expected.contains(&attribution),
            "final state attributed to an unexpected writer: {attribution}"
        );
        assert!(
            (1..=ROUNDS).contains(&loaded.updated_at_unix),
            "final state carries a torn timestamp: {}",
            loaded.updated_at_unix
        );
        assert_eq!(
            loaded.reason.as_deref(),
            Some(format!("round {}", loaded.updated_at_unix).as_str())
        );
        assert!(
            staging_leftovers(&dir).is_empty(),
            "staging files survived publication: {:?}",
            staging_leftovers(&dir)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn publication_does_not_clobber_a_legacy_staging_file() {
        let dir = temp_control_dir("foreign-staging");
        let path = dir.join(CONTROL_FILE);
        // Preserve a staging pathname still owned by an older concurrent binary.
        // This tests compatibility with the old name, not create_new retries.
        let sentinel = dir.join(format!("{CONTROL_FILE}.tmp"));
        fs::write(&sentinel, "sentinel\n").expect("write sentinel staging file");

        let state = McpControlState {
            mode: McpControlMode::Paused,
            updated_at_unix: 7,
            updated_by: Some("publisher".to_string()),
            reason: None,
        };
        save_control_state_to_path(&path, &state).expect("publish state");

        assert_eq!(
            fs::read_to_string(&sentinel).ok().as_deref(),
            Some("sentinel\n"),
            "another writer's staging file was clobbered"
        );
        let loaded = load_existing_control_state_from_path(&path).expect("published state loads");
        assert_eq!(loaded.mode, McpControlMode::Paused);
        assert_eq!(loaded.updated_by.as_deref(), Some("publisher"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_publication_cleans_up_its_own_staging_file() {
        let dir = temp_control_dir("publication-failure");
        let path = dir.join(CONTROL_FILE);
        // A non-empty directory at the target makes the final rename fail.
        fs::create_dir_all(path.join("occupied")).expect("occupy control path");

        let error = save_control_state_to_path(&path, &McpControlState::default())
            .expect_err("publication over a non-empty directory should fail");
        assert!(
            error.to_string().contains("failed to move"),
            "unexpected failure: {error}"
        );

        let leftovers = staging_leftovers(&dir);
        assert!(
            leftovers.is_empty(),
            "failed publication left staging files behind: {leftovers:?}"
        );

        let _ = fs::remove_dir_all(&dir);
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
