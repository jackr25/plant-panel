//! Text rendering of a [`Report`] in the report-line grammar (`plant_icd::text`).
//! Readable as-is, `grep`-able by key, and compact enough to paste into an LLM prompt.

use std::fmt::Write;

use chrono::{Local, TimeZone};
use plant_icd::api::{PlantSummary, Report, Status};
use plant_icd::text::{REPORT_HEADER, Rfc3339, UNKNOWN};

/// `Thu 2026-11-05 09:00` in the collector's local time zone.
pub fn local_time(t: i64) -> String {
    Local.timestamp_opt(t, 0).single().map_or_else(
        || UNKNOWN.into(),
        |d| d.format("%a %Y-%m-%d %H:%M").to_string(),
    )
}

/// `2026-11-05` in the collector's local time zone.
pub fn local_date(t: i64) -> String {
    Local.timestamp_opt(t, 0).single().map_or_else(
        || Rfc3339(t).to_string()[..10].into(),
        |d| d.format("%Y-%m-%d").to_string(),
    )
}

pub fn render(r: &Report, kind: &str) -> String {
    let mut o = String::with_capacity(512 + r.plants.len() * 1024);
    let needs_water: Vec<&str> = r
        .plants
        .iter()
        .filter(|p| matches!(p.status, Status::WaterNow | Status::WaterSoon))
        .map(|p| p.key.as_str())
        .collect();

    let _ = writeln!(o, "{REPORT_HEADER}");
    let _ = writeln!(
        o,
        "# Lines are \"<scope>.<key>: <value>\". Times are RFC 3339 UTC; *_local values are"
    );
    let _ = writeln!(
        o,
        "# the collector's local time. Moisture is percent. Missing values read \"{UNKNOWN}\"."
    );
    let _ = writeln!(o, "report.kind: {kind}");
    let _ = writeln!(o, "report.generated: {}", Rfc3339(r.generated));
    let _ = writeln!(o, "report.generated_local: {}", local_time(r.generated));
    let _ = writeln!(o, "report.horizon_days: {}", r.horizon_days);
    let _ = writeln!(o, "report.plants: {}", r.plants.len());
    let _ = writeln!(
        o,
        "report.needs_water: {}",
        if needs_water.is_empty() {
            "none".into()
        } else {
            needs_water.join(",")
        }
    );
    for p in &r.plants {
        o.push('\n');
        plant(&mut o, p, r.generated);
    }
    o
}

fn plant(o: &mut String, p: &PlantSummary, now: i64) {
    let mut kv = |k: &str, v: &dyn std::fmt::Display| {
        let _ = writeln!(o, "plant.{}.{k}: {v}", p.key);
    };
    let pct = |v: Option<f32>| v.map_or_else(|| UNKNOWN.into(), |v| format!("{v:.1}"));
    let time = |v: Option<i64>| v.map_or_else(|| UNKNOWN.into(), |t| Rfc3339(t).to_string());
    let local = |v: Option<i64>| v.map_or_else(|| UNKNOWN.into(), local_time);
    let f = p.forecast.as_ref();
    let h = p.history.as_ref();
    let eta_days = p
        .next_watering
        .map(|t| ((t - now) as f32 / 86_400.0).max(0.0));

    kv("name", &p.name);
    // One line: notes may hold newlines and runs of spaces.
    let notes = p
        .notes
        .as_deref()
        .map(|n| n.split_whitespace().collect::<Vec<_>>().join(" "));
    kv("notes", &notes.as_deref().unwrap_or(UNKNOWN));
    kv("sensor_id", &p.sensor_id);
    kv("status", &p.status.as_str());
    kv("moisture_pct", &pct(p.moisture_pct));
    kv("last_seen", &time(p.last_seen));
    kv("dry_threshold_pct", &format!("{:.1}", p.dry_threshold_pct));
    kv("day_min_pct", &pct(p.day.as_ref().map(|d| d.min_pct)));
    kv("day_mean_pct", &pct(p.day.as_ref().map(|d| d.mean_pct)));
    kv("day_max_pct", &pct(p.day.as_ref().map(|d| d.max_pct)));
    kv(
        "drying_rate_pct_per_day",
        &pct(f.map(|f| f.rate_pct_per_day)),
    );
    kv("model", &f.map_or(UNKNOWN, |f| f.model.as_str()));
    kv(
        "forecast_confidence",
        &f.map_or_else(|| UNKNOWN.into(), |f| format!("{:.2}", f.confidence)),
    );
    kv("last_watered", &time(p.last_watered));
    kv(
        "waterings_in_horizon",
        &h.map_or_else(|| UNKNOWN.into(), |h| h.waterings.to_string()),
    );
    kv(
        "median_watering_interval_days",
        &pct(h.and_then(|h| h.median_interval_days)),
    );
    kv("next_watering", &time(p.next_watering));
    kv("next_watering_local", &local(p.next_watering));
    kv("next_watering_in_days", &pct(eta_days));
    kv("summary", &summary(p, now, eta_days));
}

/// One plain-English sentence or three, for people and LLMs.
fn summary(p: &PlantSummary, now: i64, eta_days: Option<f32>) -> String {
    let Some(m) = p.moisture_pct else {
        return format!("{} has not reported a reading yet.", p.name);
    };
    let mut s = String::new();
    if p.status == Status::Offline {
        let _ = write!(
            s,
            "{}'s sensor is offline; last reading {m:.1}% at {}.",
            p.name,
            local_time(p.last_seen.unwrap_or(now))
        );
        return s;
    }
    let _ = write!(s, "{} is at {m:.1}% moisture", p.name);
    if let Some(f) = &p.forecast {
        let _ = write!(s, ", drying about {:.1}%/day", f.rate_pct_per_day);
    }
    s.push('.');
    if let Some(w) = p.last_watered {
        let _ = write!(s, " Last watered {}.", ago(now - w));
    }
    match (p.status, eta_days, p.next_watering) {
        (Status::WaterNow, ..) => {
            let _ = write!(
                s,
                " It is below its {:.0}% threshold: water now.",
                p.dry_threshold_pct
            );
        }
        (_, Some(d), Some(t)) => {
            let _ = write!(s, " Water in about {d:.1} days (around {}).", local_time(t));
        }
        _ => s.push_str(" Not enough drydown history to forecast the next watering yet."),
    }
    s
}

fn ago(secs: i64) -> String {
    if secs < 3_600 {
        "less than an hour ago".into()
    } else if secs < 2 * 86_400 {
        format!("{} hours ago", secs / 3_600)
    } else {
        format!("{:.1} days ago", secs as f32 / 86_400.0)
    }
}
