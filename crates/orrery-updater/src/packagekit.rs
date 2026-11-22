//! PackageKit, the system's package service, over D-Bus, as Moonraker's
//! `PackageKitProvider` uses it (`update_manager/system_deploy.py`). Each call runs in a
//! transaction of its own, which reports packages and its end with signals.

use std::time::Duration;

use orrery_dbus::{Bus, Call, Value};
use tokio::sync::mpsc::UnboundedSender;

/// Where progress messages go.
pub type Messages = UnboundedSender<String>;

const SERVICE: &str = "org.freedesktop.PackageKit";
const TRANSACTION: &str = "org.freedesktop.PackageKit.Transaction";
/// An install or an upgrade may take this long, as in Moonraker.
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(3600);

/// PackageKit's flags are bits at their enum's index.
const fn bit(index: u32) -> u64 {
    1 << index
}
/// `NOT_INSTALLED | BASENAME | NEWEST | ARCH`: Moonraker's filter for packages to install.
const INSTALL_FILTER: u64 = bit(3) | bit(14) | bit(16) | bit(18);
pub const FILTER_NONE: u64 = bit(1);
/// Only packages from signed sources.
const ONLY_TRUSTED: u64 = bit(1);

const INFO: &[&str] = &[
    "Unknown",
    "Installed",
    "Available",
    "Low",
    "Enhancement",
    "Normal",
    "Bugfix",
    "Important",
    "Security",
    "Blocked",
    "Downloading",
    "Updating",
    "Installing",
    "Removing",
    "Cleanup",
    "Obsoleting",
    "Collection installed",
    "Collection available",
    "Finished",
    "Reinstalling",
    "Downgrading",
    "Preparing",
    "Decompressing",
    "Untrusted",
    "Trusted",
    "Unavailable",
    "Critical",
    "Install",
    "Remove",
    "Obsolete",
    "Downgrade",
];
const INFO_FINISHED: u32 = 18;
const EXIT: &[&str] = &[
    "Unknown",
    "Success",
    "Failed",
    "Cancelled",
    "Key required",
    "Eula required",
    "Killed",
    "Media change required",
    "Need untrusted",
    "Cancelled priority",
    "Skip transaction",
    "Repair required",
];
const EXIT_SUCCESS: u32 = 1;
const ERRORS: &[&str] = &[
    "UNKNOWN",
    "OUT_OF_MEMORY",
    "NO_NETWORK",
    "NOT_SUPPORTED",
    "INTERNAL_ERROR",
    "GPG_FAILURE",
    "PACKAGE_ID_INVALID",
    "PACKAGE_NOT_INSTALLED",
    "PACKAGE_NOT_FOUND",
    "PACKAGE_ALREADY_INSTALLED",
    "PACKAGE_DOWNLOAD_FAILED",
    "GROUP_NOT_FOUND",
    "GROUP_LIST_INVALID",
    "DEP_RESOLUTION_FAILED",
    "FILTER_INVALID",
    "CREATE_THREAD_FAILED",
    "TRANSACTION_ERROR",
    "TRANSACTION_CANCELLED",
    "NO_CACHE",
    "REPO_NOT_FOUND",
    "CANNOT_REMOVE_SYSTEM_PACKAGE",
    "PROCESS_KILL",
    "FAILED_INITIALIZATION",
    "FAILED_FINALISE",
    "FAILED_CONFIG_PARSING",
    "CANNOT_CANCEL",
    "CANNOT_GET_LOCK",
    "NO_PACKAGES_TO_UPDATE",
    "CANNOT_WRITE_REPO_CONFIG",
    "LOCAL_INSTALL_FAILED",
    "BAD_GPG_SIGNATURE",
    "MISSING_GPG_SIGNATURE",
    "CANNOT_INSTALL_SOURCE_PACKAGE",
    "REPO_CONFIGURATION_ERROR",
    "NO_LICENSE_AGREEMENT",
    "FILE_CONFLICTS",
    "PACKAGE_CONFLICTS",
    "REPO_NOT_AVAILABLE",
    "INVALID_PACKAGE_FILE",
    "PACKAGE_INSTALL_BLOCKED",
    "PACKAGE_CORRUPT",
    "ALL_PACKAGES_ALREADY_INSTALLED",
    "FILE_NOT_FOUND",
    "NO_MORE_MIRRORS_TO_TRY",
    "NO_DISTRO_UPGRADE_DATA",
    "INCOMPATIBLE_ARCHITECTURE",
    "NO_SPACE_ON_DEVICE",
    "MEDIA_CHANGE_REQUIRED",
    "NOT_AUTHORIZED",
    "UPDATE_NOT_FOUND",
    "CANNOT_INSTALL_REPO_UNSIGNED",
    "CANNOT_UPDATE_REPO_UNSIGNED",
    "CANNOT_GET_FILELIST",
    "CANNOT_GET_REQUIRES",
    "CANNOT_DISABLE_REPOSITORY",
    "RESTRICTED_DOWNLOAD",
    "PACKAGE_FAILED_TO_CONFIGURE",
    "PACKAGE_FAILED_TO_BUILD",
    "PACKAGE_FAILED_TO_INSTALL",
    "PACKAGE_FAILED_TO_REMOVE",
    "UPDATE_FAILED_DUE_TO_RUNNING_PROCESS",
    "PACKAGE_DATABASE_CHANGED",
    "PROVIDE_TYPE_NOT_SUPPORTED",
    "INSTALL_ROOT_INVALID",
    "CANNOT_FETCH_SOURCES",
    "CANCELLED_PRIORITY",
    "UNFINISHED_TRANSACTION",
    "LOCK_REQUIRED",
    "REPO_ALREADY_SET",
];

fn name(table: &[&str], index: u32) -> String {
    table.get(index as usize).map_or_else(|| format!("{index}"), |n| (*n).to_owned())
}

/// A package a transaction reported: its ID, `name;version;arch;repo`, and what PackageKit
/// says about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    pub id: String,
    pub info: u32,
}

impl Package {
    pub fn name(&self) -> &str {
        self.id.split(';').next().unwrap_or(&self.id)
    }
}

pub struct PackageKit {
    bus: Bus,
}

impl PackageKit {
    pub async fn connect() -> Result<Self, String> {
        let bus = Bus::system().await.map_err(|e| format!("Can't reach the system bus: {e}"))?;
        Ok(Self { bus })
    }

    /// Moonraker's `install_packages`: fresh package lists, then the named packages that
    /// aren't installed yet, from signed sources only.
    pub async fn install(&self, names: &[String], on_message: &Messages) -> Result<(), String> {
        self.run("RefreshCache", vec![Value::Bool(false)], Some(on_message)).await?;
        let resolve =
            vec![Value::U64(INSTALL_FILTER), Value::strs(&names.iter().map(String::as_str).collect::<Vec<_>>())];
        let ids: Vec<String> = self.run("Resolve", resolve, None).await?.into_iter().map(|p| p.id).collect();
        if ids.is_empty() {
            return Ok(());
        }
        tracing::info!("Installing packages: {}", ids.join(", "));
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.run("InstallPackages", vec![Value::U64(ONLY_TRUSTED), Value::strs(&ids)], Some(on_message)).await.map(drop)
    }

    /// Fresh package lists, then the packages with updates, as Moonraker's `refresh_packages`
    /// and `get_packages` find them. A failed refresh is the second value: the updates then
    /// come from the lists PackageKit has, as apt keeps the old list of a source it can't
    /// read. Moonraker lists nothing then.
    pub async fn updates(&self, on_message: Option<&Messages>) -> Result<(Vec<Package>, Option<String>), String> {
        let refreshed = self.run("RefreshCache", vec![Value::Bool(false)], on_message).await.err();
        let packages = self.run("GetUpdates", vec![Value::U64(FILTER_NONE)], None).await?;
        Ok((packages, refreshed))
    }

    /// Moonraker's `upgrade_system`: every update, from signed sources only. It needs fresh
    /// package lists.
    pub async fn upgrade(&self, on_message: &Messages) -> Result<(), String> {
        let (packages, refresh_error) = self.updates(Some(on_message)).await?;
        if let Some(e) = refresh_error {
            return Err(e);
        }
        let ids: Vec<String> = packages.into_iter().map(|p| p.id).collect();
        if ids.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.run("UpdatePackages", vec![Value::U64(ONLY_TRUSTED), Value::strs(&ids)], Some(on_message)).await.map(drop)
    }

    /// Runs `member` in a new transaction, and returns the packages it reported. With
    /// `on_message`, the progress goes there as Moonraker words it.
    pub async fn run(
        &self,
        member: &str,
        args: Vec<Value>,
        on_message: Option<&Messages>,
    ) -> Result<Vec<Package>, String> {
        let fail = |e: orrery_dbus::Error| format!("PackageKit {member} failed: {e}");
        let reply = self
            .bus
            .call(Call {
                destination: SERVICE,
                path: "/org/freedesktop/PackageKit",
                interface: SERVICE,
                member: "CreateTransaction",
                args: Vec::new(),
            })
            .await
            .map_err(fail)?;
        let Some(Value::Path(tid)) = reply.into_iter().next() else {
            return Err("PackageKit didn't return a transaction".into());
        };
        let rule = format!("type='signal',sender='{SERVICE}',path='{tid}'");
        let mut signals = self.bus.subscribe(&rule).await.map_err(fail)?;
        let call = Call { destination: SERVICE, path: &tid, interface: TRANSACTION, member, args };
        let result = match self.bus.call(call).await {
            Err(e) => Err(fail(e)),
            Ok(_) => {
                let wait = async {
                    let mut packages = Vec::new();
                    let mut error = String::new();
                    // PackageKit names some sources more than once.
                    let mut sources: Vec<String> = Vec::new();
                    loop {
                        let msg = match signals.recv().await {
                            Ok(m) => m,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => return Err("The D-Bus connection closed".to_owned()),
                        };
                        if msg.path.as_deref() != Some(tid.as_str()) {
                            continue;
                        }
                        let arg = |i: usize| msg.body.get(i);
                        let u32_at = |i| arg(i).and_then(Value::as_u32).unwrap_or(0);
                        let str_at = |i| arg(i).and_then(Value::as_str).unwrap_or_default().to_owned();
                        match msg.member.as_deref() {
                            Some("Package") => {
                                let code = u32_at(0);
                                let info = if code & 0xFFFF == 0 { code >> 16 } else { code & 0xFFFF };
                                let package = Package { id: str_at(1), info };
                                if let Some(f) = on_message
                                    && info != INFO_FINISHED
                                {
                                    let mut parts = package.id.split(';');
                                    let (n, v) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                                    let _ = f.send(format!("{}: {n} ({v})", name(INFO, info)));
                                }
                                packages.push(package);
                            }
                            Some("RepoDetail") => {
                                if let Some(f) = on_message {
                                    let id = str_at(0);
                                    let repo = if id.trim().is_empty() { str_at(1) } else { id };
                                    if !sources.contains(&repo) {
                                        let _ = f.send(format!("GET: {repo}"));
                                        sources.push(repo);
                                    }
                                }
                            }
                            Some("ErrorCode") => error = format!("{}: {}", name(ERRORS, u32_at(0)), str_at(1)),
                            Some("Finished") => {
                                let (exit, ms) = (u32_at(0), u32_at(1));
                                let message = format!(
                                    "Transaction {}: Exit {}, Run time: {:.2} seconds",
                                    role(member),
                                    name(EXIT, exit),
                                    f64::from(ms) / 1000.0
                                );
                                match on_message {
                                    Some(f) => drop(f.send(message)),
                                    None => tracing::info!("{message}"),
                                }
                                return if exit == EXIT_SUCCESS {
                                    Ok(packages)
                                } else {
                                    Err(if error.is_empty() { name(EXIT, exit) } else { error })
                                };
                            }
                            _ => {}
                        }
                    }
                };
                tokio::time::timeout(TRANSACTION_TIMEOUT, wait)
                    .await
                    .unwrap_or_else(|_| Err(format!("PackageKit {member} ran for more than an hour")))
            }
        };
        let _ = self
            .bus
            .call(Call {
                destination: "org.freedesktop.DBus",
                path: "/org/freedesktop/DBus",
                interface: "org.freedesktop.DBus",
                member: "RemoveMatch",
                args: vec![Value::str(&rule)],
            })
            .await;
        result
    }
}

/// How Moonraker names a transaction's role in its messages.
fn role(member: &str) -> &str {
    match member {
        "RefreshCache" => "Refresh cache",
        "Resolve" => "Resolve",
        "InstallPackages" => "Install packages",
        "GetUpdates" => "Get updates",
        "UpdatePackages" => "Update packages",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_match_moonraker() {
        // Moonraker sends 344072 for its install filter, and 2 for ONLY_TRUSTED.
        assert_eq!((INSTALL_FILTER, FILTER_NONE, ONLY_TRUSTED), (344_072, 2, 2));
        let p = Package { id: "libc6;2.41-12;arm64;debian-security".into(), info: 0 };
        assert_eq!(p.name(), "libc6");
        assert_eq!(name(INFO, INFO_FINISHED), "Finished");
        assert_eq!(name(EXIT, EXIT_SUCCESS), "Success");
        assert_eq!(name(ERRORS, 13), "DEP_RESOLUTION_FAILED");
        assert_eq!(ERRORS.len(), 69);
    }
}
