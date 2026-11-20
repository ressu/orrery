//! A small D-Bus client: enough to talk to systemd, logind, and polkit on the system bus.
//!
//! It connects over a unix socket, logs in with `EXTERNAL` (the kernel vouches for orrery's user
//! id), makes method calls, and receives the signals it subscribes to. It doesn't offer objects
//! of its own: a call to orrery gets an `UnknownMethod` error. See
//! `docs/design/2026-11-19-service-control.md` for why it's orrery's own and not `zbus`.

pub mod message;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, oneshot};

pub use message::{Kind, Message, Value};

const SYSTEM_BUS: &str = "/run/dbus/system_bus_socket";
/// How long a call waits for its reply, as libdbus's default.
const CALL_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The bus refused the login, or the address isn't one orrery can use.
    Connect(String),
    Format(message::FormatError),
    /// The other side answered with an error, such as `org.freedesktop.DBus.Error.AccessDenied`.
    Remote {
        name: String,
        message: String,
    },
    Timeout,
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Connect(e) => write!(f, "{e}"),
            Error::Format(e) => write!(f, "{e}"),
            Error::Remote { name, message } if message.is_empty() => write!(f, "{name}"),
            Error::Remote { name, message } => write!(f, "{message} ({name})"),
            Error::Timeout => write!(f, "no reply in {} s", CALL_TIMEOUT.as_secs()),
            Error::Closed => write!(f, "the D-Bus connection closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<message::FormatError> for Error {
    fn from(e: message::FormatError) -> Self {
        Error::Format(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A connection to a bus. Clones share it.
#[derive(Clone)]
pub struct Bus {
    inner: Arc<Inner>,
}

struct Inner {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    serial: AtomicU32,
    /// Calls waiting for their reply, by serial. Dropped when the connection closes, so the
    /// callers wake with [`Error::Closed`].
    pending: Mutex<Option<HashMap<u32, oneshot::Sender<Message>>>>,
    signals: broadcast::Sender<Arc<Message>>,
    name: OnceLock<String>,
}

/// A method call to make: who, what, and with which arguments.
pub struct Call<'a> {
    pub destination: &'a str,
    pub path: &'a str,
    pub interface: &'a str,
    pub member: &'a str,
    pub args: Vec<Value>,
}

impl Bus {
    /// Connects to the system bus: `DBUS_SYSTEM_BUS_ADDRESS`, or the usual socket.
    pub async fn system() -> Result<Self> {
        match std::env::var("DBUS_SYSTEM_BUS_ADDRESS") {
            Ok(address) => Self::connect(&socket_path(&address)?).await,
            Err(_) => Self::connect(Path::new(SYSTEM_BUS)).await,
        }
    }

    /// Connects to the bus at a unix socket, and logs in.
    pub async fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket).await?;
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let uid = rustix::process::getuid().as_raw().to_string();
        let hex: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
        write.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes()).await?;
        let mut line = String::new();
        read.read_line(&mut line).await?;
        if !line.starts_with("OK ") {
            return Err(Error::Connect(format!("the bus refused orrery's login: {}", line.trim())));
        }
        write.write_all(b"BEGIN\r\n").await?;
        let (signals, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            writer: tokio::sync::Mutex::new(write),
            serial: AtomicU32::new(1),
            pending: Mutex::new(Some(HashMap::new())),
            signals,
            name: OnceLock::new(),
        });
        tokio::spawn(read_messages(Arc::downgrade(&inner), read));
        let bus = Bus { inner };
        let reply = bus
            .call(Call {
                destination: "org.freedesktop.DBus",
                path: "/org/freedesktop/DBus",
                interface: "org.freedesktop.DBus",
                member: "Hello",
                args: vec![],
            })
            .await?;
        let name = reply.first().and_then(Value::as_str).unwrap_or_default().to_owned();
        let _ = bus.inner.name.set(name);
        Ok(bus)
    }

    /// The name the bus gave this connection, such as `:1.42`.
    pub fn name(&self) -> &str {
        self.inner.name.get().map_or("", String::as_str)
    }

    /// Makes a method call and waits for its reply's values.
    pub async fn call(&self, call: Call<'_>) -> Result<Vec<Value>> {
        let mut msg = Message::new(Kind::MethodCall);
        msg.destination = Some(call.destination.into());
        msg.path = Some(call.path.into());
        msg.interface = Some(call.interface.into());
        msg.member = Some(call.member.into());
        msg.body = call.args;
        let (tx, rx) = oneshot::channel();
        let serial = self.next_serial();
        msg.serial = serial;
        self.inner.pending.lock().expect("pending lock").as_mut().ok_or(Error::Closed)?.insert(serial, tx);
        if let Err(e) = self.send(&msg).await {
            self.forget(serial);
            return Err(e);
        }
        let reply = match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => return Err(Error::Closed),
            Err(_) => {
                self.forget(serial);
                return Err(Error::Timeout);
            }
        };
        match reply.kind {
            Kind::Error => Err(Error::Remote {
                name: reply.error_name.unwrap_or_default(),
                message: reply.body.first().and_then(Value::as_str).unwrap_or_default().to_owned(),
            }),
            _ => Ok(reply.body),
        }
    }

    /// Reads a property, `org.freedesktop.DBus.Properties.Get`. The value comes without its
    /// variant.
    pub async fn property(&self, destination: &str, path: &str, interface: &str, name: &str) -> Result<Value> {
        let reply = self
            .call(Call {
                destination,
                path,
                interface: "org.freedesktop.DBus.Properties",
                member: "Get",
                args: vec![Value::str(interface), Value::str(name)],
            })
            .await?;
        match reply.into_iter().next() {
            Some(Value::Variant(v)) => Ok(*v),
            _ => Err(Error::Format(message::FormatError("Get didn't return a variant".into()))),
        }
    }

    /// Sends a signal.
    pub async fn emit(&self, path: &str, interface: &str, member: &str, args: Vec<Value>) -> Result<()> {
        let mut msg = Message::new(Kind::Signal);
        msg.path = Some(path.into());
        msg.interface = Some(interface.into());
        msg.member = Some(member.into());
        msg.body = args;
        msg.serial = self.next_serial();
        self.send(&msg).await
    }

    /// Asks the bus for the signals that match `rule`, such as
    /// `type='signal',interface='org.freedesktop.DBus.Properties'`, and returns a receiver for
    /// every signal this connection gets. Subscribe before the call whose signals you need.
    pub async fn subscribe(&self, rule: &str) -> Result<broadcast::Receiver<Arc<Message>>> {
        let rx = self.inner.signals.subscribe();
        self.call(Call {
            destination: "org.freedesktop.DBus",
            path: "/org/freedesktop/DBus",
            interface: "org.freedesktop.DBus",
            member: "AddMatch",
            args: vec![Value::str(rule)],
        })
        .await?;
        Ok(rx)
    }

    /// Whether the connection is still open.
    pub fn is_open(&self) -> bool {
        self.inner.pending.lock().expect("pending lock").is_some()
    }

    fn next_serial(&self) -> u32 {
        // Serials must not be 0; after 4 billion messages they start over at 1.
        loop {
            let n = self.inner.serial.fetch_add(1, Ordering::Relaxed);
            if n != 0 {
                return n;
            }
        }
    }

    fn forget(&self, serial: u32) {
        if let Some(p) = self.inner.pending.lock().expect("pending lock").as_mut() {
            p.remove(&serial);
        }
    }

    async fn send(&self, msg: &Message) -> Result<()> {
        send_on(&self.inner, msg).await
    }
}

async fn send_on(inner: &Inner, msg: &Message) -> Result<()> {
    let bytes = msg.encode();
    let mut w = inner.writer.lock().await;
    w.write_all(&bytes).await?;
    Ok(())
}

/// Reads messages until the connection closes: replies go to their callers, and signals to the
/// subscribers. Holds only a weak reference, so dropping every [`Bus`] ends it.
async fn read_messages(inner: std::sync::Weak<Inner>, mut read: BufReader<OwnedReadHalf>) {
    let result: Result<()> = async {
        loop {
            let mut head = [0u8; 16];
            read.read_exact(&mut head).await?;
            let len = Message::length(&head)?;
            let mut buf = head.to_vec();
            buf.resize(len, 0);
            read.read_exact(&mut buf[16..]).await?;
            let msg = Message::decode(&buf)?;
            let Some(inner) = inner.upgrade() else { return Ok(()) };
            match msg.kind {
                Kind::Return | Kind::Error => {
                    let waiting =
                        msg.reply_serial.and_then(|s| inner.pending.lock().expect("pending lock").as_mut()?.remove(&s));
                    if let Some(tx) = waiting {
                        let _ = tx.send(msg);
                    }
                }
                Kind::Signal => {
                    let _ = inner.signals.send(Arc::new(msg));
                }
                Kind::MethodCall => {
                    if msg.flags & message::NO_REPLY_EXPECTED == 0 {
                        let mut reply = Message::new(Kind::Error);
                        reply.error_name = Some("org.freedesktop.DBus.Error.UnknownMethod".into());
                        reply.reply_serial = Some(msg.serial);
                        reply.destination = msg.sender.clone();
                        reply.serial = inner.serial.fetch_add(1, Ordering::Relaxed).max(1);
                        reply.body = vec![Value::str("orrery offers no D-Bus methods")];
                        send_on(&inner, &reply).await?;
                    }
                }
            }
        }
    }
    .await;
    if let Some(inner) = inner.upgrade() {
        if let Err(e) = result {
            tracing::warn!("The D-Bus connection closed: {e}");
        }
        inner.pending.lock().expect("pending lock").take();
    }
}

/// The socket path in a D-Bus address such as `unix:path=/run/dbus/system_bus_socket`.
fn socket_path(address: &str) -> Result<PathBuf> {
    // An address can list several ways to connect, separated by `;`. orrery takes the first
    // unix socket path.
    for entry in address.split(';') {
        let Some(params) = entry.strip_prefix("unix:") else { continue };
        for param in params.split(',') {
            if let Some(path) = param.strip_prefix("path=") {
                return Ok(PathBuf::from(unescape(path)));
            }
        }
    }
    Err(Error::Connect(format!("orrery can only use a unix:path= D-Bus address, not {address}")))
}

/// Undoes the `%xx` escapes of a D-Bus address.
fn unescape(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(socket_path("unix:path=/run/dbus/system_bus_socket").unwrap(), Path::new(SYSTEM_BUS));
        assert_eq!(socket_path("tcp:host=x;unix:guid=1,path=/tmp/a%20b").unwrap(), Path::new("/tmp/a b"));
        assert!(socket_path("unix:abstract=/tmp/x").is_err());
    }
}
