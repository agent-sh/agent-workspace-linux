//! Fixed i3 commands on the workspace-owned IPC endpoint. X11 still owns pixel
//! capture and input; arbitrary i3 commands are never exposed through workspace IPC.
use crate::workspace::WorkspaceStatus;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    time::Duration,
};

pub(crate) fn request(runtime: &Path, pid: u32, kind: u32, payload: &str) -> Result<Value> {
    let path = crate::session::i3::socket_path(runtime);
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        bail!("invalid i3 IPC path");
    }
    for (out, byte) in address.sun_path.iter_mut().zip(bytes) {
        *out = *byte as libc::c_char;
    }
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        };
        if result == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        // A loaded WM may temporarily fill its listen backlog. Retry only the
        // connect, before any command is sent, within the normal IPC deadline.
        if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        return Err(error).context("connect private i3 IPC without blocking");
    }
    let mut socket = UnixStream::from(fd);
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    if ok != 0 || peer.pid as u32 != pid || peer.uid != unsafe { libc::geteuid() } {
        bail!("i3 IPC peer is not this workspace's window manager");
    }
    // One write: i3 reads the header from a nonblocking socket and drops the
    // client on EAGAIN (libi3 ipc_recv_message), so a header split across
    // writes intermittently resets the connection.
    let mut message = Vec::with_capacity(14 + payload.len());
    message.extend_from_slice(b"i3-ipc");
    message.extend_from_slice(&u32::try_from(payload.len())?.to_ne_bytes());
    message.extend_from_slice(&kind.to_ne_bytes());
    message.extend_from_slice(payload.as_bytes());
    socket.write_all(&message)?;
    let mut header = [0u8; 14];
    socket.read_exact(&mut header)?;
    if &header[..6] != b"i3-ipc" || u32::from_ne_bytes(header[10..14].try_into()?) != kind {
        bail!("invalid i3 IPC reply header");
    }
    let length = u32::from_ne_bytes(header[6..10].try_into()?);
    // Parse through a length-limited reader rather than allocating the advertised
    // payload size. The connected peer was verified against our owned child PID.
    let mut reader = std::io::BufReader::new(socket).take(u64::from(length));
    let value = serde_json::from_reader(&mut reader).context("decode i3 reply")?;
    if reader.limit() != 0 {
        bail!("truncated i3 IPC reply");
    }
    Ok(value)
}

fn numeric_window(id: &str) -> Result<u32> {
    let id = id.trim();
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!("window id must be a decimal X11 id");
    }
    let id: u32 = id.parse().context("X11 window id exceeds 32 bits")?;
    if id == 0 {
        bail!("window id must not be zero");
    }
    Ok(id)
}

fn find_window(tree: &Value, id: u32) -> bool {
    tree.get("window").and_then(Value::as_u64) == Some(u64::from(id))
        || ["nodes", "floating_nodes"].iter().any(|field| {
            tree.get(field)
                .and_then(Value::as_array)
                .is_some_and(|nodes| nodes.iter().any(|node| find_window(node, id)))
        })
}

pub(crate) fn command(status: &WorkspaceStatus, id: &str, operation: &str) -> Result<()> {
    let id = numeric_window(id)?;
    let pid = status
        .window_manager_pid
        .context("workspace has no i3 process")?;
    let tree = request(&status.runtime_dir, pid, 4, "")?;
    if !find_window(&tree, id) {
        bail!("window {id} is not managed by this workspace's i3");
    }
    let reply = request(
        &status.runtime_dir,
        pid,
        0,
        &format!("[id={id}] {operation}"),
    )?;
    let replies = reply
        .as_array()
        .context("i3 command reply must be an array")?;
    if replies.is_empty()
        || replies
            .iter()
            .any(|reply| reply.get("success").and_then(Value::as_bool) != Some(true))
    {
        bail!("i3 window operation failed: {reply}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selectors_reject_command_injection_and_overflow() {
        assert_eq!(numeric_window(" 123 ").unwrap(), 123);
        for id in ["0", "", "4294967296", "1] exec sh", "1;exit", "-1"] {
            assert!(numeric_window(id).is_err(), "{id}");
        }
    }
}
