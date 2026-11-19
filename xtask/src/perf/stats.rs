//! The statistics of the A/B gate: medians and the exact one-sided Wilcoxon signed-rank test.

use serde::Serialize;

/// The median of `values`: the middle value, or the mean of the two middle values when the count is even.
/// `None` without values.
pub fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(sorted[n / 2]),
        _ => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0),
    }
}

/// The alternative of a one-sided test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tail {
    /// The differences tend to be negative, so a small positive-rank sum is the evidence.
    Lower,
    /// The differences tend to be positive, so a large positive-rank sum is the evidence.
    Upper,
}

/// The outcome of an exact one-sided Wilcoxon signed-rank test.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SignedRank {
    /// The non-zero differences, which are the ones ranked.
    pub n: usize,
    /// The sum of the ranks of the positive differences (tied absolute values share their mean rank).
    pub w_plus: f64,
    /// The exact one-sided p-value.
    pub p: f64,
}

/// The exact one-sided Wilcoxon signed-rank test of `differences`.
///
/// Zero differences are dropped (Wilcoxon's treatment). The rest are ranked by absolute value, and tied
/// absolute values share their mean rank. Under the null hypothesis each difference is as likely to be
/// positive as negative, so `w_plus` is distributed as the rank sum of a uniformly random subset of the
/// ranks. The p-value counts that distribution exactly over all `2^n` subsets, with doubled ranks so that
/// mean ranks stay integral; the counts fit `u128` for up to 127 differences. Without a non-zero
/// difference the p-value is 1.
pub fn signed_rank(differences: &[f64], tail: Tail) -> SignedRank {
    let mut nonzero: Vec<f64> = differences.iter().copied().filter(|d| *d != 0.0).collect();
    nonzero.sort_by(|a, b| a.abs().total_cmp(&b.abs()));
    let n = nonzero.len();
    // Twice each rank: the tie group at sorted positions start..end holds ranks start + 1 ..= end, whose
    // mean doubled is start + 1 + end.
    let mut doubled = vec![0usize; n];
    let mut start = 0;
    while start < n {
        let size = nonzero[start].abs();
        let end = (start..n).find(|&k| nonzero[k].abs() != size).unwrap_or(n);
        doubled[start..end].fill(start + 1 + end);
        start = end;
    }
    let observed: usize = nonzero
        .iter()
        .zip(&doubled)
        .filter(|(d, _)| **d > 0.0)
        .map(|(_, rank)| rank)
        .sum();
    let total: usize = doubled.iter().sum();
    // ways[s]: how many subsets of the ranks have the doubled rank sum s.
    let mut ways = vec![0u128; total + 1];
    ways[0] = 1;
    for &rank in &doubled {
        for s in (rank..=total).rev() {
            ways[s] += ways[s - rank];
        }
    }
    let tail_ways: u128 = match tail {
        Tail::Lower => ways[..=observed].iter().sum(),
        Tail::Upper => ways[observed..].iter().sum(),
    };
    SignedRank {
        n,
        w_plus: observed as f64 / 2.0,
        p: tail_ways as f64 / (n as f64).exp2(),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Differences of absolute values 1..=n, with the positive ones chosen so that their rank sum is `w`.
    fn with_w_plus(n: usize, mut w: usize) -> Vec<f64> {
        let mut differences = vec![0.0; n];
        for rank in (1..=n).rev() {
            let positive = rank <= w;
            if positive {
                w -= rank;
            }
            differences[rank - 1] = if positive {
                rank as f64
            } else {
                -(rank as f64)
            };
        }
        assert_eq!(w, 0, "every sum up to n(n+1)/2 is a sum of distinct ranks");
        differences
    }

    /// The share of sign assignments whose positive-rank sum `w` satisfies `keep(w, observed)`, by
    /// enumerating all of them, with mean ranks computed by counting.
    fn enumerate(differences: &[f64], keep: impl Fn(f64, f64) -> bool) -> f64 {
        let nonzero: Vec<f64> = differences.iter().copied().filter(|d| *d != 0.0).collect();
        let n = nonzero.len();
        let rank = |i: usize| {
            let size = nonzero[i].abs();
            let below = nonzero.iter().filter(|d| d.abs() < size).count();
            let equal = nonzero.iter().filter(|d| d.abs() == size).count();
            below as f64 + (equal as f64 + 1.0) / 2.0
        };
        let observed: f64 = (0..n).filter(|&i| nonzero[i] > 0.0).map(rank).sum();
        let hits = (0u32..1 << n)
            .filter(|mask| {
                let w: f64 = (0..n).filter(|i| mask & (1 << i) != 0).map(rank).sum();
                keep(w, observed)
            })
            .count();
        hits as f64 / f64::from(1u32 << n)
    }

    fn brute_force(differences: &[f64], tail: Tail) -> f64 {
        match tail {
            Tail::Lower => enumerate(differences, |w, observed| w <= observed),
            Tail::Upper => enumerate(differences, |w, observed| w >= observed),
        }
    }

    #[test]
    fn five_pairs_all_in_one_direction_give_one_in_thirty_two() {
        // docs/testing/perf.md: with exactly 5 pairs, p < 0.05 needs all five in the regression direction.
        let down = [-3.0, -1.0, -4.0, -1.5, -9.0];
        assert_eq!(signed_rank(&down, Tail::Lower).p, 1.0 / 32.0);
        assert_eq!(signed_rank(&down, Tail::Upper).p, 1.0);
        let up = down.map(|d| -d);
        assert_eq!(signed_rank(&up, Tail::Upper).p, 1.0 / 32.0);
        let four_of_five = [-3.0, 1.0, -4.0, -1.5, -9.0];
        assert!(signed_rank(&four_of_five, Tail::Lower).p >= 0.05);
    }

    #[test]
    fn exact_values_counted_from_subsets() {
        // P(W+ <= w) is the number of subsets of {1..n} with sum at most w, over 2^n.
        for (n, w, subsets) in [(5, 0, 1), (6, 2, 3), (8, 5, 10), (10, 8, 25), (10, 10, 43)] {
            let result = signed_rank(&with_w_plus(n, w), Tail::Lower);
            assert_eq!(result.n, n);
            assert_eq!(result.w_plus, w as f64);
            assert_eq!(
                result.p,
                f64::from(subsets) / (n as f64).exp2(),
                "n {n} w {w}"
            );
        }
    }

    #[test]
    fn one_sided_critical_values_match_the_published_table() {
        // The largest W+ with a lower-tail p of at most 0.05, for n = 5..=20.
        let critical = [0, 2, 3, 5, 8, 10, 13, 17, 21, 25, 30, 35, 41, 47, 53, 60];
        for (n, &w) in (5..).zip(&critical) {
            let at = signed_rank(&with_w_plus(n, w), Tail::Lower).p;
            let above = signed_rank(&with_w_plus(n, w + 1), Tail::Lower).p;
            assert!(
                at <= 0.05 && above > 0.05,
                "n {n}: p({w}) = {at}, p({}) = {above}",
                w + 1
            );
        }
    }

    #[test]
    fn zeros_are_dropped_and_none_left_means_p_one() {
        assert_eq!(
            signed_rank(&[0.0, 0.0], Tail::Lower),
            SignedRank {
                n: 0,
                w_plus: 0.0,
                p: 1.0
            }
        );
        assert_eq!(signed_rank(&[], Tail::Upper).p, 1.0);
        let with_zeros = signed_rank(&[0.0, -2.0, 0.0, -1.0, -3.0, -4.0, -5.0], Tail::Lower);
        assert_eq!((with_zeros.n, with_zeros.p), (5, 1.0 / 32.0));
    }

    #[test]
    fn ties_share_their_mean_rank() {
        // |d| = 1, 2, 2, 3: ranks 1, 2.5, 2.5, 4. Positive: the 2 and the 3.
        let result = signed_rank(&[-1.0, 2.0, -2.0, 3.0], Tail::Upper);
        assert_eq!(result.w_plus, 6.5);
        assert_eq!(result.p, brute_force(&[-1.0, 2.0, -2.0, 3.0], Tail::Upper));
    }

    #[test]
    fn medians() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[5.0, 1.0, 3.0]), Some(3.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[10.0, 20.0]), Some(15.0));
    }

    /// Differences with plenty of zeros and ties: small integers, sometimes scaled.
    fn differences() -> impl Strategy<Value = Vec<f64>> {
        proptest::collection::vec((-4i32..=4, prop::bool::ANY), 0..=12).prop_map(|values| {
            values
                .into_iter()
                .map(|(v, scaled)| f64::from(v) * if scaled { 1.5 } else { 1.0 })
                .collect()
        })
    }

    proptest! {
        /// The counted distribution equals the enumeration of every sign assignment.
        #[test]
        fn matches_brute_force(differences in differences()) {
            for tail in [Tail::Lower, Tail::Upper] {
                let exact = signed_rank(&differences, tail).p;
                let brute = brute_force(&differences, tail);
                prop_assert!((exact - brute).abs() < 1e-12, "{tail:?}: {exact} vs {brute}");
            }
        }

        /// The two tails sum to one plus the probability of the observed statistic, which both count, and
        /// mirroring the differences swaps them.
        #[test]
        fn tails_are_complementary_and_mirror(differences in differences()) {
            let lower = signed_rank(&differences, Tail::Lower).p;
            let upper = signed_rank(&differences, Tail::Upper).p;
            let point = enumerate(&differences, |w, observed| w == observed);
            prop_assert!((lower + upper - 1.0 - point).abs() < 1e-12, "{lower} + {upper} vs 1 + {point}");
            let mirrored: Vec<f64> = differences.iter().map(|d| -d).collect();
            prop_assert_eq!(signed_rank(&mirrored, Tail::Upper).p, lower);
            prop_assert_eq!(signed_rank(&mirrored, Tail::Lower).p, upper);
        }
    }
}
