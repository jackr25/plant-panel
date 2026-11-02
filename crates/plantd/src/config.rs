use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::analysis::detect::DetectParams;

/// `/etc/plant-panel/plantd.toml`. Every field has a default, so an empty file is valid.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub data_dir: PathBuf,
    /// HTTP API bind address for front ends.
    pub listen: String,
    /// Polls run on wall-clock multiples of this (300 -> :00, :05, :10 ...).
    pub poll_interval_s: u32,
    pub poll_timeout_ms: u64,
    /// Max nodes polled at once.
    pub poll_concurrency: usize,
    /// Consecutive failed polls before a plant is reported offline.
    pub offline_after_polls: u32,
    /// Raw samples older than this are deleted (whole months at a time).
    pub retention_days: u32,
    /// If set, nodes whose own sample interval differs are reconfigured to this.
    pub node_sample_interval_s: Option<u32>,
    /// Dry threshold for plants that don't set their own.
    pub default_dry_pct: f32,
    pub discovery: Discovery,
    pub detect: DetectParams,
    pub analysis: Analysis,
    #[serde(rename = "plant")]
    pub plants: Vec<PlantConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Discovery {
    pub mdns: bool,
    pub service: String,
    /// `host:port` entries polled whether or not mDNS finds them.
    pub static_hosts: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Analysis {
    /// Local time of day for the daily analysis, `HH:MM`.
    pub time: String,
    /// Days of history the daily analysis reads.
    pub horizon_days: u32,
    /// Hours after a watering excluded from drydown fits (drainage / redistribution).
    pub settle_hours: f32,
    /// Asymptote for the exponential drydown model.
    pub floor_pct: f32,
    /// `water_soon` when the projected watering is within this many hours.
    pub soon_hours: f32,
    /// Minimum samples in a drydown segment for it to count.
    pub min_segment_points: usize,
    pub reports_keep_days: u32,
}

/// Names and thresholds for a sensor, matched by the node's `device.id`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlantConfig {
    pub sensor_id: String,
    pub name: Option<String>,
    pub dry_pct: Option<f32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/var/lib/plant-panel"),
            listen: "0.0.0.0:8080".into(),
            poll_interval_s: 300,
            poll_timeout_ms: 3000,
            poll_concurrency: 8,
            offline_after_polls: 3,
            retention_days: 730,
            node_sample_interval_s: None,
            default_dry_pct: 30.0,
            discovery: Discovery::default(),
            detect: DetectParams::default(),
            analysis: Analysis::default(),
            plants: Vec::new(),
        }
    }
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            mdns: true,
            service: plant_icd::node::MDNS_SERVICE.into(),
            static_hosts: Vec::new(),
        }
    }
}

impl Default for Analysis {
    fn default() -> Self {
        Self {
            time: "03:30".into(),
            horizon_days: 21,
            settle_hours: 3.0,
            floor_pct: 0.0,
            soon_hours: 24.0,
            min_segment_points: 12,
            reports_keep_days: 90,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::warn!("{} not found, using defaults", path.display());
                String::new()
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        if !(10..=86_400).contains(&self.poll_interval_s) {
            return Err("poll_interval_s must be 10..=86400".into());
        }
        self.analysis_time()?;
        for p in &self.plants {
            if !crate::store::valid_id(&p.sensor_id) {
                return Err(format!(
                    "plant sensor_id {:?} is not a node id",
                    p.sensor_id
                ));
            }
        }
        Ok(())
    }

    /// `analysis.time` as (hour, minute).
    pub fn analysis_time(&self) -> Result<(u32, u32), String> {
        let bad = || format!("analysis.time {:?} must be HH:MM", self.analysis.time);
        let (h, m) = self.analysis.time.split_once(':').ok_or_else(bad)?;
        let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
        if h > 23 || m > 59 {
            return Err(bad());
        }
        Ok((h, m))
    }

    pub fn plant(&self, sensor_id: &str) -> Option<&PlantConfig> {
        self.plants.iter().find(|p| p.sensor_id == sensor_id)
    }
}
