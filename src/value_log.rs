use crate::value::{self, Request, ValuePointer};
use crate::wal::{Header, Wal};
use crate::AgateOptions;
use crate::{Error, Result};

use bytes::{Bytes, BytesMut};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex, RwLock};

fn vlog_file_path(dir: impl AsRef<Path>, fid: u32) -> PathBuf {
    dir.as_ref().join(format!("{:06}.vlog", fid))
}

struct Core {
    /// `files_map` stores mapping from value log ID to WAL object.
    /// TODO: use scheme like memtable to separate current vLog
    /// and previous logs, so as to reduce usage of Mutex.
    files_map: HashMap<u32, Arc<Mutex<Wal>>>,
    /// maximum file ID opened
    max_fid: u32,
    files_to_delete: Vec<u32>,
    num_entries_written: u32,
}

impl Core {
    fn new() -> Self {
        Self {
            files_map: HashMap::new(),
            max_fid: 0,
            files_to_delete: vec![],
            num_entries_written: 0,
        }
    }

    fn drop_no_fail(&mut self) -> Result<()> {
        for (_, wal) in &mut self.files_map {
            let mut wal = wal.lock()?;
            wal.mark_close_and_save();
        }

        Ok(())
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        crate::util::no_fail(self.drop_no_fail(), "ValueLog::Core::drop");
    }
}

/// ValueLog stores all value logs of an agatedb instance.
pub struct ValueLog {
    /// value log directory
    dir_path: PathBuf,
    /// value log file mapping, use `RwLock` to support concurrent read
    core: Arc<RwLock<Core>>,
    /// offset of next write
    writeable_log_offset: AtomicU32,
    opts: AgateOptions,
}

impl ValueLog {
    /// Create value logs from agatedb options
    pub fn new(opts: AgateOptions) -> Result<Option<Self>> {
        let core = if opts.in_memory {
            None
        } else {
            let core = Self {
                core: Arc::new(RwLock::new(Core::new())),
                dir_path: opts.value_dir.clone(),
                opts,
                writeable_log_offset: AtomicU32::new(0),
            };
            // TODO: garbage collection
            // TODO: discard stats
            core.open()?;
            Some(core)
        };

        Ok(core)
    }

    fn file_path(&self, fid: u32) -> PathBuf {
        vlog_file_path(&self.dir_path, fid)
    }

    fn populate_files_map(&self) -> Result<()> {
        let dir = std::fs::read_dir(&self.dir_path)?;
        let mut core = self.core.write().unwrap();
        for file in dir {
            let file = file?;
            if let Ok(filename) = file.file_name().into_string() {
                if filename.ends_with(".vlog") {
                    let fid: u32 = filename[..filename.len() - 5].parse().map_err(|err| {
                        Error::CustomError(format!("failed to parse file ID {:?}", err))
                    })?;
                    let wal = Wal::open(file.path(), self.opts.clone())?;
                    let wal = Arc::new(Mutex::new(wal));
                    if let Some(_) = core.files_map.insert(fid, wal) {
                        return Err(Error::CustomError(format!("duplicated vlog found {}", fid)));
                    }
                    if core.max_fid < fid {
                        core.max_fid = fid;
                    }
                }
            }
        }
        Ok(())
    }

    fn create_vlog_file(&self) -> Result<(u32, Arc<Mutex<Wal>>)> {
        let mut core = self.core.write().unwrap();
        let fid = core.max_fid + 1;
        let path = self.file_path(fid);
        let wal = Wal::open(path, self.opts.clone())?;
        // TODO: only create new files
        let wal = Arc::new(Mutex::new(wal));
        assert!(core.files_map.insert(fid, wal.clone()).is_none());
        assert!(core.max_fid < fid);
        core.max_fid = fid;
        // TODO: add vlog header
        self.writeable_log_offset
            .store(0, std::sync::atomic::Ordering::SeqCst);
        core.num_entries_written = 0;
        Ok((fid, wal))
    }

    fn sorted_fids(&self) -> Vec<u32> {
        let core = self.core.read().unwrap();
        let mut to_be_deleted = HashSet::new();
        for fid in core.files_to_delete.iter().cloned() {
            to_be_deleted.insert(fid);
        }
        let mut result = vec![];
        for (fid, _) in core.files_map.iter() {
            if !to_be_deleted.contains(fid) {
                result.push(*fid);
            }
        }
        result.sort();
        result
    }

    /// Open value log directory
    fn open(&self) -> Result<()> {
        self.populate_files_map()?;
        // TODO find empty files and iterate vlogs
        self.create_vlog_file()?;
        Ok(())
    }

    fn w_offset(&self) -> u32 {
        self.writeable_log_offset
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// write requests to vlog, and put vlog pointers back in `Request`.
    /// `write` should not be called concurrently, otherwise this will lead to wrong result.
    pub fn write(&self, requests: &mut [Request]) -> Result<()> {
        let result = self.write_inner(requests);
        if self.opts.sync_writes {
            let core = self.core.read().unwrap();
            let current_log_id = core.max_fid;
            let current_log_ptr = core.files_map.get(&current_log_id).unwrap().clone();
            let mut current_log = current_log_ptr.lock().unwrap();
            drop(core);
            current_log.sync()?;
        }
        result
    }

    pub fn write_inner(&self, requests: &mut [Request]) -> Result<()> {
        // TODO: validate writes

        let core = self.core.read().unwrap();
        let mut current_log_id = core.max_fid;
        let mut current_log = core.files_map.get(&current_log_id).unwrap().clone();
        drop(core);

        let write = |buf: &[u8], current_log: Arc<Mutex<Wal>>| -> Result<()> {
            let mut current_log = current_log.lock().unwrap();
            if buf.is_empty() {
                return Ok(());
            }
            let n = buf.len() as u32;
            let start = self
                .writeable_log_offset
                .fetch_add(n, std::sync::atomic::Ordering::SeqCst);
            let end_offset = start + n;

            // expand file size if space is not enough
            // TODO: handle value >= 4GB case
            if end_offset >= current_log.size() {
                current_log.set_len(end_offset as u64)?;
            }
            (&mut current_log.data()[start as usize..end_offset as usize]).clone_from_slice(buf);
            current_log.set_size(end_offset);
            Ok(())
        };

        let to_disk = |current_log: Arc<Mutex<Wal>>| -> Result<bool> {
            let mut current_log = current_log.lock().unwrap();
            let core = self.core.read().unwrap();
            if self.w_offset() as u64 > self.opts.value_log_file_size
                || core.num_entries_written > self.opts.value_log_max_entries
            {
                current_log.done_writing(self.w_offset())?;
                Ok(true)
            } else {
                Ok(false)
            }
        };

        let mut buf = BytesMut::new();
        for req in requests.iter_mut() {
            req.ptrs.clear();

            let mut written = 0;

            for mut entry in req.entries.iter_mut() {
                buf.clear();

                if self.opts.skip_vlog(&entry) {
                    req.ptrs.push(ValuePointer::default());
                    continue;
                }

                let mut p = ValuePointer::default();

                p.file_id = current_log_id;
                p.offset = self.w_offset();

                let orig_meta = entry.meta;
                entry.meta = entry.meta & (!value::VALUE_FIN_TXN | value::VALUE_TXN);

                let plen = Wal::encode_entry(&mut buf, &entry);
                entry.meta = orig_meta;
                p.len = plen as u32;
                req.ptrs.push(p);
                write(&buf, current_log.clone())?;

                written += 1;
            }

            if to_disk(current_log.clone())? {
                let (log_id, log) = self.create_vlog_file()?;
                current_log_id = log_id;
                current_log = log;
            }

            let mut core = self.core.write().unwrap();
            core.num_entries_written += written;
        }

        if to_disk(current_log.clone())? {
            self.create_vlog_file()?;
        }
        Ok(())
    }

    fn get_file(&self, value_ptr: &ValuePointer) -> Result<Arc<Mutex<Wal>>> {
        let core = self.core.read().unwrap();
        let file = core.files_map.get(&value_ptr.file_id).cloned();
        if let Some(file) = file {
            let max_fid = core.max_fid;
            // TODO: read-only
            if value_ptr.file_id == max_fid {
                let current_offset = self.w_offset();
                if value_ptr.offset >= current_offset {
                    return Err(Error::InvalidLogOffset(value_ptr.offset, current_offset));
                }
            }

            Ok(file)
        } else {
            return Err(Error::VlogNotFound(value_ptr.file_id));
        }
    }

    /// Read data from vlogs.
    /// The returned value is a `Bytes`, including the whole entry.
    /// You may need to manually decode it with `Wal::decode_wntry`.
    ///
    /// TODO: let user to decide when to unlock instead of blocking.
    /// TODO: return header together with k-v pair.
    pub(crate) fn read(&self, value_ptr: ValuePointer) -> Result<Bytes> {
        let log_file = self.get_file(&value_ptr)?;
        let r = log_file.lock().unwrap();
        let mut buf = r.read(&value_ptr)?;
        let original_buf = buf.slice(..);
        drop(r);

        // TODO: verify checksum

        let mut header = Header::default();
        header.decode(&mut buf)?;
        let kv = buf;

        if (kv.len() as u32) < header.key_len + header.value_len {
            return Err(Error::InvalidValuePointer {
                vptr: value_ptr,
                kvlen: kv.len(),
                range: header.key_len..header.key_len + header.value_len,
            });
        }
        Ok(original_buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Entry;
    use tempdir::TempDir;
    use value::VALUE_POINTER;

    #[test]
    fn test_value_basic() {
        let mut opts = AgateOptions::default();
        let tmp_dir = TempDir::new("agatedb").unwrap();
        opts.value_dir = tmp_dir.path().to_path_buf();
        opts.value_threshold = 32;
        opts.value_log_file_size = 1024;
        let vlog = ValueLog::new(opts.clone()).unwrap().unwrap();

        let val1 = b"sampleval012345678901234567890123";
        let val2 = b"samplevalb012345678901234567890123";

        assert!(val1.len() > opts.value_threshold as usize);

        let mut e1 = Entry::new(
            Bytes::from_static(b"samplekey"),
            Bytes::copy_from_slice(val1),
        );
        e1.meta = VALUE_POINTER;
        let mut e2 = Entry::new(
            Bytes::from_static(b"samplekeyb"),
            Bytes::copy_from_slice(val2),
        );
        e2.meta = VALUE_POINTER;

        let mut reqs = vec![Request {
            entries: vec![e1, e2],
            ptrs: vec![],
            done: None,
        }];

        vlog.write(&mut reqs).unwrap();
        let req = reqs.pop().unwrap();
        assert_eq!(req.ptrs.len(), 2);

        let mut buf1 = vlog.read(req.ptrs[0].clone()).unwrap();
        let mut buf2 = vlog.read(req.ptrs[1].clone()).unwrap();

        let e1 = Wal::decode_entry(&mut buf1).unwrap();
        let e2 = Wal::decode_entry(&mut buf2).unwrap();

        assert_eq!(&e1.key[..], b"samplekey");
        assert_eq!(&e1.value[..], val1);

        assert_eq!(&e2.key[..], b"samplekeyb");
        assert_eq!(&e2.value[..], val2);
    }
}
