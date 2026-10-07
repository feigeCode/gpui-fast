//! Memory shared with the display server: a `memfd` mapped into this
//! process.

use std::{
    ffi::c_void,
    os::fd::{FromRawFd as _, OwnedFd},
    ptr::NonNull,
};

use anyhow::Context as _;

/// A `memfd` of `len` bytes mapped into this process.
pub(crate) struct Mapping {
    fd: OwnedFd,
    ptr: NonNull<c_void>,
    len: usize,
}

impl Mapping {
    pub(crate) fn fd(&self) -> &OwnedFd {
        &self.fd
    }

    /// The memory as pixels. Only to be written while the display server
    /// is not reading it.
    pub(crate) fn pixels_mut(&mut self) -> &mut [u32] {
        // SAFETY: `mmap` returned `len` bytes, page aligned, mapped as long as
        // `self` lives.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().cast::<u32>(), self.len / 4) }
    }

    pub(crate) fn new(len: usize) -> anyhow::Result<Self> {
        // SAFETY: plain system calls; every result is checked.
        unsafe {
            let raw = libc::memfd_create(c"gpui-cpu-frame".as_ptr(), libc::MFD_CLOEXEC);
            if raw < 0 {
                return Err(std::io::Error::last_os_error()).context("memfd_create");
            }
            let fd = OwnedFd::from_raw_fd(raw);
            if libc::ftruncate(raw, len as libc::off_t) < 0 {
                return Err(std::io::Error::last_os_error()).context("ftruncate");
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                raw,
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error()).context("mmap");
            }
            Ok(Self {
                fd,
                ptr: NonNull::new(ptr).context("mmap returned null")?,
                len,
            })
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `len` are the mapping `mmap` returned. The
        // display server maps the file itself, so its view is unaffected.
        unsafe {
            libc::munmap(self.ptr.as_ptr(), self.len);
        }
    }
}
