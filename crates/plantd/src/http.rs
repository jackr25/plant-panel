//! Front-end API (see docs/ICD.md). One thread, sequential requests: traffic is a few
//! dashboards refreshing now and then.

use std::io::{self, Read};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;

use plant_icd::api::{
    Event, EventKind, PHOTO_MAX_BYTES, Photo, PlantUpdate, Report, Series, WateringEntry,
};
use plant_icd::record::{MAGIC, SampleRecord};
use plant_icd::text::Rfc3339;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::Command;
use crate::analysis::ANALYSIS_FILE;
use crate::report;
use crate::state::{self, PROFILES_FILE, REGISTRY_FILE, State};
use crate::store::{LATEST_JSON, LATEST_TXT, Store};

const JSON: &str = "application/json";
const TEXT: &str = "text/plain; charset=utf-8";
const CSV: &str = "text/csv";
const BIN: &str = "application/octet-stream";
const MAX_RANGE_S: i64 = 400 * 86_400;
/// How far ahead of our clock a client's "now" may be when entering a watering.
const CLOCK_SLACK_S: i64 = 300;
/// Content types accepted for photos, with the magic bytes that identify them.
const IMAGE_TYPES: [(&str, &[u8]); 3] = [
    ("image/jpeg", b"\xff\xd8\xff"),
    ("image/png", b"\x89PNG\r\n\x1a\n"),
    ("image/webp", b"RIFF"),
];

pub struct Ctx {
    pub state: Arc<RwLock<State>>,
    pub store: Arc<Store>,
    pub commands: Sender<Command>,
}

pub fn spawn(listen: &str, ctx: Ctx) -> io::Result<()> {
    let server = Server::http(listen).map_err(io::Error::other)?;
    log::info!("API listening on http://{listen}{}", plant_icd::API_PREFIX);
    thread::Builder::new().name("http".into()).spawn(move || {
        for mut req in server.incoming_requests() {
            let (method, url) = (req.method().clone(), req.url().to_owned());
            let reply = match read_body(&mut req) {
                Ok(body) => route(&ctx, &method, &url, &body),
                Err(reply) => reply,
            };
            log::debug!("{method} {url} -> {}", reply.0);
            respond(req, reply);
        }
    })?;
    Ok(())
}

type Reply = (u16, &'static str, Vec<u8>);

fn ok(ct: &'static str, body: impl Into<Vec<u8>>) -> Reply {
    (200, ct, body.into())
}

fn json<T: serde::Serialize>(v: &T) -> Reply {
    ok(JSON, serde_json::to_vec(v).unwrap_or_default())
}

fn error(code: u16, msg: &str) -> Reply {
    (
        code,
        JSON,
        serde_json::to_vec(&serde_json::json!({ "error": msg })).unwrap_or_default(),
    )
}

fn respond(req: Request, (code, ct, body): Reply) {
    let header =
        |k: &str, v: &str| Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("ASCII header");
    let resp = Response::from_data(body)
        .with_status_code(code)
        .with_header(header("Content-Type", ct))
        .with_header(header("Access-Control-Allow-Origin", "*"))
        .with_header(header(
            "Access-Control-Allow-Methods",
            "GET, POST, PUT, PATCH, DELETE, OPTIONS",
        ))
        .with_header(header("Access-Control-Allow-Headers", "Content-Type"))
        .with_header(header("Cache-Control", "no-cache"));
    if let Err(e) = req.respond(resp) {
        log::debug!("response failed: {e}");
    }
}

/// The request body, refused with 413 past the largest thing we accept (a photo).
fn read_body(req: &mut Request) -> Result<Vec<u8>, Reply> {
    let too_big = || error(413, "request body too large");
    if req.body_length().is_some_and(|n| n > PHOTO_MAX_BYTES) {
        return Err(too_big());
    }
    let mut body = Vec::new();
    req.as_reader()
        .take(PHOTO_MAX_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| error(400, &e.to_string()))?;
    if body.len() > PHOTO_MAX_BYTES {
        return Err(too_big());
    }
    Ok(body)
}

fn query<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn route(ctx: &Ctx, method: &Method, url: &str, body: &[u8]) -> Reply {
    let (path, q) = url.split_once('?').unwrap_or((url, ""));
    let Some(path) = path.strip_prefix(plant_icd::API_PREFIX) else {
        return error(404, "not found");
    };
    let now = crate::unix_now();
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();

    match (method, parts.as_slice()) {
        (Method::Options, _) => (204, TEXT, Vec::new()),
        (Method::Get, ["health"]) => json(&state::read(&ctx.state).health()),
        (Method::Get, ["plants"]) => json(&state::read(&ctx.state).summaries(now)),
        (Method::Get, ["plants", key]) => {
            let st = state::read(&ctx.state);
            st.find(key).map_or_else(
                || error(404, "no such plant"),
                |s| json(&st.summary(s, now)),
            )
        }
        (Method::Patch, ["plants", key]) => update_plant(ctx, key, body, now),
        (Method::Get, ["plants", key, "samples"]) => samples(ctx, key, q, now),
        (Method::Get, ["plants", key, "photo"]) => get_photo(ctx, key),
        (Method::Put, ["plants", key, "photo"]) => put_photo(ctx, key, body, now),
        (Method::Delete, ["plants", key, "photo"]) => delete_photo(ctx, key, now),
        (Method::Post, ["plants", key, "waterings"]) => add_watering(ctx, key, body, now),
        (Method::Delete, ["plants", key, "history"]) => clear_history(ctx, key, now),
        (Method::Get, ["events"]) => events(ctx, q, now),
        (Method::Get, ["report.txt"]) => file_or_404(ctx, LATEST_TXT, TEXT),
        (Method::Get, ["report.json"]) => file_or_404(ctx, LATEST_JSON, JSON),
        (Method::Get, ["live.txt"]) => {
            let st = state::read(&ctx.state);
            let r = Report {
                api: plant_icd::API_VERSION,
                generated: now,
                horizon_days: st.cfg.analysis.horizon_days,
                plants: st.summaries(now),
            };
            ok(TEXT, report::render(&r, "live"))
        }
        (Method::Post, ["analyze"]) => match ctx.commands.send(Command::Analyze) {
            Ok(()) => (202, JSON, br#"{"queued":true}"#.to_vec()),
            Err(_) => error(503, "scheduler not running"),
        },
        _ => error(404, "not found"),
    }
}

fn sensor_id(ctx: &Ctx, key: &str) -> Option<String> {
    state::read(&ctx.state).find(key).map(|s| s.id.clone())
}

/// Applies `edit` to the plant on sensor `id`, saves the profiles (and the registry,
/// which records keys) and replies with the plant's new summary.
fn edit_plant(
    ctx: &Ctx,
    id: &str,
    now: i64,
    edit: impl FnOnce(&mut State) -> Result<(), String>,
) -> Reply {
    let (summary, profiles, registry) = {
        let mut st = state::write(&ctx.state);
        if let Err(e) = edit(&mut st) {
            return error(400, &e);
        }
        let Some(s) = st.sensors.get(id) else {
            return error(404, "no such plant");
        };
        (st.summary(s, now), st.profiles.clone(), st.registry())
    };
    let saved = ctx
        .store
        .save_json(PROFILES_FILE, &profiles)
        .and_then(|()| ctx.store.save_json(REGISTRY_FILE, &registry));
    match saved {
        Ok(()) => json(&summary),
        Err(e) => error(500, &format!("saving plant: {e}")),
    }
}

fn update_plant(ctx: &Ctx, key: &str, body: &[u8], now: i64) -> Reply {
    let Some(id) = sensor_id(ctx, key) else {
        return error(404, "no such plant");
    };
    let update: PlantUpdate = match serde_json::from_slice(body) {
        Ok(u) => u,
        Err(e) => return error(400, &format!("bad plant update: {e}")),
    };
    edit_plant(ctx, &id, now, |st| st.update_plant(&id, update))
}

fn get_photo(ctx: &Ctx, key: &str) -> Reply {
    let found = {
        let st = state::read(&ctx.state);
        st.find(key).map(|s| {
            let photo = st.profiles.get(&s.id).and_then(|p| p.photo.clone());
            (s.id.clone(), photo)
        })
    };
    let Some((id, Some(photo))) = found else {
        return error(404, "no photo");
    };
    let ct = IMAGE_TYPES
        .iter()
        .find(|(t, _)| *t == photo.content_type)
        .map_or(BIN, |(t, _)| t);
    ctx.store
        .read_photo(&id)
        .map_or_else(|_| error(404, "no photo"), |b| ok(ct, b))
}

fn put_photo(ctx: &Ctx, key: &str, body: &[u8], now: i64) -> Reply {
    let Some(id) = sensor_id(ctx, key) else {
        return error(404, "no such plant");
    };
    let Some(content_type) = sniff_image(body) else {
        return error(415, "photo must be a JPEG, PNG or WebP image");
    };
    if let Err(e) = ctx.store.write_photo(&id, body) {
        return error(500, &format!("saving photo: {e}"));
    }
    let photo = Photo {
        content_type: content_type.into(),
        bytes: body.len() as u32,
        updated: now,
    };
    edit_plant(ctx, &id, now, |st| {
        st.set_photo(&id, Some(photo));
        Ok(())
    })
}

fn delete_photo(ctx: &Ctx, key: &str, now: i64) -> Reply {
    let Some(id) = sensor_id(ctx, key) else {
        return error(404, "no such plant");
    };
    let reply = edit_plant(ctx, &id, now, |st| {
        st.set_photo(&id, None);
        Ok(())
    });
    if let Err(e) = ctx.store.delete_photo(&id) {
        log::warn!("{id}: deleting photo: {e}");
    }
    reply
}

/// Asks the scheduler to re-run the analysis, so watering counts and the forecast
/// reflect an edit to the history without waiting for the daily run.
fn reanalyze(ctx: &Ctx) {
    if ctx.commands.send(Command::Analyze).is_err() {
        log::warn!("scheduler not running; analysis not queued");
    }
}

fn add_watering(ctx: &Ctx, key: &str, body: &[u8], now: i64) -> Reply {
    let entry: WateringEntry = if body.iter().all(u8::is_ascii_whitespace) {
        WateringEntry::default()
    } else {
        match serde_json::from_slice(body) {
            Ok(e) => e,
            Err(e) => return error(400, &format!("bad watering: {e}")),
        }
    };
    let (id, plant, cooldown, oldest) = {
        let st = state::read(&ctx.state);
        let Some(s) = st.find(key) else {
            return error(404, "no such plant");
        };
        (
            s.id.clone(),
            s.key.clone(),
            i64::from(st.cfg.detect.cooldown_min) * 60,
            now - i64::from(st.cfg.retention_days) * 86_400,
        )
    };
    let t = entry.t.unwrap_or(now);
    if t > now + CLOCK_SLACK_S {
        return error(400, "t must not be in the future");
    }
    if t < oldest {
        return error(400, "t is older than the retention period");
    }
    let t = t.min(now);

    // One watering, one event: refuse if one (sensed or entered) is already that close.
    let existing = match ctx.store.read_events(t - cooldown + 1) {
        Ok(evs) => evs.into_iter().find(|e| {
            e.sensor_id == id && e.kind == EventKind::Watering && (e.t - t).abs() < cooldown
        }),
        Err(e) => return error(500, &e.to_string()),
    };
    if let Some(e) = existing {
        return error(
            409,
            &format!("a watering is already recorded at {}", Rfc3339(e.t)),
        );
    }

    let ev = Event {
        t,
        kind: EventKind::Watering,
        plant,
        sensor_id: id.clone(),
        from_pct: None,
        pct: None,
        manual: true,
    };
    if let Err(e) = ctx.store.append_event(&ev) {
        return error(500, &format!("saving watering: {e}"));
    }
    let summary = {
        let mut st = state::write(&ctx.state);
        let Some(s) = st.sensors.get_mut(&id) else {
            return error(404, "no such plant");
        };
        s.note_watering(t);
        let s = &st.sensors[&id];
        st.summary(s, now)
    };
    log::info!("{}: manual watering at {}", summary.key, Rfc3339(t));
    reanalyze(ctx);
    let (_, ct, body) = json(&summary);
    (201, ct, body)
}

fn clear_history(ctx: &Ctx, key: &str, now: i64) -> Reply {
    let Some(id) = sensor_id(ctx, key) else {
        return error(404, "no such plant");
    };
    let (summary, analysis) = {
        let mut st = state::write(&ctx.state);
        // Under the lock: the poller appends samples while holding it.
        if let Err(e) = ctx.store.delete_samples(&id) {
            return error(500, &format!("deleting samples: {e}"));
        }
        st.clear_history(&id);
        let Some(s) = st.sensors.get(&id) else {
            return error(404, "no such plant");
        };
        (st.summary(s, now), st.analysis_file())
    };
    let removed = match ctx.store.delete_events(&id) {
        Ok(n) => n,
        Err(e) => return error(500, &format!("deleting events: {e}")),
    };
    if let Err(e) = ctx.store.save_json(ANALYSIS_FILE, &analysis) {
        return error(500, &format!("saving analysis: {e}"));
    }
    log::info!("{}: history cleared ({removed} events)", summary.key);
    reanalyze(ctx);
    json(&summary)
}

fn sniff_image(b: &[u8]) -> Option<&'static str> {
    IMAGE_TYPES
        .iter()
        .find(|(t, magic)| {
            b.starts_with(magic) && (*t != "image/webp" || b.get(8..12) == Some(b"WEBP"))
        })
        .map(|(t, _)| *t)
}

fn file_or_404(ctx: &Ctx, name: &str, ct: &'static str) -> Reply {
    ctx.store
        .read_report(name)
        .map_or_else(|_| error(404, "no report yet"), |b| ok(ct, b))
}

fn samples(ctx: &Ctx, key: &str, q: &str, now: i64) -> Reply {
    let Some(id) = state::read(&ctx.state).find(key).map(|s| s.id.clone()) else {
        return error(404, "no such plant");
    };
    let until = query(q, "until")
        .and_then(|v| v.parse().ok())
        .unwrap_or(now);
    let since = query(q, "since")
        .and_then(|v| v.parse().ok())
        .unwrap_or(until - 86_400);
    if since > until || until - since > MAX_RANGE_S {
        return error(400, "since..until must be ordered and at most 400 days");
    }
    let step: u32 = query(q, "step").and_then(|v| v.parse().ok()).unwrap_or(0);
    let records = match ctx.store.load_samples(&id, since, until) {
        Ok(r) if step > 0 => downsample(&r, step),
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };

    match query(q, "format").unwrap_or("json") {
        "json" => json(&Series {
            sensor_id: id,
            step_s: step,
            t: records.iter().map(|r| i64::from(r.t)).collect(),
            pct: records.iter().map(SampleRecord::moisture_pct).collect(),
        }),
        "csv" => {
            let mut out = String::from("t,pct\n");
            for r in &records {
                out.push_str(&format!("{},{:.2}\n", r.t, r.moisture_pct()));
            }
            ok(CSV, out)
        }
        "bin" => {
            let mut out = Vec::with_capacity(8 * (records.len() + 1));
            out.extend_from_slice(&MAGIC);
            records
                .iter()
                .for_each(|r| out.extend_from_slice(&r.encode()));
            ok(BIN, out)
        }
        _ => error(400, "format must be json, csv or bin"),
    }
}

/// Bucket means, timestamped at the bucket start. Flags are OR-ed, RSSI is the last.
fn downsample(records: &[SampleRecord], step: u32) -> Vec<SampleRecord> {
    let mut out: Vec<SampleRecord> = Vec::new();
    let (mut sum, mut n) = (0u64, 0u64);
    for r in records {
        let bucket = r.t - r.t % step;
        match out.last_mut() {
            Some(last) if last.t == bucket => {
                sum += u64::from(r.moisture_cpct);
                n += 1;
                last.moisture_cpct = (sum / n) as u16;
                last.flags |= r.flags;
                last.rssi = r.rssi;
            }
            _ => {
                (sum, n) = (u64::from(r.moisture_cpct), 1);
                out.push(SampleRecord { t: bucket, ..*r });
            }
        }
    }
    out
}

fn events(ctx: &Ctx, q: &str, now: i64) -> Reply {
    let since = query(q, "since")
        .and_then(|v| v.parse().ok())
        .unwrap_or(now - 7 * 86_400);
    let plant = query(q, "plant");
    let mut evs = match ctx.store.read_events(since) {
        Ok(e) => e,
        Err(e) => return error(500, &e.to_string()),
    };
    {
        // The log keeps the key a plant had at the time; report the current one, so
        // renamed plants keep their history.
        let st = state::read(&ctx.state);
        if let Some(p) = plant {
            let id = st.find(p).map(|s| s.id.as_str());
            evs.retain(|e| Some(e.sensor_id.as_str()) == id || e.plant == p || e.sensor_id == p);
        }
        for e in &mut evs {
            if let Some(s) = st.sensors.get(&e.sensor_id) {
                e.plant.clone_from(&s.key);
            }
        }
    }
    match query(q, "format").unwrap_or("json") {
        "json" => json(&evs),
        "txt" => {
            let mut out = String::new();
            for e in &evs {
                let _ = e.write_line(&mut out);
                out.push('\n');
            }
            ok(TEXT, out)
        }
        _ => error(400, "format must be json or txt"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsamples_to_bucket_means() {
        let r = |t, pct| SampleRecord::new(t, pct, -60, 0);
        let out = downsample(&[r(3600, 40.0), r(3900, 42.0), r(7200, 30.0)], 3600);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].t, out[0].moisture_pct()), (3600, 41.0));
        assert_eq!((out[1].t, out[1].moisture_pct()), (7200, 30.0));
    }

    #[test]
    fn sniffs_images() {
        assert_eq!(sniff_image(b"\xff\xd8\xff\xe0rest"), Some("image/jpeg"));
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert_eq!(sniff_image(b"<svg"), None);
    }
}
