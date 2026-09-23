//! Filesystem utilities for WASM.

pub use crate::sys::stubs::fs::*;

impl crate::sys::fs::PathExt for std::path::Path {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn executable(&self) -> bool {
        // wasip2 has no `access(X_OK)`; returning `true` unconditionally made PATH searches
        // (`type <cmd>`, `find_executables_in_path`) report a phantom hit for every candidate,
        // so an unknown command resolved to `/usr/local/bin/<name>` instead of "not found".
        // A real existence check is the honest wasip2 behavior (`test -e` works on this target),
        // excluding directories so a PATH dir is never mistaken for the executable itself.
        self.exists() && !self.is_dir()
    }

    fn exists_and_is_block_device(&self) -> bool {
        has_file_type(self, FileType::Block)
    }

    fn exists_and_is_char_device(&self) -> bool {
        has_file_type(self, FileType::Char)
    }

    fn exists_and_is_fifo(&self) -> bool {
        has_file_type(self, FileType::Fifo)
    }

    fn exists_and_is_socket(&self) -> bool {
        has_file_type(self, FileType::Socket)
    }

    fn exists_and_is_setgid(&self) -> bool {
        false
    }

    fn exists_and_is_setuid(&self) -> bool {
        false
    }

    fn exists_and_is_sticky_bit(&self) -> bool {
        false
    }

    fn get_device_and_inode(&self) -> Result<(u64, u64), crate::error::Error> {
        Ok((0, 0))
    }
}

#[derive(Clone, Copy)]
enum FileType {
    Block,
    Char,
    Fifo,
    Socket,
}

/// Whether `path` exists and, following links, has this type. Stable std names only regular
/// files, directories and links on WASI, so the mode comes from libc's `stat`.
#[cfg(target_os = "wasi")]
fn has_file_type(path: &std::path::Path, file_type: FileType) -> bool {
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: an all-zero `stat` is a valid value of this plain C struct.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is writable storage for one `stat`.
    if unsafe { libc::stat(path.as_ptr(), &raw mut stat) } != 0 {
        return false;
    }
    let format = match file_type {
        FileType::Block => libc::S_IFBLK,
        FileType::Char => libc::S_IFCHR,
        FileType::Fifo => libc::S_IFIFO,
        FileType::Socket => libc::S_IFSOCK,
    };
    stat.st_mode & libc::S_IFMT == format
}

#[cfg(not(target_os = "wasi"))]
const fn has_file_type(_path: &std::path::Path, _file_type: FileType) -> bool {
    false
}

/// Splits a PATH-like value into individual paths.
///
/// On WASM, `std::env::split_paths` is not available, so this
/// implementation splits by the `:` separator.
pub fn split_paths<T: AsRef<std::ffi::OsStr> + ?Sized>(
    s: &T,
) -> impl Iterator<Item = std::path::PathBuf> {
    s.as_ref()
        .to_str()
        .unwrap_or_default()
        .split(':')
        .map(std::path::PathBuf::from)
        .collect::<Vec<_>>()
        .into_iter()
}
