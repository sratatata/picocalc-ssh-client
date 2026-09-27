//! Ring arithmetic for a free-running DMA capture buffer.
//!
//! The firmware's push-to-talk capture lets the RP2350's DMA copy the PIO RX
//! FIFO into a circular buffer forever (transfer-count "endless" mode with the
//! write address wrapped on the ring), and a task drains that buffer at
//! leisure. That removes the CPU from the per-chunk loop: with a one-shot
//! transfer per chunk, the eight-word FIFO fills while the next transfer is
//! being armed, the state machine stalls on `in`, and the microphone's bit
//! clock stops for as long as the CPU was late - which is the mechanism that
//! silences or glitches captures under load.
//!
//! The wrap arithmetic that a reader of such a buffer needs is pure and lives
//! here, host-tested, rather than being written twice in the firmware and in
//! its tests.

/// Words the DMA has written into the ring since the reader last caught up.
///
/// `write_index` is the DMA's live write position and `read_index` the
/// reader's, both in words modulo `capacity`. The two wrap positions are
/// indistinguishable when the DMA has completed exactly one lap, so a reader
/// that can be late by more than a full ring has to notice that itself (the
/// firmware compares the time since its last poll against the ring's duration
/// and resynchronises when it must have been lapped).
pub const fn available_words(write_index: usize, read_index: usize, capacity: usize) -> usize {
    (write_index + capacity - read_index) % capacity
}

/// Splits a run of `count` words starting at `read_index` in a ring of
/// `capacity` words into at most two contiguous `(start, len)` segments, in
/// order. `count` must not exceed `capacity`.
pub fn segments(read_index: usize, count: usize, capacity: usize) -> [(usize, usize); 2] {
    if count == 0 {
        return [(read_index, 0), (0, 0)];
    }
    let first = core::cmp::min(count, capacity - read_index);
    [(read_index, first), (0, count - first)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_counts_written_words_without_wrapping() {
        assert_eq!(available_words(10, 4, 16), 6);
        assert_eq!(available_words(4, 4, 16), 0);
    }

    #[test]
    fn available_counts_across_the_wrap() {
        // 2 written at the end of the ring, 3 at the start, 5 unread.
        assert_eq!(available_words(3, 14, 16), 5);
    }

    #[test]
    fn available_is_zero_after_a_full_lap() {
        // A lap is indistinguishable from no progress; the reader must detect
        // that case itself (see the module docs).
        assert_eq!(available_words(4, 4, 16), 0);
    }

    #[test]
    fn segments_are_one_run_when_unwrapped() {
        assert_eq!(segments(4, 5, 16), [(4, 5), (0, 0)]);
        assert_eq!(segments(4, 0, 16), [(4, 0), (0, 0)]);
    }

    #[test]
    fn segments_split_at_the_wrap() {
        assert_eq!(segments(14, 5, 16), [(14, 2), (0, 3)]);
    }

    #[test]
    fn segments_cover_a_whole_ring_as_one_run() {
        assert_eq!(segments(0, 16, 16), [(0, 16), (0, 0)]);
    }

    /// Whatever the start position and the amount, the segments must describe
    /// exactly `count` words in order, never more than the ring holds and never
    /// running past its end.
    #[test]
    fn segments_are_always_in_bounds_and_exact() {
        for capacity in [2usize, 4, 8, 16] {
            for read_index in 0..capacity {
                for count in 0..=capacity {
                    let [(a_start, a_len), (b_start, b_len)] =
                        segments(read_index, count, capacity);
                    assert_eq!(a_len + b_len, count, "count {count} at {read_index}");
                    assert!(a_start + a_len <= capacity, "first segment in bounds");
                    assert!(b_start + b_len <= capacity, "second segment in bounds");
                    assert_eq!(a_start, read_index);
                    assert!(b_len == 0 || b_start == 0);
                    // Reading the segments in order visits exactly the run's
                    // words, in the order the DMA wrote them.
                    let visited: Vec<usize> = (a_start..a_start + a_len)
                        .chain(b_start..b_start + b_len)
                        .collect();
                    let expected: Vec<usize> =
                        (0..count).map(|i| (read_index + i) % capacity).collect();
                    assert_eq!(visited, expected, "run {count} at {read_index}");
                }
            }
        }
    }
}
