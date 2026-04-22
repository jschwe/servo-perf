// tools/servoperf/src/stats.rs
//! Quantile aggregation and deltas. Mann-Whitney U is a stub (see
//! Appendix A of the design doc; add when CDN runs become routine).

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub p25: f64,
    pub p50: f64,
    pub mean: f64,
    pub p75: f64,
    pub p90: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct SummaryDelta {
    pub abs_ms: f64,
    pub pct: f64,
}

pub fn summarise(samples: &[f64]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    let mut xs: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    let q = |p: f64| -> f64 {
        let idx = ((n as f64 - 1.0) * p).round() as usize;
        xs[idx.min(n - 1)]
    };
    let mean = xs.iter().sum::<f64>() / n as f64;
    Some(Summary {
        n,
        min: xs[0],
        p25: q(0.25),
        p50: q(0.50),
        mean,
        p75: q(0.75),
        p90: q(0.90),
        max: xs[n - 1],
    })
}

/// p50-based delta: `abs_ms = patch.p50 - base.p50` and `pct` relative to base.
pub fn delta(base: &Summary, patch: &Summary) -> SummaryDelta {
    let abs_ms = patch.p50 - base.p50;
    let pct = if base.p50.abs() < f64::EPSILON {
        0.0
    } else {
        100.0 * abs_ms / base.p50
    };
    SummaryDelta { abs_ms, pct }
}

/// Deferred — see design doc Appendix A.
#[allow(dead_code)]
pub fn mann_whitney_u(_base: &[f64], _patch: &[f64]) -> ! {
    unimplemented!("Mann-Whitney U deferred; see docs/superpowers/specs/…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_of_known_distribution() {
        let xs: Vec<f64> = (1..=9).map(|v| v as f64).collect();
        let s = summarise(&xs).unwrap();
        assert_eq!(s.n, 9);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 9.0);
        assert_eq!(s.p50, 5.0);
        assert_eq!(s.mean, 5.0);
    }

    #[test]
    fn summarise_empty_returns_none() {
        assert!(summarise(&[]).is_none());
        assert!(summarise(&[f64::NAN, f64::NAN]).is_none());
    }

    #[test]
    fn delta_is_p50_based() {
        let base = summarise(&[100.0, 110.0, 120.0, 130.0, 140.0]).unwrap();
        let patch = summarise(&[80.0, 90.0, 100.0, 110.0, 120.0]).unwrap();
        let d = delta(&base, &patch);
        assert_eq!(d.abs_ms, -20.0);
        assert!((d.pct + 16.666_666).abs() < 0.01);
    }
}
