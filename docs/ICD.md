# plant-panel ICD, v1

This is the contract between `plantd` and anything that reads from it: a dashboard on
the same Pi, a Windows app, another embedded Linux box, an MCU with a display, or a
local LLM. Rust clients can depend on the `plant-icd` crate (`no_std` + `alloc`),
which implements everything below. Other languages need only HTTP, JSON or a regex.

Versioning: everything is under `/api/v1`. Additive changes (new fields, routes,
event kinds, report keys) do not bump the version, so **clients must ignore unknown
fields and keys**. Breaking changes move to `/api/v2` and bump `API_VERSION`.

## Conventions

| Thing | Rule |
|---|---|
| Time (JSON, query params, binary) | Unix seconds, UTC, integer |
| Time (text formats) | RFC 3339 UTC, `2026-11-05T09:00:00Z`; keys ending `_local` are the collector's local time, `Thu 2026-11-05 10:00` |
| Moisture | Percent, 0–100, float. Volumetric as calibrated on the node |
| Missing | JSON `null`; text `unknown` |
| Plant key | `[a-z0-9_]+`, unique, stable while the name is unchanged. Routes accept a key or a sensor id |
| Sensor id | The node's `device.id`, 12 lowercase hex digits of its MAC. Stable forever |
| CORS | `Access-Control-Allow-Origin: *` on every response |
| Errors | Non-2xx with `{"error": "..."}` |

## Routes

Base: `http://<collector>:8080/api/v1`

| Method | Path | Returns |
|---|---|---|
| GET | `/health` | `Health` |
| GET | `/plants` | `[PlantSummary]`, live, ordered by key |
| GET | `/plants/{key}` | `PlantSummary` |
| GET | `/plants/{key}/samples` | sample series, see below |
| GET | `/events` | `[Event]`, or event lines with `format=txt` |
| GET | `/report.json` | latest daily `Report` |
| GET | `/report.txt` | latest daily report, text grammar |
| GET | `/live.txt` | same text grammar, built from live state now |
| POST | `/analyze` | `202`, queues the daily analysis to run now |

### `/plants/{key}/samples`

| Param | Default | Meaning |
|---|---|---|
| `since` | `until - 86400` | inclusive |
| `until` | now | inclusive; range at most 400 days |
| `step` | `0` | if > 0, bucket means of this many seconds, stamped at bucket start |
| `format` | `json` | `json`, `csv` or `bin` |

- `json`: `{"sensor_id": "...", "step_s": 0, "t": [..], "pct": [..]}`, columnar.
- `csv`: header `t,pct`, then one row per sample.
- `bin`: the 8-byte header `PLNTREC\x01`, then 8-byte records (see below). This is
  the cheapest format for an MCU: no parser, fixed stride. One day at 5-minute polling
  with `step=3600` is 24 records, 200 bytes.

### `/events`

`since` (default: 7 days ago), `plant` (key or sensor id), `format` (`json` | `txt`).

## JSON types

```jsonc
// Health
{"api": 1, "version": "0.1.0", "uptime_s": 3600, "plants": 3,
 "poll_interval_s": 300, "last_poll": 1793649360, "last_analysis": 1793590200}

// PlantSummary
{
  "key": "monstera", "name": "Monstera", "sensor_id": "a1b2c3d4e5f6", "hostname": "soil-c3d4e5",
  "status": "ok",                 // ok | water_soon | water_now | offline | no_data
  "moisture_pct": 42.1,
  "last_seen": 1793649300,        // time of the latest stored sample
  "rssi": -61,
  "dry_threshold_pct": 30.0,
  "last_watered": 1793210000,
  "next_watering": 1793840000,    // projected crossing of dry_threshold_pct
  "forecast": {                   // null until enough drydown history (~1 day)
    "model": "exponential",       // linear | exponential
    "k_per_day": 0.08,            // %/day (linear) or 1/day (exponential)
    "floor_pct": 0.0,
    "rate_pct_per_day": 3.4,      // at the current moisture
    "confidence": 0.82,           // 0..1
    "points": 2210,
    "fitted_at": 1793590200
  },
  "day": {"min_pct": 40.9, "mean_pct": 42.6, "max_pct": 44.0, "samples": 288},
  "history": {"horizon_days": 21, "waterings": 3, "median_interval_days": 6.2}
}

// Event
{"t": 1793629805, "kind": "watering", "plant": "monstera", "sensor_id": "a1b2c3d4e5f6",
 "from_pct": 31.2, "pct": 58.0}   // from_pct / pct present when meaningful

// Report
{"api": 1, "generated": 1793590200, "horizon_days": 21, "plants": [PlantSummary, ...]}
```

`status` precedence: `no_data` > `offline` > `water_now` (below threshold) >
`water_soon` (forecast within `soon_hours`) > `ok`.

To re-project `next_watering` from a fresher reading `m`, in days:
linear `(m - thr) / k`; exponential `ln((m - floor) / (thr - floor)) / k`.

### Event kinds

| Kind | Meaning | Fields |
|---|---|---|
| `watering` | moisture rose ≥ `watering_rise_pct` within `watering_window_min` | `from_pct`, `pct` |
| `rapid_drop` | moisture fell ≥ `rapid_drop_pct` within `rapid_drop_window_min`: probe pulled or a fault | `from_pct`, `pct` |
| `dry` | crossed below the plant's dry threshold | `pct` |
| `offline` | `offline_after_polls` consecutive failed polls | `pct` (last) |
| `online` | back after `offline` | `pct` (last) |
| `node_reboot` | node uptime went backwards | — |

## Binary sample record

Little endian, 8 bytes, no padding. The same layout is used on disk
(`samples/<id>/YYYY-MM.bin`, with the header) and in `format=bin`.

| Offset | Type | Field |
|---|---|---|
| 0 | u32 | `t`, unix seconds |
| 4 | u16 | `moisture_cpct`, moisture × 100 |
| 6 | i8 | `rssi`, dBm, 0 = unknown |
| 7 | u8 | `flags`: bit 0 = node rebooted before this sample |

Header: ASCII `PLNTREC` followed by the version byte `0x01`. Decoders skip it if
present and ignore a trailing partial record.

## Text grammars

Both formats are one fact per line, ASCII, and LF-terminated, built for `grep`,
simple regexes and LLM context.

### Event line

```text
2026-11-02T14:30:05Z t=1793629805 event=watering plant=monstera sensor=a1b2c3d4e5f6 from_pct=31.2 pct=58.0
```

An RFC 3339 time, then `key=value` pairs separated by single spaces. Values never
contain whitespace. `t`, `event`, `plant` and `sensor` are always present.

```regex
^(\S+) t=(\d+) event=(\w+) plant=(\w+) sensor=(\w+)
```

### Report

```text
# plant-panel report v1
# Lines are "<scope>.<key>: <value>". ...
report.kind: daily
report.generated: 2026-11-03T03:30:00Z
report.generated_local: Tue 2026-11-03 04:30
report.horizon_days: 21
report.plants: 2
report.needs_water: fern

plant.monstera.name: Monstera
plant.monstera.status: ok
plant.monstera.moisture_pct: 42.1
plant.monstera.next_watering: 2026-11-05T09:00:00Z
plant.monstera.summary: Monstera is at 42.1% moisture, drying about 3.4%/day. Last watered 4.4 days ago. Water in about 2.2 days (around Thu 2026-11-05 10:00).
...
```

Grammar: `<scope>.<key>: <value>`, where scope is `report` or `plant.<plant key>`.
Lines starting with `#` and blank lines are comments. Values run to end of line.

```regex
^plant\.(\w+)\.(\w+): (.*)$          # every plant fact
^plant\.(\w+)\.next_watering: (\S+)$ # one fact across all plants
^report\.needs_water: (.*)$          # comma-separated keys, or "none"
```

`report.*` keys: `kind` (`daily` | `live`), `generated`, `generated_local`,
`horizon_days`, `plants`, `needs_water`.

`plant.<key>.*` keys, always all present and in this order: `name`, `sensor_id`,
`status`, `moisture_pct`, `last_seen`, `dry_threshold_pct`, `day_min_pct`,
`day_mean_pct`, `day_max_pct`, `drying_rate_pct_per_day`, `model`,
`forecast_confidence`, `last_watered`, `waterings_in_horizon`,
`median_watering_interval_days`, `next_watering`, `next_watering_local`,
`next_watering_in_days`, `summary`.

`summary` is plain English and meant for people and LLMs. Don't parse it; use the
keys above it.

## Upstream: soil node API (consumed by plantd)

For reference: the firmware in `soil-sensor` advertises `_soilnode._tcp` over mDNS
(TXT `id`, `fw`, `api`) and serves `GET /api/v1/status`. plantd reads
`device.{id,hostname,uptime_s,rssi}`, `config.sample_interval_s` and
`reading.{moisture_pct,age_s}`, and stamps each sample at `receive time - age_s`.
It may `PUT /api/v1/config {"sample_interval_s": N}` when `node_sample_interval_s` is
configured. Front ends should talk to plantd, not to nodes.
