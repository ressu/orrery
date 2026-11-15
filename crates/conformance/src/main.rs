use std::collections::{BTreeMap, HashMap};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use conformance::case::{self, Case, Transport};
use conformance::client::Route;
use conformance::runner::{RunResult, Settings, run_case};
use conformance::snapshot;
use conformance::target::{Paths, TargetKind};
use tokio::sync::Semaphore;

#[derive(Parser)]
#[command(about = "Run the Moonraker conformance cases against a server")]
struct Cli {
    /// Server to test.
    #[arg(long, value_enum, default_value = "reference")]
    target: TargetKind,
    /// Only run cases whose id (`<area>/<name>`) contains this text.
    #[arg(long)]
    filter: Option<String>,
    /// Only run this transport.
    #[arg(long, value_enum)]
    transport: Option<TransportArg>,
    /// Write response-shape snapshots (reference target only).
    #[arg(long)]
    record: bool,
    /// Cases to run at once.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    /// Keep the data directories of failed runs under conformance/.work/runs.
    #[arg(long)]
    keep: bool,
    /// Keep the data directories of every run, for example to copy a database a case built.
    #[arg(long)]
    keep_all: bool,
    /// Run the `real-klippy` cases against real Klipper instead of the fake. Their response
    /// shapes are also checked against the snapshots, which were recorded with the fake.
    #[arg(long)]
    real_klipper: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum TransportArg {
    Ws,
    Http,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("conformance: {e:#}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli) -> Result<bool> {
    if cli.record && (cli.target != TargetKind::Reference || cli.real_klipper) {
        bail!("--record only works with --target reference and the fake Klippy");
    }
    let paths = Paths::discover();
    let routes_path = paths.conformance().join("http_routes.json");
    let routes: HashMap<String, Route> = serde_json::from_str(
        &std::fs::read_to_string(&routes_path).with_context(|| format!("reading {}", routes_path.display()))?,
    )?;
    let cases: Vec<Case> = case::load_all(&paths.conformance().join("cases"))?
        .into_iter()
        .filter(|c| cli.filter.as_ref().is_none_or(|f| c.id.contains(f.as_str())))
        .filter(|c| !cli.real_klipper || c.tags.iter().any(|t| t == "real-klippy"))
        .collect();
    if cases.is_empty() {
        bail!("no cases match");
    }

    let settings = Arc::new(Settings {
        target: cli.target,
        paths: paths.clone(),
        routes,
        keep_failed: cli.keep,
        keep_all: cli.keep_all,
        real_klipper: cli.real_klipper,
    });
    let jobs = Arc::new(Semaphore::new(cli.jobs.max(1)));
    let mut handles = Vec::new();
    for case in &cases {
        for &transport in &case.transports {
            if let Some(only) = cli.transport {
                let wanted = match only {
                    TransportArg::Ws => Transport::Ws,
                    TransportArg::Http => Transport::Http,
                };
                if transport != wanted {
                    continue;
                }
            }
            let (settings, jobs, case) = (settings.clone(), jobs.clone(), case.clone());
            handles.push(tokio::spawn(async move {
                let _permit = jobs.acquire_owned().await.expect("semaphore is never closed");
                run_case(&settings, &case, transport).await
            }));
        }
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await?);
    }

    let by_id: HashMap<&str, &Case> = cases.iter().map(|c| (c.id.as_str(), c)).collect();
    for r in &mut results {
        let case = by_id[r.case_id.as_str()];
        let shape = snapshot::shape_of(&r.transcript, &case.ignore);
        if cli.record {
            if r.passed() {
                snapshot::write(&paths, case, r.transport, &shape)?;
            }
        } else if (cli.target == TargetKind::Orrery || cli.real_klipper)
            && r.passed()
            && let Some(recorded) = snapshot::read(&paths, case, r.transport)?
        {
            r.failures.extend(snapshot::diff(&recorded, &shape).into_iter().map(|d| format!("snapshot: {d}")));
        }
    }
    Ok(report(&cli, &cases, &results))
}

/// Prints failures, then a pass/fail matrix per area and transport. Returns whether the run passed.
fn report(cli: &Cli, cases: &[Case], results: &[RunResult]) -> bool {
    let by_id: HashMap<&str, &Case> = cases.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut ok = true;
    let mut matrix: BTreeMap<String, BTreeMap<Transport, (usize, usize)>> = BTreeMap::new();
    let mut sorted: Vec<&RunResult> = results.iter().collect();
    sorted.sort_by(|a, b| (&a.case_id, a.transport).cmp(&(&b.case_id, b.transport)));
    for r in &sorted {
        let case = by_id[r.case_id.as_str()];
        let area = r.case_id.split('/').next().unwrap_or_default().to_owned();
        let cell = matrix.entry(area).or_default().entry(r.transport).or_default();
        cell.1 += 1;
        if r.passed() {
            cell.0 += 1;
        }
        let status = match (&case.known_regression, r.passed()) {
            (None, true) => {
                if let Some(kept) = &r.kept {
                    println!("pass: {} [{}]\n    data dir kept: {kept}", r.case_id, r.transport.as_str());
                }
                continue;
            }
            (Some(id), true) => format!("PASS (known regression {id} no longer fails; remove the mark)"),
            (Some(id), false) => format!("known regression {id}"),
            (None, false) => {
                ok = false;
                "FAIL".to_owned()
            }
        };
        println!("{status}: {} [{}]", r.case_id, r.transport.as_str());
        for f in &r.failures {
            println!("    {f}");
        }
        if let Some(kept) = &r.kept {
            println!("    data dir kept: {kept}");
        }
    }
    let klippy = if cli.real_klipper { "real Klipper" } else { "fake Klippy" };
    let mode = if cli.record { ", recording snapshots" } else { "" };
    println!("\n{} target, {klippy}{mode}", cli.target.as_str());
    println!("{:<20} {:>9} {:>9}", "area", "ws", "http");
    for (area, cells) in &matrix {
        let cell = |t| cells.get(&t).map(|(p, n)| format!("{p}/{n}")).unwrap_or_else(|| "-".into());
        println!("{:<20} {:>9} {:>9}", area, cell(Transport::Ws), cell(Transport::Http));
    }
    let passed = results.iter().filter(|r| r.passed()).count();
    println!("{passed}/{} runs passed", results.len());
    ok
}
