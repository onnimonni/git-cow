//! Minimal git-lfs support without the git-lfs binary or network access:
//! parse pointers, verify smudged files, and smudge from the local object store.
//!
//! libgit2 doesn't run filter drivers, so without this an LFS file would look modified
//! (smudged content vs pointer blob) and be overwritten with the pointer text.

use anyhow::Result;
use git2::{AttrCheckFlags, Index, Oid, Repository};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Pointer files are small; anything larger is real content.
const MAX_POINTER_SIZE: usize = 1024;

pub struct Pointer {
    /// sha256 hex
    pub oid: String,
    pub size: u64,
    /// Uses `ext-*` transformations (e.g. compression), which we can't apply.
    pub has_extensions: bool,
}

impl Pointer {
    pub fn parse(data: &[u8]) -> Option<Pointer> {
        let text = std::str::from_utf8(data).ok()?;
        let mut lines = text.lines();
        let version = lines.next()?.strip_prefix("version ")?;
        if !matches!(
            version,
            "https://git-lfs.github.com/spec/v1" | "https://hawser.github.com/spec/v1"
        ) {
            return None;
        }
        let (mut oid, mut size, mut has_extensions) = (None, None, false);
        for line in lines {
            if let Some(hex) = line.strip_prefix("oid sha256:") {
                oid = Some(hex.to_owned());
            } else if let Some(n) = line.strip_prefix("size ") {
                size = n.parse().ok();
            } else if line.starts_with("ext-") {
                has_extensions = true;
            }
        }
        let oid = oid.filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))?;
        Some(Pointer {
            oid,
            size: size?,
            has_extensions,
        })
    }
}

/// Whether any `.gitattributes` in `index` (or `info/attributes`) mentions `filter=lfs`,
/// so repositories without LFS skip the per-file attribute lookups.
pub fn used_in(repo: &Repository, index: &Index) -> bool {
    let mentions_lfs = |data: &[u8]| data.windows(10).any(|w| w == b"filter=lfs");
    let info = fs::read(repo.commondir().join("info/attributes")).unwrap_or_default();
    mentions_lfs(&info)
        || index.iter().any(|e| {
            e.path.ends_with(b".gitattributes")
                && repo
                    .find_blob(e.id)
                    .is_ok_and(|blob| mentions_lfs(blob.content()))
        })
}

/// Whether `path` has `filter=lfs` in the attributes of the index (the target commit);
/// cloned or deleted `.gitattributes` files in the worktree are never consulted.
pub fn is_tracked(repo: &Repository, path: &Path) -> bool {
    repo.get_attr(path, "filter", AttrCheckFlags::INDEX_ONLY)
        .is_ok_and(|v| v == Some("lfs"))
}

/// The pointer stored in blob `id`, if it is one (checks size before loading the blob).
pub fn read_pointer(repo: &Repository, id: Oid) -> Result<Option<Pointer>> {
    let (size, _) = repo.odb()?.read_header(id)?;
    if size > MAX_POINTER_SIZE {
        return Ok(None);
    }
    Ok(Pointer::parse(repo.find_blob(id)?.content()))
}

/// Whether the file at `path` still holds pointer text (not smudged content).
pub fn file_is_pointer(path: &Path) -> io::Result<bool> {
    if fs::symlink_metadata(path)?.len() > MAX_POINTER_SIZE as u64 {
        return Ok(false);
    }
    Ok(Pointer::parse(&fs::read(path)?).is_some())
}

/// Whether the file at `path` is the smudged content of `pointer`.
pub fn file_matches(path: &Path, pointer: &Pointer) -> io::Result<bool> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() != pointer.size {
        return Ok(false);
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(hex == pointer.oid)
}

/// Where git-lfs keeps the object: `<lfs.storage or $GIT_COMMON_DIR/lfs>/objects/ab/cd/<oid>`.
pub fn object_path(repo: &Repository, pointer: &Pointer) -> PathBuf {
    let common = repo.commondir();
    let storage = repo
        .config()
        .and_then(|c| c.get_path("lfs.storage"))
        .map(|p| common.join(p))
        .unwrap_or_else(|_| common.join("lfs"));
    let oid = &pointer.oid;
    storage
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid)
}
