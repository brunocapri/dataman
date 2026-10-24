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
}

impl FileStamp {
    pub fn of(path: &Path) -> Option<FileStamp> {
        let m = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let ino = std::os::unix::fs::MetadataExt::ino(&m);
        #[cfg(unix)]
        let changed = (std::os::unix::fs::MetadataExt::ctime(&m), std::os::unix::fs::MetadataExt::ctime_nsec(&m));
        #[cfg(not(unix))]
        let (ino, changed) = (0, (0, 0));
        Some(FileStamp { modified: m.modified().ok(), len: m.len(), ino, changed })
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
}
