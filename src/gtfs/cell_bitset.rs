/// Compact bitset over partition cells for FLASH-TB arc-flags.
/// Supports any number of cells using `Vec<u64>` internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellBitSet {
    words: Vec<u64>,
}

impl CellBitSet {
    pub fn new(num_cells: u32) -> Self {
        let n_words = (num_cells as usize + 63) / 64;
        Self {
            words: vec![0; n_words],
        }
    }

    /// Grow to hold `num_cells` bits when currently empty (lazy init after pack).
    pub fn ensure_sized(&mut self, num_cells: u32) {
        let n_words = (num_cells as usize + 63) / 64;
        if self.words.len() < n_words {
            self.words.resize(n_words, 0);
        }
    }

    pub fn set_bit(&mut self, c: usize) {
        let word_idx = c / 64;
        let bit_idx = c % 64;
        if word_idx < self.words.len() {
            self.words[word_idx] |= 1 << bit_idx;
        }
    }

    pub fn get_bit(&self, c: usize) -> bool {
        let word_idx = c / 64;
        let bit_idx = c % 64;
        if word_idx < self.words.len() {
            (self.words[word_idx] & (1 << bit_idx)) != 0
        } else {
            false
        }
    }

    pub fn intersects(&self, other: &CellBitSet) -> bool {
        for (w1, w2) in self.words.iter().zip(other.words.iter()) {
            if (w1 & w2) != 0 {
                return true;
            }
        }
        false
    }

    pub fn union_with(&mut self, other: &CellBitSet) {
        if self.words.len() < other.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        for (i, &w2) in other.words.iter().enumerate() {
            self.words[i] |= w2;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }
}

impl Default for CellBitSet {
    fn default() -> Self {
        Self { words: Vec::new() }
    }
}
