//! Long-horizon analysis, run once a day (and on demand). Pure over a sample series,
//! plus [`run_daily`] which loads history, updates shared state and writes reports.

pub mod detect;
pub mod forecast;

use std::collections::BTreeMap;
use std::io;
use std::sync::RwLock;
use std::time::Instant;

use plant_icd::api::{EventKind, Forecast, History, Report};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::report;
use crate::state::{self, State};
use crate::store::Store;
use detect::{Detection, Detector};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub t: i64,
    pub pct: f32,
}

/// Per-plant result of the daily analysis. Persisted in `analysis.json`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PlantAnalysis {
    pub forecast: Option<Forecast>,
    pub history: Option<History>,
    pub last_watered: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct AnalysisFile {
    pub generated: i64,
    pub plants: BTreeMap<String, PlantAnalysis>,
}

pub const ANALYSIS_FILE: &str = "analysis.json";

/// Samples just before a detected watering may already be part of the rise.
const PRE_WATERING_TRIM_S: i64 = 30 * 60;

/// Analyses one plant's history (`samples` sorted by time). `manual` holds the times of
/// waterings entered by hand, sorted; they are fed to the detector just as the live
/// path does, so a watering both entered and sensed counts once.
pub fn analyze(
    samples: &[Sample],
    manual: &[i64],
    now: i64,
    cfg: &Config,
    dry_pct: f32,
) -> PlantAnalysis {
    let mut det = Detector::new(cfg.detect.clone());
    let mut waterings = Vec::new();
    let mut pending = manual.iter().copied().peekable();
    for s in samples {
        while let Some(t) = pending.next_if(|&t| t <= s.t) {
            det.note_watering(t);
            waterings.push(t);
        }
        if det
            .step(*s, dry_pct)
            .iter()
            .any(|d| matches!(d, Detection::Watering { .. }))
        {
            waterings.push(s.t);
        }
    }
    waterings.extend(pending);
    // Sensed shortly before a manual entry for the same watering: keep the first.
    let cooldown = i64::from(cfg.detect.cooldown_min) * 60;
    waterings.sort_unstable();
    waterings.dedup_by(|later, earlier| *later - *earlier < cooldown);

    // Drydown segments: from (previous watering + settle) to just before the next one.
    let settle = (cfg.analysis.settle_hours * 3600.0) as i64;
    let idx = |t: i64| samples.partition_point(|s| s.t < t);
    let mut segments = Vec::new();
    let mut start = 0;
    for &w in &waterings {
        segments.push(&samples[start..idx(w - PRE_WATERING_TRIM_S).max(start)]);
        start = idx(w + settle);
    }
    segments.push(&samples[start.min(samples.len())..]);
    segments.retain(|s| s.len() >= cfg.analysis.min_segment_points);

    let intervals: Vec<f32> = waterings
        .windows(2)
        .map(|w| (w[1] - w[0]) as f32 / 86_400.0)
        .collect();
    PlantAnalysis {
        forecast: forecast::best_forecast(&segments, cfg.analysis.floor_pct, now),
        history: Some(History {
            horizon_days: cfg.analysis.horizon_days,
            waterings: waterings.len() as u32,
            median_interval_days: median(intervals),
        }),
        last_watered: waterings.last().copied(),
    }
}

fn median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let m = v.len() / 2;
    Some(if v.len().is_multiple_of(2) {
        (v[m - 1] + v[m]) / 2.0
    } else {
        v[m]
    })
}

/// Runs the analysis for every known plant, publishes it to `state` and writes the
/// daily report files.
pub fn run_daily(state: &RwLock<State>, store: &Store, now: i64) -> io::Result<Report> {
    let started = Instant::now();
    let (cfg, plants) = {
        let st = state::read(state);
        let plants: Vec<(String, f32)> = st
            .sensors
            .values()
            .map(|s| (s.id.clone(), s.dry_pct))
            .collect();
        (st.cfg.clone(), plants)
    };

    let since = now - i64::from(cfg.analysis.horizon_days) * 86_400;
    let mut manual: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for e in store.read_events(since)? {
        if e.manual && e.kind == EventKind::Watering {
            manual.entry(e.sensor_id).or_default().push(e.t);
        }
    }
    let mut results = BTreeMap::new();
    for (id, dry_pct) in plants {
        let mut manual = manual.remove(&id).unwrap_or_default();
        manual.sort_unstable();
        let samples: Vec<Sample> = store
            .load_samples(&id, since, now)?
            .into_iter()
            .map(|r| Sample {
                t: i64::from(r.t),
                pct: r.moisture_pct(),
            })
            .collect();
        results.insert(id, analyze(&samples, &manual, now, &cfg, dry_pct));
    }

    let report = {
        let mut st = state::write(state);
        for (id, a) in &results {
            if let Some(s) = st.sensors.get_mut(id) {
                s.last_watered = s.last_watered.max(a.last_watered);
            }
        }
        st.analysis = results.clone();
        st.last_analysis = Some(now);
        Report {
            api: plant_icd::API_VERSION,
            generated: now,
            horizon_days: cfg.analysis.horizon_days,
            plants: st.summaries(now),
        }
    };

    store.save_json(
        ANALYSIS_FILE,
        &AnalysisFile {
            generated: now,
            plants: results,
        },
    )?;
    let json = serde_json::to_vec_pretty(&report).map_err(io::Error::other)?;
    store.write_report(
        &report::local_date(now),
        &report::render(&report, "daily"),
        &json,
    )?;
    store.prune(now, cfg.retention_days, cfg.analysis.reports_keep_days)?;

    log::info!(
        "analysis: {} plants in {:?}",
        report.plants.len(),
        started.elapsed()
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plant_icd::api::Model;

    /// Three weeks at 5-minute samples: exponential drydown (k = 0.1/day), watered back
    /// to 60 % whenever it reaches 32 %.
    fn synthetic() -> Vec<Sample> {
        let (mut out, mut pct) = (Vec::new(), 60.0f64);
        for i in 0..(21 * 288) {
            if pct < 32.0 {
                pct = 60.0;
            }
            out.push(Sample {
                t: 1_790_000_000 + i * 300,
                pct: pct as f32,
            });
            pct *= (-0.1f64 * 300.0 / 86_400.0).exp();
        }
        out
    }

    #[test]
    fn end_to_end_on_synthetic_history() {
        let s = synthetic();
        let now = s.last().unwrap().t;
        let a = analyze(&s, &[], now, &Config::default(), 30.0);
        let f = a.forecast.unwrap();
        assert_eq!(f.model, Model::Exponential);
        assert!((f.k_per_day - 0.1).abs() < 0.005, "k = {}", f.k_per_day);
        let h = a.history.unwrap();
        // ln(60/32)/0.1 = 6.3 days per cycle -> 3 waterings in 21 days
        assert_eq!(h.waterings, 3);
        assert!((h.median_interval_days.unwrap() - 6.29).abs() < 0.1);
    }

    #[test]
    fn manual_waterings_merge_with_detected_ones() {
        let s = synthetic();
        let cfg = Config::default();
        let last = s.last().unwrap().t;
        let sensed = analyze(&s, &[], last, &cfg, 30.0).last_watered.unwrap();
        // One the sensor missed (day 2, no rise), one entered 10 minutes before the
        // sensor saw the same watering, and one after the data ends.
        let manual = [s[0].t + 2 * 86_400, sensed - 600, last + 3600];
        let a = analyze(&s, &manual, last + 3600, &cfg, 30.0);
        assert_eq!(a.history.unwrap().waterings, 3 + 2);
        assert_eq!(a.last_watered, Some(last + 3600));
        assert!(a.forecast.is_some());
    }
}
