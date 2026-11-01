//! The operations the spike measures, shaped like Moonraker's database and history use.

use anyhow::Result;
use serde_json::{Value, json};

/// A history job. The fields Moonraker queries on are typed; the rest is a JSON blob.
#[derive(Debug, Clone)]
pub struct Job {
    /// 0 means "not saved yet".
    pub id: u64,
    pub start_time: f64,
    pub end_time: Option<f64>,
    pub status: String,
    pub extra: Value,
}

impl Job {
    pub fn sample(start_time: f64) -> Self {
        Self {
            id: 0,
            start_time,
            end_time: None,
            status: "in_progress".into(),
            extra: json!({
                "user": "_TRUSTED_USER_",
                "filename": "benchy_0.2mm_PLA_MK3S_1h2m.gcode",
                "print_duration": 3700.5,
                "total_duration": 3900.25,
                "filament_used": 4321.5,
                "metadata": {
                    "size": 4_521_000, "slicer": "PrusaSlicer", "slicer_version": "2.7.1",
                    "layer_height": 0.2, "first_layer_height": 0.2, "object_height": 48.0,
                    "filament_total": 4321.5, "estimated_time": 3720, "nozzle_diameter": 0.4,
                    "thumbnails": [
                        {"width": 32, "height": 32, "size": 1800, "relative_path": ".thumbs/benchy-32x32.png"},
                        {"width": 300, "height": 300, "size": 24000, "relative_path": ".thumbs/benchy-300x300.png"}
                    ]
                },
                "auxiliary_data": [],
                "instance_id": "default"
            }),
        }
    }
}

/// `server.history.list` parameters.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub since: Option<f64>,
    pub before: Option<f64>,
    pub limit: usize,
    pub start: usize,
    pub ascending: bool,
}

pub trait Store {
    fn put_item(&mut self, namespace: &str, key: &str, value: &Value) -> Result<()>;
    fn get_item(&self, namespace: &str, key: &str) -> Result<Option<Value>>;
    /// Inserts a job (`id == 0`) or replaces it. Returns its id.
    fn save_job(&mut self, job: &Job) -> Result<u64>;
    /// Returns the number of matching jobs and one page of them.
    fn list_jobs(&self, query: &ListQuery) -> Result<(usize, Vec<Job>)>;
}

fn job_value(job: &Job) -> Value {
    json!({"start_time": job.start_time, "end_time": job.end_time, "status": job.status, "extra": job.extra})
}

fn job_from_value(id: u64, v: Value) -> Job {
    Job {
        id,
        start_time: v["start_time"].as_f64().unwrap_or_default(),
        end_time: v["end_time"].as_f64(),
        status: v["status"].as_str().unwrap_or_default().to_owned(),
        extra: v["extra"].clone(),
    }
}

#[cfg(feature = "redb")]
pub mod redb_store {
    use std::path::Path;

    use anyhow::Result;
    use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
    use serde_json::Value;

    use super::{Job, ListQuery, Store, job_from_value, job_value};

    const NAMESPACES: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("namespaces");
    /// Job id -> JSON. Ids grow with start time, as Moonraker's do, so id order is time order.
    const JOBS: TableDefinition<u64, &[u8]> = TableDefinition::new("jobs");
    /// (start time, job id) -> (). Start times are positive, so their f64 bit patterns sort in
    /// numeric order.
    const JOBS_BY_START: TableDefinition<(u64, u64), ()> = TableDefinition::new("jobs_by_start");

    pub struct RedbStore {
        db: Database,
    }

    impl RedbStore {
        pub fn open(path: &Path) -> Result<Self> {
            let db = Database::create(path)?;
            let tx = db.begin_write()?;
            tx.open_table(NAMESPACES)?;
            tx.open_table(JOBS)?;
            tx.open_table(JOBS_BY_START)?;
            tx.commit()?;
            Ok(Self { db })
        }
    }

    impl Store for RedbStore {
        fn put_item(&mut self, namespace: &str, key: &str, value: &Value) -> Result<()> {
            let tx = self.db.begin_write()?;
            tx.open_table(NAMESPACES)?.insert((namespace, key), serde_json::to_vec(value)?.as_slice())?;
            tx.commit()?;
            Ok(())
        }

        fn get_item(&self, namespace: &str, key: &str) -> Result<Option<Value>> {
            let tx = self.db.begin_read()?;
            let table = tx.open_table(NAMESPACES)?;
            Ok(match table.get((namespace, key))? {
                Some(v) => Some(serde_json::from_slice(v.value())?),
                None => None,
            })
        }

        fn save_job(&mut self, job: &Job) -> Result<u64> {
            let tx = self.db.begin_write()?;
            let id = {
                let mut table = tx.open_table(JOBS)?;
                let id = if job.id == 0 { table.last()?.map_or(1, |(k, _)| k.value() + 1) } else { job.id };
                table.insert(id, serde_json::to_vec(&job_value(job))?.as_slice())?;
                tx.open_table(JOBS_BY_START)?.insert((job.start_time.to_bits(), id), ())?;
                id
            };
            tx.commit()?;
            Ok(id)
        }

        fn list_jobs(&self, q: &ListQuery) -> Result<(usize, Vec<Job>)> {
            let tx = self.db.begin_read()?;
            let table = tx.open_table(JOBS)?;
            if q.since.is_none() && q.before.is_none() {
                // No filter: the count is the table length and only one page is decoded.
                let count = table.len()? as usize;
                let iter = table.iter()?;
                let iter: Box<dyn Iterator<Item = _>> = if q.ascending { Box::new(iter) } else { Box::new(iter.rev()) };
                let mut page = Vec::new();
                for entry in iter.skip(q.start).take(q.limit) {
                    let (k, v) = entry?;
                    page.push(job_from_value(k.value(), serde_json::from_slice(v.value())?));
                }
                return Ok((count, page));
            }
            // A time filter walks the start-time index and decodes only the page.
            let index = tx.open_table(JOBS_BY_START)?;
            let lo = q.since.map_or(0, |s| s.to_bits() + 1);
            let hi = q.before.map_or(u64::MAX, f64::to_bits);
            let range = index.range((lo, 0)..(hi, 0))?;
            let ids: Vec<u64> = range.map(|e| e.map(|(k, _)| k.value().1)).collect::<Result<_, _>>()?;
            let count = ids.len();
            let ordered: Box<dyn Iterator<Item = &u64>> =
                if q.ascending { Box::new(ids.iter()) } else { Box::new(ids.iter().rev()) };
            let mut page = Vec::new();
            for &id in ordered.skip(q.start).take(q.limit) {
                if let Some(v) = table.get(id)? {
                    page.push(job_from_value(id, serde_json::from_slice(v.value())?));
                }
            }
            Ok((count, page))
        }
    }
}

#[cfg(feature = "sqlite")]
pub mod sqlite_store {
    use std::path::Path;

    use anyhow::Result;
    use rusqlite::{Connection, OptionalExtension, params};
    use serde_json::Value;

    use super::{Job, ListQuery, Store};

    pub struct SqliteStore {
        conn: Connection,
    }

    impl SqliteStore {
        /// The same tables Moonraker uses, with its default settings (rollback journal,
        /// synchronous=FULL).
        pub fn open(path: &Path) -> Result<Self> {
            let conn = Connection::open(path)?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS namespace_store (
                    namespace TEXT NOT NULL, key TEXT NOT NULL, value record NOT NULL,
                    PRIMARY KEY (namespace, key));
                 CREATE TABLE IF NOT EXISTS job_history (
                    job_id INTEGER PRIMARY KEY ASC, status TEXT NOT NULL, start_time REAL NOT NULL,
                    end_time REAL, extra TEXT NOT NULL);",
            )?;
            Ok(Self { conn })
        }
    }

    impl Store for SqliteStore {
        fn put_item(&mut self, namespace: &str, key: &str, value: &Value) -> Result<()> {
            self.conn.execute(
                "INSERT OR REPLACE INTO namespace_store VALUES (?1, ?2, ?3)",
                params![namespace, key, serde_json::to_vec(value)?],
            )?;
            Ok(())
        }

        fn get_item(&self, namespace: &str, key: &str) -> Result<Option<Value>> {
            let mut stmt =
                self.conn.prepare_cached("SELECT value FROM namespace_store WHERE namespace = ?1 AND key = ?2")?;
            let bytes: Option<Vec<u8>> = stmt.query_row(params![namespace, key], |r| r.get(0)).optional()?;
            Ok(match bytes {
                Some(b) => Some(serde_json::from_slice(&b)?),
                None => None,
            })
        }

        fn save_job(&mut self, job: &Job) -> Result<u64> {
            let id = if job.id == 0 { None } else { Some(job.id as i64) };
            self.conn.execute(
                "REPLACE INTO job_history VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, job.status, job.start_time, job.end_time, job.extra.to_string()],
            )?;
            Ok(self.conn.last_insert_rowid() as u64)
        }

        fn list_jobs(&self, q: &ListQuery) -> Result<(usize, Vec<Job>)> {
            let filter = "WHERE (?1 IS NULL OR start_time > ?1) AND (?2 IS NULL OR start_time < ?2)";
            let count: i64 = self
                .conn
                .prepare_cached(&format!("SELECT COUNT(*) FROM job_history {filter}"))?
                .query_row(params![q.since, q.before], |r| r.get(0))?;
            let order = if q.ascending { "ASC" } else { "DESC" };
            let mut stmt = self.conn.prepare_cached(&format!(
                "SELECT job_id, status, start_time, end_time, extra FROM job_history {filter}
                 ORDER BY job_id {order} LIMIT ?3 OFFSET ?4"
            ))?;
            let rows = stmt.query_map(params![q.since, q.before, q.limit as i64, q.start as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, String>(4)?))
            })?;
            let mut page = Vec::new();
            for row in rows {
                let (id, status, start_time, end_time, extra) = row?;
                page.push(Job { id: id as u64, status, start_time, end_time, extra: serde_json::from_str(&extra)? });
            }
            Ok((count as usize, page))
        }
    }
}
