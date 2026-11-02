//! Drydown model fitting and projection.
//!
//! Each watering starts a new drydown segment at a different moisture level, but the
//! pot / soil / plant dry at a characteristic rate. So instead of fitting only the
//! current segment, fit one shared slope across all segments in the horizon with a
//! per-segment intercept (pooled within-segment least squares). The rate then survives
//! a fresh watering, and is projected forward from whatever the live reading is.

use plant_icd::api::{Forecast, Model};

use super::Sample;

const DAY: f64 = 86_400.0;

#[derive(Clone, Copy, Debug)]
pub struct Fit {
    pub model: Model,
    /// d(y)/dt per day, where y is moisture (linear) or ln(moisture - floor) (exponential).
    pub slope_per_day: f64,
    /// Residual sum of squares in moisture space, so the models are comparable.
    pub sse: f64,
    pub r2: f64,
    pub points: usize,
    pub span_s: i64,
}

/// Fits `model` to `segments`. `None` if there isn't enough variation in time.
pub fn fit(segments: &[&[Sample]], model: Model, floor: f32) -> Option<Fit> {
    let y_of = |pct: f32| -> Option<f64> {
        match model {
            Model::Linear => Some(f64::from(pct)),
            Model::Exponential => (pct > floor + 0.5).then(|| f64::from(pct - floor).ln()),
        }
    };
    let pct_of = |y: f64| -> f64 {
        match model {
            Model::Linear => y,
            Model::Exponential => f64::from(floor) + y.exp(),
        }
    };

    // Per-segment means (t in days, relative to the segment start for precision).
    let means: Vec<Option<(f64, f64)>> = segments
        .iter()
        .map(|seg| {
            let t0 = seg.first()?.t;
            let (mut n, mut st, mut sy) = (0.0, 0.0, 0.0);
            for s in *seg {
                if let Some(y) = y_of(s.pct) {
                    n += 1.0;
                    st += (s.t - t0) as f64 / DAY;
                    sy += y;
                }
            }
            (n >= 3.0).then(|| (st / n, sy / n))
        })
        .collect();

    let (mut sxy, mut sxx, mut syy, mut points, mut span_s) = (0.0, 0.0, 0.0, 0, 0);
    for (seg, m) in segments.iter().zip(&means) {
        let Some((tm, ym)) = *m else { continue };
        let t0 = seg[0].t;
        for s in *seg {
            if let Some(y) = y_of(s.pct) {
                let dt = (s.t - t0) as f64 / DAY - tm;
                let dy = y - ym;
                sxy += dt * dy;
                sxx += dt * dt;
                syy += dy * dy;
                points += 1;
            }
        }
        span_s += seg[seg.len() - 1].t - t0;
    }
    if sxx <= f64::EPSILON {
        return None;
    }
    let slope = sxy / sxx;
    let r2 = if syy > 0.0 {
        (sxy * sxy) / (sxx * syy)
    } else {
        0.0
    };

    let mut sse = 0.0;
    for (seg, m) in segments.iter().zip(&means) {
        let Some((tm, ym)) = *m else { continue };
        let t0 = seg[0].t;
        for s in seg.iter().filter(|s| y_of(s.pct).is_some()) {
            let pred = pct_of(ym + slope * ((s.t - t0) as f64 / DAY - tm));
            sse += (f64::from(s.pct) - pred).powi(2);
        }
    }
    Some(Fit {
        model,
        slope_per_day: slope,
        sse,
        r2,
        points,
        span_s,
    })
}

/// Fits both models and keeps the better drying fit as a [`Forecast`].
pub fn best_forecast(segments: &[&[Sample]], floor: f32, now: i64) -> Option<Forecast> {
    let best = [Model::Linear, Model::Exponential]
        .into_iter()
        .filter_map(|m| fit(segments, m, floor))
        .filter(|f| f.slope_per_day < 0.0)
        .min_by(|a, b| a.sse.total_cmp(&b.sse))?;

    // Confidence: fit quality, discounted until there are two days and two dozen points.
    let span_factor = (best.span_s as f64 / (2.0 * DAY)).min(1.0);
    let points_factor = (best.points as f64 / 24.0).min(1.0);
    let k = -best.slope_per_day as f32;
    Some(Forecast {
        model: best.model,
        k_per_day: k,
        floor_pct: floor,
        rate_pct_per_day: 0.0, // filled by `rate_at` against a live reading
        confidence: (best.r2 * span_factor * points_factor).clamp(0.0, 1.0) as f32,
        points: best.points as u32,
        fitted_at: now,
    })
}

/// Drying rate in %/day at moisture `pct`.
pub fn rate_at(f: &Forecast, pct: f32) -> f32 {
    match f.model {
        Model::Linear => f.k_per_day,
        Model::Exponential => f.k_per_day * (pct - f.floor_pct).max(0.0),
    }
}

/// Days from moisture `pct` until it falls to `threshold`. `Some(0.0)` if already there,
/// `None` if the model never reaches it.
pub fn days_until(f: &Forecast, pct: f32, threshold: f32) -> Option<f64> {
    if pct <= threshold {
        return Some(0.0);
    }
    if f.k_per_day <= 0.0 {
        return None;
    }
    let k = f64::from(f.k_per_day);
    match f.model {
        Model::Linear => Some(f64::from(pct - threshold) / k),
        Model::Exponential if threshold > f.floor_pct => {
            Some((f64::from(pct - f.floor_pct) / f64::from(threshold - f.floor_pct)).ln() / k)
        }
        Model::Exponential => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp_segment(start: i64, m0: f32, k: f64, days: f64) -> Vec<Sample> {
        let n = (days * 288.0) as i64;
        (0..n)
            .map(|i| {
                let t = start + i * 300;
                let d = (i * 300) as f64 / DAY;
                Sample {
                    t,
                    pct: (f64::from(m0) * (-k * d).exp()) as f32,
                }
            })
            .collect()
    }

    #[test]
    fn recovers_exponential_rate_across_segments() {
        let a = exp_segment(0, 55.0, 0.12, 5.0);
        let b = exp_segment(6 * 86_400, 60.0, 0.12, 4.0);
        let f = best_forecast(&[&a, &b], 0.0, 0).unwrap();
        assert_eq!(f.model, Model::Exponential);
        assert!((f.k_per_day - 0.12).abs() < 1e-3, "k = {}", f.k_per_day);
        assert!(f.confidence > 0.95);
        // 50% -> 30% at k=0.12/day: ln(50/30)/0.12 = 4.26 days
        let d = days_until(&f, 50.0, 30.0).unwrap();
        assert!((d - 4.257).abs() < 0.05, "{d}");
    }

    #[test]
    fn recovers_linear_rate() {
        let s: Vec<Sample> = (0..864)
            .map(|i| Sample {
                t: i * 300,
                pct: 60.0 - (i * 300) as f32 / 86_400.0 * 4.0,
            })
            .collect();
        let f = best_forecast(&[&s], 0.0, 0).unwrap();
        assert_eq!(f.model, Model::Linear);
        assert!((f.k_per_day - 4.0).abs() < 1e-2);
        assert_eq!(days_until(&f, 30.0, 30.0), Some(0.0));
    }

    #[test]
    fn wetting_is_not_a_forecast() {
        let s: Vec<Sample> = (0..100)
            .map(|i| Sample {
                t: i * 300,
                pct: 30.0 + i as f32 * 0.01,
            })
            .collect();
        assert!(best_forecast(&[&s], 0.0, 0).is_none());
    }
}
