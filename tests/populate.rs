use git2::{Repository, Signature, StatusOptions};
use git_cow::{populate, PopulateOptions, Report};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

const BIG: usize = 8 * 1024 * 1024;

fn commit_all(repo: &Repository, msg: &str) {
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = Signature::now("test", "test@example.com").unwrap();
    let parent = repo.head().ok().map(|h| h.peel_to_commit().unwrap());
    let parents: Vec<_> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
        .unwrap();
}

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Write a file with an mtime `age` in the past, so git's index doesn't treat it as racy.
fn write_aged(path: &Path, content: impl AsRef<[u8]>, age: Duration) {
    fs::write(path, content).unwrap();
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
}

const HOUR: Duration = Duration::from_secs(3600);

/// Repo with two commits (small.txt v1 -> v2), a big file, and a dirty / untracked /
/// ignored state in the main worktree.
fn fixture() -> (TempDir, Repository) {
    let tmp = TempDir::new().unwrap();
    let repo = Repository::init(tmp.path().join("repo")).unwrap();
    let wd = repo.workdir().unwrap().to_path_buf();
    write_aged(&wd.join("big.bin"), pseudo_random(BIG, 1), 2 * HOUR);
    write_aged(&wd.join(".gitignore"), "node_modules/\n", 2 * HOUR);
    fs::create_dir(wd.join("src")).unwrap();
    write_aged(&wd.join("src/lib.rs"), "fn main() {}\n", 2 * HOUR);
    write_aged(&wd.join("small.txt"), "v1\n", 2 * HOUR);
    commit_all(&repo, "c1");
    write_aged(&wd.join("small.txt"), "v2\n", HOUR);
    commit_all(&repo, "c2");

    fs::create_dir(wd.join("node_modules")).unwrap();
    fs::write(wd.join("node_modules/dep.js"), "x").unwrap();
    fs::write(wd.join("untracked.txt"), "scratch").unwrap();
    fs::write(wd.join("src/lib.rs"), "dirty\n").unwrap();
    (tmp, repo)
}

enum BranchMode {
    New(String),
    Detach,
}

struct AddOptions {
    path: PathBuf,
    commit_ish: Option<String>,
    branch: BranchMode,
    from: Option<PathBuf>,
    include_ignored: bool,
}

fn opts(path: PathBuf, commit_ish: Option<&str>, branch: BranchMode) -> AddOptions {
    AddOptions {
        path,
        commit_ish: commit_ish.map(str::to_owned),
        branch,
        from: None,
        include_ignored: false,
    }
}

fn new_branch(name: &str) -> BranchMode {
    BranchMode::New(name.into())
}

/// What `git worktree add --no-checkout` leaves behind (the git wrapper runs that),
/// followed by `git cow populate`.
fn add(repo: &Repository, o: &AddOptions) -> anyhow::Result<Report> {
    let spec = o.commit_ish.as_deref().unwrap_or("HEAD");
    let commit = repo.revparse_single(spec)?.peel_to_commit()?;
    let head = match &o.branch {
        BranchMode::New(name) => {
            repo.branch(name, &commit, false)?;
            format!("ref: refs/heads/{name}\n")
        }
        BranchMode::Detach => format!("{}\n", commit.id()),
    };
    fs::create_dir_all(&o.path)?;
    let path = o.path.canonicalize()?;
    let name = path.file_name().unwrap();
    let admin = repo.path().canonicalize()?.join("worktrees").join(name);
    fs::create_dir_all(&admin)?;
    fs::write(admin.join("HEAD"), head)?;
    fs::write(admin.join("commondir"), "../..\n")?;
    fs::write(
        admin.join("gitdir"),
        format!("{}\n", path.join(".git").display()),
    )?;
    fs::write(path.join(".git"), format!("gitdir: {}\n", admin.display()))?;

    let populate_opts = PopulateOptions {
        from: o.from.clone(),
        include_ignored: o.include_ignored,
    };
    populate(&path, &populate_opts)
}

#[test]
fn refuses_worktree_with_files() {
    let (tmp, repo) = fixture();
    let wt = tmp.path().join("wt");
    add(&repo, &opts(wt.clone(), None, new_branch("b1"))).unwrap();
    let opts = PopulateOptions {
        from: None,
        include_ignored: true,
    };
    let err = populate(&wt, &opts).unwrap_err();
    assert!(err.to_string().contains("not a fresh worktree"), "{err}");
}

fn status(path: &Path) -> Vec<String> {
    let repo = Repository::open(path).unwrap();
    let mut o = StatusOptions::new();
    o.include_untracked(true).include_ignored(true);
    let statuses = repo.statuses(Some(&mut o)).unwrap();
    statuses
        .iter()
        .map(|s| format!("{:?} {}", s.status(), s.path().unwrap()))
        .collect()
}

/// Like [`status`] but without ignored files (carried build caches are expected).
fn changes(path: &Path) -> Vec<String> {
    status(path)
        .into_iter()
        .filter(|s| !s.starts_with("Status(IGNORED)"))
        .collect()
}

/// Physical location of the file's first block.
#[cfg(target_os = "macos")]
fn physical_offset(path: &Path) -> u64 {
    use std::os::fd::AsRawFd;
    let file = fs::File::open(path).unwrap();
    file.sync_all().unwrap();
    let mut l2p = libc::log2phys {
        l2p_flags: 0,
        l2p_contigbytes: 0,
        l2p_devoffset: 0,
    };
    // SAFETY: fd is open and l2p is a valid log2phys struct.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_LOG2PHYS, &mut l2p) };
    assert_eq!(rc, 0, "F_LOG2PHYS failed");
    l2p.l2p_devoffset as u64
}

/// Physical location of the file's first extent (FIEMAP).
#[cfg(target_os = "linux")]
fn physical_offset(path: &Path) -> u64 {
    use std::os::fd::AsRawFd;

    #[repr(C)]
    #[derive(Default)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }
    #[repr(C)]
    #[derive(Default)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
        extents: [Extent; 1],
    }
    const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
    const FIEMAP_FLAG_SYNC: u32 = 1;

    let file = fs::File::open(path).unwrap();
    let mut map = Fiemap {
        length: u64::MAX,
        flags: FIEMAP_FLAG_SYNC,
        extent_count: 1,
        ..Default::default()
    };
    // SAFETY: fd is open and map is a fiemap with room for one extent.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, &mut map) };
    assert_eq!(rc, 0, "FIEMAP failed");
    assert_eq!(map.mapped_extents, 1);
    map.extents[0].physical
}

/// Asserts `a` and `b` share storage, unless the filesystem can't clone (then the
/// worktree is a regular checkout; only allowed off macOS where tmp is always APFS).
fn assert_shared(report: &Report, a: &Path, b: &Path) {
    if report.cloned == 0 && !cfg!(target_os = "macos") {
        eprintln!("{} can't clone, skipping block check", report.filesystem);
        return;
    }
    assert_eq!(
        physical_offset(a),
        physical_offset(b),
        "{a:?} and {b:?} don't share blocks"
    );
}

#[test]
fn same_commit_shares_blocks_and_is_clean() {
    let (tmp, repo) = fixture();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), None, new_branch("b1"))).unwrap();

    let source_big = repo.workdir().unwrap().join("big.bin");
    assert_shared(&report, &source_big, &wt.join("big.bin"));
    // control: a written (not cloned) copy must not share blocks
    let copy = tmp.path().join("copy.bin");
    fs::write(&copy, fs::read(&source_big).unwrap()).unwrap();
    assert_ne!(physical_offset(&source_big), physical_offset(&copy));
    assert!(status(&wt).is_empty(), "{:?}", status(&wt));
    if report.cloned > 0 {
        assert_eq!(report.rewritten, 1); // only the dirty file differs from the commit
    }
    assert_eq!(
        fs::read_to_string(wt.join("src/lib.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert!(!wt.join("node_modules").exists());
    assert!(!wt.join("untracked.txt").exists());
    let wt_repo = Repository::open(&wt).unwrap();
    assert_eq!(wt_repo.head().unwrap().name().unwrap(), "refs/heads/b1");
}

#[test]
fn reuses_stat_data_of_clean_source_files() {
    let (tmp, repo) = fixture();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), None, new_branch("b1"))).unwrap();
    if report.cloned == 0 {
        return; // regular checkout on this filesystem
    }
    // big.bin, .gitignore, small.txt; src/lib.rs is dirty in the source
    assert_eq!(report.stat_reused, 3);
}

#[test]
fn older_commit_rewrites_only_changed_files() {
    let (tmp, repo) = fixture();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), Some("HEAD~1"), BranchMode::Detach)).unwrap();

    if report.cloned > 0 {
        assert_eq!(report.rewritten, 2); // small.txt + dirty src/lib.rs
    }
    assert_eq!(fs::read_to_string(wt.join("small.txt")).unwrap(), "v1\n");
    assert!(status(&wt).is_empty(), "{:?}", status(&wt));
    assert!(Repository::open(&wt).unwrap().head_detached().unwrap());
}

#[test]
fn include_ignored_keeps_ignored_files() {
    let (tmp, repo) = fixture();
    let wt = tmp.path().join("wt");
    let mut o = opts(wt.clone(), None, new_branch("b1"));
    o.include_ignored = true;
    let report = add(&repo, &o).unwrap();
    if report.cloned == 0 {
        return;
    }
    assert!(wt.join("node_modules/dep.js").exists());
    assert!(!wt.join("untracked.txt").exists());
}

#[test]
fn clones_from_other_worktree() {
    let (tmp, repo) = fixture();
    let wt1 = tmp.path().join("wt1");
    add(&repo, &opts(wt1.clone(), None, new_branch("b1"))).unwrap();
    let wt2 = tmp.path().join("wt2");
    let mut o = opts(wt2.clone(), None, new_branch("b2"));
    o.from = Some(wt1.clone());
    let report = add(&repo, &o).unwrap();

    if report.cloned > 0 {
        assert_eq!(report.rewritten, 0);
    }
    assert_shared(&report, &wt1.join("big.bin"), &wt2.join("big.bin"));
    assert!(status(&wt2).is_empty());
}

// --- git-lfs ---

fn pointer(content: &[u8]) -> (String, String) {
    let oid: String = Sha256::digest(content)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let text = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        content.len()
    );
    (oid, text)
}

fn store_lfs_object(repo: &Repository, oid: &str, content: &[u8]) -> PathBuf {
    let dir = repo
        .path()
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4]);
    fs::create_dir_all(&dir).unwrap();
    write_aged(&dir.join(oid), content, 2 * HOUR);
    dir.join(oid)
}

/// Commits pointers for model.bin (A, then B) like git-lfs' clean filter would, leaves
/// the smudged B content in the worktree and both objects in the local LFS store.
fn lfs_fixture() -> (TempDir, Repository, [Vec<u8>; 2], [PathBuf; 2]) {
    let tmp = TempDir::new().unwrap();
    let repo = Repository::init(tmp.path().join("repo")).unwrap();
    let wd = repo.workdir().unwrap().to_path_buf();
    let (a, b) = (pseudo_random(BIG, 1), pseudo_random(BIG, 2));
    let ((oid_a, ptr_a), (oid_b, ptr_b)) = (pointer(&a), pointer(&b));

    write_aged(
        &wd.join(".gitattributes"),
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        2 * HOUR,
    );
    write_aged(&wd.join("model.bin"), ptr_a, 2 * HOUR);
    commit_all(&repo, "c1");
    write_aged(&wd.join("model.bin"), ptr_b, HOUR);
    commit_all(&repo, "c2");
    fs::write(wd.join("model.bin"), &b).unwrap();

    let objects = [
        store_lfs_object(&repo, &oid_a, &a),
        store_lfs_object(&repo, &oid_b, &b),
    ];
    (tmp, repo, [a, b], objects)
}

#[test]
fn lfs_keeps_smudged_clone() {
    let (tmp, repo, [_, b], _) = lfs_fixture();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), None, new_branch("b1"))).unwrap();

    assert_eq!(fs::read(wt.join("model.bin")).unwrap(), b);
    assert!(status(&wt).is_empty(), "{:?}", status(&wt));
    if report.cloned > 0 {
        assert_eq!((report.lfs_cloned, report.rewritten), (1, 0));
        assert_shared(
            &report,
            &repo.workdir().unwrap().join("model.bin"),
            &wt.join("model.bin"),
        );
    }
}

#[test]
fn lfs_smudges_changed_file_from_store() {
    let (tmp, repo, [a, _], [object_a, _]) = lfs_fixture();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), Some("HEAD~1"), BranchMode::Detach)).unwrap();

    assert_eq!(report.lfs_smudged, 1);
    assert_eq!(fs::read(wt.join("model.bin")).unwrap(), a);
    assert!(status(&wt).is_empty(), "{:?}", status(&wt));
    assert_shared(&report, &object_a, &wt.join("model.bin"));
}

#[test]
fn lfs_reports_missing_objects() {
    let (tmp, repo, [a, _], [object_a, _]) = lfs_fixture();
    fs::remove_file(object_a).unwrap();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), Some("HEAD~1"), BranchMode::Detach)).unwrap();

    assert_eq!(report.lfs_missing, [PathBuf::from("model.bin")]);
    assert_eq!(
        fs::read_to_string(wt.join("model.bin")).unwrap(),
        pointer(&a).1
    );
}

#[test]
fn lfs_rejects_corrupt_store_object() {
    let (tmp, repo, [a, _], [object_a, _]) = lfs_fixture();
    // same size, different bytes
    write_aged(&object_a, pseudo_random(BIG, 3), 2 * HOUR);
    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), Some("HEAD~1"), BranchMode::Detach)).unwrap();

    assert_eq!(report.lfs_corrupt, [PathBuf::from("model.bin")]);
    assert_eq!(report.lfs_smudged, 0);
    assert_eq!(
        fs::read_to_string(wt.join("model.bin")).unwrap(),
        pointer(&a).1
    );
}

#[test]
fn lfs_leaves_extension_pointers_alone() {
    let (tmp, repo, [_, b], _) = lfs_fixture();
    let wd = repo.workdir().unwrap().to_path_buf();
    let (oid_b, ptr_b) = pointer(&b);
    let ext_ptr = ptr_b.replacen("oid ", &format!("ext-0-foo sha256:{oid_b}\noid "), 1);
    write_aged(&wd.join("model.bin"), &ext_ptr, HOUR / 2);
    commit_all(&repo, "ext");
    fs::write(wd.join("model.bin"), &b).unwrap();

    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), None, new_branch("b1"))).unwrap();

    assert_eq!(report.lfs_unsupported, [PathBuf::from("model.bin")]);
    assert_eq!((report.lfs_cloned, report.lfs_smudged), (0, 0));
    assert_eq!(fs::read_to_string(wt.join("model.bin")).unwrap(), ext_ptr);
}

#[test]
fn lfs_smudges_pointer_files_cloned_from_source() {
    let (tmp, repo, [_, b], _) = lfs_fixture();
    // source checked out without smudging: pointer text, clean in its index
    let wd = repo.workdir().unwrap().to_path_buf();
    write_aged(&wd.join("model.bin"), pointer(&b).1, HOUR);
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("model.bin")).unwrap();
    index.write().unwrap();

    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), None, new_branch("b1"))).unwrap();

    assert_eq!(report.lfs_smudged, 1);
    assert_eq!(fs::read(wt.join("model.bin")).unwrap(), b);
    assert!(status(&wt).is_empty(), "{:?}", status(&wt));
}

#[test]
fn lfs_file_leaving_lfs_gets_pointer_text() {
    let (tmp, repo, [_, b], _) = lfs_fixture();
    // target commit keeps the pointer blob but drops the LFS attribute
    let wd = repo.workdir().unwrap().to_path_buf();
    fs::remove_file(wd.join(".gitattributes")).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_path(Path::new(".gitattributes")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = Signature::now("test", "test@example.com").unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "no lfs", &tree, &[&head])
        .unwrap();

    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), None, new_branch("b1"))).unwrap();

    assert_eq!(report.lfs_cloned, 0);
    assert_eq!(
        fs::read_to_string(wt.join("model.bin")).unwrap(),
        pointer(&b).1
    );
}

// --- worktree metadata and branch semantics ---

#[test]
fn attribute_change_disables_stat_reuse() {
    let (tmp, repo) = fixture();
    let wd = repo.workdir().unwrap().to_path_buf();
    write_aged(&wd.join(".gitattributes"), "*.txt text\n", HOUR / 2);
    commit_all(&repo, "attrs");

    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), Some("HEAD~1"), BranchMode::Detach)).unwrap();
    assert_eq!(report.stat_reused, 0);
    assert!(status(&wt).is_empty(), "{:?}", status(&wt));
}

#[test]
fn submodule_cleanup_does_not_follow_symlinked_parent() {
    let (tmp, repo) = fixture();
    let wd = repo.workdir().unwrap().to_path_buf();
    // gitlink at modules/sub in the commit
    let mut index = repo.index().unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    let mut entry = index.get_path(Path::new("small.txt"), 0).unwrap();
    entry.mode = 0o160000;
    entry.id = head.id();
    entry.path = b"modules/sub".to_vec();
    index.add(&entry).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = Signature::now("test", "test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "gitlink", &tree, &[&head])
        .unwrap();
    // ...while the source has `modules` as a symlink to a directory outside
    let outside = tmp.path().join("outside");
    fs::create_dir_all(outside.join("sub")).unwrap();
    fs::write(outside.join("sub/keep.txt"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, wd.join("modules")).unwrap();

    let wt = tmp.path().join("wt");
    add(&repo, &opts(wt.clone(), None, new_branch("b1"))).unwrap();

    assert!(
        outside.join("sub/keep.txt").exists(),
        "followed the symlink"
    );
    assert!(fs::symlink_metadata(wt.join("modules")).unwrap().is_dir());
}

// --- carrying ignored files (agent worktrees) ---

/// Ignored build caches, a virtualenv under an unusual name, runtime files, a nested
/// ignored tmp dir next to tracked files, and untracked scratch.
fn agent_fixture() -> (TempDir, Repository) {
    let (tmp, repo) = fixture();
    let wd = repo.workdir().unwrap().to_path_buf();
    write_aged(
        &wd.join(".gitignore"),
        "node_modules/\n_build/\nenv/\ntmp/\n*.sock\napp/cache/\n",
        HOUR / 2,
    );
    fs::create_dir(wd.join("app")).unwrap();
    write_aged(&wd.join("app/big.bin"), pseudo_random(BIG, 4), HOUR / 2);
    commit_all(&repo, "agent");
    fs::write(wd.join("scratch.txt"), "scratch").unwrap();

    fs::create_dir_all(wd.join("_build/dev")).unwrap();
    fs::write(wd.join("_build/dev/app.beam"), pseudo_random(BIG, 5)).unwrap();
    fs::create_dir_all(wd.join("env/bin")).unwrap();
    fs::write(wd.join("env/pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    fs::create_dir(wd.join("tmp")).unwrap();
    fs::write(wd.join("tmp/x"), "x").unwrap();
    fs::create_dir(wd.join("app/cache")).unwrap();
    fs::write(wd.join("app/cache/c"), "c").unwrap();
    // live runtime state
    let _listener = std::os::unix::net::UnixListener::bind(wd.join("server.sock")).unwrap();
    let _nested = std::os::unix::net::UnixListener::bind(wd.join("node_modules/dep.sock")).unwrap();
    (tmp, repo)
}

fn carry(path: PathBuf) -> AddOptions {
    let mut o = opts(path, None, BranchMode::Detach);
    o.include_ignored = true;
    o
}

#[test]
fn carries_build_caches_and_excludes_location_bound_state() {
    let (tmp, repo) = agent_fixture();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &carry(wt.clone())).unwrap();
    if report.cloned == 0 {
        return; // regular checkout on this filesystem
    }
    let paths = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
    assert_eq!(
        report.carried,
        paths(&["_build", "app/cache", "node_modules"])
    );
    assert_eq!(report.excluded, paths(&["env", "server.sock", "tmp"]));
    assert!(wt.join("node_modules/dep.js").exists());
    assert!(!wt.join("env").exists() && !wt.join("tmp").exists());
    assert!(!wt.join("scratch.txt").exists());
    let source = repo.workdir().unwrap();
    assert_shared(
        &report,
        &source.join("_build/dev/app.beam"),
        &wt.join("_build/dev/app.beam"),
    );
    assert!(changes(&wt).is_empty(), "{:?}", changes(&wt));
}

#[test]
fn excluded_nested_path_keeps_siblings_cloned() {
    let (tmp, repo) = agent_fixture();
    repo.config()
        .unwrap()
        .set_multivar("cow.exclude", "^$", "app/cache")
        .unwrap();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &carry(wt.clone())).unwrap();
    if report.cloned == 0 {
        return;
    }
    assert!(report.excluded.contains(&PathBuf::from("app/cache")));
    assert!(!wt.join("app/cache").exists());
    let source = repo.workdir().unwrap();
    assert_shared(
        &report,
        &source.join("app/big.bin"),
        &wt.join("app/big.bin"),
    );
    assert!(changes(&wt).is_empty(), "{:?}", changes(&wt));
}

#[test]
fn cow_exclude_config_adds_and_removes_patterns() {
    let (tmp, repo) = agent_fixture();
    let mut config = repo.config().unwrap();
    config
        .set_multivar("cow.exclude", "^$", "node_modules")
        .unwrap();
    config.set_multivar("cow.exclude", "^$", "!tmp").unwrap();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &carry(wt.clone())).unwrap();
    if report.cloned == 0 {
        return;
    }
    assert!(!wt.join("node_modules").exists());
    assert!(wt.join("tmp/x").exists());
    assert!(report.excluded.contains(&PathBuf::from("node_modules")));
}

#[test]
fn no_ignored_skips_all_ignored_files() {
    let (tmp, repo) = agent_fixture();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &opts(wt.clone(), None, BranchMode::Detach)).unwrap();
    assert!(report.carried.is_empty());
    for path in ["node_modules", "_build", "app/cache", "tmp", "env"] {
        assert!(!wt.join(path).exists(), "{path}");
    }
    assert!(status(&wt).is_empty(), "{:?}", status(&wt));
}

#[test]
fn worktreeinclude_limits_what_is_carried() {
    let (tmp, repo) = agent_fixture();
    let wd = repo.workdir().unwrap();
    // listing tmp/ overrides the built-in default exclude
    fs::write(wd.join(".worktreeinclude"), "_build/\ntmp/\n").unwrap();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &carry(wt.clone())).unwrap();
    if report.cloned == 0 {
        return;
    }
    let paths = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
    assert_eq!(report.carried, paths(&["_build", "tmp"]));
    assert!(!wt.join("node_modules").exists());
    assert!(wt.join("tmp/x").exists());
}

#[test]
fn worktreeinclude_can_select_inside_an_ignored_dir() {
    let (tmp, repo) = agent_fixture();
    let wd = repo.workdir().unwrap();
    fs::create_dir_all(wd.join("_build/prod")).unwrap();
    fs::write(wd.join("_build/prod/app.beam"), "prod").unwrap();
    fs::write(wd.join(".worktreeinclude"), "_build/dev/\n").unwrap();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &carry(wt.clone())).unwrap();
    if report.cloned == 0 {
        return;
    }
    assert_eq!(report.carried, [PathBuf::from("_build/dev")]);
    assert!(wt.join("_build/dev/app.beam").exists());
    assert!(!wt.join("_build/prod").exists());
}

#[test]
fn require_include_carries_nothing_without_worktreeinclude() {
    let (tmp, repo) = agent_fixture();
    repo.config()
        .unwrap()
        .set_bool("cow.requireInclude", true)
        .unwrap();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &carry(wt.clone())).unwrap();
    assert!(report.carried.is_empty(), "{:?}", report.carried);
    assert!(!wt.join("node_modules").exists());
}

#[test]
fn nested_worktrees_and_tool_state_are_never_carried() {
    let (tmp, repo) = agent_fixture();
    let wd = repo.workdir().unwrap();
    // e.g. Claude Code's .claude/worktrees/<name>/.git (a file) and worktrunk's .worktrees
    write_aged(
        &wd.join(".gitignore"),
        "node_modules/\n_build/\nenv/\ntmp/\n*.sock\napp/cache/\nagents/\n.worktrees/\n",
        HOUR / 2,
    );
    fs::create_dir_all(wd.join("agents/one")).unwrap();
    fs::write(wd.join("agents/one/.git"), "gitdir: /elsewhere\n").unwrap();
    fs::create_dir_all(wd.join(".worktrees/two")).unwrap();
    fs::write(wd.join(".worktreeinclude"), "agents/\n.worktrees/\n").unwrap();
    commit_all(&repo, "gitignore");

    let wt = tmp.path().join("wt");
    let report = add(&repo, &carry(wt.clone())).unwrap();
    if report.cloned == 0 {
        return;
    }
    assert!(report.carried.is_empty(), "{:?}", report.carried);
    assert!(report.excluded.contains(&PathBuf::from("agents")));
    assert!(report.excluded.contains(&PathBuf::from(".worktrees")));
    assert!(!wt.join("agents").exists() && !wt.join(".worktrees").exists());
}

#[test]
fn carry_ignored_config_turns_carrying_off() {
    let (tmp, repo) = agent_fixture();
    repo.config()
        .unwrap()
        .set_bool("cow.carryIgnored", false)
        .unwrap();
    let wt = tmp.path().join("wt");
    let report = add(&repo, &carry(wt.clone())).unwrap();
    assert!(report.carried.is_empty(), "{:?}", report.carried);
    assert!(!wt.join("node_modules").exists());
}
