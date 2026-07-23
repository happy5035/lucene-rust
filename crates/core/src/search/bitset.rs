//! FixedBitSet for the multi-term bitset-materialization path (search spec
//! M2 §4): per-segment doc set of `max_doc` bits. Mirrors the slice of
//! util/FixedBitSet.java we need (set :239, nextSetBit :274, cardinality :193).

/// A fixed-size bit set over `[0, num_bits)`, backed by u64 words
/// (FixedBitSet.java:124).
pub struct FixedBitSet {
    bits: Vec<u64>,
    num_bits: usize,
}

impl FixedBitSet {
    /// All bits clear.
    pub fn new(num_bits: usize) -> Self {
        FixedBitSet {
            bits: vec![0u64; (num_bits + 63) / 64],
            num_bits,
        }
    }

    /// FixedBitSet.numBits.
    pub fn num_bits(&self) -> usize {
        self.num_bits
    }

    /// FixedBitSet.set(int) (:239).
    pub fn set(&mut self, index: usize) {
        debug_assert!(index < self.num_bits);
        self.bits[index >> 6] |= 1u64 << (index & 63);
    }

    /// FixedBitSet.get(int).
    pub fn get(&self, index: usize) -> bool {
        debug_assert!(index < self.num_bits);
        self.bits[index >> 6] & (1u64 << (index & 63)) != 0
    }

    /// FixedBitSet.nextSetBit(int) (:274): smallest set bit >= `from`,
    /// or None. Bits at index >= num_bits are never returned.
    pub fn next_set_bit(&self, from: usize) -> Option<usize> {
        if from >= self.num_bits {
            return None;
        }
        let mut word_idx = from >> 6;
        let mut word = self.bits[word_idx] & (u64::MAX << (from & 63));
        loop {
            if word != 0 {
                let idx = (word_idx << 6) + word.trailing_zeros() as usize;
                return if idx < self.num_bits { Some(idx) } else { None };
            }
            word_idx += 1;
            if word_idx == self.bits.len() {
                return None;
            }
            word = self.bits[word_idx];
        }
    }

    /// FixedBitSet.cardinality() (:193).
    pub fn popcount(&self) -> u64 {
        self.bits.iter().map(|w| w.count_ones() as u64).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::FixedBitSet;

    #[test]
    fn new_set_all_clear() {
        let b = FixedBitSet::new(130);
        assert_eq!(b.num_bits(), 130);
        assert_eq!(b.popcount(), 0);
        assert!(!b.get(0));
        assert!(!b.get(129));
        assert_eq!(b.next_set_bit(0), None);
    }

    #[test]
    fn set_get_round_trip_across_words() {
        let mut b = FixedBitSet::new(130);
        for i in [0usize, 1, 63, 64, 65, 127, 128, 129] {
            b.set(i);
        }
        for i in 0..130 {
            assert_eq!(
                b.get(i),
                [0, 1, 63, 64, 65, 127, 128, 129].contains(&i),
                "bit {i}"
            );
        }
        assert_eq!(b.popcount(), 8);
    }

    #[test]
    fn next_set_bit_scans_gaps_and_words() {
        let mut b = FixedBitSet::new(200);
        b.set(5);
        b.set(64);
        b.set(199);
        assert_eq!(b.next_set_bit(0), Some(5));
        assert_eq!(b.next_set_bit(5), Some(5)); // inclusive
        assert_eq!(b.next_set_bit(6), Some(64));
        assert_eq!(b.next_set_bit(64), Some(64));
        assert_eq!(b.next_set_bit(65), Some(199));
        assert_eq!(b.next_set_bit(199), Some(199));
        assert_eq!(b.next_set_bit(200), None);
    }

    #[test]
    fn next_set_bit_respects_num_bits() {
        // bits beyond num_bits must never be returned even though the
        // backing word has room for them
        let mut b = FixedBitSet::new(3);
        b.set(2);
        assert_eq!(b.next_set_bit(3), None);
        assert_eq!(b.popcount(), 1);
    }

    #[test]
    fn popcount_dense() {
        let mut b = FixedBitSet::new(128);
        for i in 0..128 {
            b.set(i);
        }
        assert_eq!(b.popcount(), 128);
        assert_eq!(b.next_set_bit(127), Some(127));
        assert_eq!(b.next_set_bit(128), None);
    }
}
