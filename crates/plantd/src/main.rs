//! plantd: polls soil nodes, stores a lean sample history, detects watering events,
//! forecasts the next watering, and serves it all to front ends.
//!
//! Threads: the scheduler (this one, asleep between ticks), the HTTP server, and the
//! mDNS browser. Polls spawn short-lived scoped threads, one per node.

mod analysis;
mod config;
mod discovery;
mod http;
mod logger;
mod node_client;
mod poller;
mod report;
mod state;
mod store;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};

use config::Config;
use discovery::Discovery;
use state::State;
use store::Store;

const DEFAULT_CONFIG: &str = "/etc/plant-panel/plantd.toml";
/// The Pi has no RTC: until NTP syncs, the clock may read 1970 or the last shutdown time.
const MIN_SANE_UNIX: i64 = 1_767_225_600; // 2026-01-01
/// Re-check the clock at least this often, so NTP steps and suspend don't skew the schedule.
const MAX_SLEEP_S: i64 = 60;

pub enum Command {
    Analyze,
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

const USAGE: &str = "usage: plantd [--config PATH] [run | analyze | check]

  run      start the collector (default)
  analyze  run the daily analysis once against the data dir and print the report
  check    validate the config and exit

The config path defaults to $PLANTD_CONFIG or /etc/plant-panel/plantd.toml.
Log level: PLANTD_LOG=error|warn|info|debug|trace.";

fn main() -> ExitCode {
    logger::init();
    let mut config_path = std::env::var_os("PLANTD_CONFIG")
        .map_or_else(|| PathBuf::from(DEFAULT_CONFIG), PathBuf::from);
    let mut command = String::from("run");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => match args.next() {
                Some(p) => config_path = p.into(),
                None => return usage(),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "run" | "analyze" | "check" => command = a,
            _ => return usage(),
        }
    }

    let result = Config::load(&config_path).and_then(|cfg| match command.as_str() {
        "check" => {
            println!(
                "{}: ok ({} plants configured)",
                config_path.display(),
                cfg.plants.len()
            );
            Ok(())
        }
        "analyze" => analyze_once(cfg),
        _ => run(cfg),
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn open(cfg: Config) -> Result<(Arc<RwLock<State>>, Arc<Store>), String> {
    let store =
        Store::open(&cfg.data_dir).map_err(|e| format!("{}: {e}", cfg.data_dir.display()))?;
    let mut st = State::new(Arc::new(cfg));
    st.restore(&store, unix_now());
    Ok((Arc::new(RwLock::new(st)), Arc::new(store)))
}

fn analyze_once(cfg: Config) -> Result<(), String> {
    let (state, store) = open(cfg)?;
    let report = analysis::run_daily(&state, &store, unix_now()).map_err(|e| e.to_string())?;
    print!("{}", report::render(&report, "daily"));
    Ok(())
}

fn run(cfg: Config) -> Result<(), String> {
    let (hour, minute) = cfg.analysis_time()?;
    let interval = i64::from(cfg.poll_interval_s);
    let (listen, discovery_cfg) = (cfg.listen.clone(), cfg.discovery.clone());
    let (state, store) = open(cfg)?;

    let (tx, rx) = mpsc::channel();
    http::spawn(
        &listen,
        http::Ctx {
            state: state.clone(),
            store: store.clone(),
            commands: tx,
        },
    )
    .map_err(|e| format!("listen {listen}: {e}"))?;
    let discovery = Discovery::start(&discovery_cfg);

    let now = unix_now();
    let stale = state::read(&state)
        .last_analysis
        .is_none_or(|t| now - t > 86_400);
    let mut next_poll = 0;
    let mut next_analysis = if stale {
        0
    } else {
        next_daily(hour, minute, now)
    };
    let mut warned_clock = false;
    log::info!(
        "plantd {} polling every {interval}s, analysis daily at {hour:02}:{minute:02}",
        env!("CARGO_PKG_VERSION")
    );

    loop {
        let now = unix_now();
        if now < MIN_SANE_UNIX {
            if !std::mem::replace(&mut warned_clock, true) {
                log::warn!("system clock not set yet; waiting for time sync");
            }
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        if now >= next_poll {
            poller::poll_cycle(&state, &store, &discovery, now);
            // Align to wall-clock multiples so every node is sampled on the same grid.
            next_poll = (now / interval + 1) * interval;
        }
        if now >= next_analysis {
            if let Err(e) = analysis::run_daily(&state, &store, now) {
                log::error!("analysis failed: {e}");
            }
            next_analysis = next_daily(hour, minute, now);
        }

        let wait = (next_poll.min(next_analysis) - unix_now()).clamp(1, MAX_SLEEP_S);
        match rx.recv_timeout(Duration::from_secs(wait as u64)) {
            Ok(Command::Analyze) => next_analysis = 0,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("HTTP thread exited".into()),
        }
    }
}

/// Next occurrence of local `hour:minute` after `now`. DST-safe: a skipped time
/// resolves to the next day, an ambiguous one to its first occurrence.
fn next_daily(hour: u32, minute: u32, now: i64) -> i64 {
    let mut date = Local
        .timestamp_opt(now, 0)
        .single()
        .map_or_else(|| Local::now().date_naive(), |d| d.date_naive());
    for _ in 0..3 {
        let at = date
            .and_hms_opt(hour, minute, 0)
            .and_then(|n| Local.from_local_datetime(&n).earliest());
        if let Some(t) = at.map(|t| t.timestamp()).filter(|&t| t > now) {
            return t;
        }
        date = date.succ_opt().unwrap_or(date);
    }
    now + 86_400
}
