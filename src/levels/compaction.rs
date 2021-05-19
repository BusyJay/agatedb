use std::collections::HashSet;

use std::sync::{Arc, RwLock};

use bytes::{Bytes, BytesMut};

use super::LevelHandler;
use crate::format::{key_with_ts_first, key_with_ts_last, user_key};
use crate::util::{KeyComparator, COMPARATOR};
use crate::{Error, Result, Table};

/// Represents a range of keys from `left` to `right`
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum KeyRange {
    Range { left: Bytes, right: Bytes },
    Inf,
    Empty,
}

impl KeyRange {
    pub fn new(left: Bytes, right: Bytes) -> Self {
        // left must <= right
        assert!(COMPARATOR.compare_key(&left, &right) != std::cmp::Ordering::Greater);
        Self::Range { left, right }
    }

    /// Extend current key range with another
    pub fn extend(&self, other: &Self) -> Self {
        use KeyRange::{Empty, Inf, Range};
        match (self, other) {
            (current, Empty) => current.clone(),
            (Empty, dest) => dest.clone(),
            (Inf, _) | (_, Inf) => Inf,
            (
                Range {
                    left: self_left,
                    right: self_right,
                },
                Range {
                    left: other_left,
                    right: other_right,
                },
            ) => {
                let left =
                    if COMPARATOR.compare_key(other_left, self_left) == std::cmp::Ordering::Less {
                        other_left
                    } else {
                        self_left
                    }
                    .clone();
                let right = if COMPARATOR.compare_key(other_right, self_right)
                    == std::cmp::Ordering::Greater
                {
                    other_right
                } else {
                    self_right
                }
                .clone();
                Range { left, right }
            }
        }
    }

    /// Check if two key ranges overlap
    pub fn overlaps_with(&self, other: &Self) -> bool {
        use KeyRange::{Empty, Inf, Range};
        match (self, other) {
            // If my range is empty, other ranges always overlap with me
            (Empty, _) => true,
            // If my range is not empty, other empty ranges always don't overlap with me
            (_, Empty) => false,
            (Inf, _) | (_, Inf) => true,
            (
                Range {
                    left: self_left,
                    right: self_right,
                },
                Range {
                    left: other_left,
                    right: other_right,
                },
            ) => {
                // [other_left, other_right] ... [self_left, self_right]
                if COMPARATOR.compare_key(other_right, self_left) == std::cmp::Ordering::Less {
                    return false;
                }
                // [self_left, self_right] ... [other_left, other_right]
                if COMPARATOR.compare_key(self_right, other_left) == std::cmp::Ordering::Less {
                    return false;
                }
                true
            }
        }
    }

    /// Returns `true` if current key range is infinite
    pub fn is_inf(&self) -> bool {
        return matches!(self, KeyRange::Inf);
    }

    /// Returns `true` if current key range is empty
    pub fn is_empty(&self) -> bool {
        return matches!(self, KeyRange::Empty);
    }
}

#[derive(Default)]
pub struct LevelCompactStatus {
    pub ranges: Vec<KeyRange>,
    pub del_size: u64,
}

impl LevelCompactStatus {
    /// Remove a `KeyRange` from level ranges, return `true` if success.
    pub fn remove(&mut self, dst: &KeyRange) -> bool {
        let prev_ranges_len = self.ranges.len();
        self.ranges.retain(|r| r != dst);
        prev_ranges_len != self.ranges.len()
    }

    pub fn overlaps_with(&self, dst: &KeyRange) -> bool {
        self.ranges.iter().any(|r| r.overlaps_with(dst))
    }
}

pub struct CompactStatus {
    pub levels: Vec<LevelCompactStatus>,
    pub tables: HashSet<u64>,
}

#[derive(Clone)]
pub struct CompactDef {
    pub compactor_id: usize,
    pub this_level: Arc<RwLock<LevelHandler>>,
    pub next_level: Arc<RwLock<LevelHandler>>,
    pub this_level_id: usize,
    pub next_level_id: usize,
    pub targets: Targets,
    pub prios: CompactionPriority,

    pub this_range: KeyRange,
    pub next_range: KeyRange,
    pub splits: Vec<KeyRange>,

    pub top: Vec<Table>,
    pub bot: Vec<Table>,

    pub this_size: u64,

    pub drop_prefixes: Vec<Bytes>,
}

impl CompactDef {
    pub fn new(
        compactor_id: usize,
        this_level: Arc<RwLock<LevelHandler>>,
        this_level_id: usize,
        next_level: Arc<RwLock<LevelHandler>>,
        next_level_id: usize,
        prios: CompactionPriority,
        targets: Targets,
    ) -> Self {
        Self {
            compactor_id,
            this_level,
            next_level,
            this_level_id,
            next_level_id,
            this_range: KeyRange::Empty,
            next_range: KeyRange::Empty,
            splits: vec![],
            this_size: 0,
            drop_prefixes: vec![],
            top: vec![],
            bot: vec![],
            targets,
            prios,
        }
    }

    pub fn all_tables(&self) -> Vec<Table> {
        let mut tables = self.top.clone();
        tables.append(&mut self.bot.clone());
        tables
    }
}

impl CompactStatus {
    pub fn delete(&mut self, compact_def: &CompactDef) {
        let this_level_id = compact_def.this_level_id;
        assert!(
            this_level_id < self.levels.len() - 1,
            "compaction on invalid level"
        );

        let this_level = &mut self.levels[this_level_id];
        let next_level_id = compact_def.next_level_id;

        this_level.del_size -= compact_def.this_size;
        let mut found = this_level.remove(&compact_def.this_range);

        if !compact_def.next_range.is_empty() {
            let next_level = &mut self.levels[next_level_id];
            found = next_level.remove(&compact_def.next_range) && found;
        }

        if !found {
            let this = compact_def.this_range.clone();
            let next = compact_def.next_range.clone();
            panic!("try looking for {:?} in this level and {:?} in next level, but key range not found", this, next);
        }

        for table in compact_def.top.iter() {
            assert!(self.tables.remove(&table.id()));
        }

        for table in compact_def.bot.iter() {
            assert!(self.tables.remove(&table.id()));
        }
    }

    pub fn compare_and_add(&mut self, compact_def: &CompactDef) -> Result<()> {
        let this_level = compact_def.this_level_id;
        assert!(
            this_level < self.levels.len() - 1,
            "compaction on invalid level"
        );

        let next_level = compact_def.next_level_id;
        if self.levels[this_level].overlaps_with(&compact_def.this_range) {
            return Err(Error::CompactionError(format!(
                "{:?} overlap with this level {} {:?}",
                compact_def.this_range, compact_def.this_level_id, self.levels[this_level].ranges
            )));
        }
        if self.levels[next_level].overlaps_with(&compact_def.next_range) {
            return Err(Error::CompactionError(format!(
                "{:?} overlap with next level {} {:?}",
                compact_def.next_range, compact_def.next_level_id, self.levels[next_level].ranges
            )));
        }

        // TODO: if we could sort key range by left point, we could use
        // binary search throughout this module.
        self.levels[this_level]
            .ranges
            .push(compact_def.this_range.clone());
        self.levels[next_level]
            .ranges
            .push(compact_def.next_range.clone());

        self.levels[this_level].del_size += compact_def.this_size;

        for table in compact_def.top.iter() {
            assert!(self.tables.insert(table.id()));
        }

        for table in compact_def.bot.iter() {
            assert!(self.tables.insert(table.id()));
        }

        Ok(())
    }

    pub fn overlaps_with(&self, level: usize, this: &KeyRange) -> bool {
        let this_level = &self.levels[level];
        this_level.overlaps_with(this)
    }
}

#[derive(Clone, Debug)]
pub struct CompactionPriority {
    pub level: usize,
    pub score: f64,
    pub adjusted: f64,
    pub drop_prefixes: Vec<Bytes>,
    pub targets: Targets,
}

#[derive(Clone, Debug)]
pub struct Targets {
    pub base_level: usize,
    pub target_size: Vec<u64>,
    pub file_size: Vec<u64>,
}

impl Targets {
    pub fn new() -> Self {
        Self {
            base_level: 0,
            target_size: vec![],
            file_size: vec![],
        }
    }
}

pub fn get_key_range(tables: &[Table]) -> Option<KeyRange> {
    if tables.is_empty() {
        return None;
    }

    let mut smallest = tables[0].smallest();
    let mut biggest = tables[0].biggest();

    for item in tables.iter().skip(1) {
        if COMPARATOR.compare_key(item.smallest(), &smallest) == std::cmp::Ordering::Less {
            smallest = item.smallest();
        }
        if COMPARATOR.compare_key(item.biggest(), &biggest) == std::cmp::Ordering::Greater {
            biggest = item.biggest();
        }
    }
    let mut smallest_buf = BytesMut::with_capacity(smallest.len() + 8);
    let mut biggest_buf = BytesMut::with_capacity(biggest.len() + 8);
    smallest_buf.extend_from_slice(user_key(&smallest));
    biggest_buf.extend_from_slice(user_key(&biggest));
    return Some(KeyRange::new(
        key_with_ts_first(smallest_buf),
        // the appended key will be `<biggest_key><u64::MAX>`.
        key_with_ts_last(biggest_buf),
    ));
}

pub fn get_key_range_single(table: &Table) -> KeyRange {
    get_key_range(std::slice::from_ref(table)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keyrange_non_overlap() {
        let k1 = KeyRange::new(
            Bytes::from_static(b"000000000000"),
            Bytes::from_static(b"dddd00000000"),
        );
        let k2 = KeyRange::new(
            Bytes::from_static(b"eeee00000000"),
            Bytes::from_static(b"ffff00000000"),
        );

        assert_eq!(
            k1.extend(&k2),
            KeyRange::new(
                Bytes::from_static(b"000000000000"),
                Bytes::from_static(b"ffff00000000")
            )
        );

        assert_eq!(
            k2.extend(&k1),
            KeyRange::new(
                Bytes::from_static(b"000000000000"),
                Bytes::from_static(b"ffff00000000")
            )
        );

        assert!(!k1.overlaps_with(&k2));
        assert!(!k2.overlaps_with(&k1));
    }

    #[test]
    fn test_keyrange_overlap() {
        let k1 = KeyRange::new(
            Bytes::from_static(b"000000000000"),
            Bytes::from_static(b"eeee00000000"),
        );
        let k2 = KeyRange::new(
            Bytes::from_static(b"dddd00000000"),
            Bytes::from_static(b"ffff00000000"),
        );

        assert_eq!(
            k1.extend(&k2),
            KeyRange::new(
                Bytes::from_static(b"000000000000"),
                Bytes::from_static(b"ffff00000000")
            )
        );

        assert_eq!(
            k2.extend(&k1),
            KeyRange::new(
                Bytes::from_static(b"000000000000"),
                Bytes::from_static(b"ffff00000000")
            )
        );

        assert!(k1.overlaps_with(&k2));
        assert!(k2.overlaps_with(&k1));
    }

    #[test]
    fn test_keyrange_inf() {
        let k1 = KeyRange::Inf;
        let k2 = KeyRange::new(
            Bytes::from_static(b"dddd00000000"),
            Bytes::from_static(b"ffff00000000"),
        );

        assert_eq!(k1.extend(&k2), KeyRange::Inf);
        assert_eq!(k2.extend(&k1), KeyRange::Inf);
        assert_eq!(k1.extend(&KeyRange::Empty), k1);
        assert_eq!(k2.extend(&KeyRange::Empty), k2);
        assert!(!KeyRange::Inf.overlaps_with(&KeyRange::Empty));
        assert!(KeyRange::Empty.overlaps_with(&KeyRange::Inf));
        assert!(KeyRange::Empty.overlaps_with(&KeyRange::Empty));
    }
}
