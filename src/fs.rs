//! SD-card access through libctru directly.
//!
//! `std::fs` support on this target is defined by the libctru shim and is not
//! documented well enough to build a file browser on, so this uses the FS
//! service calls the same way native homebrew does.

use std::ffi::c_void;
use std::ptr;

use ctru_sys as sys;

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub struct Sdmc {
    archive: sys::FS_Archive,
}

impl Sdmc {
    pub fn open() -> Result<Self, String> {
        unsafe {
            let rc = sys::fsInit();
            if rc < 0 {
                return Err(format!("fsInit failed: {:#x}", rc));
            }
            let mut archive: sys::FS_Archive = 0;
            let empty = sys::FS_Path {
                type_: sys::PATH_EMPTY,
                size: 0,
                data: ptr::null(),
            };
            let rc = sys::FSUSER_OpenArchive(&mut archive, sys::ARCHIVE_SDMC, empty);
            if rc < 0 {
                return Err(format!("could not open the SD card: {:#x}", rc));
            }
            Ok(Sdmc { archive })
        }
    }

    fn path(&self, path: &str) -> (Vec<u16>, sys::FS_Path) {
        let mut buf: Vec<u16> = path.encode_utf16().collect();
        buf.push(0);
        let p = sys::FS_Path {
            type_: sys::PATH_UTF16,
            size: (buf.len() * 2) as u32,
            data: buf.as_ptr() as *const c_void,
        };
        (buf, p)
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<Entry>, String> {
        let (_buf, p) = self.path(path);
        let mut handle: sys::Handle = 0;
        unsafe {
            let rc = sys::FSUSER_OpenDirectory(&mut handle, self.archive, p);
            if rc < 0 {
                return Err(format!("cannot open {path}: {:#x}", rc));
            }
        }
        let mut out = Vec::new();
        let mut entries = [sys::FS_DirectoryEntry::default(); 32];
        loop {
            let mut read: u32 = 0;
            let rc = unsafe {
                sys::FSDIR_Read(
                    handle,
                    &mut read,
                    32,
                    entries.as_mut_ptr(),
                )
            };
            if rc < 0 || read == 0 {
                break;
            }
            for e in entries.iter().take(read as usize) {
                let name = utf16_name(&e.name);
                if name.is_empty() || name == "." || name == ".." {
                    continue;
                }
                out.push(Entry {
                    name,
                    is_dir: e.attributes & 0x1 != 0,
                    size: e.fileSize,
                });
            }
        }
        unsafe {
            sys::FSDIR_Close(handle);
        }
        Ok(out)
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let (_buf, p) = self.path(path);
        let mut handle: sys::Handle = 0;
        unsafe {
            let rc = sys::FSUSER_OpenFile(
                &mut handle,
                self.archive,
                p,
                sys::FS_OPEN_READ as u32,
                0,
            );
            if rc < 0 {
                return Err(format!("cannot open {path}: {:#x}", rc));
            }
        }

        let mut size: u64 = 0;
        let rc = unsafe { sys::FSFILE_GetSize(handle, &mut size) };
        if rc < 0 {
            unsafe { sys::FSFILE_Close(handle) };
            return Err(format!("cannot stat {path}: {:#x}", rc));
        }

        // A 3DS has 128-256 MB of RAM in total; a SWF larger than this can never
        // be held in memory alongside the engine.
        const MAX: u64 = 64 * 1024 * 1024;
        if size > MAX {
            unsafe { sys::FSFILE_Close(handle) };
            return Err(format!("{path} is {size} bytes, larger than the {MAX} byte limit"));
        }

        let mut data = vec![0u8; size as usize];
        let mut read_total: u32 = 0;
        while (read_total as u64) < size {
            let mut got: u32 = 0;
            let rc = unsafe {
                sys::FSFILE_Read(
                    handle,
                    &mut got,
                    read_total as u64,
                    data[read_total as usize..].as_mut_ptr() as *mut c_void,
                    (size - read_total as u64) as u32,
                )
            };
            if rc < 0 || got == 0 {
                unsafe { sys::FSFILE_Close(handle) };
                return Err(format!("read error in {path}: {:#x}", rc));
            }
            read_total += got;
        }
        unsafe {
            sys::FSFILE_Close(handle);
        }
        data.truncate(read_total as usize);
        Ok(data)
    }
}

fn utf16_name(raw: &[u16]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

/// Joins a directory and a file name with a single slash.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Parent of an absolute SD path, never above the root.
pub fn parent(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    }
}
