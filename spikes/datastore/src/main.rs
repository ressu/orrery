//! Runs Moonraker-shaped workloads against each storage engine and prints timings and file sizes.
//!
//! Usage: datastore-spike [<dir>] [--jobs <n>]
//! The databases go in a temp folder inside <dir> (default: the current folder), so the run
//! measures the disk you point it at, for example a Pi's SD card.

mod sqlite_read;
mod store;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::json;

use store::{Job, ListQuery, Store};

struct Args {
    dir: PathBuf,
    jobs: u64,
    read_sqlite: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args { dir: PathBuf::from("."), jobs: 2000, read_sqlite: None };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--jobs" => args.jobs = it.next().unwrap_or_default().parse()?,
            "--read-sqlite" => args.read_sqlite = it.next().map(PathBuf::from),
            _ if a.starts_with('-') => bail!("unknown option {a}"),
            _ => args.dir = PathBuf::from(a),
        }
    }
    Ok(args)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if let Some(path) = &args.read_sqlite {
        return sqlite_read::dump(path);
    }
    let tmp = tempfile::Builder::new().prefix("spike-").tempdir_in(&args.dir)?;
    println!("databases in {}, {} history jobs", tmp.path().display(), args.jobs);
    println!("{:<28} {:>12} {:>12}", "workload", "redb", "sqlite");
    let mut rows: Vec<(String, Vec<String>)> = Vec::new();
    let mut engines: Vec<Box<dyn Fn(&Path) -> Result<Box<dyn Store>>>> = Vec::new();
    #[cfg(feature = "redb")]
    engines.push(Box::new(|p| Ok(Box::new(store::redb_store::RedbStore::open(&p.join("orrery.redb"))?))));
    #[cfg(feature = "sqlite")]
    engines.push(Box::new(|p| Ok(Box::new(store::sqlite_store::SqliteStore::open(&p.join("orrery.sqlite"))?))));
    for (i, open) in engines.iter().enumerate() {
        let dir = tmp.path().join(format!("e{i}"));
        std::fs::create_dir(&dir)?;
        let results = run_workloads(open.as_ref(), &dir, args.jobs)?;
        for (name, value) in results {
            match rows.iter_mut().find(|(n, _)| *n == name) {
                Some((_, values)) => values.push(value),
                None => rows.push((name, vec![value])),
            }
        }
    }
    for (name, values) in rows {
        println!("{name:<28} {:>12} {:>12}", values.first().map_or("-", |v| v), values.get(1).map_or("-", |v| v));
    }
    Ok(())
}

fn timed(results: &mut Vec<(String, String)>, name: &str, count: u64, f: impl FnOnce() -> Result<()>) -> Result<()> {
    let start = Instant::now();
    f()?;
    let elapsed = start.elapsed();
    let per = elapsed.as_secs_f64() * 1e6 / count as f64;
    results.push((name.to_owned(), format!("{per:.0} µs/op")));
    Ok(())
}

fn run_workloads(
    open: &dyn Fn(&Path) -> Result<Box<dyn Store>>,
    dir: &Path,
    jobs: u64,
) -> Result<Vec<(String, String)>> {
    let mut results = Vec::new();
    let mut store = open(dir)?;

    // Frontend settings: Fluidd writes one key per change, each committed on its own.
    let writes = 500;
    timed(&mut results, "namespace write (commit)", writes, || {
        for i in 0..writes {
            let value = json!({"layout": {"dashboard": [i, i + 1, i + 2]}, "theme": {"dark": i % 2 == 0}});
            store.put_item("fluidd", &format!("uiSettings{}", i % 50), &value)?;
        }
        Ok(())
    })?;
    let reads = 5000;
    timed(&mut results, "namespace read", reads, || {
        for i in 0..reads {
            if store.get_item("fluidd", &format!("uiSettings{}", i % 50))?.is_none() {
                bail!("missing key");
            }
        }
        Ok(())
    })?;

    // History: each job is saved when it starts and again when it finishes.
    timed(&mut results, "history save (commit)", jobs * 2, || {
        for i in 0..jobs {
            let mut job = Job::sample(i as f64 * 3600.0);
            job.id = store.save_job(&job)?;
            job.status = "completed".into();
            job.end_time = Some(job.start_time + 1800.0);
            store.save_job(&job)?;
        }
        Ok(())
    })?;
    let queries = 200;
    timed(&mut results, "history list newest 50", queries, || {
        for _ in 0..queries {
            let (count, page) = store.list_jobs(&ListQuery { limit: 50, ..ListQuery::default() })?;
            if count as u64 != jobs || page.len() != 50.min(jobs as usize) {
                bail!("list returned {count} jobs, page {}", page.len());
            }
        }
        Ok(())
    })?;
    timed(&mut results, "history list since/before", queries, || {
        for q in 0..queries {
            let since = (q % 100) as f64 * 3600.0 * 10.0;
            store.list_jobs(&ListQuery {
                since: Some(since),
                before: Some(since + 3600.0 * 100.0),
                limit: 50,
                start: 10,
                ascending: true,
            })?;
        }
        Ok(())
    })?;
    drop(store);

    let start = Instant::now();
    let store = open(dir)?;
    store.list_jobs(&ListQuery { limit: 1, ..ListQuery::default() })?;
    results.push(("reopen + first list".into(), format!("{:.1} ms", start.elapsed().as_secs_f64() * 1e3)));
    drop(store);

    let size: u64 =
        std::fs::read_dir(dir)?.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
    results.push(("file size".into(), format!("{:.1} MiB", size as f64 / 1048576.0)));
    Ok(results)
}
