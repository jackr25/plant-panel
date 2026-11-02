//! The soil node's HTTP API (firmware: `soil-sensor`), as far as `plantd` uses it.
//!
//! Unknown fields are ignored, so the node can add fields without breaking us.

use alloc::string::String;
use serde::{Deserialize, Serialize};

/// mDNS service type nodes advertise. TXT records: `id`, `fw`, `api`.
pub const MDNS_SERVICE: &str = "_soilnode._tcp.local.";
pub const STATUS_PATH: &str = "/api/v1/status";
pub const CONFIG_PATH: &str = "/api/v1/config";

/// `GET /api/v1/status`
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Status {
    pub device: Device,
    #[serde(default)]
    pub config: Option<Config>,
    /// `null` until the node's first successful reading.
    #[serde(default)]
    pub reading: Option<Reading>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Device {
    /// 12 hex digits of the station MAC. Stable across reflashes; the primary key.
    pub id: String,
    pub hostname: String,
    #[serde(default)]
    pub fw_version: String,
    /// A decrease means the node rebooted and its in-RAM statistics reset.
    pub uptime_s: u64,
    #[serde(default)]
    pub rssi: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub sample_interval_s: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Reading {
    pub moisture_pct: f32,
    /// Seconds since the node took this reading. Sample time = receive time - age.
    pub age_s: u64,
}
