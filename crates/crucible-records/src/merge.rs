//! The merge of a `groupUniqArrayArray(N)` column, shared by the aggregating rows.

/// Merges the distinct values of `other` into `into`, keeping the `cap` smallest distinct values in ascending
/// order.
///
/// The result is a set function of the inputs, so the merge is commutative and associative. ClickHouse's own merge
/// may keep a different subset once the union exceeds `cap`; below the cap they agree as sets.
pub(crate) fn merge_distinct<T: Ord + Clone>(into: &mut Vec<T>, other: &[T], cap: usize) {
    into.extend_from_slice(other);
    into.sort_unstable();
    into.dedup();
    into.truncate(cap);
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn merged(a: &[u8], b: &[u8], cap: usize) -> Vec<u8> {
        let mut out = a.to_vec();
        merge_distinct(&mut out, b, cap);
        out
    }

    #[test]
    fn keeps_the_smallest_distinct_values() {
        assert_eq!(merged(&[3, 1], &[2, 1, 5], 3), vec![1, 2, 3]);
        assert_eq!(merged(&[], &[], 4), Vec::<u8>::new());
    }

    proptest! {
        #[test]
        fn is_commutative_and_associative(
            a in proptest::collection::vec(0u8..20, 0..10),
            b in proptest::collection::vec(0u8..20, 0..10),
            c in proptest::collection::vec(0u8..20, 0..10),
            cap in 0usize..12,
        ) {
            // Inputs as produced by earlier merges: distinct, ascending, capped.
            let a = merged(&[], &a, cap);
            let b = merged(&[], &b, cap);
            let c = merged(&[], &c, cap);
            prop_assert_eq!(merged(&a, &b, cap), merged(&b, &a, cap));
            prop_assert_eq!(merged(&merged(&a, &b, cap), &c, cap), merged(&a, &merged(&b, &c, cap), cap));
        }
    }
}
