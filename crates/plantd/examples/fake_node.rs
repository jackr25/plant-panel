//! Stand-in for a soil node, for developing plantd without hardware. Serves the same
//! `/api/v1/status` shape as the firmware, with moisture drying exponentially.
//!
//!   cargo run --example fake_node -- 127.0.0.1:8181 a1b2c3d4e5f6 [start_pct] [k_per_hour]
//!   curl -X POST 127.0.0.1:8181/water     # simulate a watering (+25 %)
//!
//! A high k_per_hour (e.g. 2.0) makes drydown visible within minutes.

use std::time::Instant;

use tiny_http::{Header, Method, Response, Server};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let listen = args.first().map_or("127.0.0.1:8181", String::as_str);
    let id = args.get(1).map_or("fa4e00000001", String::as_str);
    let mut pct: f64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(55.0);
    let k_per_s: f64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(0.004) / 3600.0;

    let server = Server::http(listen).expect("bind");
    let boot = Instant::now();
    let mut last = Instant::now();
    let mut interval = 60u32;
    let hostname = format!("soil-{}", &id[id.len().saturating_sub(6)..]);
    eprintln!("fake node {id} ({hostname}) on {listen}");

    for mut req in server.incoming_requests() {
        pct *= (-k_per_s * last.elapsed().as_secs_f64()).exp();
        last = Instant::now();
        let body = match (req.method(), req.url()) {
            (Method::Get, "/api/v1/status") => format!(
                r#"{{"device":{{"id":"{id}","hostname":"{hostname}","fw_version":"fake","uptime_s":{},"rssi":-58}},
"config":{{"sample_interval_s":{interval}}},
"reading":{{"moisture_pct":{pct:.2},"millivolts":1500,"raw":1900,"age_s":0}},
"sampler":{{"samples":1,"read_errors":0}},"stats":{{}}}}"#,
                boot.elapsed().as_secs()
            ),
            (Method::Put, "/api/v1/config") => {
                let mut s = String::new();
                let _ = req.as_reader().read_to_string(&mut s);
                if let Some(v) = s
                    .split(':')
                    .nth(1)
                    .and_then(|v| v.trim_matches(|c: char| !c.is_ascii_digit()).parse().ok())
                {
                    interval = v;
                }
                format!(r#"{{"sample_interval_s":{interval}}}"#)
            }
            (Method::Post, "/water") => {
                pct = (pct + 25.0).min(70.0);
                format!(r#"{{"moisture_pct":{pct:.2}}}"#)
            }
            _ => {
                let _ = req.respond(Response::from_string("not found").with_status_code(404));
                continue;
            }
        };
        let ct = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
        let _ = req.respond(Response::from_string(body).with_header(ct));
    }
}
