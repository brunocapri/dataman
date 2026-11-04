use std::path::Path;
use std::time::SystemTime;

/// What tells another process's write apart: Linux and Windows file times move by a clock tick,
/// so two writes in one tick share an mtime; the writes replace the file, so the inode (the
/// NTFS file index) changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    ino: u64,
    /// A rewrite in place with its mtime put back (`touch -r`, `cp -p`) still moves the ctime.
    changed: (i64, i64),
}

impl FileStamp {
    pub fn of(path: &Path) -> Option<FileStamp> {
        let m = std::fs::metadata(path).ok()?;
        #[cfg(windows)]
        let ids = {
            use std::os::windows::fs::OpenOptionsExt as _;
            // No access asked: metadata only, whoever else has the file open.
            let f = std::fs::OpenOptions::new().access_mode(0).custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS).open(path).ok();
            f.and_then(|f| windows_ids(&f))
        };
        #[cfg(not(windows))]
        let ids = None;
        Some(Self::from_meta(&m, ids))
    }

    /// A file already open: the one being read, whatever its name points at by now.
    pub fn of_file(f: &std::fs::File) -> Option<FileStamp> {
        let m = f.metadata().ok()?;
        #[cfg(windows)]
        let ids = windows_ids(f);
        #[cfg(not(windows))]
        let ids = None;
        Some(Self::from_meta(&m, ids))
    }

    /// `ids`: the NTFS file index and change time, what the inode and ctime are on Unix (where
    /// the metadata has them).
    fn from_meta(m: &std::fs::Metadata, ids: Option<(u64, (i64, i64))>) -> FileStamp {
        #[cfg(unix)]
        let (ino, changed) = {
            use std::os::unix::fs::MetadataExt as _;
            let _ = ids;
            (m.ino(), (m.ctime(), m.ctime_nsec()))
        };
        #[cfg(not(unix))]
        let (ino, changed) = ids.unwrap_or((0, (0, 0)));
        FileStamp { modified: m.modified().ok(), len: m.len(), ino, changed }
    }

    pub fn size(&self) -> u64 {
        self.len
    }

    /// Written over a second ago. A write in place within the same clock tick, the same length,
    /// leaves the stamp as it was: a stamp still fresh is no proof, so its file is read again
    /// (git's "racily clean").
    pub fn settled(&self) -> bool {
        self.modified.and_then(|m| m.elapsed().ok()).is_some_and(|d| d >= std::time::Duration::from_secs(1))
    }
}

#[cfg(windows)]
fn windows_ids(f: &std::fs::File) -> Option<(u64, (i64, i64))> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx};
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

/// Held while a file and the approved rules that go with it are written, or read together:
/// another Dataman process (a second app, `dataman-mcp`) never reads one write's file with the
/// next write's rules, which would look like an edit made outside Dataman. One lock file per
/// file kept, next to it; released when dropped.
pub(crate) fn hold(file: &Path) -> Option<std::fs::File> {
    let name = file.file_name()?.to_string_lossy().into_owned();
    let path = file.with_file_name(format!(".{name}.lock"));
    // No folder yet: no file either, nothing to read half of.
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).ok()?;
    // Not taken: the write goes on unguarded, but said so (it never fails silently).
    f.lock().inspect_err(|e| log::warn!("{}: not locked: {e}", path.display())).ok()?;
    Some(f)
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
        assert!(!FileStamp::of(&p).unwrap().settled(), "just written");
    }
}
