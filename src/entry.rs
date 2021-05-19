use crate::value::{ValuePointer, VALUE_DELETE};
use bytes::Bytes;

#[derive(Clone)]
pub struct Entry {
    pub key: Bytes,
    pub value: Bytes,
    pub(crate) meta: u8,
    pub user_meta: u8,
    pub expires_at: u64,
    pub(crate) version: u64,
}

pub struct EntryRef<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
    pub(crate) meta: u8,
    pub user_meta: u8,
    pub expires_at: u64,
    pub(crate) version: u64,
}

impl<'a> EntryRef<'a> {
    pub fn is_zero(&self) -> bool {
        self.key.is_empty()
    }
}

impl Entry {
    pub fn new(key: Bytes, value: Bytes) -> Entry {
        Entry {
            key,
            value,
            meta: 0,
            user_meta: 0,
            expires_at: 0,
            version: 0,
        }
    }

    pub fn mark_delete(&mut self) {
        self.meta |= VALUE_DELETE;
    }

    pub fn estimate_size(&self, threshold: usize) -> usize {
        // The estimated size of an entry will be key length + value length +
        // two bytes of metadata + expires_at.
        const METADATA_SIZE: usize = std::mem::size_of::<u8>() * 2 + std::mem::size_of::<u64>();
        if self.value.len() < threshold {
            // For those values < threshold, key and value will be directly stored in LSM tree.
            self.key.len() + self.value.len() + METADATA_SIZE
        } else {
            // For those values >= threshold, only key will be stored in LSM tree.
            self.key.len() + ValuePointer::encoded_size() + METADATA_SIZE
        }
    }

    // TODO: entry encoding will be done later, as current WAL encodes header and key / value separately
    /*
    pub fn encoded_len(&self) -> usize {
        let kl = self.key.len();
        let vl = self.value.len();
        kl + vl + varint_len(kl) + varint_len(vl) + 1
    }

    pub fn encode(&self, bytes: &mut BytesMut) {
        let encoded_len = self.encoded_len();
        bytes.reserve(encoded_len);
        unsafe {
            let buf = bytes.bytes_mut();
            *(*buf.get_unchecked_mut(0)).as_mut_ptr() = self.meta;
            let mut read = 1;
            read += encode_varint_uncheck(buf.get_unchecked_mut(read..), self.key.len() as u64);
            read += encode_varint_uncheck(buf.get_unchecked_mut(read..), self.value.len() as u64);
            ptr::copy_nonoverlapping(
                self.key.as_ptr(),
                buf.as_mut_ptr().add(read) as _,
                self.key.len(),
            );
            ptr::copy_nonoverlapping(
                self.value.as_ptr(),
                buf.as_mut_ptr().add(read + self.key.len()) as _,
                self.value.len(),
            );
        }
        unsafe {
            bytes.advance_mut(encoded_len);
        }
    }*/
}
