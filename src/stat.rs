//! Index stat data handling.
//!
//! Clones keep mtime, size and mode but get a new inode and ctime. If the source
//! worktree's index says a file was clean, the clone holds the same content, so its
//! entry only needs fresh lstat data instead of re-hashing the file.

use git2::{Index, IndexEntry, IndexTime, Oid, Repository};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Record `meta` in `entry` the way git does (fields truncated to 32 bits).
pub fn fill(entry: &mut IndexEntry, meta: &Metadata) {
    entry.ctime = IndexTime::new(meta.ctime() as i32, meta.ctime_nsec() as u32);
    entry.mtime = IndexTime::new(meta.mtime() as i32, meta.mtime_nsec() as u32);
    entry.dev = meta.dev() as u32;
    entry.ino = meta.ino() as u32;
    entry.uid = meta.uid();
    entry.gid = meta.gid();
    entry.file_size = meta.size() as u32;
}

fn same_mtime_and_size(entry: &IndexEntry, meta: &Metadata) -> bool {
    entry.mtime.seconds() == meta.mtime() as i32
        && entry.mtime.nanoseconds() == meta.mtime_nsec() as u32
        && entry.file_size == meta.size() as u32
}

/// Whether `meta` still matches the stat data git recorded (file unchanged since).
fn unchanged_since_indexed(entry: &IndexEntry, meta: &Metadata) -> bool {
    same_mtime_and_size(entry, meta)
        && entry.ctime.seconds() == meta.ctime() as i32
        && entry.ctime.nanoseconds() == meta.ctime_nsec() as u32
        && entry.ino == meta.ino() as u32
}

fn same_stat(a: &Metadata, b: &Metadata) -> bool {
    (
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
        a.ino(),
        a.size(),
    ) == (
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
        b.ino(),
        b.size(),
    )
}

fn stage(entry: &IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

fn gitattributes<'a>(entries: impl Iterator<Item = &'a IndexEntry>) -> Vec<(Vec<u8>, Oid)> {
    let mut attrs: Vec<_> = entries
        .filter(|e| e.path.ends_with(b".gitattributes"))
        .map(|e| (e.path.clone(), e.id))
        .collect();
    attrs.sort();
    attrs
}

/// The source worktree's index as it was *before* cloning, so files changed while the
/// clone ran can't borrow stat data they don't deserve.
pub struct SourceSnapshot {
    root: PathBuf,
    /// Clean, non-racy entries and the lstat of their file when captured.
    trusted: HashMap<Vec<u8>, (IndexEntry, Metadata)>,
    attributes: Vec<(Vec<u8>, Oid)>,
    tracked_files: HashSet<Vec<u8>>,
    tracked_dirs: HashSet<Vec<u8>>,
}

impl SourceSnapshot {
    /// None when `root` isn't a git worktree or its index can't be read by libgit2
    /// (split index, ...); everything is then hashed instead.
    pub fn capture(root: &Path) -> Option<SourceSnapshot> {
        let repo = Repository::open(root).ok()?;
        let index = repo.index().ok()?;
        let index_mtime = fs::metadata(repo.path().join("index"))
            .and_then(|m| m.modified())
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?;

        let entries: Vec<IndexEntry> = index.iter().collect();
        let mut snapshot = SourceSnapshot {
            root: root.to_path_buf(),
            trusted: HashMap::new(),
            attributes: gitattributes(entries.iter()),
            tracked_files: HashSet::new(),
            tracked_dirs: HashSet::new(),
        };
        for entry in entries {
            let mut dir = entry.path.as_slice();
            while let Some(slash) = dir.iter().rposition(|&b| b == b'/') {
                dir = &dir[..slash];
                if !snapshot.tracked_dirs.insert(dir.to_vec()) {
                    break; // ancestors already recorded
                }
            }
            snapshot.tracked_files.insert(entry.path.clone());

            // racily clean entries (modified in the same second the index was written)
            // can't be trusted, same as in git
            let racy = entry.mtime.seconds() as u64 >= index_mtime.as_secs();
            if stage(&entry) != 0 || racy {
                continue;
            }
            let path = root.join(OsStr::from_bytes(&entry.path));
            if let Ok(meta) = fs::symlink_metadata(path) {
                if unchanged_since_indexed(&entry, &meta) {
                    snapshot.trusted.insert(entry.path.clone(), (entry, meta));
                }
            }
        }
        Some(snapshot)
    }

    pub fn is_tracked_file(&self, rel: &[u8]) -> bool {
        self.tracked_files.contains(rel)
    }

    pub fn is_tracked_dir(&self, rel: &[u8]) -> bool {
        self.tracked_dirs.contains(rel)
    }

    /// For every entry of `index` (the target commit) whose content the snapshot vouches
    /// for, fill in stat data from the clone in `dest`. Returns how many were filled.
    pub fn reuse(&self, index: &mut Index, dest: &Path) -> usize {
        // Different attributes can mean a different clean/smudge conversion (e.g. a
        // file leaving LFS), so the same blob no longer implies the same bytes on disk.
        if gitattributes(index.iter().collect::<Vec<_>>().iter()) != self.attributes {
            return 0;
        }
        let reusable: Vec<IndexEntry> = index
            .iter()
            .filter_map(|mut entry| {
                let (src, captured) = self.trusted.get(&entry.path)?;
                if src.id != entry.id || src.mode != entry.mode {
                    return None;
                }
                let path = Path::new(OsStr::from_bytes(&entry.path));
                // the source file must not have changed while it was being cloned
                let now = fs::symlink_metadata(self.root.join(path)).ok()?;
                let dst_meta = fs::symlink_metadata(dest.join(path)).ok()?;
                if !same_stat(captured, &now) || !same_mtime_and_size(src, &dst_meta) {
                    return None;
                }
                fill(&mut entry, &dst_meta);
                Some(entry)
            })
            .collect();

        reusable.iter().filter(|e| index.add(e).is_ok()).count()
    }
}
