//! Front-end API (see docs/ICD.md). One thread, sequential requests: traffic is a few
//! dashboards refreshing now and then.

use std::io;
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;

use plant_icd::api::{Report, Series};
use plant_icd::record::{MAGIC, SampleRecord};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::Command;
use crate::report;
use crate::state::{self, State};
use crate::store::{LATEST_JSON, LATEST_TXT, Store};

const JSON: &str = "application/json";
const TEXT: &str = "text/plain; charset=utf-8";
const CSV: &str = "text/csv";
const BIN: &str = "application/octet-stream";
const MAX_RANGE_S: i64 = 400 * 86_400;

pub struct Ctx {
    pub state: Arc<RwLock<State>>,
    pub store: Arc<Store>,
    pub commands: Sender<Command>,
}

pub fn spawn(listen: &str, ctx: Ctx) -> io::Result<()> {
    let server = Server::http(listen).map_err(io::Error::other)?;
    log::info!("API listening on http://{listen}{}", plant_icd::API_PREFIX);
    thread::Builder::new().name("http".into()).spawn(move || {
        for req in server.incoming_requests() {
            let (method, url) = (req.method().clone(), req.url().to_owned());
            let reply = route(&ctx, &method, &url);
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
        .with_header(header("Cache-Control", "no-cache"));
    if let Err(e) = req.respond(resp) {
        log::debug!("response failed: {e}");
    }
}

fn query<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn route(ctx: &Ctx, method: &Method, url: &str) -> Reply {
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
        (Method::Get, ["plants", key, "samples"]) => samples(ctx, key, q, now),
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
    if let Some(p) = plant {
        evs.retain(|e| e.plant == p || e.sensor_id == p);
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
}
