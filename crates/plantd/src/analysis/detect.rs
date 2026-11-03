//! Streaming event detection. One `Detector` per sensor, fed every stored sample, both
//! live (poller) and when replaying history (daily analysis), so the two always agree.

use std::collections::VecDeque;

use serde::Deserialize;

use super::Sample;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetectParams {
    /// Rise (percentage points) over `watering_window_min` that counts as a watering.
    pub watering_rise_pct: f32,
    pub watering_window_min: u32,
    /// Fall over `rapid_drop_window_min` that is too fast to be drying.
    pub rapid_drop_pct: f32,
    pub rapid_drop_window_min: u32,
    /// Minimum time between two events of the same kind.
    pub cooldown_min: u32,
    /// Moisture must recover this far above the dry threshold before `dry` can fire again.
    pub dry_hysteresis_pct: f32,
}

impl Default for DetectParams {
    fn default() -> Self {
        Self {
            watering_rise_pct: 8.0,
            watering_window_min: 120,
            rapid_drop_pct: 10.0,
            rapid_drop_window_min: 60,
            cooldown_min: 360,
            dry_hysteresis_pct: 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Detection {
    Watering { from: f32, to: f32 },
    RapidDrop { from: f32, to: f32 },
    Dry { pct: f32 },
}

#[derive(Clone, Debug)]
pub struct Detector {
    p: DetectParams,
    window: VecDeque<Sample>,
    prev: Option<Sample>,
    last_watering: Option<i64>,
    last_drop: Option<i64>,
    dry: bool,
}

impl Detector {
    pub fn new(p: DetectParams) -> Self {
        Self {
            p,
            window: VecDeque::new(),
            prev: None,
            last_watering: None,
            last_drop: None,
            dry: false,
        }
    }

    /// Tells the detector about a watering it didn't see itself (entered by hand), so
    /// the rise that follows isn't reported again within the cooldown.
    pub fn note_watering(&mut self, t: i64) {
        self.last_watering = self.last_watering.max(Some(t));
    }

    /// Feeds one sample (in time order) and returns what it triggered. Allocates only
    /// when something fires.
    pub fn step(&mut self, s: Sample, dry_pct: f32) -> Vec<Detection> {
        let water_w = i64::from(self.p.watering_window_min) * 60;
        let drop_w = i64::from(self.p.rapid_drop_window_min) * 60;
        let cooldown = i64::from(self.p.cooldown_min) * 60;
        let cooled = |last: Option<i64>| last.is_none_or(|l| s.t - l >= cooldown);

        while self
            .window
            .front()
            .is_some_and(|f| f.t < s.t - water_w.max(drop_w))
        {
            self.window.pop_front();
        }
        let in_window = |w: i64| {
            self.window
                .iter()
                .filter(move |x| x.t >= s.t - w)
                .map(|x| x.pct)
        };
        // Compare against the previous sample too, so a watering during a long outage
        // is still seen when the node comes back.
        let low = in_window(water_w)
            .chain(self.prev.map(|p| p.pct))
            .reduce(f32::min);
        let high = in_window(drop_w).reduce(f32::max);

        let mut out = Vec::new();
        if let Some(lo) = low.filter(|lo| s.pct - lo >= self.p.watering_rise_pct)
            && cooled(self.last_watering)
        {
            self.last_watering = Some(s.t);
            out.push(Detection::Watering {
                from: lo,
                to: s.pct,
            });
        } else if let Some(hi) = high.filter(|hi| hi - s.pct >= self.p.rapid_drop_pct)
            && cooled(self.last_drop)
        {
            self.last_drop = Some(s.t);
            out.push(Detection::RapidDrop {
                from: hi,
                to: s.pct,
            });
        }

        if !self.dry && s.pct < dry_pct {
            self.dry = true;
            out.push(Detection::Dry { pct: s.pct });
        } else if self.dry && s.pct >= dry_pct + self.p.dry_hysteresis_pct {
            self.dry = false;
        }

        self.window.push_back(s);
        self.prev = Some(s);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(series: &[(i64, f32)]) -> Vec<(i64, Detection)> {
        let mut d = Detector::new(DetectParams::default());
        series
            .iter()
            .flat_map(|&(t, pct)| {
                d.step(Sample { t, pct }, 30.0)
                    .into_iter()
                    .map(move |e| (t, e))
            })
            .collect()
    }

    #[test]
    fn watering_is_detected_once() {
        let mut s: Vec<(i64, f32)> = (0..12).map(|i| (i * 300, 40.0 - i as f32 * 0.1)).collect();
        s.extend([(3600, 45.0), (3900, 55.0), (4200, 56.0), (4500, 55.8)]);
        let ev = run(&s);
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0], (3900, Detection::Watering { .. })));
    }

    #[test]
    fn watering_across_outage() {
        let ev = run(&[(0, 35.0), (300, 34.9), (86_400, 55.0)]);
        assert!(matches!(ev[..], [(86_400, Detection::Watering { .. })]));
    }

    #[test]
    fn slow_drying_is_quiet_until_dry() {
        // 50% -> 25% over 5 days at 5-minute samples.
        let s: Vec<(i64, f32)> = (0..1440)
            .map(|i| (i * 300, 50.0 - i as f32 * 25.0 / 1440.0))
            .collect();
        let ev = run(&s);
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0].1, Detection::Dry { .. }));
    }

    #[test]
    fn noted_watering_suppresses_detection() {
        let mut d = Detector::new(DetectParams::default());
        d.step(Sample { t: 0, pct: 35.0 }, 30.0);
        d.note_watering(200);
        assert!(d.step(Sample { t: 300, pct: 55.0 }, 30.0).is_empty());
        // ...but not once the cooldown has passed.
        let mut d = Detector::new(DetectParams::default());
        d.step(Sample { t: 0, pct: 35.0 }, 30.0);
        d.note_watering(-86_400);
        assert_eq!(d.step(Sample { t: 300, pct: 55.0 }, 30.0).len(), 1);
    }

    #[test]
    fn rapid_drop() {
        let ev = run(&[(0, 50.0), (300, 50.0), (600, 36.0)]);
        assert!(matches!(ev[..], [(600, Detection::RapidDrop { .. })]));
    }
}
