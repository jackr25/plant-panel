//! JSON bodies served by `plantd` under [`crate::API_PREFIX`].
//!
//! Conventions: times are unix seconds (UTC) as integers, moisture is percent as a
//! float, absent values are `null`. Clients must ignore unknown fields.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// `GET /api/v1/health`
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Health {
    pub api: u32,
    pub version: String,
    pub uptime_s: u64,
    pub plants: u32,
    pub poll_interval_s: u32,
    pub last_poll: Option<i64>,
    pub last_analysis: Option<i64>,
}

/// What the plant needs right now. Ordered from least to most urgent after `Ok`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    WaterSoon,
    WaterNow,
    Offline,
    NoData,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::WaterSoon => "water_soon",
            Status::WaterNow => "water_now",
            Status::Offline => "offline",
            Status::NoData => "no_data",
        }
    }
}

/// Drydown model fitted by the daily analysis.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Model {
    /// `m(t) = m0 - k*t`, `k` in %/day.
    Linear,
    /// `m(t) = floor + (m0 - floor) * exp(-k*t)`, `k` in 1/day.
    Exponential,
}

impl Model {
    pub fn as_str(self) -> &'static str {
        match self {
            Model::Linear => "linear",
            Model::Exponential => "exponential",
        }
    }
}

/// `GET /api/v1/plants` returns `[PlantSummary]`; `GET /api/v1/plants/{key}` one of them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct PlantSummary {
    /// URL-safe slug of `name`, unique per installation. Use it in routes.
    pub key: String,
    pub name: String,
    pub sensor_id: String,
    pub hostname: String,
    pub status: Status,
    pub moisture_pct: Option<f32>,
    /// When the latest stored sample was taken.
    pub last_seen: Option<i64>,
    pub rssi: Option<i8>,
    /// Water when moisture falls below this.
    pub dry_threshold_pct: f32,
    pub last_watered: Option<i64>,
    /// Projected time moisture reaches `dry_threshold_pct`. Past or now if already below.
    pub next_watering: Option<i64>,
    pub forecast: Option<Forecast>,
    /// Last 24 h.
    pub day: Option<DayStats>,
    pub history: Option<History>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Forecast {
    pub model: Model,
    /// Model parameter: %/day for `linear`, 1/day for `exponential`.
    pub k_per_day: f32,
    /// Asymptote for `exponential` (ignored for `linear`).
    pub floor_pct: f32,
    /// Drying rate at the current moisture, %/day (positive = drying).
    pub rate_pct_per_day: f32,
    /// 0..1, from fit quality and how much drydown data backed it.
    pub confidence: f32,
    pub points: u32,
    pub fitted_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DayStats {
    pub min_pct: f32,
    pub mean_pct: f32,
    pub max_pct: f32,
    pub samples: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct History {
    pub horizon_days: u32,
    pub waterings: u32,
    pub median_interval_days: Option<f32>,
}

/// `GET /api/v1/plants/{key}/samples?format=json`: columnar to keep it small.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Series {
    pub sensor_id: String,
    /// Bucket width when downsampled with `step=`, else 0.
    pub step_s: u32,
    pub t: Vec<i64>,
    pub pct: Vec<f32>,
}

/// `GET /api/v1/report.json`: the daily report. `report.txt` renders the same data.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Report {
    pub api: u32,
    pub generated: i64,
    pub horizon_days: u32,
    pub plants: Vec<PlantSummary>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Moisture rose sharply.
    Watering,
    /// Moisture fell sharply (probe pulled, disturbed, or a fault).
    RapidDrop,
    /// Moisture crossed below the plant's dry threshold.
    Dry,
    Offline,
    Online,
    NodeReboot,
}

impl EventKind {
    pub const ALL: [EventKind; 6] = [
        EventKind::Watering,
        EventKind::RapidDrop,
        EventKind::Dry,
        EventKind::Offline,
        EventKind::Online,
        EventKind::NodeReboot,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Watering => "watering",
            EventKind::RapidDrop => "rapid_drop",
            EventKind::Dry => "dry",
            EventKind::Offline => "offline",
            EventKind::Online => "online",
            EventKind::NodeReboot => "node_reboot",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// `GET /api/v1/events` returns `[Event]`; `format=txt` returns event lines (see [`crate::text`]).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Event {
    pub t: i64,
    pub kind: EventKind,
    pub plant: String,
    pub sensor_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct: Option<f32>,
}
