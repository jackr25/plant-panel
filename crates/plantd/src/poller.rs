//! One poll cycle: pick up discovered nodes, fetch every node's status concurrently,
//! store new samples, emit events.

use std::collections::HashSet;
use std::io;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;

use plant_icd::api::{Event, EventKind};
use plant_icd::node;
use plant_icd::record::{SampleRecord, flags};

use crate::analysis::Sample;
use crate::analysis::detect::Detection;
use crate::discovery::Discovery;
use crate::node_client;
use crate::state::{self, REGISTRY_FILE, Sensor, State};
use crate::store::{Store, valid_id};

pub fn poll_cycle(state: &RwLock<State>, store: &Store, discovery: &Discovery, now: i64) {
    let found = discovery.drain();
    let (cfg, targets, mut registry_dirty) = {
        let mut st = state::write(state);
        let mut dirty = false;
        let mut targets: Vec<String> = Vec::new();
        for f in found {
            match f.id {
                Some(id) => {
                    let s = st.ensure_sensor(&id, f.hostname.as_deref().unwrap_or(&id), &f.addr);
                    if s.addr != f.addr {
                        log::info!("{}: address {} -> {}", s.key, s.addr, f.addr);
                        s.addr = f.addr;
                    }
                    dirty = true;
                }
                None => targets.push(f.addr),
            }
        }
        targets.extend(st.sensors.values().map(|s| s.addr.clone()));
        targets.extend(st.cfg.discovery.static_hosts.iter().cloned());
        targets.sort();
        targets.dedup();
        (st.cfg.clone(), targets, dirty)
    };

    let timeout = Duration::from_millis(cfg.poll_timeout_ms);
    let results = poll_all(&targets, timeout, cfg.poll_concurrency.max(1));

    let mut events = Vec::new();
    let mut reconfigure = Vec::new();
    {
        let mut st = state::write(state);
        let mut seen = HashSet::new();
        for (addr, res) in results {
            let status = match res {
                Ok(s) if valid_id(&s.device.id) => s,
                Ok(s) => {
                    log::warn!("{addr}: ignoring node with invalid id {:?}", s.device.id);
                    continue;
                }
                Err(e) => {
                    log::debug!("{addr}: poll failed: {e}");
                    continue;
                }
            };
            let id = status.device.id.clone();
            if !seen.insert(id.clone()) {
                continue; // same node reached at two addresses
            }
            let sensor = st.ensure_sensor(&id, &status.device.hostname, &addr);
            if sensor.addr != addr {
                sensor.addr = addr.clone();
                registry_dirty = true;
            }
            registry_dirty |= sensor.last_uptime.is_none();
            apply_status(sensor, &status, now, store, &mut events);

            let interval = status.config.as_ref().map(|c| c.sample_interval_s);
            if let Some(want) = cfg
                .node_sample_interval_s
                .filter(|w| interval.is_some_and(|i| i != *w))
            {
                reconfigure.push((addr, want));
            }
        }

        for s in st.sensors.values_mut().filter(|s| !seen.contains(&s.id)) {
            s.failures += 1;
            if s.failures == cfg.offline_after_polls {
                s.offline = true;
                events.push(event(s, EventKind::Offline, now, None, s.moisture));
            }
        }
        st.last_poll = Some(now);
        if registry_dirty && let Err(e) = store.save_json(REGISTRY_FILE, &st.registry()) {
            log::error!("saving {REGISTRY_FILE}: {e}");
        }
    }

    for ev in &events {
        let mut line = String::new();
        let _ = ev.write_line(&mut line);
        log::info!("event: {line}");
        if let Err(e) = store.append_event(ev) {
            log::error!("writing event: {e}");
        }
    }
    for (addr, secs) in reconfigure {
        match node_client::set_sample_interval(&addr, secs, timeout) {
            Ok(()) => log::info!("{addr}: node sample interval set to {secs}s"),
            Err(e) => log::warn!("{addr}: setting sample interval: {e}"),
        }
    }
}

fn apply_status(
    s: &mut Sensor,
    status: &node::Status,
    now: i64,
    store: &Store,
    events: &mut Vec<Event>,
) {
    s.failures = 0;
    s.rssi = Some(status.device.rssi.clamp(-128, 0) as i8);
    if s.offline {
        s.offline = false;
        events.push(event(s, EventKind::Online, now, None, s.moisture));
    }
    let mut rec_flags = 0;
    if s.last_uptime.is_some_and(|u| status.device.uptime_s < u) {
        rec_flags |= flags::NODE_REBOOT;
        events.push(event(s, EventKind::NodeReboot, now, None, None));
    }
    s.last_uptime = Some(status.device.uptime_s);

    let Some(reading) = &status.reading else {
        return;
    };
    // Timestamp at the node's sample time, not the poll time. If the node samples
    // less often than we poll, the same reading comes back and is skipped.
    let t = now - reading.age_s as i64;
    if s.last_sample_t.is_some_and(|last| t <= last + 1) {
        return;
    }
    let rec = SampleRecord::new(
        t as u32,
        reading.moisture_pct,
        s.rssi.unwrap_or(0),
        rec_flags,
    );
    if let Err(e) = store.append_sample(&s.id, &rec) {
        log::error!("{}: storing sample: {e}", s.key);
    }
    for d in s.ingest(Sample {
        t,
        pct: rec.moisture_pct(),
    }) {
        events.push(match d {
            Detection::Watering { from, to } => {
                event(s, EventKind::Watering, t, Some(from), Some(to))
            }
            Detection::RapidDrop { from, to } => {
                event(s, EventKind::RapidDrop, t, Some(from), Some(to))
            }
            Detection::Dry { pct } => event(s, EventKind::Dry, t, None, Some(pct)),
        });
    }
}

fn event(s: &Sensor, kind: EventKind, t: i64, from_pct: Option<f32>, pct: Option<f32>) -> Event {
    Event {
        t,
        kind,
        plant: s.key.clone(),
        sensor_id: s.id.clone(),
        from_pct,
        pct,
        manual: false,
    }
}

/// Polls every address, at most `concurrency` at a time. Blocking I/O on short-lived
/// scoped threads: nothing runs between cycles.
fn poll_all(
    targets: &[String],
    timeout: Duration,
    concurrency: usize,
) -> Vec<(String, io::Result<node::Status>)> {
    let mut out = Vec::with_capacity(targets.len());
    for batch in targets.chunks(concurrency) {
        thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|addr| {
                    scope.spawn(move || (addr.clone(), node_client::get_status(addr, timeout)))
                })
                .collect();
            out.extend(handles.into_iter().filter_map(|h| h.join().ok()));
        });
    }
    out
}
