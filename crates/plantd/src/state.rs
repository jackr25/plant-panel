//! In-memory state shared by the scheduler and the HTTP thread.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

use plant_icd::api::{DayStats, Health, PlantSummary, Status};
use plant_icd::text::slug;
use serde::{Deserialize, Serialize};

use crate::analysis::detect::{Detection, Detector};
use crate::analysis::{ANALYSIS_FILE, AnalysisFile, PlantAnalysis, Sample, forecast};
use crate::config::Config;
use crate::store::Store;

pub const REGISTRY_FILE: &str = "sensors.json";
const DAY_S: i64 = 86_400;
const STALE_FLOOR_S: i64 = 2 * 3600;

pub fn read(l: &RwLock<State>) -> RwLockReadGuard<'_, State> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

pub fn write(l: &RwLock<State>) -> RwLockWriteGuard<'_, State> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

/// A node as remembered across restarts (`sensors.json`).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RegistryEntry {
    pub id: String,
    pub hostname: String,
    pub addr: String,
}

pub struct Sensor {
    pub id: String,
    pub hostname: String,
    pub addr: String,
    pub key: String,
    pub name: String,
    pub dry_pct: f32,
    pub last_sample_t: Option<i64>,
    pub moisture: Option<f32>,
    pub rssi: Option<i8>,
    pub last_uptime: Option<u64>,
    pub failures: u32,
    pub offline: bool,
    pub last_watered: Option<i64>,
    /// Last 24 h of stored samples, for day stats.
    pub day: VecDeque<Sample>,
    pub detector: Detector,
}

impl Sensor {
    /// Records a new stored sample and runs live detection on it.
    pub fn ingest(&mut self, s: Sample) -> Vec<Detection> {
        while self.day.front().is_some_and(|f| f.t <= s.t - DAY_S) {
            self.day.pop_front();
        }
        self.day.push_back(s);
        self.last_sample_t = Some(s.t);
        self.moisture = Some(s.pct);
        let found = self.detector.step(s, self.dry_pct);
        if found
            .iter()
            .any(|d| matches!(d, Detection::Watering { .. }))
        {
            self.last_watered = Some(s.t);
        }
        found
    }

    fn day_stats(&self) -> Option<DayStats> {
        let n = self.day.len();
        if n == 0 {
            return None;
        }
        let (mut min, mut max, mut sum) = (f32::MAX, f32::MIN, 0.0f64);
        for s in &self.day {
            min = min.min(s.pct);
            max = max.max(s.pct);
            sum += f64::from(s.pct);
        }
        Some(DayStats {
            min_pct: min,
            mean_pct: (sum / n as f64) as f32,
            max_pct: max,
            samples: n as u32,
        })
    }
}

pub struct State {
    pub cfg: Arc<Config>,
    /// Keyed by node id.
    pub sensors: BTreeMap<String, Sensor>,
    pub analysis: BTreeMap<String, PlantAnalysis>,
    pub started: Instant,
    pub last_poll: Option<i64>,
    pub last_analysis: Option<i64>,
}

impl State {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self {
            cfg,
            sensors: BTreeMap::new(),
            analysis: BTreeMap::new(),
            started: Instant::now(),
            last_poll: None,
            last_analysis: None,
        }
    }

    /// Reloads known nodes and the last analysis, and replays the last day of samples
    /// so live detection and day stats pick up where they left off.
    pub fn restore(&mut self, store: &Store, now: i64) {
        if let Some(file) = store.load_json::<AnalysisFile>(ANALYSIS_FILE) {
            self.last_analysis = Some(file.generated);
            self.analysis = file.plants;
        }
        let replay_from = now - DAY_S;
        for e in store
            .load_json::<Vec<RegistryEntry>>(REGISTRY_FILE)
            .unwrap_or_default()
        {
            let records = store
                .load_samples(&e.id, replay_from, now)
                .unwrap_or_else(|err| {
                    log::warn!("{}: cannot replay samples: {err}", e.id);
                    Vec::new()
                });
            let sensor = self.ensure_sensor(&e.id, &e.hostname, &e.addr);
            for r in records {
                sensor.ingest(Sample {
                    t: i64::from(r.t),
                    pct: r.moisture_pct(),
                });
            }
        }
        for (id, a) in &self.analysis {
            if let Some(s) = self.sensors.get_mut(id) {
                s.last_watered = s.last_watered.max(a.last_watered);
            }
        }
        log::info!("restored {} sensors", self.sensors.len());
    }

    /// Returns the sensor for `id`, creating it (named from the config, or the
    /// hostname) if it is new.
    pub fn ensure_sensor(&mut self, id: &str, hostname: &str, addr: &str) -> &mut Sensor {
        if !self.sensors.contains_key(id) {
            let plant = self.cfg.plant(id);
            let name = plant
                .and_then(|p| p.name.clone())
                .unwrap_or_else(|| hostname.to_owned());
            let mut key = slug(&name);
            if self.sensors.values().any(|s| s.key == key) {
                key = format!("{key}_{}", &id[id.len().saturating_sub(6)..]);
            }
            log::info!("new sensor {id} ({hostname} at {addr}) as plant '{key}'");
            self.sensors.insert(
                id.to_owned(),
                Sensor {
                    id: id.to_owned(),
                    hostname: hostname.to_owned(),
                    addr: addr.to_owned(),
                    key,
                    name,
                    dry_pct: plant
                        .and_then(|p| p.dry_pct)
                        .unwrap_or(self.cfg.default_dry_pct),
                    last_sample_t: None,
                    moisture: None,
                    rssi: None,
                    last_uptime: None,
                    failures: 0,
                    offline: false,
                    last_watered: None,
                    day: VecDeque::new(),
                    detector: Detector::new(self.cfg.detect.clone()),
                },
            );
        }
        self.sensors.get_mut(id).expect("inserted above")
    }

    pub fn registry(&self) -> Vec<RegistryEntry> {
        self.sensors
            .values()
            .map(|s| RegistryEntry {
                id: s.id.clone(),
                hostname: s.hostname.clone(),
                addr: s.addr.clone(),
            })
            .collect()
    }

    /// Finds a sensor by plant key or node id.
    pub fn find(&self, key: &str) -> Option<&Sensor> {
        self.sensors
            .get(key)
            .or_else(|| self.sensors.values().find(|s| s.key == key))
    }

    pub fn summary(&self, s: &Sensor, now: i64) -> PlantSummary {
        let analysis = self.analysis.get(&s.id);
        let mut forecast = analysis.and_then(|a| a.forecast.clone());
        let mut next_watering = None;
        if let (Some(pct), Some(t)) = (s.moisture, s.last_sample_t) {
            if let Some(f) = forecast.as_mut() {
                f.rate_pct_per_day = forecast::rate_at(f, pct);
                next_watering =
                    forecast::days_until(f, pct, s.dry_pct).map(|d| t + (d * 86_400.0) as i64);
            } else if pct < s.dry_pct {
                next_watering = Some(t);
            }
        }

        let soon_s = (self.cfg.analysis.soon_hours * 3600.0) as i64;
        // Also judge by data age, for summaries built without live polling (`plantd analyze`).
        // Generous floor: a node may sample less often than we poll (its max is 1 h).
        let stale_s = (i64::from(self.cfg.offline_after_polls)
            * i64::from(self.cfg.poll_interval_s))
        .max(STALE_FLOOR_S);
        let stale = s.last_sample_t.is_some_and(|t| now - t > stale_s);
        let status = match (s.moisture, next_watering) {
            (None, _) => Status::NoData,
            _ if s.offline || stale => Status::Offline,
            (Some(pct), _) if pct < s.dry_pct => Status::WaterNow,
            (_, Some(at)) if at - now <= soon_s => Status::WaterSoon,
            _ => Status::Ok,
        };

        PlantSummary {
            key: s.key.clone(),
            name: s.name.clone(),
            sensor_id: s.id.clone(),
            hostname: s.hostname.clone(),
            status,
            moisture_pct: s.moisture,
            last_seen: s.last_sample_t,
            rssi: s.rssi,
            dry_threshold_pct: s.dry_pct,
            last_watered: s.last_watered,
            next_watering,
            forecast,
            day: s.day_stats(),
            history: analysis.and_then(|a| a.history.clone()),
        }
    }

    /// All plants, ordered by key.
    pub fn summaries(&self, now: i64) -> Vec<PlantSummary> {
        let mut out: Vec<_> = self
            .sensors
            .values()
            .map(|s| self.summary(s, now))
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    pub fn health(&self) -> Health {
        Health {
            api: plant_icd::API_VERSION,
            version: env!("CARGO_PKG_VERSION").into(),
            uptime_s: self.started.elapsed().as_secs(),
            plants: self.sensors.len() as u32,
            poll_interval_s: self.cfg.poll_interval_s,
            last_poll: self.last_poll,
            last_analysis: self.last_analysis,
        }
    }
}
