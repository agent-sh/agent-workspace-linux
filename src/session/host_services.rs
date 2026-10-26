//! The user's keyring services on the workspace session bus.
//!
//! Workspace apps use the workspace's own session bus, so single-instance
//! apps open their windows in the workspace and not on the user's desktop.
//! The keyring lives on the user's session bus. Each relay owns one keyring
//! name on the workspace bus and forwards what apps send it to the same name
//! on the user's bus: method calls with their raw bodies, their replies by
//! serial, and the service's signals. Chrome, Secret Service clients and KDE
//! apps then see the logins the user already has.
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    ffi::OsString,
    num::NonZeroU32,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    thread::{self, JoinHandle},
    time::Duration,
};
use zbus::{
    blocking::{fdo::DBusProxy, Connection, MessageIterator},
    message::{Flags, Type},
    Message,
};

/// The user's services forwarded when the user's session bus has them: the
/// keyring, and dconf so GSettings changes an app makes are the user's own.
pub(crate) const SERVICES: &[&str] = &[
    "org.freedesktop.secrets",
    "org.kde.kwalletd6",
    "org.kde.kwalletd5",
    "ca.desrt.dconf",
];

/// The user's session bus. MCP hosts may start the server with a bare
/// environment, so the standard socket paths stand in for the variable.
pub(crate) fn user_bus_address() -> Option<String> {
    if let Some(address) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        return Some(address.to_string_lossy().into_owned());
    }
    let uid = unsafe { libc::getuid() };
    [
        std::env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("bus")),
        Some(PathBuf::from(format!("/run/user/{uid}/bus"))),
    ]
    .into_iter()
    .flatten()
    .find(|socket| socket.exists())
    .map(|socket| crate::session::private::socket_address(&socket))
}

/// A variable of the user's session: the server's own environment, else the
/// systemd user manager's, which login sessions export (SSH_AUTH_SOCK from
/// gcr or ssh-agent, XDG_RUNTIME_DIR) and bare MCP hosts drop.
pub(crate) fn user_session_var(name: &str) -> Option<OsString> {
    if let Some(value) = std::env::var_os(name) {
        return Some(value);
    }
    if let Some(value) = user_manager_environment().get(name) {
        return Some(value.into());
    }
    (name == "XDG_RUNTIME_DIR")
        .then(|| OsString::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

fn user_manager_environment() -> &'static HashMap<String, String> {
    static ENVIRONMENT: OnceLock<HashMap<String, String>> = OnceLock::new();
    ENVIRONMENT.get_or_init(|| {
        let read = || -> Result<Vec<String>> {
            let bus = connect(&user_bus_address().context("no user session bus")?)?;
            let reply = bus.call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.freedesktop.systemd1.Manager", "Environment"),
            )?;
            let value: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
            Ok(Vec::<String>::try_from(value)?)
        };
        read()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                let (name, value) = entry.split_once('=')?;
                Some((name.to_string(), value.to_string()))
            })
            .collect()
    })
}

/// Start a relay for each keyring service the user's session offers. A
/// service that fails to relay is logged and the others still start.
pub(crate) fn start(workspace_bus: &str) -> Result<Vec<Relay>> {
    let Some(user_bus) = user_bus_address() else {
        eprintln!("agent workspace: no user session bus found; the keyring is not relayed");
        return Ok(Vec::new());
    };
    let probe = connect(&user_bus).context("connect to the user's session bus")?;
    let dbus = DBusProxy::new(&probe)?;
    let activatable = dbus.list_activatable_names().unwrap_or_default();
    let mut relays = Vec::new();
    for name in SERVICES {
        let offered = dbus
            .name_has_owner(zbus::names::BusName::try_from(*name)?)
            .unwrap_or(false)
            || activatable.iter().any(|n| n.as_str() == *name);
        if !offered {
            continue;
        }
        match Relay::start(name, &user_bus, workspace_bus) {
            Ok(relay) => relays.push(relay),
            Err(error) => eprintln!("agent workspace: {name} not relayed: {error:#}"),
        }
    }
    Ok(relays)
}

fn connect(address: &str) -> Result<Connection> {
    Ok(zbus::blocking::connection::Builder::address(address)?
        .method_timeout(Duration::from_secs(5))
        .build()?)
}

pub(crate) struct Relay {
    user: Connection,
    workspace: Connection,
    threads: Vec<JoinHandle<()>>,
}

impl Relay {
    fn start(name: &'static str, user_bus: &str, workspace_bus: &str) -> Result<Self> {
        let user = connect(user_bus)?;
        let workspace = connect(workspace_bus)?;
        workspace.request_name(name)?;
        DBusProxy::new(&user)?.add_match_rule(
            format!("type='signal',sender='{name}'")
                .as_str()
                .try_into()?,
        )?;
        // Calls forwarded to the user's bus, by the serial they went out with.
        let pending = Arc::new(Mutex::new(HashMap::<NonZeroU32, Message>::new()));
        let calls = {
            let (user, workspace, pending) = (user.clone(), workspace.clone(), pending.clone());
            thread::Builder::new()
                .name(format!("relay-calls-{name}"))
                .spawn(move || forward_calls(name, &workspace, &user, &pending))?
        };
        let answers = {
            let (user, workspace) = (user.clone(), workspace.clone());
            thread::Builder::new()
                .name(format!("relay-answers-{name}"))
                .spawn(move || forward_answers(&user, &workspace, &pending))?
        };
        Ok(Self {
            user,
            workspace,
            threads: vec![calls, answers],
        })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.workspace.clone().close();
        let _ = self.user.clone().close();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Method calls apps send the relayed name, re-sent to the user's bus.
fn forward_calls(
    name: &str,
    workspace: &Connection,
    user: &Connection,
    pending: &Mutex<HashMap<NonZeroU32, Message>>,
) {
    let own = workspace.unique_name().map(|n| n.to_string());
    for message in MessageIterator::from(workspace) {
        let Ok(call) = message else { break };
        let header = call.header();
        if header.message_type() != Type::MethodCall {
            continue;
        }
        let destination = header.destination().map(|d| d.to_string());
        if destination.as_deref() != Some(name) && destination != own {
            continue;
        }
        let expects_reply = !call
            .primary_header()
            .flags()
            .contains(Flags::NoReplyExpected);
        match forwarded_call(name, &call) {
            Ok(forward) => {
                if expects_reply {
                    pending
                        .lock()
                        .expect("relay pending calls")
                        .insert(forward.primary_header().serial_num(), call.clone());
                }
                if user.send(&forward).is_err() {
                    pending
                        .lock()
                        .expect("relay pending calls")
                        .remove(&forward.primary_header().serial_num());
                    reply_error(workspace, &call, "the user's session bus is gone");
                }
            }
            Err(error) if expects_reply => reply_error(workspace, &call, &error.to_string()),
            Err(_) => {}
        }
    }
}

/// Replies to forwarded calls and the service's signals, sent back to apps.
fn forward_answers(
    user: &Connection,
    workspace: &Connection,
    pending: &Mutex<HashMap<NonZeroU32, Message>>,
) {
    for message in MessageIterator::from(user) {
        let Ok(answer) = message else { break };
        let header = answer.header();
        // A reply for an app that has gone is dropped; the loop ends with the bus.
        let _ = match header.message_type() {
            Type::MethodReturn | Type::Error => {
                let Some(call) = header.reply_serial().and_then(|serial| {
                    pending.lock().expect("relay pending calls").remove(&serial)
                }) else {
                    continue;
                };
                reply(&call, &answer).and_then(|reply| Ok(workspace.send(&reply)?))
            }
            Type::Signal => signal(&answer).and_then(|signal| Ok(workspace.send(&signal)?)),
            Type::MethodCall => continue,
        };
    }
}

fn forwarded_call(name: &str, call: &Message) -> Result<Message> {
    let header = call.header();
    let mut builder = Message::method_call(
        header
            .path()
            .context("method call without a path")?
            .to_owned(),
        header
            .member()
            .context("method call without a member")?
            .to_owned(),
    )?
    .destination(name)?;
    if call
        .primary_header()
        .flags()
        .contains(Flags::NoReplyExpected)
    {
        builder = builder.with_flags(Flags::NoReplyExpected)?;
    }
    if let Some(interface) = header.interface() {
        builder = builder.interface(interface.to_owned())?;
    }
    raw_body(builder, call)
}

fn reply(call: &Message, answer: &Message) -> Result<Message> {
    let builder = match answer.header().error_name() {
        Some(error) => Message::error(&call.header(), error.to_owned())?,
        None => Message::method_return(&call.header())?,
    };
    raw_body(builder, answer)
}

fn signal(message: &Message) -> Result<Message> {
    let header = message.header();
    let builder = Message::signal(
        header.path().context("signal without a path")?.to_owned(),
        header
            .interface()
            .context("signal without an interface")?
            .to_owned(),
        header
            .member()
            .context("signal without a member")?
            .to_owned(),
    )?;
    raw_body(builder, message)
}

/// The same body bytes and signature under a new header. Keyring calls carry
/// no file descriptors; a message that does is refused rather than altered.
fn raw_body(builder: zbus::message::Builder<'_>, source: &Message) -> Result<Message> {
    let body = source.body();
    let data = body.data();
    anyhow::ensure!(data.fds().is_empty(), "file descriptors are not relayed");
    anyhow::ensure!(
        source.primary_header().endian_sig() == zbus::message::NATIVE_ENDIAN_SIG,
        "a foreign-endian message is not relayed"
    );
    // SAFETY: the bytes and signature are a body some peer's bus already
    // accepted under this signature, in this machine's byte order.
    Ok(unsafe { builder.build_raw_body(data.bytes(), body.signature().clone(), Vec::new())? })
}

fn reply_error(workspace: &Connection, call: &Message, why: &str) {
    if let Ok(error) = Message::error(&call.header(), "org.freedesktop.DBus.Error.Failed")
        .and_then(|builder| builder.build(&why))
    {
        let _ = workspace.send(&error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        time::Instant,
    };

    struct Bus {
        child: Child,
        address: String,
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn bus(dir: &Path, name: &str) -> Bus {
        let socket = dir.join(name);
        let address = format!("unix:path={}", socket.display());
        let child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--address"])
            .arg(&address)
            .stdout(Stdio::null())
            .spawn()
            .expect("dbus-daemon");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "dbus-daemon did not listen");
            thread::sleep(Duration::from_millis(10));
        }
        Bus { child, address }
    }

    struct Keyring;

    #[zbus::interface(name = "org.freedesktop.Secret.Service")]
    impl Keyring {
        fn search_items(&self, label: &str) -> (Vec<String>, u32) {
            (vec![format!("found {label}")], 7)
        }
        fn fail(&self) -> zbus::fdo::Result<()> {
            Err(zbus::fdo::Error::AccessDenied("locked".into()))
        }
    }

    // Chrome and libsecret call the keyring by its name on their own bus.
    #[test]
    fn workspace_apps_reach_the_users_keyring_through_the_relay() {
        let dir: PathBuf =
            std::env::temp_dir().join(format!("agent-workspace-relay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("bus dir");
        let user = bus(&dir, "user-bus");
        let workspace = bus(&dir, "workspace-bus");
        let keyring = zbus::blocking::connection::Builder::address(user.address.as_str())
            .unwrap()
            .name("org.freedesktop.secrets")
            .unwrap()
            .serve_at("/org/freedesktop/secrets", Keyring)
            .unwrap()
            .build()
            .unwrap();
        let relay = Relay::start("org.freedesktop.secrets", &user.address, &workspace.address)
            .expect("relay");

        let app = connect(&workspace.address).unwrap();
        let signals = MessageIterator::for_match_rule(
            "type='signal',interface='org.freedesktop.Secret.Service'",
            &app,
            None,
        )
        .unwrap();
        let reply = app
            .call_method(
                Some("org.freedesktop.secrets"),
                "/org/freedesktop/secrets",
                Some("org.freedesktop.Secret.Service"),
                "SearchItems",
                &("github",),
            )
            .expect("relayed call");
        let (items, count): (Vec<String>, u32) = reply.body().deserialize().unwrap();
        assert_eq!((items, count), (vec!["found github".to_string()], 7));

        let error = app
            .call_method(
                Some("org.freedesktop.secrets"),
                "/org/freedesktop/secrets",
                Some("org.freedesktop.Secret.Service"),
                "Fail",
                &(),
            )
            .expect_err("the service's error comes back");
        assert!(error.to_string().contains("locked"), "{error}");

        keyring
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/secrets",
                "org.freedesktop.Secret.Service",
                "CollectionCreated",
                &("/org/freedesktop/secrets/collection/new",),
            )
            .unwrap();
        let signal = signals.into_iter().next().unwrap().unwrap();
        assert_eq!(
            signal.header().member().unwrap().as_str(),
            "CollectionCreated"
        );
        let collection: String = signal.body().deserialize().unwrap();
        assert_eq!(collection, "/org/freedesktop/secrets/collection/new");

        drop(relay);
        drop(keyring);
        std::fs::remove_dir_all(dir).ok();
    }
}
