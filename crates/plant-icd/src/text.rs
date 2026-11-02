//! Line-oriented text grammars. Both are designed for `grep`, simple regexes and LLM
//! context windows: one fact per line, no nesting, no quoting.
//!
//! **Event line** (`events.log`, `GET /api/v1/events?format=txt`):
//! ```text
//! 2026-11-02T14:30:05Z t=1793629805 event=watering plant=monstera sensor=a1b2c3d4e5f6 from_pct=31.2 pct=58.0
//! ```
//! An RFC 3339 UTC time, then space-separated `key=value` pairs. Values never contain spaces.
//! Regex: `^(\S+) t=(\d+) event=(\w+) plant=(\w+) sensor=(\w+)`
//!
//! **Report line** (`report.txt`, `live.txt`):
//! ```text
//! plant.monstera.next_watering: 2026-11-05T09:00:00Z
//! ```
//! `<scope>.<key>: <value>`, where scope is `report` or `plant.<plant key>`. Lines
//! starting with `#` and blank lines are comments. Missing values are written `unknown`.
//! Regex: `^plant\.(\w+)\.(\w+): (.*)$`

use alloc::string::String;
use core::fmt;

use crate::api::{Event, EventKind};

pub const REPORT_HEADER: &str = "# plant-panel report v1";
pub const UNKNOWN: &str = "unknown";

/// Broken-down UTC time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Civil {
    /// Converts unix seconds to a UTC calendar date (proleptic Gregorian).
    pub fn from_unix(t: i64) -> Self {
        // Howard Hinnant's civil_from_days.
        let days = t.div_euclid(86_400);
        let secs = t.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self {
            year: year as i32,
            month: month as u8,
            day: day as u8,
            hour: (secs / 3_600) as u8,
            minute: (secs / 60 % 60) as u8,
            second: (secs % 60) as u8,
        }
    }
}

/// Displays unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
#[derive(Clone, Copy, Debug)]
pub struct Rfc3339(pub i64);

impl fmt::Display for Rfc3339 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = Civil::from_unix(self.0);
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            c.year, c.month, c.day, c.hour, c.minute, c.second
        )
    }
}

/// Lowercase `[a-z0-9_]` slug used for plant keys. Never empty.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("plant");
    }
    out
}

impl Event {
    pub fn write_line(&self, out: &mut impl fmt::Write) -> fmt::Result {
        write!(
            out,
            "{} t={} event={} plant={} sensor={}",
            Rfc3339(self.t),
            self.t,
            self.kind.as_str(),
            self.plant,
            self.sensor_id
        )?;
        if let Some(v) = self.from_pct {
            write!(out, " from_pct={v:.1}")?;
        }
        if let Some(v) = self.pct {
            write!(out, " pct={v:.1}")?;
        }
        Ok(())
    }

    /// Parses an event line. Unknown keys are ignored; required keys are `t`, `event`,
    /// `plant` and `sensor`.
    pub fn parse_line(line: &str) -> Option<Self> {
        let mut words = line.split_ascii_whitespace();
        words.next()?; // RFC 3339 time; `t=` is authoritative
        let (mut t, mut kind, mut plant, mut sensor) = (None, None, None, None);
        let (mut from_pct, mut pct) = (None, None);
        for w in words {
            let (k, v) = w.split_once('=')?;
            match k {
                "t" => t = v.parse().ok(),
                "event" => kind = EventKind::parse(v),
                "plant" => plant = Some(v),
                "sensor" => sensor = Some(v),
                "from_pct" => from_pct = v.parse().ok(),
                "pct" => pct = v.parse().ok(),
                _ => {}
            }
        }
        Some(Event {
            t: t?,
            kind: kind?,
            plant: plant?.into(),
            sensor_id: sensor?.into(),
            from_pct,
            pct,
        })
    }
}

/// Splits a report line into `(scope, key, value)`. For `plant.monstera.status: ok`
/// that is `("plant.monstera", "status", "ok")`. Returns `None` for comments and blanks.
pub fn parse_report_line(line: &str) -> Option<(&str, &str, &str)> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (path, value) = line.split_once(": ")?;
    let (scope, key) = path.rsplit_once('.')?;
    Some((scope, key, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn rfc3339() {
        assert_eq!(Rfc3339(0).to_string(), "1970-01-01T00:00:00Z");
        assert_eq!(Rfc3339(951_782_400).to_string(), "2000-02-29T00:00:00Z");
        assert_eq!(Rfc3339(1_793_629_805).to_string(), "2026-11-02T14:30:05Z");
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Monstera Deliciosa"), "monstera_deliciosa");
        assert_eq!(slug("soil-a1b2c3"), "soil_a1b2c3");
        assert_eq!(slug("  ~~ "), "plant");
    }

    #[test]
    fn event_line_roundtrip() {
        let ev = Event {
            t: 1_793_629_805,
            kind: EventKind::Watering,
            plant: "monstera".into(),
            sensor_id: "a1b2c3d4e5f6".into(),
            from_pct: Some(31.2),
            pct: Some(58.0),
        };
        let mut line = String::new();
        ev.write_line(&mut line).unwrap();
        assert_eq!(
            line,
            "2026-11-02T14:30:05Z t=1793629805 event=watering plant=monstera \
             sensor=a1b2c3d4e5f6 from_pct=31.2 pct=58.0"
        );
        assert_eq!(Event::parse_line(&line), Some(ev));
    }

    #[test]
    fn report_line() {
        assert_eq!(
            parse_report_line("plant.monstera.next_watering: 2026-11-05T09:00:00Z"),
            Some(("plant.monstera", "next_watering", "2026-11-05T09:00:00Z"))
        );
        assert_eq!(parse_report_line("# comment"), None);
    }
}
