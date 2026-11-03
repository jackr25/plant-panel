//! In-memory state shared by the scheduler and the HTTP thread.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

use plant_icd::api::{
    DayStats, EventKind, Health, NAME_MAX_CHARS, NOTES_MAX_CHARS, Photo, PlantSummary,
    PlantUpdate, Status,
};
use plant_icd::text::slug;
use serde::{Deserialize, Serialize};

use crate::analysis::detect::{DetectParams, Detection, Detector};
use crate::analysis::{ANALYSIS_FILE, AnalysisFile, PlantAnalysis, Sample, forecast};
use crate::config::Config;
use crate::store::Store;

pub const REGISTRY_FILE: &str = "sensors.json";
pub const PROFILES_FILE: &str = "plants.json";
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
    /// Kept so keys of plants with the same name don't depend on startup order.
    #[serde(default)]
    pub key: Option<String>,
}

/// What front ends have set for a plant (`plants.json`, keyed by sensor id). Unset
/// fields fall back to the `[[plant]]` config entry, then to the defaults.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo: Option<Photo>,
}

impl Profile {
    fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.notes.is_none()
            && self.dry_pct.is_none()
            && self.photo.is_none()
    }
}

/// Trims, normalises line endings and maps empty to `None`. Rejects control characters
/// (other than newlines when `multiline`) so values stay safe in the text grammars.
fn clean_text(v: Option<String>, max: usize, multiline: bool) -> Result<Option<String>, String> {
    let Some(v) = v else { return Ok(None) };
    let v = v.replace("\r\n", "\n");
    let v = v.trim();
    if v.chars().count() > max {
        return Err(format!("must be at most {max} characters"));
    }
    if v.chars()
        .any(|c| c.is_control() && !(multiline && (c == '\n' || c == '\t')))
    {
        return Err("must not contain control characters".into());
    }
    Ok((!v.is_empty()).then(|| v.to_owned()))
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

    /// Records a watering entered by hand at `t`.
    pub fn note_watering(&mut self, t: i64) {
        self.last_watered = self.last_watered.max(Some(t));
        self.detector.note_watering(t);
    }

    /// Forgets everything learnt from samples. Connection state (address, RSSI, uptime,
    /// online/offline) is kept: it describes the node, not the plant's history.
    fn clear_history(&mut self, detect: DetectParams) {
        self.last_sample_t = None;
        self.moisture = None;
        self.last_watered = None;
        self.day.clear();
        self.detector = Detector::new(detect);
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
    /// Keyed by node id. May hold entries for nodes not seen since a restart.
    pub profiles: BTreeMap<String, Profile>,
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
            profiles: BTreeMap::new(),
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
        self.profiles = store.load_json(PROFILES_FILE).unwrap_or_default();
        let replay_from = now - DAY_S;
        let registry = store
            .load_json::<Vec<RegistryEntry>>(REGISTRY_FILE)
            .unwrap_or_default();
        for e in &registry {
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
        // Saved keys were unique; keep them unless the name changed while we were down.
        for e in &registry {
            if let (Some(key), Some(s)) = (&e.key, self.sensors.get_mut(&e.id))
                && key.starts_with(&slug(&s.name))
            {
                s.key.clone_from(key);
            }
        }
        for (id, a) in &self.analysis {
            if let Some(s) = self.sensors.get_mut(id) {
                s.last_watered = s.last_watered.max(a.last_watered);
            }
        }
        // Manual waterings aren't in the samples, so replay doesn't see them.
        let events = store.read_events(replay_from).unwrap_or_else(|err| {
            log::warn!("cannot read events: {err}");
            Vec::new()
        });
        for e in events.iter().filter(|e| e.manual && e.kind == EventKind::Watering) {
            if let Some(s) = self.sensors.get_mut(&e.sensor_id) {
                s.note_watering(e.t);
            }
        }
        log::info!("restored {} sensors", self.sensors.len());
    }

    /// Returns the sensor for `id`, creating it if it is new.
    pub fn ensure_sensor(&mut self, id: &str, hostname: &str, addr: &str) -> &mut Sensor {
        if !self.sensors.contains_key(id) {
            self.sensors.insert(
                id.to_owned(),
                Sensor {
                    id: id.to_owned(),
                    hostname: hostname.to_owned(),
                    addr: addr.to_owned(),
                    key: String::new(),
                    name: String::new(),
                    dry_pct: self.cfg.default_dry_pct,
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
            self.apply_profile(id);
            let s = &self.sensors[id];
            log::info!(
                "new sensor {id} ({hostname} at {addr}) as plant '{}'",
                s.key
            );
        }
        self.sensors.get_mut(id).expect("inserted above")
    }

    /// Recomputes a sensor's name, key and threshold: profile, then config, then defaults.
    /// The key only changes when the name does.
    fn apply_profile(&mut self, id: &str) {
        let (profile, plant) = (self.profiles.get(id), self.cfg.plant(id));
        let Some(s) = self.sensors.get(id) else {
            return;
        };
        let name = profile
            .and_then(|p| p.name.clone())
            .or_else(|| plant.and_then(|p| p.name.clone()))
            .unwrap_or_else(|| s.hostname.clone());
        let dry_pct = profile
            .and_then(|p| p.dry_pct)
            .or_else(|| plant.and_then(|p| p.dry_pct))
            .unwrap_or(self.cfg.default_dry_pct);
        let key = if name == s.name {
            s.key.clone()
        } else {
            let base = slug(&name);
            let taken = |k: &str| self.sensors.values().any(|o| o.id != id && o.key == k);
            if taken(&base) {
                format!("{base}_{}", &id[id.len().saturating_sub(6)..])
            } else {
                base
            }
        };
        let s = self.sensors.get_mut(id).expect("checked above");
        if !s.key.is_empty() && s.key != key {
            log::info!("plant '{}' is now '{key}'", s.key);
        }
        (s.name, s.key, s.dry_pct) = (name, key, dry_pct);
    }

    /// Applies a front-end edit to the plant on sensor `id`. On success the caller
    /// persists [`Self::profiles`].
    pub fn update_plant(&mut self, id: &str, u: PlantUpdate) -> Result<(), String> {
        let mut p = self.profiles.get(id).cloned().unwrap_or_default();
        if let Some(name) = u.name {
            p.name = clean_text(name, NAME_MAX_CHARS, false).map_err(|e| format!("name {e}"))?;
        }
        if let Some(notes) = u.notes {
            p.notes = clean_text(notes, NOTES_MAX_CHARS, true).map_err(|e| format!("notes {e}"))?;
        }
        if let Some(dry) = u.dry_threshold_pct {
            if dry.is_some_and(|d| !(d > 0.0 && d < 100.0)) {
                return Err("dry_threshold_pct must be between 0 and 100".into());
            }
            p.dry_pct = dry;
        }
        self.set_profile(id, p);
        Ok(())
    }

    /// Drops the in-memory history of sensor `id` (live readings, detector, analysis).
    /// The caller deletes the files and persists [`Self::analysis_file`].
    pub fn clear_history(&mut self, id: &str) {
        let detect = self.cfg.detect.clone();
        if let Some(s) = self.sensors.get_mut(id) {
            s.clear_history(detect);
        }
        self.analysis.remove(id);
    }

    pub fn analysis_file(&self) -> AnalysisFile {
        AnalysisFile {
            generated: self.last_analysis.unwrap_or(0),
            plants: self.analysis.clone(),
        }
    }

    pub fn set_photo(&mut self, id: &str, photo: Option<Photo>) {
        let mut p = self.profiles.get(id).cloned().unwrap_or_default();
        p.photo = photo;
        self.set_profile(id, p);
    }

    fn set_profile(&mut self, id: &str, p: Profile) {
        if p.is_empty() {
            self.profiles.remove(id);
        } else {
            self.profiles.insert(id.to_owned(), p);
        }
        self.apply_profile(id);
    }

    pub fn registry(&self) -> Vec<RegistryEntry> {
        self.sensors
            .values()
            .map(|s| RegistryEntry {
                id: s.id.clone(),
                hostname: s.hostname.clone(),
                addr: s.addr.clone(),
                key: Some(s.key.clone()),
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
        let profile = self.profiles.get(&s.id);
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
            notes: profile.and_then(|p| p.notes.clone()),
            photo: profile.and_then(|p| p.photo.clone()),
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
