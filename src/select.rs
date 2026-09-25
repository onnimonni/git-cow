//! Which paths of the source worktree get cloned.
//!
//! Tracked files are always cloned. Untracked scratch files never are. Ignored files
//! (build caches: node_modules, _build, deps, target, ...) are carried over, chosen by:
//!
//! 1. never: sockets/fifos/devices, VCS metadata, tool state (`.worktrees`, ...),
//!    nested worktrees or repositories, and build state bound to its location
//!    (virtualenvs, CMake build dirs)
//! 2. `git config cow.exclude <gitignore pattern>` (multi-valued) excludes more
//! 3. with a `.worktreeinclude` (gitignore syntax, shared with worktrunk and Claude
//!    Code) only matching paths are carried
//! 4. without it: everything except location-bound or live state (virtualenvs, devenv
//!    state, `tmp/`, `log/`, pid files); `git config cow.requireInclude true` carries
//!    nothing instead, like Claude Code
//!
//! Only tracked directories are walked, so a large ignored tree costs one clone call.

use crate::stat::SourceSnapshot;
use git2::Repository;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

pub const INCLUDE_FILE: &str = ".worktreeinclude";

/// Never carried, whatever `.worktreeinclude` says.
const NEVER: &[&str] = &[
    ".bzr",
    ".hg",
    ".jj",
    ".pijul",
    ".sl",
    ".svn",
    ".conductor",
    ".entire",
    ".worktrees",
];

/// Carried only when listed in `.worktreeinclude`: tied to their location or live state.
const DEFAULT_EXCLUDES: &[&str] = &[
    ".venv/", "venv/", ".devenv/", ".direnv/", "tmp/", "log/", "*.pid", "*.sock",
];

pub struct Rules {
    /// Gitignore-style matcher of `.worktreeinclude`, if the source has one.
    include: Option<Gitignore>,
    /// Its patterns, to see whether one reaches below an ignored directory.
    include_lines: Vec<String>,
    /// Built-in default excludes; `cow.exclude` lines can negate them (`!tmp/`).
    defaults: Gitignore,
    /// `cow.exclude`.
    user: Gitignore,
    require_include: bool,
}

impl Rules {
    pub fn load(repo: &Repository, source: &Path) -> Rules {
        let config = repo.config().ok();
        let mut user_lines = Vec::new();
        if let Some(values) = config
            .as_ref()
            .and_then(|c| c.multivar("cow.exclude", None).ok())
        {
            let _ =
                values.for_each(|entry| user_lines.extend(entry.value().ok().map(str::to_owned)));
        }
        let build = |lines: &mut dyn Iterator<Item = &str>| {
            let mut builder = GitignoreBuilder::new(source);
            for line in lines {
                let _ = builder.add_line(None, line);
            }
            builder.build().unwrap_or_else(|_| Gitignore::empty())
        };
        let include_file = source.join(INCLUDE_FILE);
        Rules {
            include: include_file
                .is_file()
                .then(|| Gitignore::new(&include_file).0),
            include_lines: fs::read_to_string(&include_file)
                .unwrap_or_default()
                .lines()
                .map(|l| l.trim().trim_start_matches('/').to_owned())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect(),
            defaults: build(
                &mut DEFAULT_EXCLUDES.iter().copied().chain(
                    user_lines
                        .iter()
                        .filter(|l| l.starts_with('!'))
                        .map(String::as_str),
                ),
            ),
            user: build(
                &mut user_lines
                    .iter()
                    .filter(|l| !l.starts_with('!'))
                    .map(String::as_str),
            ),
            require_include: config
                .and_then(|c| c.get_bool("cow.requireInclude").ok())
                .unwrap_or(false),
        }
    }

    fn decide(&self, rel: &Path, abs: &Path, kind: fs::FileType) -> Decision {
        let is_dir = kind.is_dir();
        let name = rel.file_name().unwrap_or_default();
        if kind.is_socket()
            || kind.is_fifo()
            || kind.is_block_device()
            || kind.is_char_device()
            || NEVER.iter().any(|n| name == OsStr::new(n))
            || (is_dir && (contains_worktrees(abs) || is_location_bound(abs)))
            || self
                .user
                .matched_path_or_any_parents(rel, is_dir)
                .is_ignore()
        {
            return Decision::Exclude;
        }
        if let Some(include) = &self.include {
            return if include.matched_path_or_any_parents(rel, is_dir).is_ignore() {
                Decision::Carry
            } else if is_dir && self.include_reaches_into(rel) {
                Decision::Descend
            } else {
                Decision::Skip
            };
        }
        if self.require_include {
            Decision::Skip
        } else if self
            .defaults
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
        {
            Decision::Exclude
        } else {
            Decision::Carry
        }
    }

    /// Whether a `.worktreeinclude` pattern names something below `dir` (e.g.
    /// `target/debug/` while only `target/` is the ignored entry), so the directory
    /// has to be walked instead of skipped.
    fn include_reaches_into(&self, dir: &Path) -> bool {
        let prefix = format!("{}/", dir.to_string_lossy());
        self.include_lines
            .iter()
            .any(|l| l.starts_with(&prefix) || l.starts_with("**/"))
    }
}

/// Files marking build state with absolute paths to its own location: a clone would
/// keep building (or installing into) the source worktree.
const LOCATION_BOUND_MARKERS: &[&str] = &[
    "pyvenv.cfg",     // Python virtualenv (under any name)
    "CMakeCache.txt", // CMake build directory
];

/// `dir` or one of its direct subdirectories (`build/debug/`) holds a
/// [`LOCATION_BOUND_MARKERS`] file.
fn is_location_bound(dir: &Path) -> bool {
    let has_marker = |d: &Path| LOCATION_BOUND_MARKERS.iter().any(|m| d.join(m).is_file());
    has_marker(dir)
        || fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_type().is_ok_and(|t| t.is_dir()) && has_marker(&e.path()))
        })
}

/// A repository or worktree (`.git` inside), or a directory of worktrees such as
/// `.claude/worktrees/`: cloning those would copy other checkouts.
fn contains_worktrees(dir: &Path) -> bool {
    if fs::symlink_metadata(dir.join(".git")).is_ok() {
        return true;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| fs::symlink_metadata(e.path().join(".git")).is_ok_and(|m| m.is_file()))
}

enum Decision {
    Carry,
    /// Left behind on purpose (reported).
    Exclude,
    /// Not selected by `.worktreeinclude` / `cow.requireInclude` (not reported).
    Skip,
    /// Walk into the directory, deciding for its entries.
    Descend,
}

#[derive(Default)]
pub struct Selection {
    /// Paths (relative to the worktree root) not to clone.
    pub skip: HashSet<PathBuf>,
    /// Ancestors of skipped paths; these directories are recreated, not cloned whole.
    pub partial: HashSet<PathBuf>,
    /// Ignored paths carried over.
    pub carried: Vec<PathBuf>,
    /// Ignored paths left behind on purpose (never carried or excluded).
    pub excluded: Vec<PathBuf>,
}

pub struct Selector<'a> {
    pub source: &'a Path,
    pub dest: &'a Path,
    pub repo: &'a Repository,
    pub snapshot: &'a SourceSnapshot,
    pub rules: &'a Rules,
    pub include_ignored: bool,
}

impl Selector<'_> {
    pub fn select(&self) -> io::Result<Selection> {
        let mut selection = Selection::default();
        self.walk(Path::new(""), false, &mut selection)?;
        for path in &selection.skip {
            selection
                .partial
                .extend(path.ancestors().skip(1).map(Path::to_path_buf));
        }
        selection.partial.remove(Path::new(""));
        selection.carried.sort();
        selection.excluded.sort();
        Ok(selection)
    }

    /// `in_ignored`: `rel` is inside an ignored directory being walked for
    /// `.worktreeinclude` patterns (everything below is ignored too).
    fn walk(&self, rel: &Path, in_ignored: bool, selection: &mut Selection) -> io::Result<()> {
        for entry in fs::read_dir(self.source.join(rel))? {
            let entry = entry?;
            let name = entry.file_name();
            let path = rel.join(&name);
            let kind = entry.file_type()?;
            let key = path.as_os_str().as_bytes();

            // .git is per-worktree; never clone the new worktree into itself
            if (rel.as_os_str().is_empty() && name == ".git") || self.dest.starts_with(entry.path())
            {
                selection.skip.insert(path);
                continue;
            }
            if !in_ignored {
                if self.snapshot.is_tracked_dir(key) {
                    if kind.is_dir() {
                        self.walk(&path, false, selection)?;
                    }
                    continue;
                }
                if self.snapshot.is_tracked_file(key) {
                    continue; // cloned with its directory
                }
                if !self.repo.is_path_ignored(&path).unwrap_or(false) {
                    // untracked scratch: agents start clean. Directories may still hold
                    // ignored caches (`vendor/bundle/` ignored, `vendor/` not).
                    if kind.is_dir() && self.include_ignored {
                        self.walk_for_carried(&path, false, selection)?;
                    } else {
                        selection.skip.insert(path);
                    }
                    continue;
                }
            }
            if !self.include_ignored {
                selection.skip.insert(path);
                continue;
            }
            match self.rules.decide(&path, &entry.path(), kind) {
                Decision::Carry => selection.carried.push(path),
                Decision::Exclude => {
                    selection.excluded.push(path.clone());
                    selection.skip.insert(path);
                }
                Decision::Skip => {
                    selection.skip.insert(path);
                }
                Decision::Descend => self.walk_for_carried(&path, true, selection)?,
            }
        }
        Ok(())
    }

    /// Walk `dir` for entries to carry; skip it as a whole when there are none, so it
    /// isn't recreated as an empty directory.
    fn walk_for_carried(
        &self,
        dir: &Path,
        in_ignored: bool,
        selection: &mut Selection,
    ) -> io::Result<()> {
        let carried = selection.carried.len();
        self.walk(dir, in_ignored, selection)?;
        if selection.carried.len() == carried {
            selection.skip.retain(|p| !p.starts_with(dir));
            selection.skip.insert(dir.to_path_buf());
        }
        Ok(())
    }
}

/// Display helper: `a, b, c`.
pub fn join(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| OsStr::to_string_lossy(p.as_os_str()).into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}
