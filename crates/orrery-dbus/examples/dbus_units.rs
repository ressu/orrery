//! Lists the loaded services on the system bus, read-only, to try the D-Bus client on a
//! printer: `dbus_units [pid]`. With a process id, it also shows that process's unit.

use orrery_dbus::{Bus, Call, Value};

const SYSTEMD: &str = "org.freedesktop.systemd1";
const MANAGER: &str = "/org/freedesktop/systemd1";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bus = Bus::system().await?;
    println!("connected as {}", bus.name());
    let call = |member, args| Call {
        destination: SYSTEMD,
        path: MANAGER,
        interface: "org.freedesktop.systemd1.Manager",
        member,
        args,
    };
    let units = bus.call(call("ListUnitsFiltered", vec![Value::strs(&["loaded"])])).await?;
    for unit in units[0].items().unwrap_or_default() {
        let f = unit.items().unwrap_or_default();
        let name = f[0].as_str().unwrap_or_default();
        if name.ends_with(".service")
            && ["klipper", "moonraker", "crowsnest", "aldis", "ssh"].iter().any(|s| name.starts_with(s))
        {
            println!(
                "{name}: {} {} at {}",
                f[3].as_str().unwrap_or_default(),
                f[4].as_str().unwrap_or_default(),
                f[6].as_str().unwrap_or_default()
            );
        }
    }
    let virt = bus.property(SYSTEMD, MANAGER, "org.freedesktop.systemd1.Manager", "Virtualization").await?;
    println!("virtualization: {:?}", virt.as_str());
    if let Some(pid) = std::env::args().nth(1).and_then(|p| p.parse::<u32>().ok()) {
        let path = bus.call(call("GetUnitByPID", vec![Value::U32(pid)])).await?;
        let path = path[0].as_str().unwrap_or_default().to_owned();
        let id = bus.property(SYSTEMD, &path, "org.freedesktop.systemd1.Unit", "Id").await?;
        println!("process {pid} is in {:?}", id.as_str());
    }
    // polkit: may this process manage units? Asked without interaction, so nothing pops up.
    let subject = Value::Struct(vec![
        Value::str("unix-process"),
        Value::Dict(
            "s".into(),
            "v".into(),
            vec![
                (Value::str("pid"), Value::Variant(Box::new(Value::U32(std::process::id())))),
                (Value::str("start-time"), Value::Variant(Box::new(Value::U64(0)))),
            ],
        ),
    ]);
    let check = Call {
        destination: "org.freedesktop.PolicyKit1",
        path: "/org/freedesktop/PolicyKit1/Authority",
        interface: "org.freedesktop.PolicyKit1.Authority",
        member: "CheckAuthorization",
        args: vec![
            subject,
            Value::str("org.freedesktop.systemd1.manage-units"),
            Value::Dict("s".into(), "s".into(), vec![]),
            Value::U32(0),
            Value::str(""),
        ],
    };
    let result = bus.call(check).await?;
    let r = result[0].items().unwrap_or_default();
    println!("may manage units: {:?} (challenge: {:?})", r[0].as_bool(), r[1].as_bool());
    Ok(())
}
