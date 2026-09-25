//! Filesystem detection and copy-on-write cloning.
//!
//! - macOS: clonefile(2), which clones a whole directory tree in one call (APFS)
//! - Linux: FICLONE per file via `reflink-copy` (btrfs, XFS with reflink, bcachefs, ZFS 2.2+, ...)

use std::io;
use std::path::Path;

pub struct Filesystem {
    pub name: String,
    /// False for filesystems known to lack copy-on-write; unknown ones are probed.
    pub may_clone: bool,
}

#[cfg(target_os = "macos")]
pub fn detect(path: &Path) -> io::Result<Filesystem> {
    let st = statfs(path)?;
    // SAFETY: f_fstypename is a NUL-terminated C string filled in by statfs.
    let name = unsafe { std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let may_clone = !matches!(
        name.as_str(),
        "hfs" | "msdos" | "exfat" | "ntfs" | "nfs" | "smbfs"
    );
    Ok(Filesystem { name, may_clone })
}

#[cfg(target_os = "linux")]
pub fn detect(path: &Path) -> io::Result<Filesystem> {
    let st = statfs(path)?;
    #[allow(clippy::unnecessary_cast)]
    let magic = st.f_type as u64 & 0xffff_ffff;
    let (name, may_clone) = match magic {
        0x9123_683e => ("btrfs", true),
        0x5846_5342 => ("xfs", true),
        0xca45_1a4e => ("bcachefs", true),
        0x2fc1_2fc1 => ("zfs", true),
        0x7461_636f => ("ocfs2", true),
        0x794c_7630 => ("overlayfs", true),
        0xef53 => ("ext4", false),
        0x0102_1994 => ("tmpfs", false),
        0x6969 => ("nfs", false),
        0x6573_5546 => ("fuse", false),
        other => {
            return Ok(Filesystem {
                name: format!("0x{other:x}"),
                may_clone: true,
            })
        }
    };
    Ok(Filesystem {
        name: name.into(),
        may_clone,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn detect(_path: &Path) -> io::Result<Filesystem> {
    Ok(Filesystem {
        name: "unknown".into(),
        may_clone: true,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn statfs(path: &Path) -> io::Result<libc::statfs> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is NUL-terminated and st points to writable memory for one statfs.
    if unsafe { libc::statfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs succeeded, so st is initialised.
    Ok(unsafe { st.assume_init() })
}

/// Clone `src` (file, symlink or directory tree) to `dst`, which must not exist.
/// Data blocks are shared until either side is modified; mtimes are preserved.
#[cfg(target_os = "macos")]
pub fn clone_tree(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Not exported by the libc crate; from <sys/clonefile.h>.
    const CLONE_NOFOLLOW: u32 = 0x0001;

    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both pointers are valid NUL-terminated strings for the duration of the call.
    if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), CLONE_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Clone `src` (file, symlink or directory tree) to `dst`, which must not exist.
/// Data blocks are shared until either side is modified; mtimes are preserved so
/// git's stat data stays valid.
#[cfg(not(target_os = "macos"))]
pub fn clone_tree(src: &Path, dst: &Path) -> io::Result<()> {
    use std::fs;

    let meta = fs::symlink_metadata(src)?;
    let kind = meta.file_type();
    if kind.is_dir() {
        fs::create_dir(dst)?;
        fs::set_permissions(dst, meta.permissions())?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            clone_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else if kind.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else if kind.is_file() {
        reflink_copy::reflink(src, dst)?;
        fs::File::open(dst)?.set_modified(meta.modified()?)?;
    }
    Ok(())
}

/// Clone a single regular file, falling back to a plain copy without copy-on-write.
pub fn clone_or_copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    reflink_copy::reflink_or_copy(src, dst).map(|_| ())
}

/// Errors meaning cloning cannot work here at all (other volume, no CoW support).
pub fn is_unsupported(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    let Some(code) = err.raw_os_error() else {
        return false;
    };
    [libc::EXDEV, libc::ENOTSUP, libc::EOPNOTSUPP, libc::ENOTTY, libc::ENOSYS].contains(&code)
        // FICLONE reports EINVAL when the filesystem can't reflink the file
        || (cfg!(target_os = "linux") && code == libc::EINVAL)
}
