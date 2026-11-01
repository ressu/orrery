//! orrery's store, on redb, and the `server.database.*` endpoints, after Moonraker's
//! `database.py`.
//!
//! Namespaces hold top-level keys with JSON values. A dotted key (`a.b.c`) or a list key reaches
//! into a value's objects. The `moonraker` namespace can be read but not written by clients, and
//! the `database` namespace can't be reached at all.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Request, RequestType, Transports, handler, python_type};
use crate::config::Config;
use crate::server::{App, Paths};

/// `(namespace, key) -> JSON`.
const NAMESPACES: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("namespaces");
/// orrery's own data, like gcode metadata, which the API doesn't reach. `(namespace, key) -> JSON`.
const LOCAL: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("local");

pub struct Database {
    db: redb::Database,
    path: std::path::PathBuf,
    /// Namespaces with at least one key, plus the registered ones.
    namespaces: Mutex<BTreeSet<String>>,
    protected: BTreeSet<String>,
    forbidden: BTreeSet<String>,
    /// Held while a write or a backup runs, so a backup copies a consistent file.
    write_lock: tokio::sync::Mutex<()>,
}

impl Database {
    pub fn open(config: &Config, paths: &Paths) -> Result<Self> {
        let section = config.section("database");
        let dir = match section.get_opt_str("database_path") {
            Some(p) => Path::new(&p).to_path_buf(),
            None => paths.database_dir(),
        };
        section.get_bool("enable_database_debug", false)?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("orrery.redb");
        let db = redb::Database::create(&path).with_context(|| format!("opening {}", path.display()))?;
        let tx = db.begin_write()?;
        tx.open_table(NAMESPACES)?;
        tx.open_table(LOCAL)?;
        tx.commit()?;
        let mut namespaces: BTreeSet<String> = ["moonraker".to_owned(), "database".to_owned()].into();
        {
            let tx = db.begin_read()?;
            let table = tx.open_table(NAMESPACES)?;
            for entry in table.iter()? {
                let (k, _) = entry?;
                namespaces.insert(k.value().0.to_owned());
            }
        }
        let this = Self {
            db,
            path,
            namespaces: Mutex::new(namespaces),
            protected: ["moonraker".to_owned()].into(),
            forbidden: ["database".to_owned()].into(),
            write_lock: tokio::sync::Mutex::new(()),
        };
        if this.get_record("moonraker", "instance_id")?.is_none() {
            this.put_record("moonraker", "instance_id", &json!("default"))?;
        }
        Ok(this)
    }

    fn get_record(&self, namespace: &str, key: &str) -> Result<Option<Value>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(NAMESPACES)?;
        Ok(match table.get((namespace, key))? {
            Some(v) => Some(serde_json::from_slice(v.value())?),
            None => None,
        })
    }

    fn put_record(&self, namespace: &str, key: &str, value: &Value) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(NAMESPACES)?.insert((namespace, key), serde_json::to_vec(value)?.as_slice())?;
        tx.commit()?;
        self.namespaces.lock().expect("namespaces lock").insert(namespace.to_owned());
        Ok(())
    }

    fn delete_record(&self, namespace: &str, key: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(NAMESPACES)?.remove((namespace, key))?;
        tx.commit()?;
        Ok(())
    }

    fn namespace_items(&self, namespace: &str) -> Result<Map<String, Value>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(NAMESPACES)?;
        let mut out = Map::new();
        for entry in table.range((namespace, "")..)? {
            let (k, v) = entry?;
            let (ns, key) = k.value();
            if ns != namespace {
                break;
            }
            out.insert(key.to_owned(), serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    /// Forgets a namespace whose last key was deleted, unless it's registered.
    fn drop_if_empty(&self, namespace: &str) -> Result<()> {
        if namespace != "moonraker" && namespace != "database" && self.namespace_items(namespace)?.is_empty() {
            self.namespaces.lock().expect("namespaces lock").remove(namespace);
        }
        Ok(())
    }

    pub fn namespaces(&self) -> Vec<String> {
        self.namespaces.lock().expect("namespaces lock").iter().cloned().collect()
    }

    /// Reads a key, or the whole namespace when `key` is `None`.
    pub fn get_item(&self, namespace: &str, key: Option<&[String]>) -> ApiResult {
        let Some(keys) = key else {
            if !self.namespaces.lock().expect("namespaces lock").contains(namespace) {
                return Err(ApiError::not_found(format!("Namespace '{namespace}' not found")));
            }
            return Ok(Value::Object(self.namespace_items(namespace)?));
        };
        let not_found =
            || ApiError::not_found(format!("Key '{}' in namespace '{namespace}' not found", keys.join(".")));
        let record = self.get_record(namespace, &keys[0])?.ok_or_else(not_found)?;
        keys[1..].iter().try_fold(record, |v, k| v.get(k).cloned().ok_or_else(not_found))
    }

    pub fn insert_item(&self, namespace: &str, keys: &[String], value: Value) -> Result<(), ApiError> {
        // Moonraker silently skips storing a null.
        if value.is_null() {
            return Ok(());
        }
        let record = if keys.len() == 1 {
            value
        } else {
            let mut record = match self.get_record(namespace, &keys[0])? {
                Some(v @ Value::Object(_)) => v,
                // A plain value below which a key is written is replaced by an object.
                _ => json!({}),
            };
            let mut item = &mut record;
            for k in &keys[1..keys.len() - 1] {
                let obj = item.as_object_mut().expect("checked below");
                item = obj.entry(k.clone()).or_insert_with(|| json!({}));
                if !item.is_object() {
                    return Err(ApiError::bad_request(format!(
                        "Item at key '{}' in namespace '{namespace}'is not a dictionary object, cannot insert",
                        keys[..keys.len() - 1].join(".")
                    )));
                }
            }
            item.as_object_mut().expect("checked above").insert(keys[keys.len() - 1].clone(), value);
            record
        };
        self.put_record(namespace, &keys[0], &record)?;
        Ok(())
    }

    pub fn delete_item(&self, namespace: &str, keys: &[String]) -> ApiResult {
        let not_found =
            || ApiError::not_found(format!("Key '{}' in namespace '{namespace}' not found", keys.join(".")));
        let mut record = self.get_record(namespace, &keys[0])?.ok_or_else(not_found)?;
        if keys.len() == 1 {
            self.delete_record(namespace, &keys[0])?;
            self.drop_if_empty(namespace)?;
            return Ok(record);
        }
        let mut item = &mut record;
        for k in &keys[1..keys.len() - 1] {
            item = item.get_mut(k).ok_or_else(not_found)?;
        }
        let removed = item.as_object_mut().and_then(|o| o.remove(&keys[keys.len() - 1])).ok_or_else(not_found)?;
        if record.as_object().is_some_and(Map::is_empty) {
            self.delete_record(namespace, &keys[0])?;
            self.drop_if_empty(namespace)?;
        } else {
            self.put_record(namespace, &keys[0], &record)?;
        }
        Ok(removed)
    }

    /// Every key in one of orrery's own namespaces.
    pub fn local_items(&self, namespace: &str) -> Result<Vec<(String, Value)>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(LOCAL)?;
        let mut out = Vec::new();
        for entry in table.range((namespace, "")..)? {
            let (k, v) = entry?;
            let (ns, key) = k.value();
            if ns != namespace {
                break;
            }
            out.push((key.to_owned(), serde_json::from_slice(v.value())?));
        }
        Ok(out)
    }

    pub fn local_put(&self, namespace: &str, key: &str, value: &Value) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(LOCAL)?.insert((namespace, key), serde_json::to_vec(value)?.as_slice())?;
        tx.commit()?;
        Ok(())
    }

    pub fn local_delete(&self, namespace: &str, keys: &[String]) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(LOCAL)?;
            for k in keys {
                table.remove((namespace, k.as_str()))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Copies the store to `dest`. Writes wait until the copy is done.
    pub async fn backup(&self, dest: &Path) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&self.path, dest).with_context(|| format!("copying the store to {}", dest.display()))?;
        Ok(())
    }
}

/// Splits a key the way Moonraker's `parse_namespace_key` does.
fn parse_key(key: &Value) -> Result<Vec<String>, ApiError> {
    let parts: Vec<String> = match key {
        Value::String(s) => s.split('.').map(str::to_owned).collect(),
        Value::Array(items) => items.iter().map(|i| i.as_str().unwrap_or_default().to_owned()).collect(),
        _ => Vec::new(),
    };
    if parts.is_empty() || parts.iter().any(String::is_empty) {
        let shown = match key {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        return Err(ApiError::bad_request(format!("Invalid Key Format: '{shown}'")));
    }
    Ok(parts)
}

pub fn register_endpoints(app: &Arc<App>) {
    let mut r = app.registry.write().expect("registry lock");
    r.register(
        "/server/database/item",
        &[RequestType::Get, RequestType::Post, RequestType::Delete],
        Transports::All,
        handler(item_request),
    );
    r.register("/server/database/list", &[RequestType::Get], Transports::All, handler(list_request));
    r.register(
        "/server/database/backup",
        &[RequestType::Post, RequestType::Delete],
        Transports::All,
        handler(backup_request),
    );
}

async fn item_request(app: Arc<App>, req: Request) -> ApiResult {
    let db = &app.database;
    let namespace = req.args.str("namespace")?;
    if db.forbidden.contains(&namespace) {
        return Err(ApiError::new(403, format!("Read/Write access to namespace '{namespace}' is forbidden")));
    }
    let key = req.args.get("key").cloned().unwrap_or(Value::Null);
    let value = match req.kind {
        RequestType::Get => {
            let keys = match &key {
                Value::Null => None,
                Value::String(_) | Value::Array(_) => Some(parse_key(&key)?),
                other => {
                    return Err(ApiError::bad_request(format!(
                        "Value for argument 'key' is an invalid type: {}",
                        python_type(other)
                    )));
                }
            };
            db.get_item(&namespace, keys.as_deref())?
        }
        kind => {
            if db.protected.contains(&namespace) {
                return Err(ApiError::new(403, format!("Write access to namespace '{namespace}' is forbidden")));
            }
            let key = req.args.require("key")?.clone();
            if !matches!(key, Value::String(_) | Value::Array(_)) {
                return Err(ApiError::bad_request(format!(
                    "Value for argument 'key' is an invalid type: {}",
                    python_type(&key)
                )));
            }
            let keys = parse_key(&key)?;
            let _guard = db.write_lock.lock().await;
            if kind == RequestType::Post {
                let value = req.args.require("value")?.clone();
                db.insert_item(&namespace, &keys, value.clone())?;
                value
            } else {
                db.delete_item(&namespace, &keys)?
            }
        }
    };
    Ok(json!({"namespace": namespace, "key": key, "value": value}))
}

async fn list_request(app: Arc<App>, _req: Request) -> ApiResult {
    let db = &app.database;
    let namespaces: Vec<String> = db.namespaces().into_iter().filter(|n| !db.forbidden.contains(n)).collect();
    let dir = app.paths.backup_dir();
    let mut backups: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    backups.sort();
    Ok(json!({"namespaces": namespaces, "backups": backups}))
}

async fn backup_request(app: Arc<App>, req: Request) -> ApiResult {
    let dir = app.paths.backup_dir();
    let name = match req.kind {
        RequestType::Post => match req.args.opt_str("filename")? {
            Some(n) => n,
            None => format!("orrery-backup-{}.redb", timestamp()),
        },
        _ => req.args.str("filename")?,
    };
    let path = dir.join(&name);
    let inside = Path::new(&name).components().all(|c| matches!(c, std::path::Component::Normal(_)));
    if !inside || name.is_empty() {
        return Err(ApiError::bad_request(format!("Invalid name {name}.")));
    }
    if req.kind == RequestType::Post {
        app.database.backup(&path).await?;
    } else {
        if !path.is_file() {
            return Err(ApiError::not_found(format!("Backup file {name} does not exist")));
        }
        std::fs::remove_file(&path).map_err(|e| ApiError::new(500, e.to_string()))?;
    }
    Ok(json!({"backup_path": path.to_string_lossy()}))
}

/// `YYYYmmdd-HHMMSS` in UTC.
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86400, secs % 86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &Path) -> Database {
        let config = Config::from_str("[server]\n").unwrap();
        let paths = Paths { data: dir.to_path_buf(), config_file: dir.join("moonraker.conf") };
        Database::open(&config, &paths).unwrap()
    }

    fn keys(s: &str) -> Vec<String> {
        parse_key(&json!(s)).unwrap()
    }

    #[test]
    fn nested_keys() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path());
        db.insert_item("t", &keys("a.b.c"), json!(1)).unwrap();
        assert_eq!(db.get_item("t", Some(&keys("a"))).unwrap(), json!({"b": {"c": 1}}));
        db.insert_item("t", &keys("n"), json!(5)).unwrap();
        db.insert_item("t", &keys("n.x"), json!(2)).unwrap();
        assert_eq!(db.get_item("t", Some(&keys("n"))).unwrap(), json!({"x": 2}));
        assert_eq!(db.delete_item("t", &keys("a.b")).unwrap(), json!({"c": 1}));
        assert_eq!(db.get_item("t", Some(&keys("a"))).unwrap_err().status, 404);
        db.delete_item("t", &keys("n")).unwrap();
        assert!(!db.namespaces().contains(&"t".to_owned()));
    }

    #[test]
    fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        open(dir.path()).insert_item("t", &keys("k"), json!([1, "two"])).unwrap();
        let db = open(dir.path());
        assert_eq!(db.get_item("t", None).unwrap(), json!({"k": [1, "two"]}));
    }

    #[test]
    fn bad_keys() {
        assert!(parse_key(&json!("a..b")).is_err());
        assert!(parse_key(&json!("")).is_err());
        assert_eq!(parse_key(&json!(["a", "b"])).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn timestamp_shape() {
        assert_eq!(timestamp().len(), 15);
    }
}
