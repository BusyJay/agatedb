use super::builder::{Header, HEADER_SIZE};
use super::{Block, TableInner};
use crate::util::{self, KeyComparator, COMPARATOR};
use crate::value::Value;
use bytes::{Bytes, BytesMut};
use std::sync::Arc;

/// Errors that may encounter during iterator operation
#[derive(Clone, Debug)]
pub enum IteratorError {
    EOF,
    // TODO: As we need to clone Error from block iterator to table iterator,
    // we had to save `crate::Error` as String. In the future, we could let all
    // seek-related function to return a `Result<()>` instead of saving an
    // error inside struct.
    Error(String),
}

impl IteratorError {
    /// Check if iterator has reached its end
    pub fn is_eof(&self) -> bool {
        matches!(self, IteratorError::EOF)
    }

    /// Utility function to check if an Option<IteratorError> is EOF
    pub fn check_eof(err: &Option<IteratorError>) -> bool {
        matches!(err, Some(IteratorError::EOF))
    }
}

enum SeekPos {
    Origin,
    Current,
}

/// Block iterator iterates on an SST block
// TODO: support custom comparator
struct BlockIterator {
    /// current index of iterator
    idx: usize,
    /// base key of the block
    base_key: Bytes,
    /// key of current entry
    key: BytesMut,
    /// raw value of current entry
    val: Bytes,
    /// block data in bytes
    data: Bytes,
    /// block struct
    // TODO: use `&'a Block` if possible
    block: Arc<Block>,
    /// previous overlap key, used to construct key of current entry from
    /// previous one faster
    perv_overlap: u16,
    /// iterator error in last operation
    err: Option<IteratorError>,
}

impl BlockIterator {
    pub fn new(block: Arc<Block>) -> Self {
        let data = block.data.slice(..block.entries_index_start);
        Self {
            block,
            err: None,
            base_key: Bytes::new(),
            key: BytesMut::new(),
            val: Bytes::new(),
            data,
            perv_overlap: 0,
            idx: 0,
        }
    }

    /// Replace block inside iterator and reset the iterator
    pub fn set_block(&mut self, block: Arc<Block>) {
        self.err = None;
        self.idx = 0;
        self.base_key.clear();
        self.perv_overlap = 0;
        self.key.clear();
        self.val.clear();
        self.data = block.data.slice(..block.entries_index_start);
        self.block = block;
    }

    #[inline]
    fn entry_offsets(&self) -> &[u32] {
        &self.block.entry_offsets
    }

    fn set_idx(&mut self, i: usize) {
        self.idx = i;
        if i >= self.entry_offsets().len() {
            self.err = Some(IteratorError::EOF);
            return;
        }

        self.err = None;
        let start_offset = self.entry_offsets()[i] as u32;

        if self.base_key.is_empty() {
            let mut base_header = Header::default();
            base_header.decode(&mut self.data.slice(..));
            // TODO: combine this decode with header decode to avoid slice ptr copy
            self.base_key = self
                .data
                .slice(HEADER_SIZE..HEADER_SIZE + base_header.diff as usize);
        }

        let end_offset = if self.idx + 1 == self.entry_offsets().len() {
            self.data.len()
        } else {
            self.entry_offsets()[self.idx + 1] as usize
        };

        let mut entry_data = self.data.slice(start_offset as usize..end_offset as usize);
        let mut header = Header::default();
        header.decode(&mut entry_data);

        // TODO: merge this truncate with the following key truncate
        if header.overlap > self.perv_overlap {
            self.key.truncate(self.perv_overlap as usize);
            self.key.extend_from_slice(
                &self.base_key[self.perv_overlap as usize..header.overlap as usize],
            );
        }
        self.perv_overlap = header.overlap;

        let diff_key = &entry_data[..header.diff as usize];
        self.key.truncate(header.overlap as usize);
        self.key.extend_from_slice(diff_key);
        self.val = entry_data.slice(header.diff as usize..);
    }

    /// Check if last operation of iterator is error
    /// TODO: use `Result<()>` for all iterator operation and remove this if possible
    pub fn valid(&self) -> bool {
        self.err.is_none()
    }

    /// Return error of last operation
    pub fn error(&self) -> Option<&IteratorError> {
        self.err.as_ref()
    }

    /// Seek to the first entry that is equal or greater than key
    pub fn seek(&mut self, key: &Bytes, whence: SeekPos) {
        self.err = None;
        let start_index = match whence {
            SeekPos::Origin => 0,
            SeekPos::Current => self.idx,
        };

        let found_entry_idx = util::search(self.entry_offsets().len(), |idx| {
            use std::cmp::Ordering::*;
            if idx < start_index {
                return false;
            }
            self.set_idx(idx);
            match COMPARATOR.compare_key(&self.key, &key) {
                Less => false,
                _ => true,
            }
        });

        self.set_idx(found_entry_idx);
    }

    pub fn seek_to_first(&mut self) {
        self.set_idx(0);
    }

    pub fn seek_to_last(&mut self) {
        if self.entry_offsets().is_empty() {
            self.idx = std::usize::MAX;
            self.err = Some(IteratorError::EOF);
            return;
        } else {
            self.set_idx(self.entry_offsets().len() - 1);
        }
    }

    pub fn next(&mut self) {
        self.set_idx(self.idx + 1);
    }

    pub fn prev(&mut self) {
        if self.idx == 0 {
            self.idx = std::usize::MAX;
            self.err = Some(IteratorError::EOF);
            return;
        } else {
            self.set_idx(self.idx - 1);
        }
    }
}

// TODO: use `bitfield` if there are too many variants
pub const ITERATOR_REVERSED: usize = 1 << 1;
pub const ITERATOR_NOCACHE: usize = 1 << 2;

/// An iterator over SST.
///
/// Here we use generic because when initializaing a
/// table object, we need to get smallest and biggest
/// elements by using an iterator over `&TableInner`.
/// At that time, we could not build an `Arc<TableInner>`.
pub struct Iterator<T: AsRef<TableInner>> {
    table: T,
    bpos: isize,
    block_iterator: Option<BlockIterator>,
    err: Option<IteratorError>,
    opt: usize,
}

impl<T: AsRef<TableInner>> Iterator<T> {
    /// Create an iterator from `Arc<TableInner>` or `&TableInner`
    pub fn new(table: T, opt: usize) -> Self {
        Self {
            table,
            bpos: 0,
            block_iterator: None,
            err: None,
            opt,
        }
    }

    pub fn reset(&mut self) {
        self.bpos = 0;
        self.err = None;
    }

    pub fn valid(&self) -> bool {
        self.err.is_none()
    }

    pub fn use_cache(&self) -> bool {
        self.opt & ITERATOR_NOCACHE == 0
    }

    fn get_block_iterator(&mut self, block: Arc<Block>) -> &mut BlockIterator {
        if self.block_iterator.is_none() {
            self.block_iterator = Some(BlockIterator::new(block));
            self.block_iterator.as_mut().unwrap()
        } else {
            let iter = self.block_iterator.as_mut().unwrap();
            iter.set_block(block);
            iter
        }
    }

    pub fn seek_to_first(&mut self) {
        let num_blocks = self.table.as_ref().offsets_length();
        if num_blocks == 0 {
            self.err = Some(IteratorError::EOF);
            return;
        }
        self.bpos = 0;
        match self
            .table
            .as_ref()
            .block(self.bpos as usize, self.use_cache())
        {
            Ok(block) => {
                let block_iterator = self.get_block_iterator(block);
                block_iterator.seek_to_first();
                self.err = block_iterator.err.clone();
            }
            Err(err) => self.err = Some(IteratorError::Error(err.to_string())),
        }
    }

    pub fn seek_to_last(&mut self) {
        let num_blocks = self.table.as_ref().offsets_length();
        if num_blocks == 0 {
            self.err = Some(IteratorError::EOF);
            return;
        }
        self.bpos = num_blocks as isize - 1;
        match self
            .table
            .as_ref()
            .block(self.bpos as usize, self.use_cache())
        {
            Ok(block) => {
                let block_iterator = self.get_block_iterator(block);
                block_iterator.seek_to_last();
                self.err = block_iterator.err.clone();
            }
            Err(err) => self.err = Some(IteratorError::Error(err.to_string())),
        }
    }

    fn seek_helper(&mut self, block_idx: usize, key: &Bytes) {
        self.bpos = block_idx as isize;
        match self
            .table
            .as_ref()
            .block(self.bpos as usize, self.use_cache())
        {
            Ok(block) => {
                let block_iterator = self.get_block_iterator(block);
                block_iterator.seek(key, SeekPos::Origin);
                self.err = block_iterator.err.clone();
            }
            Err(err) => self.err = Some(IteratorError::Error(err.to_string())),
        }
    }

    fn seek_from(&mut self, key: &Bytes, whence: SeekPos) {
        self.err = None;
        match whence {
            SeekPos::Origin => self.reset(),
            _ => {}
        }

        let idx = util::search(self.table.as_ref().offsets_length(), |idx| {
            use std::cmp::Ordering::*;
            let block_offset = self.table.as_ref().offsets(idx).unwrap();
            match COMPARATOR.compare_key(&block_offset.key, &key) {
                Less => false,
                _ => true,
            }
        });

        if idx == 0 {
            self.seek_helper(0, key);
            return;
        }

        self.seek_helper(idx - 1, key);
        if IteratorError::check_eof(&self.err) {
            if idx == self.table.as_ref().offsets_length() {
                return;
            }
            self.seek_helper(idx, key);
        }
    }

    // seek_inner will reset iterator and seek to >= key.
    fn seek_inner(&mut self, key: &Bytes) {
        self.seek_from(key, SeekPos::Origin);
    }

    // seek_for_prev will reset iterator and seek to <= key.
    pub fn seek_for_prev(&mut self, key: &Bytes) {
        self.seek_from(key, SeekPos::Origin);
        if self.key() != key {
            self.prev_inner();
        }
    }

    fn next_inner(&mut self) {
        self.err = None;

        if self.bpos as usize > self.table.as_ref().offsets_length() {
            self.err = Some(IteratorError::EOF);
            return;
        }

        if self.block_iterator.as_ref().unwrap().data.is_empty() {
            match self
                .table
                .as_ref()
                .block(self.bpos as usize, self.use_cache())
            {
                Ok(block) => {
                    let block_iterator = self.get_block_iterator(block);
                    block_iterator.seek_to_first();
                    self.err = block_iterator.err.clone();
                }
                Err(err) => self.err = Some(IteratorError::Error(err.to_string())),
            }
        } else {
            let bi = self.block_iterator.as_mut().unwrap();
            bi.next();
            if !bi.valid() {
                self.bpos += 1;
                bi.data.clear();
                self.next_inner();
            }
        }
    }

    fn prev_inner(&mut self) {
        self.err = None;

        if self.bpos < 0 {
            self.err = Some(IteratorError::EOF);
            return;
        }

        if self.block_iterator.as_ref().unwrap().data.is_empty() {
            match self
                .table
                .as_ref()
                .block(self.bpos as usize, self.use_cache())
            {
                Ok(block) => {
                    let block_iterator = self.get_block_iterator(block);
                    block_iterator.seek_to_last();
                    self.err = block_iterator.err.clone();
                }
                Err(err) => self.err = Some(IteratorError::Error(err.to_string())),
            }
        } else {
            let bi = self.block_iterator.as_mut().unwrap();
            bi.prev();
            if !bi.valid() {
                self.bpos -= 1;
                bi.data.clear();
                self.prev_inner();
            }
        }
    }

    pub fn key(&self) -> &[u8] {
        &self.block_iterator.as_ref().unwrap().key
    }

    pub fn value(&self) -> Value {
        let mut value = Value::default();
        value.decode(&self.block_iterator.as_ref().unwrap().val);
        value
    }

    pub fn next(&mut self) {
        if self.opt & ITERATOR_REVERSED == 0 {
            self.next_inner();
        } else {
            self.prev_inner();
        }
    }

    pub(crate) fn prev(&mut self) {
        if self.opt & ITERATOR_REVERSED == 0 {
            self.prev_inner();
        } else {
            self.next_inner();
        }
    }

    pub fn rewind(&mut self) {
        if self.opt & ITERATOR_REVERSED == 0 {
            self.seek_to_first();
        } else {
            self.seek_to_last();
        }
    }

    pub fn seek(&mut self, key: &Bytes) {
        if self.opt & ITERATOR_REVERSED == 0 {
            self.seek_inner(key);
        } else {
            self.seek_for_prev(key);
        }
    }

    pub fn error(&self) -> Option<&IteratorError> {
        self.err.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iterator_error() {
        let ite1 = IteratorError::EOF;
        assert!(ite1.is_eof());

        let ite3 = IteratorError::Error("23333".to_string());
        assert!(!ite3.is_eof());
    }
}