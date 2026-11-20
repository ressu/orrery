//! The D-Bus client against a private `dbus-daemon`, so the tests don't touch the system bus.

use std::path::{Path, PathBuf};
use std::time::Duration;

use orrery::dbus::{Bus, Call, Error, Kind, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

/// A bus of the tests' own, which stops when dropped.
struct TestBus {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    _daemon: tokio::process::Child,
}

async fn test_bus() -> TestBus {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("bus");
    let config = dir.path().join("bus.conf");
    std::fs::write(
        &config,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
            socket.display()
        ),
    )
    .unwrap();
    let mut daemon = tokio::process::Command::new("dbus-daemon")
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address"])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("dbus-daemon runs; it's in the dev shell");
    // It prints its address once it listens.
    let mut line = String::new();
    BufReader::new(daemon.stdout.take().unwrap()).read_line(&mut line).await.unwrap();
    assert!(line.contains("unix:"), "{line}");
    TestBus { _dir: dir, socket, _daemon: daemon }
}

fn bus_call<'a>(member: &'a str, args: Vec<Value>) -> Call<'a> {
    Call {
        destination: "org.freedesktop.DBus",
        path: "/org/freedesktop/DBus",
        interface: "org.freedesktop.DBus",
        member,
        args,
    }
}

async fn connect(socket: &Path) -> Bus {
    Bus::connect(socket).await.unwrap()
}

#[tokio::test]
async fn calls_the_bus() {
    let t = test_bus().await;
    let bus = connect(&t.socket).await;
    assert!(bus.name().starts_with(':'), "{}", bus.name());
    let names = bus.call(bus_call("ListNames", vec![])).await.unwrap();
    let names = names[0].items().unwrap();
    assert!(names.iter().any(|n| n.as_str() == Some(bus.name())));
    let owner = bus.call(bus_call("GetNameOwner", vec![Value::str(bus.name())])).await.unwrap();
    assert_eq!(owner[0].as_str(), Some(bus.name()));
    // A property, which comes inside a variant.
    let features =
        bus.property("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Features").await;
    assert!(features.unwrap().items().is_some());
}

#[tokio::test]
async fn reports_remote_errors() {
    let t = test_bus().await;
    let bus = connect(&t.socket).await;
    match bus.call(bus_call("NoSuchMethod", vec![])).await {
        Err(Error::Remote { name, .. }) => assert_eq!(name, "org.freedesktop.DBus.Error.UnknownMethod"),
        other => panic!("{other:?}"),
    }
    // A call to a name nobody owns.
    let call =
        Call { destination: "org.orrery.Nobody", path: "/", interface: "org.orrery.X", member: "Y", args: vec![] };
    assert!(matches!(bus.call(call).await, Err(Error::Remote { .. })));
}

#[tokio::test]
async fn receives_signals() {
    let t = test_bus().await;
    let listener = connect(&t.socket).await;
    let mut signals = listener.subscribe("type='signal',interface='org.orrery.Test'").await.unwrap();
    let mut owners = listener.subscribe("type='signal',member='NameOwnerChanged'").await.unwrap();
    let sender = connect(&t.socket).await;

    // A second connection shows up as a new name.
    let joined = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = owners.recv().await.unwrap();
            if msg.member.as_deref() == Some("NameOwnerChanged") && msg.body[0].as_str() == Some(sender.name()) {
                return msg;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(joined.body[2].as_str(), Some(sender.name()));

    // Values of every kind arrive as they were sent.
    let body = vec![
        Value::str("org.freedesktop.systemd1.Unit"),
        Value::Dict(
            "s".into(),
            "v".into(),
            vec![(Value::str("ActiveState"), Value::Variant(Box::new(Value::str("active"))))],
        ),
        Value::strs(&[]),
        Value::Struct(vec![Value::Byte(7), Value::Bool(false), Value::I64(-3), Value::F64(2.5), Value::path("/a/b")]),
        Value::Array("(su)".into(), vec![Value::Struct(vec![Value::str("x"), Value::U32(9)])]),
    ];
    sender.emit("/org/orrery", "org.orrery.Test", "Changed", body.clone()).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = signals.recv().await.unwrap();
            if msg.interface.as_deref() == Some("org.orrery.Test") {
                return msg;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(got.kind, Kind::Signal);
    assert_eq!(got.sender.as_deref(), Some(sender.name()));
    assert_eq!(got.body, body);
}

#[tokio::test]
async fn notices_a_closed_bus() {
    let t = test_bus().await;
    let bus = connect(&t.socket).await;
    drop(t);
    tokio::time::timeout(Duration::from_secs(5), async {
        while bus.is_open() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(bus.call(bus_call("ListNames", vec![])).await, Err(Error::Closed | Error::Io(_))));
}
