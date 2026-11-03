//! JSON bodies served by `plantd` under [`crate::API_PREFIX`].
//!
//! Conventions: times are unix seconds (UTC) as integers, moisture is percent as a
//! float, absent values are `null`. Clients must ignore unknown fields.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Deserializer, Serialize};

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
    /// Free text about the plant, set from a front end. May contain newlines.
    #[serde(default)]
    pub notes: Option<String>,
    /// Fetch from `GET /api/v1/plants/{key}/photo`. `updated` changes on every upload,
    /// so clients can cache by it.
    #[serde(default)]
    pub photo: Option<Photo>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Photo {
    /// `image/jpeg`, `image/png` or `image/webp`.
    pub content_type: String,
    pub bytes: u32,
    pub updated: i64,
}

/// Longest accepted plant name, in characters.
pub const NAME_MAX_CHARS: usize = 64;
/// Longest accepted notes, in characters.
pub const NOTES_MAX_CHARS: usize = 4000;
/// Largest accepted photo upload. Clients should downscale first (1024 px JPEG is plenty).
pub const PHOTO_MAX_BYTES: usize = 4 << 20;

/// `PATCH /api/v1/plants/{key}`. An omitted field is left alone; `null` (or an empty
/// string) resets it to the collector's configured default.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlantUpdate {
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub notes: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub dry_threshold_pct: Option<Option<f32>>,
}

/// Tells a present `null` (`Some(None)`) apart from an absent field (`None`).
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
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
    /// Entered by hand from a front end rather than detected. Only `watering` events
    /// are manual today. Omitted when false.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub manual: bool,
}

/// `POST /api/v1/plants/{key}/waterings`: records a watering the sensor missed (or
/// that happened before it was installed). An empty body means "now".
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WateringEntry {
    /// When it was watered, unix seconds. Defaults to now; must not be in the future.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plant_update_tells_null_from_absent() {
        let u: PlantUpdate =
            serde_json::from_str(r#"{"name": "Fern", "dry_threshold_pct": null}"#).unwrap();
        assert_eq!(u.name, Some(Some("Fern".into())));
        assert_eq!(u.notes, None);
        assert_eq!(u.dry_threshold_pct, Some(None));
        assert_eq!(
            serde_json::to_string(&u).unwrap(),
            r#"{"name":"Fern","dry_threshold_pct":null}"#
        );
        assert!(serde_json::from_str::<PlantUpdate>(r#"{"nmae": "x"}"#).is_err());
    }

    #[test]
    fn manual_flag_is_omitted_when_false() {
        let ev = Event {
            t: 1,
            kind: EventKind::Watering,
            plant: "fern".into(),
            sensor_id: "abc".into(),
            from_pct: None,
            pct: None,
            manual: false,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("manual"));
        let manual = Event { manual: true, ..ev };
        let json = serde_json::to_string(&manual).unwrap();
        assert!(json.ends_with(r#""manual":true}"#));
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), manual);
    }
}
