use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::{mem, ptr};

struct ArenaCore {
    len: AtomicU32,
    cap: usize,
    ptr: *mut u8,
}

impl Drop for ArenaCore {
    fn drop(&mut self) {
        unsafe {
            let ptr = self.ptr as *mut u64;
            let cap = self.cap / 8;
            Vec::from_raw_parts(ptr, 0, cap);
        }
    }
}

pub struct Arena {
    core: Arc<ArenaCore>,
}

impl Arena {
    pub fn with_capacity(cap: u32) -> Arena {
        let mut buf: Vec<u64> = Vec::with_capacity(cap as usize / 8);
        let ptr = buf.as_mut_ptr() as *mut u8;
        let cap = buf.capacity() * 8;
        mem::forget(buf);
        Arena {
            core: Arc::new(ArenaCore {
                len: AtomicU32::new(1),
                cap,
                ptr,
            }),
        }
    }

    pub fn len(&self) -> u32 {
        self.core.len.load(Ordering::SeqCst)
    }

    pub fn alloc<N>(&self, align: usize, size: usize) -> *mut N {
        let align_mask = align - 1;
        // Leave enough padding for align.
        let size = size + align_mask;
        let offset = self.core.len.fetch_add(size as u32, Ordering::SeqCst);
        // Calculate the correct align point, it equals to
        // (offset + align_mask) / align * align.
        let ptr_offset = (offset as usize + align_mask) & !align_mask;
        assert!(offset as usize + size <= self.core.cap);
        unsafe { self.get_mut(ptr_offset as u32) as _ }
    }

    unsafe fn get_mut<N>(&self, offset: u32) -> *mut N {
        if offset == 0 {
            return ptr::null_mut();
        }
        self.core.ptr.add(offset as usize) as _
    }
}
