//! Single-instance rules and local-socket IPC (ARCHITECTURE.md §4).
//!
//! One GUI per user, one server per port; one-shot searches are always
//! allowed. Each role is a local socket: a Unix domain socket on Linux, BSD
//! and macOS, a named pipe on Windows. Whoever can connect to it finds the
//! role taken, and can talk to its holder: newline-delimited JSON, one
//! `{"command": …}` object per line, one reply object per line.
//!
//! Commands so far: `raise` (show the window), `status`, `quit`. A request
//! may carry more fields; the handler gets all of it.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::Result;

/// A role only one process may hold at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The desktop app: one per user.
    Gui,
    /// A web server: one per port.
    Server(u16),
}

impl Role {
    fn name(self) -> String {
        match self {
            Role::Gui => "gui".to_string(),
            Role::Server(port) => format!("server-{port}"),
        }
    }
}

/// Answers one request; gets the command name and the whole request,
/// returns the reply.
pub type Handler = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

/// Outcome of trying to take a role.
pub enum Claim {
    /// The role is ours; answer requests with [`Listener::serve`].
    Ours(Listener),
    /// Another process holds it; this is its `status` reply.
    Running(Value),
}

/// Takes `role` unless another process holds it.
pub fn claim(role: Role) -> Result<Claim> {
    if let Some(status) = ask(role, "status") {
        return Ok(Claim::Running(status));
    }
    // Nobody answered: any socket file left behind is stale and is replaced.
    let inner = imp::bind(role)
        .map_err(|e| format!("can't set up the local socket for {}: {e}", role.name()))?;
    Ok(Claim::Ours(Listener { inner }))
}

/// Sends `command` to whoever holds `role`. `None` if nobody does.
pub fn ask(role: Role, command: &str) -> Option<Value> {
    send(role, json!({ "command": command }))
}

/// Sends a request (`{"command": …, …}`) to whoever holds `role`. `None`
/// if nobody does.
pub fn send(role: Role, request: Value) -> Option<Value> {
    let stream = imp::connect(role).ok()?;
    Some(exchange(stream, &request).unwrap_or(Value::Null))
}

fn exchange<S: Read + Write>(stream: S, request: &Value) -> io::Result<Value> {
    let mut reader = BufReader::new(stream);
    let mut request = request.to_string();
    request.push('\n');
    reader.get_mut().write_all(request.as_bytes())?;
    reader.get_mut().flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// A role this process holds.
pub struct Listener {
    inner: imp::Listener,
}

impl Listener {
    /// Answers requests on a background thread for as long as the process runs.
    pub fn serve(self, handler: Handler) {
        let inner = self.inner;
        std::thread::Builder::new()
            .name("lost-ipc".into())
            .spawn(move || imp::serve(inner, handler))
            .expect("can't start the IPC thread");
    }
}

/// Reads requests from one connection and answers each.
fn answer<S: Read + Write>(stream: S, handler: &Handler) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let request = serde_json::from_str::<Value>(&line).unwrap_or(Value::Null);
        let command = request
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let reply = if command.is_empty() {
            json!({ "ok": false, "error": "not a LOST command" })
        } else {
            handler(&command, &request)
        };
        let mut out = reply.to_string();
        out.push('\n');
        if reader.get_mut().write_all(out.as_bytes()).is_err() || reader.get_mut().flush().is_err()
        {
            return;
        }
    }
}

/// The reply to an unknown command.
pub fn unknown(command: &str) -> Value {
    json!({ "ok": false, "error": format!("unknown command: {command}") })
}

// ── Unix: domain sockets in a private runtime folder ────────────────────

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{Handler, Role, answer};

    pub struct Listener {
        listener: UnixListener,
    }

    /// `$XDG_RUNTIME_DIR/lost` on Linux and BSD (else a per-user folder in
    /// the temp folder), `$TMPDIR/lost` on macOS (already per-user there).
    fn runtime_dir() -> io::Result<PathBuf> {
        let dir = if cfg!(target_os = "macos") {
            std::env::temp_dir().join("lost")
        } else {
            match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
                Some(d) => PathBuf::from(d).join("lost"),
                None => {
                    // SAFETY: getuid has no preconditions and can't fail.
                    let uid = unsafe { libc::getuid() };
                    std::env::temp_dir().join(format!("lost-{uid}"))
                }
            }
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        // Ours only, even if it already existed.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }

    fn path(role: Role) -> io::Result<PathBuf> {
        Ok(runtime_dir()?.join(format!("{}.sock", role.name())))
    }

    pub fn connect(role: Role) -> io::Result<UnixStream> {
        let stream = UnixStream::connect(path(role)?)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        Ok(stream)
    }

    pub fn bind(role: Role) -> io::Result<Listener> {
        let path = path(role)?;
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                // Left by a process that's gone (nobody answered on it).
                std::fs::remove_file(&path)?;
                UnixListener::bind(&path)?
            }
            Err(e) => return Err(e),
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener { listener })
    }

    pub fn serve(listener: Listener, handler: Handler) {
        for stream in listener.listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let handler = handler.clone();
            std::thread::spawn(move || answer(stream, &handler));
        }
    }
}

// ── Windows: named pipes ────────────────────────────────────────────────

#[cfg(windows)]
mod imp {
    use std::io;

    use interprocess::local_socket::prelude::*;
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Name, Stream};

    use super::{Handler, Role, answer};

    pub struct Listener {
        listener: interprocess::local_socket::Listener,
    }

    /// `\\.\pipe\lost-<user>-<role>`
    fn name(role: Role) -> io::Result<Name<'static>> {
        let user = std::env::var("USERNAME").unwrap_or_default();
        format!("lost-{user}-{}", role.name()).to_ns_name::<GenericNamespaced>()
    }

    pub fn connect(role: Role) -> io::Result<Stream> {
        Stream::connect(name(role)?)
    }

    pub fn bind(role: Role) -> io::Result<Listener> {
        let listener = ListenerOptions::new().name(name(role)?).create_sync()?;
        Ok(Listener { listener })
    }

    pub fn serve(listener: Listener, handler: Handler) {
        for stream in listener.listener.incoming().flatten() {
            let handler = handler.clone();
            std::thread::spawn(move || answer(stream, &handler));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn claim_ask_and_stale_sockets() {
        let dir = std::env::temp_dir().join(format!("lost-ipc-test-{}", std::process::id()));
        // SAFETY: tests in this module don't run other threads reading the environment.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        let role = Role::Server(65001);

        assert!(ask(role, "status").is_none());
        let Claim::Ours(listener) = claim(role).unwrap() else {
            panic!("should be free")
        };
        listener.serve(Arc::new(|c: &str, _: &Value| match c {
            "status" => json!({ "ok": true, "role": "server" }),
            other => unknown(other),
        }));
        assert_eq!(ask(role, "status").unwrap()["role"], "server");
        assert_eq!(ask(role, "bogus").unwrap()["ok"], false);
        assert!(matches!(claim(role).unwrap(), Claim::Running(_)));

        // Only this user may connect.
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("lost")), 0o700);
        assert_eq!(mode(&dir.join("lost").join("server-65001.sock")), 0o600);

        // A socket file nobody listens on is replaced.
        let stale = Role::Server(65002);
        let path = dir.join("lost").join("server-65002.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert!(matches!(claim(stale).unwrap(), Claim::Ours(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
