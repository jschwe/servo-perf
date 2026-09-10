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

/// Dispersion of a sample, and what it implies for what a comparison can
/// resolve.
///
/// A median on its own hides whether a 5% difference between two legs is a
/// result or noise. These are the numbers that answer that.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spread {
    pub n: usize,
    pub median: f64,
    pub mean: f64,
    /// Sample standard deviation (n-1).
    pub sd: f64,
    /// `sd / mean`, the scale-free spread. Comparable across metrics whose
    /// magnitudes differ by orders of magnitude.
    pub cv: f64,
    /// Standard error of the mean.
    pub sem: f64,
    /// Half-width of a rough 95% interval on this leg's own mean, as a
    /// fraction of it.
    ///
    /// Not the floor for a *comparison*: a difference of two independent means
    /// carries both legs' error, so use [`resolvable_between`] for that. The
    /// two differ by about √2, enough to mark a delta significant in one
    /// breath and unresolvable in the next.
    pub resolvable: f64,
}

/// The smallest relative difference between two legs that is not noise, as a
/// fraction of the first leg's mean.
///
/// Two standard errors of the *difference*, which is what the comparison table
/// tests a delta against — so the table and the note beside it cannot
/// disagree.
pub fn resolvable_between(a: &Spread, b: &Spread) -> f64 {
    if a.mean == 0.0 {
        return 0.0;
    }
    2.0 * (a.sem.powi(2) + b.sem.powi(2)).sqrt() / a.mean.abs()
}

pub fn spread(samples: &[f64]) -> Option<Spread> {
    let mut xs: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
    // A single sample has no dispersion to report. Returning zeros would be
    // read as *certainty*: a comparison whose noise floor is 0 marks every
    // difference as significant, and a leg cut short by failures or a
    // cancellation would look the cleanest in the report.
    if xs.len() < 2 {
        return None;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    let mean = xs.iter().sum::<f64>() / n as f64;
    // Deliberately the same nearest-rank definition `summarise` uses, so the
    // per-run report and the campaign comparison cannot print different p50s
    // for the same data — they disagreed at every even `n`, which is any cell
    // shortened by a failed or cancelled iteration.
    let median = xs[(((n as f64 - 1.0) * 0.5).round() as usize).min(n - 1)];
    let sd = (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0)).sqrt();
    let cv = if mean != 0.0 { sd / mean.abs() } else { 0.0 };
    let sem = sd / (n as f64).sqrt();
    let resolvable = if mean != 0.0 {
        2.0 * sem / mean.abs()
    } else {
        0.0
    };
    Some(Spread {
        n,
        median,
        mean,
        sd,
        cv,
        sem,
        resolvable,
    })
}

/// Iterations far enough from the median to be worth naming, by Tukey's rule
/// on the interquartile range.
///
/// Reported rather than dropped: an outlier in a measurement run is usually
/// evidence about the run — a thermal step, a crash that was not detected, a
/// page that loaded from cache — and silently trimming it would hide that.
pub fn outliers(samples: &[f64]) -> Vec<usize> {
    let mut xs: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
    if xs.len() < 4 {
        return Vec::new();
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    let q = |p: f64| -> f64 {
        let idx = ((n as f64 - 1.0) * p).round() as usize;
        xs[idx.min(n - 1)]
    };
    let (q1, q3) = (q(0.25), q(0.75));
    let iqr = q3 - q1;
    if iqr == 0.0 {
        return Vec::new();
    }
    // 3x the IQR ("far out"), not the usual 1.5x. At n=15 with the ~20%
    // spread these workloads have, 1.5x flags ordinary runs and the notice
    // stops meaning anything; 3x catches the gross divergences — a thermal
    // step, a page served from cache, an undetected crash — which is what is
    // worth a reader's attention.
    let (lo, hi) = (q1 - 3.0 * iqr, q3 + 3.0 * iqr);
    // Tukey alone is relative to the spread, so on a tight metric it names
    // iterations 10% off the median — true by the rule, useless as a notice.
    // Require a deviation large enough to be worth investigating as well.
    let median = q(0.5);
    let far_enough = |v: f64| median == 0.0 || ((v - median) / median).abs() >= 0.25;
    samples
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite() && (**v < lo || **v > hi) && far_enough(**v))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod spread_tests {
    use super::*;

    #[test]
    fn the_two_medians_agree() {
        // They disagreed at even n, so a cell shortened by one failed
        // iteration printed different p50s in report.md and comparison.md.
        for xs in [
            vec![1.0, 2.0, 3.0, 4.0],
            vec![1.0, 2.0, 3.0],
            vec![10.0, 10.0, 20.0, 30.0, 40.0, 50.0],
        ] {
            assert_eq!(
                spread(&xs).unwrap().median,
                summarise(&xs).unwrap().p50,
                "disagreement for {xs:?}"
            );
        }
    }

    #[test]
    fn a_comparison_floor_carries_both_legs_error() {
        let a = spread(&[100.0, 102.0, 98.0, 101.0]).unwrap();
        let b = spread(&[100.0, 103.0, 97.0, 100.0]).unwrap();
        // Two independent means carry more error than one, so the pairwise
        // floor must exceed either leg's own.
        assert!(resolvable_between(&a, &b) > a.resolvable);
        assert!(resolvable_between(&a, &b) > b.resolvable);
    }

    #[test]
    fn one_sample_reports_no_spread_rather_than_no_uncertainty() {
        // The trap: zeros here read as certainty downstream, and every delta
        // clears a noise floor of 0.
        assert!(spread(&[42.0]).is_none());
        assert!(spread(&[]).is_none());
        assert!(
            spread(&[f64::NAN, 42.0]).is_none(),
            "one finite value is one sample"
        );
        assert!(spread(&[42.0, 43.0]).is_some());
    }

    #[test]
    fn spread_reports_scale_free_dispersion() {
        let s = spread(&[100.0, 100.0, 100.0, 100.0]).unwrap();
        assert_eq!(s.n, 4);
        assert_eq!(s.median, 100.0);
        assert_eq!(s.cv, 0.0);
        assert_eq!(s.resolvable, 0.0);

        // Same shape at a different magnitude gives the same cv.
        let a = spread(&[90.0, 100.0, 110.0]).unwrap();
        let b = spread(&[90e6, 100e6, 110e6]).unwrap();
        assert!((a.cv - b.cv).abs() < 1e-12);
        // and a wider sample is less able to resolve a small difference
        assert!(b.resolvable > 0.0);
    }

    #[test]
    fn outliers_are_named_not_dropped() {
        // One iteration four times the rest.
        let xs = [10.0, 10.5, 9.5, 10.2, 40.0, 9.8, 10.1, 10.3];
        assert_eq!(outliers(&xs), vec![4]);
        // A clean sample has none.
        assert!(outliers(&[10.0, 10.5, 9.5, 10.2, 9.8, 10.1]).is_empty());
        // Nor does a tight one where a point is statistically far out but
        // only a few percent from the median — that is not a divergence.
        assert!(outliers(&[100.0, 100.1, 99.9, 100.0, 104.0, 100.2, 99.8, 100.1]).is_empty());
        // Too few points to judge.
        assert!(outliers(&[1.0, 99.0]).is_empty());
    }
}
