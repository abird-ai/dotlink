//! Streaming sequence matching for ordered tool inputs. No candidate-position
//! vectors, no quadratic prefix comparisons, and no content hashing.

/// Visit every (including overlapping) occurrence of a nonempty needle in a
/// haystack, starting at `start`. The KMP failure table is O(needle.len());
/// comparisons are O(needle.len() + haystack.len()). Callers supply their own
/// equivalence (exact lines, whitespace-normalized lines, etc.) and decide
/// how many matching positions to retain.
pub(crate) fn scan_sequence<P, H>(
    needle: &[P],
    haystack: &[H],
    start: usize,
    pattern_eq: impl Fn(&P, &P) -> bool,
    item_eq: impl Fn(&H, &P) -> bool,
    mut on_match: impl FnMut(usize),
) {
    if needle.is_empty() || start >= haystack.len() || needle.len() > haystack.len() - start {
        return;
    }

    let mut failure = vec![0_usize; needle.len()];
    let mut matched = 0;
    for index in 1..needle.len() {
        while matched > 0 && !pattern_eq(&needle[index], &needle[matched]) {
            matched = failure[matched - 1];
        }
        if pattern_eq(&needle[index], &needle[matched]) {
            matched += 1;
        }
        failure[index] = matched;
    }

    matched = 0;
    for (index, item) in haystack.iter().enumerate().skip(start) {
        while matched > 0 && !item_eq(item, &needle[matched]) {
            matched = failure[matched - 1];
        }
        if item_eq(item, &needle[matched]) {
            matched += 1;
        }
        if matched == needle.len() {
            on_match(index + 1 - matched);
            matched = failure[matched - 1];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_overlapping_sequences_and_honors_start() {
        let source = [1, 1, 1, 1, 1];
        let mut matches = Vec::new();
        scan_sequence(
            &[1, 1],
            &source,
            0,
            |a, b| a == b,
            |a, b| a == b,
            |at| matches.push(at),
        );
        assert_eq!(matches, [0, 1, 2, 3]);
        matches.clear();
        scan_sequence(
            &[1, 1],
            &source,
            2,
            |a, b| a == b,
            |a, b| a == b,
            |at| matches.push(at),
        );
        assert_eq!(matches, [2, 3]);
    }

    #[test]
    fn repetitive_long_prefixes_take_linear_comparison_work() {
        use std::cell::Cell;
        let source = vec!["x"; 16_000];
        let mut needle = vec!["x"; 3_999];
        needle.push("Y");
        let comparisons = Cell::new(0_usize);
        let mut hits = 0_usize;
        scan_sequence(
            &needle,
            &source,
            0,
            |a, b| {
                comparisons.set(comparisons.get() + 1);
                a == b
            },
            |a, b| {
                comparisons.set(comparisons.get() + 1);
                a == b
            },
            |_| hits += 1,
        );
        assert_eq!(hits, 0);
        assert!(
            comparisons.get() < (needle.len() + source.len()) * 4,
            "{} comparisons for {} source/needle elements",
            comparisons.get(),
            source.len()
        );
    }

    #[test]
    fn randomized_sequence_scan_matches_brute_force() {
        for seed in 0_u32..600 {
            let mut state = seed + 1;
            let mut next = || {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                state
            };
            let size = (next() % 80) as usize;
            let needle_size = (next() % 25 + 1) as usize;
            let source = (0..size).map(|_| next() % 5).collect::<Vec<_>>();
            let needle = (0..needle_size).map(|_| next() % 5).collect::<Vec<_>>();
            let start = (next() % 100) as usize;
            let expected = (start..=source.len().saturating_sub(needle.len()))
                .filter(|at| source.get(*at..at.saturating_add(needle.len())) == Some(&needle[..]))
                .collect::<Vec<_>>();
            let mut got = Vec::new();
            scan_sequence(
                &needle,
                &source,
                start,
                |a, b| a == b,
                |a, b| a == b,
                |at| got.push(at),
            );
            assert_eq!(got, expected, "seed={seed}");
        }
    }
}
