use crate::entry::{Entry, EntryRef};
use crate::util::sync_dir;
use crate::value::{EntryReader, ValuePointer};
use crate::AgateOptions;
use crate::Error;
use crate::Result;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use memmap::{MmapMut, MmapOptions};
use prost::{decode_length_delimiter, encode_length_delimiter, length_delimiter_len};
use std::fs::{File, OpenOptions};
use std::io::Cursor;
use std::path::PathBuf;

pub const MAX_HEADER_SIZE: usize = 21;

/// `Header` stores metadata of an entry in WAL and in value log.
#[derive(Default, Debug, PartialEq)]
pub struct Header {
    /// length of key
    pub key_len: u32,
    /// length of value
    pub value_len: u32,
    /// entry expire date
    pub expires_at: u64,
    /// metadata
    pub(crate) meta: u8,
    /// user metadata
    pub user_meta: u8,
}

impl Header {
    /// Get length of header if being encoded
    pub fn encoded_len(&self) -> usize {
        1 + 1
            + length_delimiter_len(self.expires_at as usize)
            + length_delimiter_len(self.key_len as usize)
            + length_delimiter_len(self.value_len as usize)
    }

    /// Encode header into bytes
    ///
    /// Header consists of a variable-size key length, variable-size value length,
    /// and fixed-size `expires_at`, `meta`, and `user_meta`.
    ///
    /// +------+-----------+---------+-----------+-----------+
    /// | meta | user_meta | key_len | value_len | expires_at|
    /// +----------------------------------------------------+
    /// | u64  |    u64    | var len |  var len  |    u64    |
    /// +------+-----------+---------+-----------+-----------+
    pub fn encode(&self, bytes: &mut BytesMut) {
        let encoded_len = self.encoded_len();
        bytes.reserve(encoded_len);

        bytes.put_u8(self.meta);
        bytes.put_u8(self.user_meta);
        encode_length_delimiter(self.key_len as usize, bytes).unwrap();
        encode_length_delimiter(self.value_len as usize, bytes).unwrap();
        encode_length_delimiter(self.expires_at as usize, bytes).unwrap();
    }

    /// Decode header from byte stream
    pub fn decode(&mut self, mut bytes: &mut impl Buf) -> Result<()> {
        if bytes.remaining() <= 2 {
            return Err(Error::VarDecode("should be at least 2 bytes"));
        }
        self.meta = bytes.get_u8();
        self.user_meta = bytes.get_u8();
        self.key_len = decode_length_delimiter(&mut bytes)? as u32;
        self.value_len = decode_length_delimiter(&mut bytes)? as u32;
        self.expires_at = decode_length_delimiter(&mut bytes)? as u64;
        Ok(())
    }
}

/// WAL of a memtable or a value log
///
/// TODO: This WAL simply stores key-value pair in sequence without checksum,
/// encryption and compression. These will be done later.
/// TODO: delete WAL file when reference to WAL (or memtable) comes to 0
pub struct Wal {
    path: PathBuf,
    file: File,
    mmap_file: MmapMut,
    opts: AgateOptions,
    write_at: u32,
    buf: BytesMut,
    size: u32,
}

impl Wal {
    /// open or create a WAL from options
    pub fn open(path: PathBuf, opts: AgateOptions) -> Result<Wal> {
        let (file, bootstrap) = if path.exists() {
            (
                OpenOptions::new()
                    .create(false)
                    .read(true)
                    .write(true)
                    .open(&path)?,
                false,
            )
        } else {
            let file = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)?;
            file.set_len(2 * opts.value_log_file_size)?;
            file.sync_all()?;
            sync_dir(&path.parent().unwrap())?;
            (file, true)
        };
        let mmap_file = unsafe { MmapOptions::new().map_mut(&file)? };
        let mut wal = Wal {
            path,
            file,
            size: mmap_file.len() as u32,
            mmap_file,
            opts,
            write_at: 0,
            // TODO: current implementation doesn't have keyID and baseIV header
            buf: BytesMut::new(),
        };

        if bootstrap {
            wal.bootstrap()?;
        }

        // TODO: We should read vlog headers and data key from WAL after we implement
        // checksum / encryption support.

        Ok(wal)
    }

    fn bootstrap(&mut self) -> Result<()> {
        self.zero_next_entry()?;
        Ok(())
    }

    pub(crate) fn write_entry(&mut self, entry: &Entry) -> Result<()> {
        self.buf.clear();
        Self::encode_entry(&mut self.buf, entry);
        self.mmap_file[self.write_at as usize..self.write_at as usize + self.buf.len()]
            .clone_from_slice(&self.buf[..]);
        self.write_at += self.buf.len() as u32;
        self.zero_next_entry()?;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.mmap_file.flush()?;
        Ok(())
    }

    pub fn zero_next_entry(&mut self) -> Result<()> {
        let range =
            &mut self.mmap_file[self.write_at as usize..self.write_at as usize + MAX_HEADER_SIZE];
        unsafe {
            std::ptr::write_bytes(range.as_mut_ptr(), 0, range.len());
        }
        Ok(())
    }

    /// Encode entry to buffer
    ///
    /// The entry is encoded to a header followed by plain key and value.
    /// +--------+-----+-------+
    /// | header | key | value |
    /// +--------+-----+-------+
    pub(crate) fn encode_entry(mut buf: &mut BytesMut, entry: &Entry) -> usize {
        let header = Header {
            key_len: entry.key.len() as u32,
            value_len: entry.value.len() as u32,
            expires_at: entry.expires_at,
            meta: entry.meta,
            user_meta: entry.user_meta,
        };

        // write header to buffer
        header.encode(&mut buf);

        // write key and value to buffer
        // TODO: encryption
        buf.extend_from_slice(&entry.key);
        buf.extend_from_slice(&entry.value);

        // TODO: add CRC32 check

        buf.len()
    }

    /// Decode entry from buffer
    pub(crate) fn decode_entry(buf: &mut Bytes) -> Result<Entry> {
        let mut header = Header::default();
        header.decode(buf)?;
        let kv = buf;
        Ok(Entry {
            meta: header.meta,
            user_meta: header.user_meta,
            expires_at: header.expires_at,
            key: kv.slice(..header.key_len as usize),
            value: kv.slice(
                header.key_len as usize..header.key_len as usize + header.value_len as usize,
            ),
            version: 0,
        })
    }

    /// Read value from WAL (when used as value log)
    pub(crate) fn read(&self, p: &ValuePointer) -> Result<Bytes> {
        let offset = p.offset;
        let size = self.mmap_file.len() as u64;
        let value_size = p.len;
        let log_size = self.size;

        if offset as u64 >= size
            || offset as u64 + value_size as u64 > size
            || offset as u64 + value_size as u64 > log_size as u64
        {
            return Err(Error::LogRead("EOF".to_string()));
        }

        Ok(Bytes::copy_from_slice(
            &self.mmap_file[offset as usize..offset as usize + value_size as usize],
        ))
    }

    /// Truncate WAL
    pub fn truncate(&mut self, end: u64) -> Result<()> {
        // TODO: check read only
        let metadata = self.file.metadata()?;
        if metadata.len() == end {
            return Ok(());
        }
        self.size = end as u32;
        self.file.set_len(end)?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Finish WAL writing
    pub(crate) fn done_writing(&mut self, offset: u32) -> Result<()> {
        if self.opts.sync_writes {
            self.file.sync_all()?;
        }
        self.truncate(offset as u64)?;
        Ok(())
    }

    /// Get WAL iterator
    pub fn iter(&mut self) -> Result<WalIterator> {
        Ok(WalIterator::new(Cursor::new(
            &self.mmap_file[0..self.size as usize],
        )))
    }

    pub fn should_flush(&self) -> bool {
        self.write_at as u64 > self.opts.value_log_file_size
    }

    /// Get real size of WAL. After truncating, WAL mmap file will have different
    /// size against real file. `size` stores the actual length.
    pub(crate) fn size(&self) -> u32 {
        self.size
    }

    /// When using WAL as value log, we will need to extend or shrink actual size
    /// of WAL file from outside functions.
    pub(crate) fn set_size(&mut self, size: u32) {
        self.size = size;
    }

    /// When using WAL as value log, we will need to extend or shrink actual size
    /// of WAL file from outside functions.
    pub(crate) fn set_len(&mut self, len: u64) -> Result<()> {
        self.file.set_len(len)?;
        Ok(())
    }

    pub(crate) fn data(&mut self) -> &mut MmapMut {
        &mut self.mmap_file
    }
}

pub struct WalIterator<'a> {
    /// `reader` stores the file to read
    reader: Cursor<&'a [u8]>,
    /// `entry_reader` operates on `reader` and buffers entry information
    entry_reader: EntryReader,
}

impl<'a> WalIterator<'a> {
    pub fn new(reader: Cursor<&'a [u8]>) -> Self {
        Self {
            reader,
            entry_reader: EntryReader::new(),
        }
    }

    /// Get next entry from WAL
    ///
    /// This function will:
    /// return Ok(None) if we reached first corrupted entry, and we could stop iteration
    /// return Ok(Some(entry)) if we read a new entry
    /// return Err if the error is not recoverable
    pub fn next(&mut self) -> Result<Option<EntryRef<'_>>> {
        use std::io::ErrorKind;

        let entry = self.entry_reader.entry(&mut self.reader);

        match entry {
            Ok(entry) => {
                if entry.is_zero() {
                    return Ok(None);
                }
                // TODO: process transaction-related metadata
                Ok(Some(entry))
            }
            // ignore prost varint decode error
            Err(Error::Decode(_)) => Ok(None),
            // ignore custom decode error (e.g. header <= 2)
            Err(Error::VarDecode(_)) => Ok(None),
            // ignore file length < key, value size
            Err(Error::Io(err)) => {
                if err.kind() == ErrorKind::UnexpectedEof {
                    Ok(None)
                } else {
                    Err(Error::Io(err))
                }
            }
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;
    #[test]
    fn test_wal_create() {
        let tmp_dir = TempDir::new("agatedb").unwrap();
        let mut opts = AgateOptions::default();
        opts.value_log_file_size = 4096;
        Wal::open(tmp_dir.path().join("1.wal"), opts).unwrap();
    }

    #[test]
    fn test_header_encode() {
        let header = Header {
            key_len: 233333,
            value_len: 2333,
            expires_at: std::u64::MAX - 2333333,
            user_meta: b'A',
            meta: b'B',
        };

        let mut buf = BytesMut::new();
        header.encode(&mut buf);
        let mut buf = buf.freeze();

        let mut new_header = Header::default();
        new_header.decode(&mut buf).unwrap();
        assert_eq!(new_header, header);
    }

    #[test]
    fn test_wal_iterator() {
        let tmp_dir = TempDir::new("agatedb").unwrap();
        let mut opts = AgateOptions::default();
        opts.value_log_file_size = 4096;
        let wal_path = tmp_dir.path().join("1.wal");
        let mut wal = Wal::open(wal_path.clone(), opts.clone()).unwrap();
        for i in 0..20 {
            let entry = Entry::new(Bytes::from(i.to_string()), Bytes::from(i.to_string()));
            wal.write_entry(&entry).unwrap();
        }
        drop(wal);

        // reopen WAL and iterate
        let mut wal = Wal::open(wal_path, opts).unwrap();
        let mut it = wal.iter().unwrap();
        let mut cnt = 0;
        while let Some(entry) = it.next().unwrap() {
            assert_eq!(entry.key, cnt.to_string().as_bytes());
            assert_eq!(entry.value, cnt.to_string().as_bytes());
            cnt += 1;
        }
        assert_eq!(cnt, 20);
    }

    #[test]
    fn test_wal_iterator_trunc() {
        let tmp_dir = TempDir::new("agatedb").unwrap();
        let mut opts = AgateOptions::default();
        opts.value_log_file_size = 4096;
        let wal_path = tmp_dir.path().join("1.wal");
        let mut wal = Wal::open(wal_path.clone(), opts.clone()).unwrap();
        for i in 0..20 {
            let entry = Entry::new(Bytes::from(i.to_string()), Bytes::from(i.to_string()));
            wal.write_entry(&entry).unwrap();
        }
        drop(wal);

        for trunc_length in (50..100).rev() {
            // truncate some data from WAL
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&wal_path)
                .unwrap();
            file.set_len(trunc_length).unwrap();
            drop(file);

            // reopen WAL and iterate
            let mut wal = Wal::open(wal_path.clone(), opts.clone()).unwrap();
            let mut it = wal.iter().unwrap();
            let mut cnt = 0;
            while let Some(entry) = it.next().unwrap() {
                assert_eq!(entry.key, cnt.to_string().as_bytes());
                assert_eq!(entry.value, cnt.to_string().as_bytes());
                cnt += 1;
            }
            assert!(cnt < 20);
        }
    }
}
