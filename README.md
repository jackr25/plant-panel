# plant-panel

`plantd` is a collector for a network of ESP32 soil-moisture nodes
([soil-sensor](../soil-sensor)). It runs as a systemd service on a Raspberry Pi. It
polls every node on a wall-clock-aligned schedule and stores a compact history. From
that history it detects waterings and other events and forecasts each plant's next
watering. Everything it knows is served over a small, versioned HTTP API.

```
 soil nodes (ESP32-C6)          plantd (Pi)                         front ends
 ┌──────────────┐  mDNS   ┌──────────────────────────┐  /api/v1  ┌──────────────────┐
 │ _soilnode._tcp├───────▶│ discovery                │◀──────────┤ dashboard, app,   │
 │ GET /status  │◀───────┤ poller ── store (8 B/rec) │           │ MCU display,      │
 └──────────────┘  poll   │   └─ live detection      │           │ local LLM (txt)   │
                          │ daily analysis ─ reports │           └──────────────────┘
                          └──────────────────────────┘
```

- **Interface:** [docs/ICD.md](docs/ICD.md) defines the API, the JSON types, the
  binary record format, and the grep/regex-friendly event and report grammars.
  Rust clients can use [`plant-icd`](crates/plant-icd), which is `no_std` + `alloc` so
  it runs on MCUs too.
- **Low overhead:** there is no async runtime. One scheduler thread sleeps between
  ticks, polls fan out on short-lived scoped threads, and a hand-rolled HTTP client
  makes one TCP connection per node per poll. In testing it used ~4 MB RSS and
  effectively zero CPU. The binary is a 1.7 MB static executable.
- **Small disk footprint:** each sample is 8 bytes, appended to a monthly file per
  sensor. At the default 5-minute polling that is about 2.3 KB per sensor per day,
  or about 0.8 MB per sensor per year. Samples are only stored when the node took a
  new reading. Retention defaults to 2 years.

## How the analysis works

| When | What |
|---|---|
| Every poll (default 300 s) | Store the sample, stamped at the node's sample time (`now - age_s`). Run streaming detection: `watering` (sharp rise), `rapid_drop`, `dry` (threshold crossing), `offline`/`online`, `node_reboot`. Events are appended to `events.log`. |
| Daily (default 03:30 local), on startup if the last analysis is stale, or `POST /api/v1/analyze` | Replay the last 21 days through the same detector. Split the history into drydown segments between waterings, dropping the settling period after each one. Fit **one shared drying rate across all segments** (pooled within-segment least squares) with both a linear and an exponential model, and keep whichever fits better. Write `reports/YYYY-MM-DD.txt`, `latest.txt` and `latest.json`, then prune old data. |
| Every API read | Project the fitted rate forward from the *current* reading. `next_watering` therefore stays accurate between daily runs and right after a watering. |

Fitting one rate across all segments matters because each watering resets the
moisture level but not the pot's drying behaviour. A forecast can therefore be made
right after a watering, without waiting for a new cycle to accumulate.

## Layout

```
crates/plant-icd/     no_std ICD: api, node, record, text (event + report grammars)
crates/plantd/
  src/main.rs         CLI + scheduler loop (wall-clock aligned polls, daily analysis)
  src/poller.rs       one poll cycle: discovery → concurrent GETs → store → events
  src/node_client.rs  minimal HTTP/1.1 client for the nodes
  src/discovery.rs    mDNS browse for _soilnode._tcp
  src/state.rs        shared live state, plant summaries
  src/store.rs        on-disk layout (samples, events, reports, json state)
  src/analysis/       detect.rs (streaming events), forecast.rs (drydown fit), mod.rs (daily job)
  src/report.rs       report text rendering
  src/http.rs         front-end API
  examples/fake_node.rs  stand-in node for development without hardware
deploy/               systemd unit, annotated default config
scripts/deploy.sh     cross-build + install on the Pi
```

## Develop (x64 Debian)

```sh
cargo test
cargo clippy --all-targets

# Two simulated nodes, drying fast enough to watch:
cargo run --example fake_node -- 127.0.0.1:18181 a1b2c3d4e5f6 45 2.0 &
cargo run --example fake_node -- 127.0.0.1:18182 0f0e0d0c0b0a 36 0.5 &
cat > /tmp/plantd.toml <<'EOF'
data_dir = "/tmp/plantd-data"
listen = "127.0.0.1:8080"
poll_interval_s = 10
[discovery]
mdns = false
static_hosts = ["127.0.0.1:18181", "127.0.0.1:18182"]
EOF
PLANTD_LOG=debug cargo run -p plantd -- -c /tmp/plantd.toml

curl -X POST 127.0.0.1:18181/water           # simulate a watering → watering event
curl 127.0.0.1:8080/api/v1/live.txt
curl '127.0.0.1:8080/api/v1/events?format=txt'
curl '127.0.0.1:8080/api/v1/plants/soil_d4e5f6/samples?format=csv&step=60'
```

`plantd -c <config> analyze` runs the daily analysis once against a data directory
and prints the report. `plantd -c <config> check` validates a config file.

## Deploy to the Pi

The target is a Pi 5 on a 64-bit OS, which the AI HAT+ 2 also needs. The build is a
static `aarch64-unknown-linux-musl` binary, linked with the `rust-lld` that ships
with rustup. No cross gcc is needed, and it doesn't matter which glibc the Pi has.

```sh
rustup target add aarch64-unknown-linux-musl   # once
cargo pi                                        # alias: release build for the Pi
scripts/deploy.sh pi@plantpi.local              # build, copy, install unit, (re)start
```

On the Pi:

```sh
sudoedit /etc/plant-panel/plantd.toml           # name plants: [[plant]] sensor_id = "...", name = "..."
sudo systemctl restart plantd
journalctl -u plantd -f
curl localhost:8080/api/v1/report.txt
```

The service runs as a transient user with data in `/var/lib/plant-panel`, under
strict sandboxing and low CPU/IO priority, so it stays out of the way of inference
on the AI HAT. The Pi has no RTC, so the deploy script enables
`systemd-time-wait-sync`, and plantd won't poll until the clock reads 2026 or later.

## Roadmap

- Front end(s) against `/api/v1` (see the ICD).
- A local LLM on the AI HAT+ 2 reads `report.txt` / `live.txt` directly as context
  and answers "what needs water this week?"
- Per-plant tuning of the detection thresholds once real watering data exists.
- A gentle security layer if the API leaves the trusted LAN: the nodes have no auth,
  and plantd currently has none either.
