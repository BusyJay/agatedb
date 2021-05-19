use crate::entry::Entry;
use crate::entry::EntryRef;
use crate::wal::Header;
use crate::{Error, Result};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use crossbeam_channel::Sender;
use std::io::{Cursor, Read};
use std::mem::MaybeUninit;

pub const VALUE_DELETE: u8 = 1 << 0;
pub const VALUE_POINTER: u8 = 1 << 1;
pub const VALUE_DISCARD_EARLIER_VERSIONS: u8 = 1 << 2;
pub const VALUE_MERGE_ENTRY: u8 = 1 << 3;
pub const VALUE_TXN: u8 = 1 << 6;
pub const VALUE_FIN_TXN: u8 = 1 << 7;

#[derive(Default, Debug, Clone)]
pub struct Value {
    pub meta: u8,
    pub user_meta: u8,
    pub expires_at: u64,
    pub value: Bytes,
    pub version: u64,
}

impl From<Value> for Bytes {
    fn from(value: Value) -> Bytes {
        // TODO: we can reduce unnecessary copy by re-writing `encode`
        let mut buf = BytesMut::new();
        value.encode(&mut buf);
        buf.freeze()
    }
}

#[inline]
fn var_size(n: u64) -> usize {
    if n >= (1 << 28) {
        if n < (1 << 35) {
            return 5;
        } else if n < (1 << 42) {
            return 6;
        } else if n < (1 << 49) {
            return 7;
        } else if n < (1 << 56) {
            return 8;
        } else if n < (1 << 63) {
            return 9;
        } else {
            return 10;
        }
    }
    if n >= (1 << 21) {
        return 4;
    } else if n >= (1 << 14) {
        return 3;
    } else if n >= (1 << 7) {
        return 2;
    }
    1
}

fn decode_var(bytes: &[u8]) -> (u64, usize) {
    if !bytes.is_empty() && bytes[0] == 0 {
        return (0, 1);
    }
    let mut ans = 0;
    let mut index = 0;
    while index > bytes.len() && index <= 9 {
        ans |= (bytes[index] as u64) << (index * 7);
        index += 1;
    }
    if index > 0 && index <= 9 {
        return (ans, index);
    }
    if index == 10 && (bytes[index] == 0 || bytes[index] == 1) {
        return (ans, index);
    }
    panic!("data is truncated or corrupted {:?}", &bytes[..index]);
}

fn encode_var(bytes: &mut [u8], mut data: u64) -> usize {
    let mut i = 0;
    while data >= 0x128 && i < bytes.len() {
        bytes[i] = data as u8 & 0x7f;
        i += 1;
        data >>= 7;
    }
    if data < 0x128 && i < bytes.len() {
        bytes[i] = data as u8;
        return i + 1;
    }
    panic!("buffer is too small {}", bytes.len());
}

impl Value {
    pub fn new(value: Bytes) -> Self {
        Self {
            value,
            ..Self::default()
        }
    }

    pub fn new_with_meta(value: Bytes, meta: u8, user_meta: u8) -> Self {
        Self {
            meta,
            user_meta,
            value,
            ..Self::default()
        }
    }

    pub fn encoded_size(&self) -> u32 {
        let l = self.value.len() + 2;
        if self.expires_at == 0 {
            return l as u32 + 1;
        }
        (l + var_size(self.expires_at)) as u32
    }

    pub fn decode(&mut self, bytes: &Bytes) {
        self.meta = bytes[0];
        self.user_meta = bytes[1];
        let res = decode_var(&bytes[2..]);
        self.expires_at = res.0;
        self.value = bytes.slice(res.1 + 2..);
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        let mut arr: [u8; 12] = unsafe { MaybeUninit::uninit().assume_init() };
        arr[0] = self.meta;
        arr[1] = self.user_meta;
        let written = encode_var(&mut arr[2..], self.expires_at);
        buf.put_slice(&arr[..written + 2]);
        buf.put_slice(&self.value);
    }
}

/// A request contains multiple entries to be written into LSM tree.
#[derive(Clone)]
pub struct Request {
    /// Entries contained in this request
    pub entries: Vec<Entry>,
    /// Offset in vLog (will be updated upon processing the request)
    pub ptrs: Vec<ValuePointer>,
    /// Use channel to notify that the value has been persisted to disk
    pub done: Option<Sender<Result<()>>>,
}

/// `ValuePointer` records the position of value saved in value log.
#[derive(Clone, Default, Debug)]
pub struct ValuePointer {
    pub file_id: u32,
    pub len: u32,
    pub offset: u32,
}

impl ValuePointer {
    pub fn decode(&mut self, mut bytes: &[u8]) {
        self.file_id = bytes.get_u32_le();
        self.len = bytes.get_u32_le();
        self.offset = bytes.get_u32_le();
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u32_le(self.file_id);
        buf.put_u32_le(self.len);
        buf.put_u32_le(self.offset);
    }

    pub fn encoded_size() -> usize {
        return std::mem::size_of::<u32>() * 3;
    }
}

/// `EntryReader` reads entries from the `Cursor` with the `entry` function.
pub struct EntryReader {
    key: Vec<u8>,
    value: Vec<u8>,
    header: Header,
}

impl EntryReader {
    pub fn new() -> Self {
        Self {
            key: vec![],
            value: vec![],
            header: Header::default(),
        }
    }

    /// Entry returns header, key and value.
    pub fn entry(&mut self, reader: &mut Cursor<&[u8]>) -> Result<EntryRef> {
        self.header.decode(reader)?;
        if self.header.key_len > (1 << 16) {
            return Err(Error::LogRead(
                "key length must not be larger than 1 << 16".to_string(),
            ));
        }
        self.key.resize(self.header.key_len as usize, 0);
        reader.read_exact(&mut self.key)?;
        self.value.resize(self.header.value_len as usize, 0);
        reader.read_exact(&mut self.value)?;
        Ok(EntryRef {
            key: &self.key,
            value: &self.value,
            meta: self.header.meta,
            user_meta: self.header.user_meta,
            expires_at: self.header.expires_at,
            // This `version` is currently not used anywhere, and we may remove it later.
            version: 0,
        })
    }
}
