use std::path::Path;
use std::time::SystemTime;

/// What tells another process's write apart: Linux file times move by a clock tick, so two
/// writes in one tick share an mtime; the writes replace the file, so the inode changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    ino: u64,
    /// A rewrite in place with its mtime put back (`touch -r`, `cp -p`) still moves the ctime.
    changed: (i64, i64),
    /// Windows' file times move by a clock tick (up to ~16 ms) and an in-place rewrite keeps the
    /// file index: there a small file's contents tell the writes apart (0 elsewhere).
    content: u64,
}

/// The files stamped are Dataman's own (connections, settings, a project, favorites): small.
#[cfg(windows)]
const HASHED_UP_TO: u64 = 4 << 20;

impl FileStamp {
    pub fn of(path: &Path) -> Option<FileStamp> {
        let m = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let ino = std::os::unix::fs::MetadataExt::ino(&m);
        #[cfg(unix)]
        let changed = (std::os::unix::fs::MetadataExt::ctime(&m), std::os::unix::fs::MetadataExt::ctime_nsec(&m));
        // The NTFS file index and change time: what the inode and ctime are on Unix.
        #[cfg(windows)]
        let (ino, changed) = windows_ids(path).unwrap_or((0, (0, 0)));
        #[cfg(not(any(unix, windows)))]
        let (ino, changed) = (0, (0, 0));
        #[cfg(windows)]
        let content = if m.is_file() && m.len() <= HASHED_UP_TO {
            use std::hash::{Hash as _, Hasher as _};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::fs::read(path).ok()?.hash(&mut h);
            h.finish()
        } else {
            0
        };
        #[cfg(not(windows))]
        let content = 0;
        Some(FileStamp { modified: m.modified().ok(), len: m.len(), ino, changed, content })
    }
}

#[cfg(windows)]
fn windows_ids(path: &Path) -> Option<(u64, (i64, i64))> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    };
    // No access asked: metadata only, whoever else has the file open.
    let f = std::fs::OpenOptions::new().access_mode(0).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path).ok()?;
    let h = f.as_raw_handle();
    // SAFETY: a valid handle and out-structures of the sizes given.
    unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if GetFileInformationByHandle(h, &mut info) == 0 {
            return None;
        }
        let mut basic: FILE_BASIC_INFO = std::mem::zeroed();
        if GetFileInformationByHandleEx(h, FileBasicInfo, (&mut basic as *mut FILE_BASIC_INFO).cast(), std::mem::size_of::<FILE_BASIC_INFO>() as u32) == 0 {
            return None;
        }
        let ino = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        // 100 ns ticks.
        Some((ino, (basic.ChangeTime / 10_000_000, (basic.ChangeTime % 10_000_000) * 100)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_file_is_a_new_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        assert_eq!(FileStamp::of(&p), None);
        std::fs::write(&p, "a").unwrap();
        let a = FileStamp::of(&p).unwrap();
        assert_eq!(FileStamp::of(&p), Some(a));
        let tmp = dir.path().join("f.tmp");
        std::fs::write(&tmp, "b").unwrap();
        std::fs::rename(&tmp, &p).unwrap();
        assert_ne!(FileStamp::of(&p), Some(a), "same length, maybe the same mtime: the inode differs");
    }

    #[test]
    fn a_file_rewritten_in_place_at_once_is_a_new_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "mcp = \"read\"").unwrap();
        let a = FileStamp::of(&p).unwrap();
        std::fs::write(&p, "mcp = \"full\"").unwrap();
        assert_ne!(FileStamp::of(&p), Some(a), "same length, same file, maybe the same tick");
    }
}
