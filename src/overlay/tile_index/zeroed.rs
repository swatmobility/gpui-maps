//! A zero-filled buffer of exactly the size asked for.
//!
//! Why not `vec![0; n]`: on macOS a large `calloc` can be served from the
//! allocator's cache of freed large blocks, and the block it hands back may
//! be far bigger than asked. Measured on the national block groups
//! (`malloc_history -allBySize`): the index's 566 MB `xy` came back as one
//! 983 MB block, and its 71 MB `min_zoom` as a 123 MB one -- ~470 MB of
//! resident, never-used heap for as long as the source is set, in the runs
//! where the demo's freed coordinate buffer was still in that cache (about
//! two runs in three). Neither `malloc_zone_pressure_relief` nor anything
//! else public empties that cache on this OS, so a buffer this large is
//! mapped on its own: an anonymous mapping of exactly `n` elements (page
//! rounded), zero-filled by the kernel and unmapped on drop.
//!
//! Below [`MAP_MIN_BYTES`], and on every target but macOS and iOS, it is a
//! plain `Vec`.

use std::ops::{Deref, DerefMut};

/// Smaller buffers stay on the heap: a mapping per small index would cost a
/// page and a syscall for nothing the allocator gets wrong.
const MAP_MIN_BYTES: usize = 16 << 20;

pub(crate) enum ZeroedBuf<T: bytemuck::Zeroable + Copy> {
    Heap(Vec<T>),
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    Mapped { ptr: std::ptr::NonNull<T>, len: usize, bytes: usize },
}

// SAFETY: `Mapped` owns its mapping exclusively, like a `Vec` owns its block.
unsafe impl<T: bytemuck::Zeroable + Copy + Send> Send for ZeroedBuf<T> {}
unsafe impl<T: bytemuck::Zeroable + Copy + Sync> Sync for ZeroedBuf<T> {}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod sys {
    pub const PROT_READ: i32 = 0x1;
    pub const PROT_WRITE: i32 = 0x2;
    pub const MAP_PRIVATE: i32 = 0x2;
    pub const MAP_ANON: i32 = 0x1000;
    pub const MAP_FAILED: *mut core::ffi::c_void = !0usize as *mut core::ffi::c_void;
    extern "C" {
        pub fn mmap(addr: *mut core::ffi::c_void, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut core::ffi::c_void;
        pub fn munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
    }
}

impl<T: bytemuck::Zeroable + Copy> ZeroedBuf<T> {
    /// `len` zeroed elements.
    pub(crate) fn zeroed(len: usize) -> Self {
        let bytes = len.saturating_mul(std::mem::size_of::<T>());
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if bytes >= MAP_MIN_BYTES && std::mem::align_of::<T>() <= 4096 {
            // SAFETY: a fresh private anonymous mapping; the kernel zero-fills
            // it, and all-zero is a valid `T` (`Zeroable`).
            let ptr = unsafe {
                sys::mmap(std::ptr::null_mut(), bytes, sys::PROT_READ | sys::PROT_WRITE, sys::MAP_PRIVATE | sys::MAP_ANON, -1, 0)
            };
            if ptr != sys::MAP_FAILED {
                if let Some(ptr) = std::ptr::NonNull::new(ptr.cast::<T>()) {
                    return Self::Mapped { ptr, len, bytes };
                }
            }
        }
        let _ = bytes;
        Self::Heap(vec![T::zeroed(); len])
    }

    /// Bytes held: the heap block's capacity, or the mapping page-rounded.
    pub(crate) fn held_bytes(&self) -> usize {
        match self {
            Self::Heap(v) => v.capacity() * std::mem::size_of::<T>(),
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::Mapped { bytes, .. } => bytes.div_ceil(16384) * 16384,
        }
    }
}

impl<T: bytemuck::Zeroable + Copy> Deref for ZeroedBuf<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Self::Heap(v) => v,
            // SAFETY: `ptr` maps `len` initialised (zeroed, then written) `T`s
            // for as long as `self` lives.
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::Mapped { ptr, len, .. } => unsafe { std::slice::from_raw_parts(ptr.as_ptr(), *len) },
        }
    }
}

impl<T: bytemuck::Zeroable + Copy> DerefMut for ZeroedBuf<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            Self::Heap(v) => v,
            // SAFETY: as `deref`, and `&mut self` makes the access exclusive.
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::Mapped { ptr, len, .. } => unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), *len) },
        }
    }
}

impl<T: bytemuck::Zeroable + Copy> Drop for ZeroedBuf<T> {
    fn drop(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Self::Mapped { ptr, bytes, .. } = self {
            // SAFETY: the mapping `zeroed` made, unmapped once.
            unsafe { sys::munmap(ptr.as_ptr().cast(), *bytes) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_buffer_is_zeroed_writable_and_exactly_sized() {
        let n = (MAP_MIN_BYTES / 8) + 3;
        let mut buf: ZeroedBuf<[u32; 2]> = ZeroedBuf::zeroed(n);
        assert_eq!(buf.len(), n);
        assert!(buf.iter().all(|&v| v == [0, 0]));
        buf[n - 1] = [7, 9];
        assert_eq!(buf[n - 1], [7, 9]);
        assert!(buf.held_bytes() >= n * 8 && buf.held_bytes() < n * 8 + 16384);
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        assert!(matches!(buf, ZeroedBuf::Mapped { .. }), "mapped on its own, not served by the allocator");
        let small: ZeroedBuf<u8> = ZeroedBuf::zeroed(100);
        assert!(matches!(small, ZeroedBuf::Heap(_)));
        assert_eq!(&small[..], &[0u8; 100][..]);
    }
}
