//! Stop only terminal servers identified by a managed launch and its session.
use super::{sanitize_terminal_id, WorkspaceApp, APP_TERMINATE_GRACE_MS};
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{FileTypeExt, MetadataExt},
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn managed_socket(runtime: &Path, app: &WorkspaceApp) -> Option<(PathBuf, PathBuf)> {
    managed_command_socket(runtime, &app.command)
}

/// Each managed terminal owns `runtime/client/terminal-ID/`, the only client
/// directory its sandbox may see. Returns that directory for a launch command
/// built by `terminal_launch_plan`; legacy runtime-root sockets have none.
pub(super) fn managed_terminal_client_dir(runtime: &Path, command: &[String]) -> Option<PathBuf> {
    let (_, socket) = managed_command_socket(runtime, command)?;
    socket
        .parent()
        .filter(|dir| dir.parent() == Some(runtime.join("client").as_path()))
        .map(Path::to_path_buf)
}

fn managed_command_socket(runtime: &Path, a: &[String]) -> Option<(PathBuf, PathBuf)> {
    if a.len() < 17
        || a[1] != "-title"
        || a[3] != "-fa"
        || a[5] != "-fs"
        || a[7] != "-geometry"
        || a[9] != "-e"
        || a[11] != "-S"
        || a[13] != "new-session"
        || a[14] != "-A"
        || a[15] != "-s"
    {
        return None;
    }
    let id = sanitize_terminal_id(&a[16]).ok()?;
    let socket = PathBuf::from(&a[12]);
    if id != a[16]
        || (socket != runtime.join(format!("terminal-{id}.tmux.sock"))
            && socket != super::client_terminal_socket_path(runtime, &id))
    {
        return None;
    }
    Some((PathBuf::from(&a[10]), socket))
}

fn owner_environment_matches(bytes: &[u8], workspace: &str, session: &str) -> bool {
    if workspace.is_empty() || session.is_empty() {
        return false;
    }
    [
        ("AGENT_WORKSPACE_ID=", workspace),
        ("AGENT_WORKSPACE_SESSION_ID=", session),
    ]
    .into_iter()
    .all(|(key, value)| {
        let mut values = bytes
            .split(|b| *b == 0)
            .filter_map(|entry| entry.strip_prefix(key.as_bytes()));
        values.next() == Some(value.as_bytes()) && values.next().is_none()
    })
}

fn pidfd_matches_pid(fd: &OwnedFd, pid: i32) -> Result<bool> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))?;
    let mut values = info.lines().filter_map(|line| line.strip_prefix("Pid:"));
    let found: i32 = values
        .next()
        .context("pidfd has no procfs PID")?
        .trim()
        .parse()?;
    if values.next().is_some() {
        bail!("ambiguous pidfd process identity");
    }
    Ok(found == pid)
}

fn socket_argv_matches(command: &[u8], program: &Path, socket: &Path) -> bool {
    let mut prefix = program.as_os_str().as_bytes().to_vec();
    prefix.extend_from_slice(b"\0-S\0");
    prefix.extend_from_slice(socket.as_os_str().as_bytes());
    prefix.push(0);
    command.starts_with(&prefix)
}

fn exited(fd: &OwnedFd, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(i32::MAX as u128) as i32;
        let result = unsafe { libc::poll(&mut poll, 1, millis) };
        if result >= 0 {
            if poll.revents & libc::POLLNVAL != 0 {
                bail!("invalid terminal process descriptor");
            }
            return Ok(poll.revents & (libc::POLLIN | libc::POLLHUP) != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
    }
}

fn signal(fd: &OwnedFd, signal: i32) -> Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("signal managed terminal server")
}

fn stop_server(
    socket: &Path,
    program: &Path,
    workspace: &str,
    session: &str,
    dry_run: bool,
) -> Result<Option<i32>> {
    let metadata = match fs::symlink_metadata(socket) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("managed terminal socket is not an owned Unix socket");
    }
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let connection = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = socket.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        bail!("invalid terminal socket path");
    }
    for (out, byte) in address.sun_path.iter_mut().zip(bytes) {
        *out = *byte as libc::c_char;
    }
    let result = unsafe {
        libc::connect(
            connection.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOENT | libc::ECONNREFUSED)
        ) {
            return Ok(None);
        }
        return Err(error).context("connect to managed terminal server without blocking");
    }
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            connection.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    if peer.pid <= 1 || peer.uid != unsafe { libc::geteuid() } {
        bail!("managed terminal peer owner mismatch");
    }
    // New kernels can pin the socket peer directly. On older kernels, pin the
    // candidate PID and revalidate the current process against the saved launch.
    let mut raw_pidfd: i32 = -1;
    size = std::mem::size_of_val(&raw_pidfd) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            connection.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut raw_pidfd as *mut i32).cast(),
            &mut size,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        if error.raw_os_error() != Some(libc::ENOPROTOOPT) {
            return Err(error).context("pin managed terminal socket peer");
        }
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, peer.pid, 0) };
        if descriptor < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("pin managed terminal candidate process");
        }
        raw_pidfd = descriptor as i32;
    }
    if raw_pidfd < 0 {
        bail!("missing terminal peer process descriptor");
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw_pidfd) };
    if exited(&pidfd, Duration::ZERO)? {
        return Ok(None);
    }
    if !pidfd_matches_pid(&pidfd, peer.pid)? {
        if exited(&pidfd, Duration::ZERO)? {
            return Ok(None);
        }
        bail!("terminal pidfd and procfs PID namespace differ");
    }
    let identity = (|| -> io::Result<_> {
        if fs::metadata(format!("/proc/{}", peer.pid))?.uid() != peer.uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "terminal process owner changed",
            ));
        }
        Ok((
            fs::read_link(format!("/proc/{}/exe", peer.pid))?,
            fs::read(format!("/proc/{}/environ", peer.pid))?,
            fs::read(format!("/proc/{}/cmdline", peer.pid))?,
        ))
    })();
    let (executable, environment, command) = match identity {
        Ok(identity) => identity,
        Err(error) => {
            if exited(&pidfd, Duration::ZERO)? {
                return Ok(None);
            }
            return Err(error).context("read managed terminal peer identity");
        }
    };
    let executable_bytes = executable.as_os_str().as_bytes();
    let executable = PathBuf::from(OsString::from_vec(
        executable_bytes
            .strip_suffix(b" (deleted)")
            .unwrap_or(executable_bytes)
            .to_vec(),
    ));
    let expected = fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    if exited(&pidfd, Duration::ZERO)? {
        return Ok(None);
    }
    if !pidfd_matches_pid(&pidfd, peer.pid)? {
        if exited(&pidfd, Duration::ZERO)? {
            return Ok(None);
        }
        bail!("terminal process identity changed during verification");
    }
    if executable != expected
        || !owner_environment_matches(&environment, workspace, session)
        || !socket_argv_matches(&command, program, socket)
    {
        if exited(&pidfd, Duration::ZERO)? {
            return Ok(None);
        }
        bail!(
            "could not verify managed terminal server ownership for pid {}",
            peer.pid
        );
    }
    if !dry_run {
        let grace = Duration::from_millis(APP_TERMINATE_GRACE_MS);
        signal(&pidfd, libc::SIGTERM)?;
        if !exited(&pidfd, grace)? {
            signal(&pidfd, libc::SIGKILL)?;
            if !exited(&pidfd, grace)? {
                bail!("managed terminal server did not exit");
            }
        }
    }
    Ok(Some(peer.pid))
}

pub(super) fn cleanup(
    runtime: &Path,
    workspace: &str,
    session: &str,
    apps: &[WorkspaceApp],
    dry_run: bool,
) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut actions = Vec::new();
    for app in apps {
        let Some((program, socket)) = managed_socket(runtime, app) else {
            continue;
        };
        if !seen.insert(socket.clone()) {
            continue;
        }
        if let Some(pid) = stop_server(&socket, &program, workspace, session, dry_run)
            .with_context(|| format!("terminal cleanup for {}", socket.display()))?
        {
            actions.push(format!(
                "{} managed terminal server pid {pid}",
                if dry_run {
                    "would terminate"
                } else {
                    "terminated"
                }
            ));
        }
    }
    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn socket_binding_is_exact_and_nul_delimited() {
        let program = Path::new("/usr/bin/tmux");
        let socket = Path::new("/runtime/terminal-a.tmux.sock");
        assert!(socket_argv_matches(
            b"/usr/bin/tmux\0-S\0/runtime/terminal-a.tmux.sock\0new-session\0",
            program,
            socket
        ));
        assert!(!socket_argv_matches(
            b"/usr/bin/tmux\0-S\0/runtime/terminal-a.tmux.sock-extra\0",
            program,
            socket
        ));
        assert!(!socket_argv_matches(
            b"/usr/bin/tmux\0-S\0/other/socket\0",
            program,
            socket
        ));
        assert!(!socket_argv_matches(
            b"/usr/bin/tmux\0-S\0/runtime/terminal-a.tmux.sock",
            program,
            socket
        ));
    }

    #[test]
    fn ownership_requires_exact_unique_workspace_and_session_tags() {
        let good = b"IGNORED=value\0AGENT_WORKSPACE_ID=qa\0AGENT_WORKSPACE_SESSION_ID=unique\0";
        assert!(owner_environment_matches(good, "qa", "unique"));
        assert!(!owner_environment_matches(good, "other", "unique"));
        assert!(!owner_environment_matches(good, "qa", "other"));
        assert!(!owner_environment_matches(good, "qa", ""));
        let duplicate = [good.as_slice(), b"AGENT_WORKSPACE_SESSION_ID=other\0"].concat();
        assert!(!owner_environment_matches(&duplicate, "qa", "unique"));
    }
}
