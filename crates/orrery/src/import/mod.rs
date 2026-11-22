//! Imports a copy of Moonraker's `moonraker-sql.db` into orrery's store, one way.
//!
//! What's imported:
//! - Every namespace clients use (frontend settings, webcams, and so on). Moonraker's own
//!   `database` and `moonraker` namespaces are skipped, and so is `gcode_metadata`, which orrery
//!   rebuilds from the files. The link from each file to its last print is kept, though.
//! - Print history and its totals.
//!
//! - Moonraker's API key, as orrery's `moonraker` key.
//!
//! Users aren't imported yet, because orrery has no user logins.
//!
//! Running the same import again changes nothing. Each row is written under its own key, and
//! the imported row wins when the store already holds a different one. History is the
//! exception: orrery numbers its own jobs like Moonraker, so a job orrery recorded can have the
//! id of an imported one. That job moves to a new id instead of being overwritten.
//!
//! The whole import is one store transaction: it's written completely or not at all.

mod sqlite;

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use redb::{ReadableTable, TableDefinition};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::database::{self, Database};
use sqlite::{SqliteFile, cell_bytes, decode_moonraker_record};

/// Each import run: `run number -> JSON` with the source, its SHA-256, the time, and the counts.
const IMPORTS: TableDefinition<u64, &[u8]> = TableDefinition::new("imports");
/// Every row an import wrote, as a JSON array key such as `["ns", "fluidd", "uiSettings"]`, so
/// `--replace` can remove them.
const IMPORTED: TableDefinition<&str, ()> = TableDefinition::new("imported_rows");

/// Moonraker's namespaces that the import skips.
pub const SKIPPED_NAMESPACES: &[&str] = &["database", "gcode_metadata", "moonraker", "update_manager"];
const METADATA_NAMESPACE: &str = "gcode_metadata";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Counts {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
}

impl Counts {
    fn count(&mut self, old: Option<&Value>, new: &Value) -> bool {
        match old {
            None => self.added += 1,
            Some(old) if old == new => {
                self.unchanged += 1;
                return false;
            }
            Some(_) => self.updated += 1,
        }
        true
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub source: String,
    pub sha256: String,
    pub settings: Counts,
    /// Skipped namespaces that the source has, with their key counts.
    pub skipped_namespaces: Vec<(String, usize)>,
    pub history: Counts,
    /// orrery's own jobs that moved to make room: `(old id, new id)`.
    pub moved_jobs: Vec<(u64, u64)>,
    pub totals: Counts,
    /// Files whose link to their last print was copied from Moonraker's metadata.
    pub linked_files: usize,
    /// Moonraker's users: added, or already imported and left as they are, since orrery may have
    /// changed them since (a new password hash, a logout).
    pub users: Counts,
    /// The names of the users the source has, imported now or before.
    pub user_names: Vec<String>,
    /// Users that orrery has of its own, with the same name, which the import leaves alone.
    pub kept_users: Vec<String>,
    /// Users who log in with LDAP, which needs an agent orrery doesn't have yet.
    pub ldap_users: Vec<String>,
    /// Users that can't be imported, and why.
    pub skipped_users: Vec<(String, String)>,
    /// Moonraker's API key was copied, as orrery's `moonraker` key.
    pub api_key: bool,
    /// Rows from earlier imports removed by `--replace`.
    pub replaced: usize,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    /// Report what would change, and write nothing.
    pub dry_run: bool,
    /// Remove every row earlier imports wrote, then import.
    pub replace: bool,
}

/// Moonraker's user for its API key (`authorization.py:69`).
const API_KEY_USER: &str = "_API_KEY_USER_";

pub fn run(store: &Database, source: &Path, options: Options) -> Result<Report> {
    let file = SqliteFile::open(source)?;
    let bytes = std::fs::read(source).with_context(|| format!("reading {}", source.display()))?;
    let mut report = Report {
        source: source.display().to_string(),
        sha256: Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect(),
        ..Default::default()
    };
    for table in ["namespace_store", "job_history", "job_totals"] {
        if !file.tables()?.iter().any(|t| t.name == table) {
            bail!("{} has no {table} table, so it isn't a Moonraker database", source.display());
        }
    }
    let tx = store.raw().begin_write()?;
    {
        let mut imported = tx.open_table(IMPORTED)?;
        let mut namespaces = tx.open_table(database::NAMESPACES)?;
        let mut history = tx.open_table(database::JOB_HISTORY)?;
        let mut totals = tx.open_table(database::JOB_TOTALS)?;
        let mut local = tx.open_table(database::LOCAL)?;
        let mut stored_users = tx.open_table(crate::users::USERS)?;

        if options.replace {
            let keys: Vec<String> =
                imported.iter()?.map(|e| e.map(|(k, _)| k.value().to_owned())).collect::<Result<_, _>>()?;
            for key in &keys {
                let parts: Vec<Value> = serde_json::from_str(key)?;
                let s = |i: usize| parts.get(i).and_then(Value::as_str).unwrap_or_default();
                let removed = match s(0) {
                    "ns" => namespaces.remove((s(1), s(2)))?.is_some(),
                    "job" => history.remove((s(1), parts.get(2).and_then(Value::as_u64).unwrap_or(0)))?.is_some(),
                    "total" => totals.remove((s(1), s(2), s(3)))?.is_some(),
                    "user" => stored_users.remove(s(1))?.is_some(),
                    _ => false,
                };
                report.replaced += usize::from(removed);
            }
            imported.retain(|_, _| false)?;
        }

        // Settings namespaces.
        let mut skipped: Vec<(String, usize)> = Vec::new();
        let mut moonraker_metadata: Vec<(String, Value)> = Vec::new();
        for row in file.table_rows("namespace_store")? {
            let (Some(ns), Some(key)) = (row["namespace"].as_str(), row["key"].as_str()) else { continue };
            let value = cell_bytes(&row["value"])
                .context("a namespace_store value isn't a blob")
                .and_then(|b| decode_moonraker_record(&b))
                .with_context(|| format!("decoding {ns}.{key}"))?;
            if SKIPPED_NAMESPACES.contains(&ns) {
                match skipped.iter_mut().find(|(n, _)| n == ns) {
                    Some((_, count)) => *count += 1,
                    None => skipped.push((ns.to_owned(), 1)),
                }
                if ns == METADATA_NAMESPACE {
                    moonraker_metadata.push((key.to_owned(), value));
                }
                continue;
            }
            let old = namespaces.get((ns, key))?.map(|v| serde_json::from_slice::<Value>(v.value())).transpose()?;
            if report.settings.count(old.as_ref(), &value) {
                namespaces.insert((ns, key), serde_json::to_vec(&value)?.as_slice())?;
            }
            imported.insert(json!(["ns", ns, key]).to_string().as_str(), ())?;
        }
        skipped.sort();
        report.skipped_namespaces = skipped;

        // Print history.
        let jobs: Vec<(String, u64, Value)> =
            file.table_rows("job_history")?.into_iter().map(history_row).collect::<Result<_>>()?;
        let was_imported = |imported: &redb::Table<&str, ()>, inst: &str, id: u64| -> Result<bool> {
            Ok(imported.get(json!(["job", inst, id]).to_string().as_str())?.is_some())
        };
        let instances: BTreeSet<&str> = jobs.iter().map(|(inst, _, _)| inst.as_str()).collect();
        for inst in instances {
            let source_ids: BTreeSet<u64> = jobs.iter().filter(|(i, _, _)| i == inst).map(|(_, id, _)| *id).collect();
            let mut conflicts = Vec::new();
            for (i, id, row) in jobs.iter().filter(|(i, _, _)| i == inst) {
                if let Some(existing) = history.get((i.as_str(), *id))? {
                    let existing: Value = serde_json::from_slice(existing.value())?;
                    if existing != *row && !was_imported(&imported, inst, *id)? {
                        conflicts.push((*id, existing));
                    }
                }
            }
            let store_max =
                history.range((inst, 0)..=(inst, u64::MAX))?.next_back().transpose()?.map_or(0, |(k, _)| k.value().1);
            let mut next = store_max.max(source_ids.last().copied().unwrap_or(0)) + 1;
            for (old_id, row) in conflicts {
                history.remove((inst, old_id))?;
                history.insert((inst, next), serde_json::to_vec(&row)?.as_slice())?;
                if inst == database::INSTANCE {
                    relink_metadata(&mut local, &job_uid(old_id), &job_uid(next))?;
                }
                report.moved_jobs.push((old_id, next));
                next += 1;
            }
        }
        for (inst, id, row) in &jobs {
            let old =
                history.get((inst.as_str(), *id))?.map(|v| serde_json::from_slice::<Value>(v.value())).transpose()?;
            if report.history.count(old.as_ref(), row) {
                history.insert((inst.as_str(), *id), serde_json::to_vec(row)?.as_slice())?;
            }
            imported.insert(json!(["job", inst, id]).to_string().as_str(), ())?;
        }

        // History totals.
        for row in file.table_rows("job_totals")? {
            let (Some(provider), Some(field), Some(inst)) =
                (row["provider"].as_str(), row["field"].as_str(), row["instance_id"].as_str())
            else {
                continue;
            };
            let value = json!([row["maximum"], row["total"]]);
            let old =
                totals.get((provider, field, inst))?.map(|v| serde_json::from_slice::<Value>(v.value())).transpose()?;
            if report.totals.count(old.as_ref(), &value) {
                totals.insert((provider, field, inst), serde_json::to_vec(&value)?.as_slice())?;
            }
            imported.insert(json!(["total", provider, field, inst]).to_string().as_str(), ())?;
        }

        // Links from files to their last print.
        for (path, md) in moonraker_metadata {
            report.linked_files += usize::from(link_metadata(&mut local, &path, &md)?);
        }

        // Moonraker keeps its API key as the password of `_API_KEY_USER_`.
        let users = file.table_rows("authorized_users")?;
        for row in users.iter().filter(|u| u["username"] != json!(API_KEY_USER)) {
            let name = row["username"].as_str().unwrap_or_default().to_owned();
            let user = match crate::users::Users::from_moonraker(row) {
                Ok(user) => user,
                Err(e) => {
                    report.skipped_users.push((name, format!("{e:#}")));
                    continue;
                }
            };
            let mark = json!(["user", name]).to_string();
            if stored_users.get(name.as_str())?.is_some() {
                if imported.get(mark.as_str())?.is_some() {
                    report.users.unchanged += 1;
                    report.user_names.push(name);
                } else {
                    report.kept_users.push(name);
                }
                continue;
            }
            if user.source == "ldap" {
                report.ldap_users.push(name.clone());
            }
            stored_users.insert(name.as_str(), serde_json::to_string(&user)?.as_str())?;
            imported.insert(mark.as_str(), ())?;
            report.users.added += 1;
            report.user_names.push(name);
        }
        if let Some(key) =
            users.iter().find(|u| u["username"] == json!(API_KEY_USER)).and_then(|u| u["password"].as_str())
        {
            tx.open_table(crate::keys::CREDENTIALS)?.insert(crate::keys::MOONRAKER_KEY, key)?;
            report.api_key = true;
        }

        if !options.dry_run {
            let mut imports = tx.open_table(IMPORTS)?;
            let run = imports.last()?.map_or(1, |(k, _)| k.value() + 1);
            let mut entry = serde_json::to_value(&report)?;
            entry["time"] = json!(crate::database::timestamp());
            imports.insert(run, serde_json::to_vec(&entry)?.as_slice())?;
        }
    }
    if options.dry_run {
        tx.abort()?;
    } else {
        tx.commit()?;
    }
    Ok(report)
}

/// A `job_history` row as orrery stores it: `(instance_id, job_id, the other columns)`.
fn history_row(row: Map<String, Value>) -> Result<(String, u64, Value)> {
    let id = row["job_id"].as_u64().context("a job_history row has no job_id")?;
    let inst = row["instance_id"].as_str().unwrap_or(database::INSTANCE).to_owned();
    let json_column = |name: &str, default: Value| -> Result<Value> {
        match cell_bytes(&row[name]) {
            Some(b) => serde_json::from_slice(&b).with_context(|| format!("job {id}: {name} isn't JSON")),
            None => Ok(default),
        }
    };
    let value = json!({
        "user": row["user"],
        "filename": row["filename"],
        "status": row["status"],
        "start_time": row["start_time"],
        "end_time": row["end_time"],
        "print_duration": row["print_duration"],
        "total_duration": row["total_duration"],
        "filament_used": row["filament_used"],
        "metadata": json_column("metadata", json!({}))?,
        "auxiliary_data": json_column("auxiliary_data", json!([]))?,
    });
    Ok((inst, id, value))
}

fn job_uid(id: u64) -> String {
    format!("{id:06X}")
}

fn metadata_entry(local: &redb::Table<(&str, &str), &[u8]>, path: &str) -> Result<Option<Map<String, Value>>> {
    Ok(match local.get((METADATA_NAMESPACE, path))? {
        Some(v) => serde_json::from_slice::<Value>(v.value())?.as_object().cloned(),
        None => None,
    })
}

/// Points files that named a moved job at its new id.
fn relink_metadata(local: &mut redb::Table<(&str, &str), &[u8]>, old: &str, new: &str) -> Result<()> {
    let mut changed = Vec::new();
    for entry in local.range((METADATA_NAMESPACE, "")..)? {
        let (k, v) = entry?;
        let (ns, path) = k.value();
        if ns != METADATA_NAMESPACE {
            break;
        }
        let mut md: Value = serde_json::from_slice(v.value())?;
        if md["job_id"] == old {
            md["job_id"] = json!(new);
            changed.push((path.to_owned(), md));
        }
    }
    for (path, md) in changed {
        local.insert((METADATA_NAMESPACE, path.as_str()), serde_json::to_vec(&md)?.as_slice())?;
    }
    Ok(())
}

/// Copies Moonraker's `job_id` and `print_start_time` onto orrery's metadata for the same file,
/// when the file hasn't changed and orrery has no newer print of it. Returns whether it did.
fn link_metadata(local: &mut redb::Table<(&str, &str), &[u8]>, path: &str, moonraker: &Value) -> Result<bool> {
    let (Some(job_id), Some(start)) = (moonraker["job_id"].as_str(), moonraker["print_start_time"].as_f64()) else {
        return Ok(false);
    };
    let Some(mut md) = metadata_entry(local, path)? else { return Ok(false) };
    let same_file = md.get("size") == moonraker.get("size")
        && match (md.get("modified").and_then(Value::as_f64), moonraker["modified"].as_f64()) {
            (Some(a), Some(b)) => (a - b).abs() <= 1e-9 * a.abs().max(b.abs()),
            _ => false,
        };
    let newer_here = md.get("print_start_time").and_then(Value::as_f64).is_some_and(|t| t >= start);
    if !same_file || newer_here || md.get("job_id").and_then(Value::as_str) == Some(job_id) {
        return Ok(false);
    }
    md.insert("print_start_time".into(), json!(start));
    md.insert("job_id".into(), json!(job_id));
    local.insert((METADATA_NAMESPACE, path), serde_json::to_vec(&md)?.as_slice())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::server::Paths;

    fn seed() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/fixtures/import/seed.db")
    }

    fn store(dir: &Path) -> Database {
        let paths = Paths { data: dir.to_path_buf(), config_file: dir.join("moonraker.conf"), log_file: None };
        Database::open(&Config::default(), &paths).unwrap()
    }

    #[test]
    fn imported_users_keep_their_passwords() {
        use crate::users::{Users, check_password};
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        // orrery's own bob stays, and Moonraker's bob isn't imported.
        let config = Config::default();
        let users = Users::new(&config, |_| false, &db).unwrap();
        users.store(&db, Users::new_local("bob", crate::users::hash_password("mine").unwrap())).unwrap();
        drop(users);
        let report = run(&db, &seed(), Options::default()).unwrap();
        assert_eq!(report.users, Counts { added: 1, updated: 0, unchanged: 0 });
        assert_eq!((report.user_names, report.kept_users), (vec!["alice".to_owned()], vec!["bob".to_owned()]));

        let users = Users::new(&config, |_| false, &db).unwrap();
        let alice = users.get("alice").unwrap();
        assert_eq!(alice.source, "local");
        assert_eq!(check_password("secret1", alice.password.as_deref().unwrap()), (true, true));
        // Moonraker's signing key stays behind, so alice logs in again for new tokens.
        assert_eq!((alice.jwt_secret, alice.jwk_id), (None, None));
        assert!(check_password("mine", users.get("bob").unwrap().password.as_deref().unwrap()).0);

        // Again: alice is left as orrery has her, since orrery may have changed her since.
        let again = run(&db, &seed(), Options::default()).unwrap();
        assert_eq!(again.users, Counts { added: 0, updated: 0, unchanged: 1 });
        // --replace removes only what imports wrote, so orrery's bob stays.
        let replaced = run(&db, &seed(), Options { replace: true, ..Options::default() }).unwrap();
        assert_eq!((replaced.users.added, replaced.kept_users), (1, vec!["bob".to_owned()]));
    }

    #[test]
    fn imports_seed_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let report = run(&db, &seed(), Options::default()).unwrap();
        assert_eq!(report.settings, Counts { added: 9, updated: 0, unchanged: 0 });
        assert_eq!(report.history, Counts { added: 2, updated: 0, unchanged: 0 });
        assert_eq!(report.totals.added, 6);
        assert_eq!(report.users, Counts { added: 2, updated: 0, unchanged: 0 });
        assert_eq!(report.user_names, ["alice", "bob"]);
        assert!(report.api_key);
        use redb::ReadableDatabase;
        let tx = db.raw().begin_read().unwrap();
        let table = tx.open_table(crate::keys::CREDENTIALS).unwrap();
        let key = table.get(crate::keys::MOONRAKER_KEY).unwrap().unwrap();
        assert_eq!(key.value(), "fc3fdeddc5d741b0842ee77747e5a15c");
        assert_eq!(
            report.skipped_namespaces,
            [("database".to_owned(), 3), ("gcode_metadata".to_owned(), 2), ("moonraker".to_owned(), 4)]
        );
        assert_eq!(db.get_item("seed", Some(&["list".to_owned()])).unwrap(), json!([1, "a", null, {"b": 2}]));
        assert_eq!(db.get_item("seed", Some(&["string".to_owned()])).unwrap(), json!("hello, wörld"));
        assert_eq!(db.get_item("seed", Some(&["float".to_owned()])).unwrap(), json!(1.5));
        let job = db.history_get(1).unwrap().unwrap();
        assert_eq!((job["filename"].as_str(), job["total_duration"].as_f64()), (Some("a.gcode"), Some(120.5)));
        assert_eq!(job["auxiliary_data"], json!([]));
        assert_eq!(db.history_next_id().unwrap(), 3);

        // The same import again changes nothing.
        let again = run(&db, &seed(), Options::default()).unwrap();
        assert_eq!(again.settings, Counts { added: 0, updated: 0, unchanged: 9 });
        assert_eq!(again.history, Counts { added: 0, updated: 0, unchanged: 2 });
        assert_eq!(again.totals, Counts { added: 0, updated: 0, unchanged: 6 });
        assert!(again.moved_jobs.is_empty());
    }

    #[test]
    fn keeps_orrerys_own_jobs_and_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let own = json!({"user": "_TRUSTED_USER_", "filename": "melody.gcode", "status": "completed"});
        db.history_put(1, &own).unwrap();
        db.local_put(METADATA_NAMESPACE, "melody.gcode", &json!({"size": 1, "job_id": "000001"})).unwrap();

        let dry = run(&db, &seed(), Options { dry_run: true, replace: false }).unwrap();
        assert_eq!(dry.moved_jobs, [(1, 3)]);
        assert_eq!(db.history_get(1).unwrap().unwrap(), own);
        assert!(db.get_item("seed", None).is_err());

        let report = run(&db, &seed(), Options::default()).unwrap();
        assert_eq!(report.moved_jobs, [(1, 3)]);
        assert_eq!(db.history_get(3).unwrap().unwrap(), own);
        assert_eq!(db.history_get(1).unwrap().unwrap()["filename"], "a.gcode");
        let md = db.local_items(METADATA_NAMESPACE).unwrap();
        assert_eq!(md[0].1["job_id"], "000003");

        // --replace removes the imported rows, then imports them again; orrery's job stays.
        let replaced = run(&db, &seed(), Options { dry_run: false, replace: true }).unwrap();
        // 17 rows, and the 2 users.
        assert_eq!(replaced.replaced, 19);
        assert_eq!(replaced.history.added, 2);
        assert!(replaced.moved_jobs.is_empty());
        assert_eq!(db.history_get(3).unwrap().unwrap(), own);
    }

    #[test]
    fn links_files_to_their_last_print() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let mut file = SqliteFile::open(&seed()).unwrap().table_rows("namespace_store").unwrap();
        file.retain(|r| r["namespace"] == "gcode_metadata" && r["key"] == "a.gcode");
        let md = decode_moonraker_record(&cell_bytes(&file[0]["value"]).unwrap()).unwrap();
        let mut ours = md.as_object().unwrap().clone();
        ours.remove("job_id");
        ours.remove("print_start_time");
        db.local_put(METADATA_NAMESPACE, "a.gcode", &Value::Object(ours)).unwrap();
        let report = run(&db, &seed(), Options::default()).unwrap();
        assert_eq!(report.linked_files, 1);
        assert_eq!(db.local_items(METADATA_NAMESPACE).unwrap()[0].1["job_id"], md["job_id"]);
    }
}
