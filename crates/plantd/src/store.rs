//! On-disk layout under `data_dir`:
//!
//! ```text
//! samples/<sensor_id>/YYYY-MM.bin   8-byte records (plant_icd::record), append-only
//! events.log                        one event line per event (plant_icd::text)
//! sensors.json                      known nodes and their last address
//! analysis.json                     latest daily analysis per sensor
//! plants.json                       names, notes, thresholds and photos set by front ends
//! photos/<sensor_id>                one image per plant, type recorded in plants.json
//! reports/YYYY-MM-DD.txt            daily report, plus latest.txt / latest.json
//! ```
//!
//! One 8-byte write per sample keeps SD-card traffic tiny: 5-minute polling is about
//! 2.3 KB per sensor per day, ~840 KB per sensor per year.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use plant_icd::api::Event;
use plant_icd::record::{MAGIC, SampleRecord};
use plant_icd::text::Civil;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub const LATEST_TXT: &str = "latest.txt";
pub const LATEST_JSON: &str = "latest.json";

/// Node ids come off the network and become directory names.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub struct Store {
    root: PathBuf,
    /// Serialises `events.log` appends against rewrites, which would lose an append
    /// landing between their read and rename.
    events: Mutex<()>,
}

fn month_of(t: i64) -> (i32, u8) {
    let c = Civil::from_unix(t);
    (c.year, c.month)
}

impl Store {
    pub fn open(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root.join("samples"))?;
        fs::create_dir_all(root.join("reports"))?;
        fs::create_dir_all(root.join("photos"))?;
        Ok(Self {
            root: root.to_owned(),
            events: Mutex::new(()),
        })
    }

    fn month_file(&self, id: &str, (y, m): (i32, u8)) -> PathBuf {
        self.root
            .join("samples")
            .join(id)
            .join(format!("{y:04}-{m:02}.bin"))
    }

    pub fn append_sample(&self, id: &str, rec: &SampleRecord) -> io::Result<()> {
        let path = self.month_file(id, month_of(i64::from(rec.t)));
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        if f.metadata()?.len() == 0 {
            let mut buf = [0u8; 16];
            buf[..8].copy_from_slice(&MAGIC);
            buf[8..].copy_from_slice(&rec.encode());
            f.write_all(&buf)
        } else {
            f.write_all(&rec.encode())
        }
    }

    /// Samples with `since <= t <= until`, in time order.
    pub fn load_samples(&self, id: &str, since: i64, until: i64) -> io::Result<Vec<SampleRecord>> {
        let mut out = Vec::new();
        let (mut month, last) = (month_of(since), month_of(until));
        while month <= last {
            match fs::read(self.month_file(id, month)) {
                Ok(bytes) => out.extend(
                    SampleRecord::decode_all(&bytes)
                        .filter(|r| (since..=until).contains(&i64::from(r.t))),
                ),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            month = if month.1 == 12 {
                (month.0 + 1, 1)
            } else {
                (month.0, month.1 + 1)
            };
        }
        Ok(out)
    }

    pub fn append_event(&self, ev: &Event) -> io::Result<()> {
        let mut line = String::new();
        ev.write_line(&mut line).map_err(io::Error::other)?;
        line.push('\n');
        let _guard = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("events.log"))?
            .write_all(line.as_bytes())
    }

    /// Events with `t >= since`, in time order (the log is in the order events were
    /// written, and manual entries can be backdated). Reads the whole log; it grows by
    /// a few lines a day.
    pub fn read_events(&self, since: i64) -> io::Result<Vec<Event>> {
        match fs::read_to_string(self.root.join("events.log")) {
            Ok(text) => {
                let mut evs: Vec<Event> = text
                    .lines()
                    .filter_map(Event::parse_line)
                    .filter(|e| e.t >= since)
                    .collect();
                evs.sort_by_key(|e| e.t); // stable: same-second events keep log order
                Ok(evs)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Removes every event of sensor `id` from the log; returns how many. Lines that
    /// don't parse are kept.
    pub fn delete_events(&self, id: &str) -> io::Result<usize> {
        let _guard = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        let text = match fs::read_to_string(self.root.join("events.log")) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut kept = String::with_capacity(text.len());
        let mut removed = 0;
        for line in text.lines() {
            if Event::parse_line(line).is_some_and(|e| e.sensor_id == id) {
                removed += 1;
            } else {
                kept.push_str(line);
                kept.push('\n');
            }
        }
        if removed > 0 {
            self.write_atomic(Path::new("events.log"), kept.as_bytes())?;
        }
        Ok(removed)
    }

    /// Deletes every stored sample of sensor `id`.
    pub fn delete_samples(&self, id: &str) -> io::Result<()> {
        match fs::remove_dir_all(self.root.join("samples").join(id)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Atomic write: temp file + rename, so a power cut never leaves half a file.
    fn write_atomic(&self, rel: &Path, data: &[u8]) -> io::Result<()> {
        let path = self.root.join(rel);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, data)?;
        fs::rename(tmp, path)
    }

    pub fn save_json<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        let data = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        self.write_atomic(Path::new(name), &data)
    }

    pub fn load_json<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        let bytes = fs::read(self.root.join(name)).ok()?;
        serde_json::from_slice(&bytes)
            .inspect_err(|e| log::warn!("ignoring unreadable {name}: {e}"))
            .ok()
    }

    pub fn write_report(&self, date: &str, txt: &str, json: &[u8]) -> io::Result<()> {
        let reports = Path::new("reports");
        self.write_atomic(&reports.join(format!("{date}.txt")), txt.as_bytes())?;
        self.write_atomic(&reports.join(LATEST_TXT), txt.as_bytes())?;
        self.write_atomic(&reports.join(LATEST_JSON), json)
    }

    pub fn read_report(&self, name: &str) -> io::Result<Vec<u8>> {
        fs::read(self.root.join("reports").join(name))
    }

    fn photo_path(id: &str) -> PathBuf {
        Path::new("photos").join(id)
    }

    pub fn write_photo(&self, id: &str, data: &[u8]) -> io::Result<()> {
        self.write_atomic(&Self::photo_path(id), data)
    }

    pub fn read_photo(&self, id: &str) -> io::Result<Vec<u8>> {
        fs::read(self.root.join(Self::photo_path(id)))
    }

    pub fn delete_photo(&self, id: &str) -> io::Result<()> {
        match fs::remove_file(self.root.join(Self::photo_path(id))) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Deletes sample months entirely older than `retention_days` and dated reports
    /// older than `reports_keep_days`.
    pub fn prune(&self, now: i64, retention_days: u32, reports_keep_days: u32) -> io::Result<()> {
        let (y, m) = month_of(now - i64::from(retention_days) * 86_400);
        let cutoff_month = format!("{y:04}-{m:02}.bin");
        for dir in fs::read_dir(self.root.join("samples"))? {
            for file in fs::read_dir(dir?.path())? {
                let file = file?;
                let name = file.file_name();
                if name
                    .to_str()
                    .is_some_and(|n| n.len() == cutoff_month.len() && *n < *cutoff_month)
                {
                    log::info!("pruning {}", file.path().display());
                    fs::remove_file(file.path())?;
                }
            }
        }

        let c = Civil::from_unix(now - i64::from(reports_keep_days) * 86_400);
        let cutoff_day = format!("{:04}-{:02}-{:02}.txt", c.year, c.month, c.day);
        for file in fs::read_dir(self.root.join("reports"))? {
            let file = file?;
            let name = file.file_name();
            let dated = name
                .to_str()
                .filter(|n| n.len() == cutoff_day.len() && n.as_bytes()[0].is_ascii_digit());
            if dated.is_some_and(|n| *n < *cutoff_day) {
                fs::remove_file(file.path())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_roundtrip_across_months() {
        let dir = std::env::temp_dir().join(format!("plantd-store-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        // 2026-10-31T23:55:00Z and 2026-11-01T00:00:00Z
        let a = SampleRecord::new(1_793_491_200 - 300, 41.0, -60, 0);
        let b = SampleRecord::new(1_793_491_200, 40.9, -60, 0);
        store.append_sample("abc123", &a).unwrap();
        store.append_sample("abc123", &b).unwrap();
        assert!(dir.join("samples/abc123/2026-10.bin").exists());
        assert!(dir.join("samples/abc123/2026-11.bin").exists());
        assert_eq!(
            store
                .load_samples("abc123", 0, i64::from(u32::MAX))
                .unwrap(),
            vec![a, b]
        );
        assert_eq!(
            store
                .load_samples("abc123", 1_793_491_200, 1_793_491_200)
                .unwrap(),
            vec![b]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deletes_one_sensors_history() {
        let dir = std::env::temp_dir().join(format!("plantd-store-del-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let ev = |id: &str, t| Event {
            t,
            kind: plant_icd::api::EventKind::Watering,
            plant: "p".into(),
            sensor_id: id.into(),
            from_pct: None,
            pct: None,
            manual: false,
        };
        for e in [ev("aaa", 1), ev("bbb", 2), ev("aaa", 3)] {
            store.append_event(&e).unwrap();
        }
        store
            .append_sample("aaa", &SampleRecord::new(1_793_491_200, 40.0, -60, 0))
            .unwrap();
        assert_eq!(store.delete_events("aaa").unwrap(), 2);
        assert_eq!(store.read_events(0).unwrap(), vec![ev("bbb", 2)]);
        store.delete_samples("aaa").unwrap();
        store.delete_samples("aaa").unwrap(); // already gone is fine
        assert!(store.load_samples("aaa", 0, i64::from(u32::MAX)).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ids() {
        assert!(valid_id("a1b2c3d4e5f6"));
        assert!(!valid_id("../etc"));
        assert!(!valid_id(""));
    }
}
