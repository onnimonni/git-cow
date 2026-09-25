//! Fill a freshly created git worktree (`git worktree add --no-checkout`) by
//! copy-on-write cloning an existing worktree, then let libgit2 write only the files
//! whose content differs from the worktree's HEAD. git itself handles branches,
//! worktree metadata and every `git worktree add` option.
//!
//! Flow of [`populate`]:
//! 1. snapshot the source worktree's index, then copy-on-write clone it (see [`cow`]):
//!    tracked files plus ignored build caches, minus excludes (see [`select`]);
//!    falls back to a regular checkout without CoW support
//! 2. load HEAD's tree into the index; reuse the snapshot's stat data for files it
//!    vouches for, hash the rest (read-only), accept verified smudged git-lfs files
//! 3. force checkout: only differing files are written, untracked files removed;
//!    git-lfs pointers are smudged from the local LFS store

mod cow;
mod lfs;
mod select;
mod stat;

pub use select::join as join_paths;

use anyhow::{bail, Context, Result};
use git2::build::CheckoutBuilder;
use git2::{CheckoutNotificationType, Delta, DiffOptions, Index, IndexEntry, Repository};
use select::{Rules, Selection, Selector};
use stat::SourceSnapshot;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct PopulateOptions {
    /// Worktree to clone files from. Defaults to the main worktree.
    pub from: Option<PathBuf>,
    /// Carry ignored files (node_modules, build caches, ...) from the source worktree.
    pub include_ignored: bool,
}

#[derive(Debug, Default)]
pub struct Report {
    pub path: PathBuf,
    pub source: Option<PathBuf>,
    /// Filesystem of the new worktree (apfs, btrfs, xfs, ...).
    pub filesystem: String,
    /// Files and directory trees cloned from the source worktree.
    pub cloned: usize,
    /// Ignored paths carried over from the source (build caches).
    pub carried: Vec<PathBuf>,
    /// Ignored paths left behind (virtualenvs, runtime state, `cow.exclude`).
    pub excluded: Vec<PathBuf>,
    /// Index entries whose stat data was taken over from the source (no hashing needed).
    pub stat_reused: usize,
    /// Files git had to write because their content differed from HEAD.
    pub rewritten: usize,
    /// git-lfs files kept from the clone after verifying their sha256.
    pub lfs_cloned: usize,
    /// git-lfs files filled from the local LFS object store.
    pub lfs_smudged: usize,
    /// git-lfs files left as pointers because the object isn't downloaded.
    pub lfs_missing: Vec<PathBuf>,
    /// git-lfs files left as pointers because the local object is damaged.
    pub lfs_corrupt: Vec<PathBuf>,
    /// git-lfs files left as pointers because they use `ext-*` extensions.
    pub lfs_unsupported: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Fill the fresh worktree at `path` (containing only its `.git` file). On error the
/// worktree is reset to that fresh state, so a regular checkout can take over.
pub fn populate(path: &Path, opts: &PopulateOptions) -> Result<Report> {
    let path = path
        .canonicalize()
        .with_context(|| format!("invalid worktree {}", path.display()))?;
    let wt = Repository::open(&path)?;
    if !wt.is_worktree() {
        bail!("{} is not a linked worktree", path.display());
    }
    ensure_fresh(&path)?;
    let opts = &PopulateOptions {
        from: opts.from.clone(),
        include_ignored: opts.include_ignored
            && wt.config()?.get_bool("cow.carryIgnored").unwrap_or(true),
    };
    let source = source_dir(&wt, opts)?;
    let mut report = Report {
        path: path.clone(),
        source: source.clone(),
        ..Default::default()
    };
    // unborn branch (--orphan): nothing to check out
    let Ok(commit) = wt.head().and_then(|h| h.peel_to_commit()) else {
        return Ok(report);
    };
    match fill(&wt, &path, source.as_deref(), &commit, opts, &mut report) {
        Ok(Some(last_old_mtime)) => wait_until_second_after(last_old_mtime),
        Ok(None) => {}
        Err(err) => {
            reset_to_fresh(&path);
            return Err(err);
        }
    }
    Ok(report)
}

/// Refuse to touch a worktree that already has files (`.git` is all a fresh one has).
fn ensure_fresh(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        if entry?.file_name() != ".git" {
            bail!(
                "{} is not a fresh worktree (create it with `git worktree add --no-checkout`)",
                path.display()
            );
        }
    }
    Ok(())
}

fn reset_to_fresh(path: &Path) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() != ".git" {
            if let Err(err) = remove_path(&entry.path()) {
                eprintln!(
                    "git-cow: could not clean up {}: {err}",
                    entry.path().display()
                );
            }
        }
    }
}

/// `--from`, or the main worktree (none for bare repositories).
fn source_dir(wt: &Repository, opts: &PopulateOptions) -> Result<Option<PathBuf>> {
    let dir = match &opts.from {
        Some(from) => from.clone(),
        None => match Repository::open(wt.commondir())?.workdir() {
            Some(main) => main.to_path_buf(),
            None => return Ok(None),
        },
    };
    let dir = dir
        .canonicalize()
        .with_context(|| format!("invalid --from {}", dir.display()))?;
    Ok(Some(dir))
}

/// Cloned files keep their old mtime, so git never treats them as racily clean and
/// relies on ctime to spot edits. git (unless built with USE_NSEC) compares ctime in
/// whole seconds, so an edit that restores the mtime (`cp -p`, `rsync -t`) within the
/// second the clone was made would go unnoticed. Returning only after that second is
/// over guarantees any later edit gets a newer ctime. Usually the rest of the work
/// already took that long.
fn wait_until_second_after(time: SystemTime) {
    let Ok(since_epoch) = time.duration_since(UNIX_EPOCH) else {
        return;
    };
    let next_second = UNIX_EPOCH + Duration::from_secs(since_epoch.as_secs() + 1);
    if let Ok(remaining) = next_second.duration_since(SystemTime::now()) {
        std::thread::sleep(remaining);
    }
}

/// Returns when the last file keeping an old mtime (clone or LFS object) was written.
fn fill(
    wt: &Repository,
    path: &Path,
    source: Option<&Path>,
    commit: &git2::Commit,
    opts: &PopulateOptions,
    report: &mut Report,
) -> Result<Option<SystemTime>> {
    let filesystem = cow::detect(path)?;
    report.filesystem = filesystem.name.clone();
    let mut last_old_mtime = None;
    // before cloning, so files changing during the clone don't get trusted
    let snapshot = source.and_then(SourceSnapshot::capture);
    if let Some(source) = source {
        clone_worktree(source, path, snapshot.as_ref(), &filesystem, opts, report)?;
        if report.cloned > 0 {
            last_old_mtime = Some(SystemTime::now());
        }
    }

    let mut index = wt.index()?;
    index.read_tree(&commit.tree()?)?;
    reset_submodule_dirs(&index, path)?;

    if let (Some(snapshot), true) = (&snapshot, report.cloned > 0) {
        report.stat_reused = snapshot.reuse(&mut index, path);
    }
    let modified = hash_workdir(wt, &index)?;
    let uses_lfs = lfs::used_in(wt, &index);
    if uses_lfs {
        accept_smudged_lfs(wt, &mut index, path, &modified, report)?;
    }
    index.write()?;

    report.rewritten = checkout(wt, &mut index, opts.include_ignored)?;
    if uses_lfs {
        smudge_lfs(wt, &mut index, path, report)?;
        if report.lfs_smudged > 0 {
            last_old_mtime = Some(SystemTime::now());
        }
    }
    index.write()?;
    Ok(last_old_mtime)
}

/// Clone the selected parts of `source` into `dest`: whole directory trees in one call
/// where nothing inside is skipped.
fn clone_worktree(
    source: &Path,
    dest: &Path,
    snapshot: Option<&SourceSnapshot>,
    filesystem: &cow::Filesystem,
    opts: &PopulateOptions,
    report: &mut Report,
) -> Result<()> {
    if !filesystem.may_clone {
        report.warnings.push(format!(
            "{} has no copy-on-write support, doing a regular checkout",
            filesystem.name
        ));
        return Ok(());
    }
    if fs::metadata(source)?.dev() != fs::metadata(dest)?.dev() {
        report.warnings.push(format!(
            "{} is on another filesystem, doing a regular checkout",
            source.display()
        ));
        return Ok(());
    }

    let selection = match (snapshot, Repository::open(source)) {
        (Some(snapshot), Ok(source_repo)) => Selector {
            source,
            dest,
            repo: &source_repo,
            snapshot,
            rules: &Rules::load(&source_repo, source),
            include_ignored: opts.include_ignored,
        }
        .select()?,
        // not a git worktree (or unreadable index): clone everything, checkout cleans up
        _ => Selection {
            skip: [PathBuf::from(".git")].into(),
            ..Default::default()
        },
    };

    if let Err(err) = clone_selected(source, dest, Path::new(""), &selection, report) {
        let reason = if cow::is_unsupported(&err) {
            format!("cannot clone on {}", filesystem.name)
        } else {
            "cloning failed".into()
        };
        report
            .warnings
            .push(format!("{reason} ({err}), git writes the remaining files"));
    }
    report.carried = selection.carried;
    report.excluded = selection.excluded;
    Ok(())
}

/// Returns early (Err) only when cloning can't work at all; other failures are warnings
/// since checkout writes whatever is missing.
fn clone_selected(
    source: &Path,
    dest: &Path,
    rel: &Path,
    selection: &Selection,
    report: &mut Report,
) -> io::Result<()> {
    for entry in fs::read_dir(source.join(rel))? {
        let entry = entry?;
        let path = rel.join(entry.file_name());
        let (from, to) = (entry.path(), dest.join(&path));
        if selection.skip.contains(&path) || dest.starts_with(&from) {
            continue;
        }
        if selection.partial.contains(&path) {
            fs::create_dir(&to)?;
            fs::set_permissions(&to, entry.metadata()?.permissions())?;
            clone_selected(source, dest, &path, selection, report)?;
            continue;
        }
        match cow::clone_tree(&from, &to) {
            Ok(()) => report.cloned += 1,
            Err(err) => {
                let _ = remove_path(&to);
                if cow::is_unsupported(&err) {
                    return Err(err);
                }
                report
                    .warnings
                    .push(format!("could not clone {}: {err}", from.display()));
            }
        }
    }
    Ok(())
}

/// Remove a file, symlink or directory tree without following symlinks.
fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Cloned submodule checkouts have `.git` files pointing into the source's gitdir;
/// leave empty directories like a fresh checkout does. Walks the path one component at
/// a time without following symlinks: a symlinked parent is removed (checkout recreates
/// it as a directory), never followed out of the worktree.
fn reset_submodule_dirs(index: &Index, root: &Path) -> Result<()> {
    const GITLINK: u32 = 0o160000;
    for entry in index.iter().filter(|e| e.mode == GITLINK) {
        let rel = Path::new(OsStr::from_bytes(&entry.path));
        let components: Vec<_> = rel.components().collect();
        let mut dir = root.to_path_buf();
        for (i, component) in components.iter().enumerate() {
            dir.push(component);
            match fs::symlink_metadata(&dir) {
                Ok(meta) if meta.is_dir() => {
                    if i + 1 == components.len() {
                        fs::remove_dir_all(&dir)?;
                        fs::create_dir(&dir)?;
                    }
                }
                Ok(_) => {
                    fs::remove_file(&dir)?;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// Hash files whose stat data is unknown. Unchanged ones get stat data recorded in the
/// index; returns the paths whose content differs.
fn hash_workdir(wt: &Repository, index: &Index) -> Result<Vec<PathBuf>> {
    let mut opts = DiffOptions::new();
    opts.update_index(true).ignore_submodules(true);
    let diff = wt.diff_index_to_workdir(Some(index), Some(&mut opts))?;
    let modified = diff
        .deltas()
        .filter(|d| d.status() == Delta::Modified)
        .filter_map(|d| d.new_file().path().map(Path::to_path_buf))
        .collect();
    Ok(modified)
}

/// Cloned LFS files hold smudged content while the index has the pointer blob;
/// keep them when their sha256 matches the pointer.
fn accept_smudged_lfs(
    wt: &Repository,
    index: &mut Index,
    root: &Path,
    paths: &[PathBuf],
    report: &mut Report,
) -> Result<()> {
    for path in paths {
        let Some(mut entry) = index.get_path(path, 0) else {
            continue;
        };
        if !lfs::is_tracked(wt, path) {
            continue;
        }
        let Some(pointer) = lfs::read_pointer(wt, entry.id)? else {
            continue;
        };
        let file = root.join(path);
        if !pointer.has_extensions && lfs::file_matches(&file, &pointer)? {
            stat::fill(&mut entry, &fs::symlink_metadata(&file)?);
            index.add(&entry)?;
            report.lfs_cloned += 1;
        }
    }
    Ok(())
}

/// Force checkout of `index`, returning how many files libgit2 had to write.
/// Untracked files are removed, ignored ones too unless they are carried over.
fn checkout(wt: &Repository, index: &mut Index, include_ignored: bool) -> Result<usize> {
    let mut updated = 0;
    let mut opts = CheckoutBuilder::new();
    opts.force()
        .remove_untracked(true)
        .remove_ignored(!include_ignored)
        .notify_on(CheckoutNotificationType::UPDATED)
        .notify(|_, _, _, _, _| {
            updated += 1;
            true
        });
    wt.checkout_index(Some(index), Some(&mut opts))?;
    drop(opts);
    Ok(updated)
}

/// Replace LFS pointer text in the worktree (written by checkout, or cloned from a
/// source without smudged files) with the verified object from the local LFS store,
/// cloned when the filesystem allows it.
fn smudge_lfs(wt: &Repository, index: &mut Index, root: &Path, report: &mut Report) -> Result<()> {
    let files: Vec<IndexEntry> = index
        .iter()
        .filter(|e| matches!(e.mode, 0o100644 | 0o100755))
        .collect();
    for mut entry in files {
        let path = PathBuf::from(OsStr::from_bytes(&entry.path));
        if !lfs::is_tracked(wt, &path) {
            continue;
        }
        let Some(pointer) = lfs::read_pointer(wt, entry.id)? else {
            continue;
        };
        let file = root.join(&path);
        // anything else was verified against the pointer (stat reuse or sha256)
        if !lfs::file_is_pointer(&file)? {
            continue;
        }
        if pointer.has_extensions {
            report.lfs_unsupported.push(path);
            continue;
        }
        let object = lfs::object_path(wt, &pointer);
        if !object.is_file() {
            report.lfs_missing.push(path);
            continue;
        }
        if !lfs::file_matches(&object, &pointer)? {
            report.lfs_corrupt.push(path);
            continue;
        }
        fs::remove_file(&file)?;
        cow::clone_or_copy_file(&object, &file)?;
        // keep the object's mtime (clonefile does, reflink/copy don't) so the entry isn't
        // racy: a racy entry gets re-hashed without the LFS filter and looks modified
        fs::File::open(&file)?.set_modified(fs::metadata(&object)?.modified()?)?;
        let mode = if entry.mode & 0o111 != 0 {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(&file, fs::Permissions::from_mode(mode))?;
        stat::fill(&mut entry, &fs::symlink_metadata(&file)?);
        index.add(&entry)?;
        report.lfs_smudged += 1;
    }
    Ok(())
}
