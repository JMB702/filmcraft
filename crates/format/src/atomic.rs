//! Crash-safe file replacement.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Most bytes of the target's file name kept in a temp name; the rest of the 255-byte limit is
/// left for the leading dot, pid, counter and `.tmp`.
const MAX_STEM_BYTES: usize = 200;

/// The temp file a save of `path` goes through (same directory, so the rename never crosses file
/// systems). Hidden, and unique per process and call.
fn temp_path(path: &Path) -> PathBuf {
    let dir = parent_dir(path);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
    // Keep the whole temp name under the 255-byte file name limit, cutting at a char boundary.
    let mut end = name.len().min(MAX_STEM_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let name = &name[..end];
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{name}.{}-{n}.tmp", std::process::id()))
}

fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Replace `path` with `data` atomically and durably:
/// write `data` to a temp file in the same directory, `fsync` it, rename it over `path`, then
/// `fsync` the directory so the rename itself survives a power cut. If anything fails the temp file
/// is removed and `path` is untouched.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    // `rename` replaces the inode, so a new staging file must inherit an existing project's
    // access mode instead of silently broadening a private file to the process default.
    let permissions = match fs::metadata(path) {
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let tmp = temp_path(path);
    let r = (|| {
        {
            // closed before the rename
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            if let Some(permissions) = permissions.as_ref() {
                f.set_permissions(permissions.clone())?;
            }
            f.write_all(data)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    })();
    if let Err(e) = r {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_dir(&parent_dir(path));
    Ok(())
}

/// Publish a complete new file without replacing an occupied final path, including a symlink.
/// Bytes are staged and synced in the destination directory before no-clobber publication.
/// A write failure leaves no partial final file; temporary-file cleanup is best effort.
#[cfg(not(target_arch = "wasm32"))]
pub fn atomic_write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    atomic_write_new_with(path, |file| file.write_all(data).and_then(|_| file.sync_all()))
}

#[cfg(target_arch = "wasm32")]
pub fn atomic_write_new(_path: &Path, _data: &[u8]) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "native exclusive file creation is unavailable in the browser"))
}

#[cfg(not(target_arch = "wasm32"))]
fn atomic_write_new_with(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let dir = parent_dir(path);
    #[cfg(unix)]
    let mut staged = {
        use std::os::unix::fs::PermissionsExt;
        // Like an ordinary file creation: request 0666 and let the process umask decide the
        // effective mode (0644 with the usual 022, 0664 with 002).
        tempfile::Builder::new().permissions(fs::Permissions::from_mode(0o666)).tempfile_in(&dir)? // NOSONAR
    };
    #[cfg(not(unix))]
    let mut staged = tempfile::NamedTempFile::new_in(&dir)?;
    write(staged.as_file_mut())?;
    publish_noclobber(staged, path, |staged, path| staged.persist_noclobber(path))?;
    sync_dir(&dir);
    Ok(())
}

/// Publish the complete staged file at `path` without replacing anything already there.
///
/// `noclobber` is `persist_noclobber`: `renamex_np(RENAME_EXCL)` / `renameat2(RENAME_NOREPLACE)`,
/// else `link`. exFAT, FAT and some network volumes support neither (macOS answers ENOTSUP, Linux
/// EINVAL then EPERM), so auto-save to a project on such a drive failed every time while Save, a
/// plain `rename`, worked. There the name is claimed with an exclusive create (which fails on any
/// existing entry, a symlink included, so nothing is overwritten) and the staged file is renamed
/// over that empty claim, so the final name still only ever holds complete bytes once published.
#[cfg(not(target_arch = "wasm32"))]
fn publish_noclobber(
    staged: tempfile::NamedTempFile,
    path: &Path,
    noclobber: impl FnOnce(tempfile::NamedTempFile, &Path) -> Result<File, tempfile::PersistError>,
) -> io::Result<()> {
    let refused = match noclobber(staged, path) {
        Ok(_published) => return Ok(()),
        Err(e) if no_exclusive_rename(&e.error) => e,
        Err(e) => return Err(e.error),
    };
    // the claim: fails with AlreadyExists when the name is taken (the caller picks another)
    OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(e) = refused.file.persist(path) {
        let _ = fs::remove_file(path);
        return Err(e.error);
    }
    Ok(())
}

/// The volume cannot rename without replacing or hard-link (see [`publish_noclobber`]).
#[cfg(not(target_arch = "wasm32"))]
fn no_exclusive_rename(e: &io::Error) -> bool {
    // ENOTSUP / EOPNOTSUPP: 45 / 102 on Apple platforms, 95 on Linux; EPERM (1) is `link` on Linux FAT
    #[cfg(target_vendor = "apple")]
    const CODES: &[i32] = &[45, 102];
    #[cfg(target_os = "linux")]
    const CODES: &[i32] = &[95, 1];
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    const CODES: &[i32] = &[];
    e.kind() == io::ErrorKind::Unsupported || e.raw_os_error().is_some_and(|c| CODES.contains(&c))
}

/// Best-effort directory sync; directory handles are not uniformly readable or syncable.
fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("filmcraft-atomic-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// What exFAT answers: no exclusive rename and no hard links (ENOTSUP).
    fn refuse(staged: tempfile::NamedTempFile, _path: &Path) -> Result<File, tempfile::PersistError> {
        Err(tempfile::PersistError { error: io::Error::from_raw_os_error(if cfg!(target_os = "linux") { 95 } else { 45 }), file: staged })
    }

    fn staged(dir: &Path, data: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new_in(dir).unwrap();
        f.write_all(data).unwrap();
        f
    }

    #[test]
    fn volumes_without_exclusive_rename_still_publish_without_clobbering() {
        let d = tmpdir("noexcl");
        let p = d.join("auto.fcproj");
        publish_noclobber(staged(&d, b"first"), &p, refuse).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"first");
        // a taken name is still refused as AlreadyExists (the auto-save loop picks a collision name)
        let e = publish_noclobber(staged(&d, b"second"), &p, refuse).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&p).unwrap(), b"first");
        let names: Vec<_> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "no staged or claim files left: {names:?}");
        // other errors are not papered over
        let denied = |f: tempfile::NamedTempFile, _: &Path| Err(tempfile::PersistError { error: io::Error::from(io::ErrorKind::StorageFull), file: f });
        assert_eq!(publish_noclobber(staged(&d, b"x"), &d.join("other.fcproj"), denied).unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert!(!d.join("other.fcproj").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_fallback_never_follows_or_replaces_a_symlink() {
        let d = tmpdir("noexcl-link");
        let target = d.join("target.txt");
        fs::write(&target, b"keep").unwrap();
        let p = d.join("auto.fcproj");
        std::os::unix::fs::symlink(&target, &p).unwrap();
        let e = publish_noclobber(staged(&d, b"new"), &p, refuse).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert!(fs::symlink_metadata(&p).unwrap().file_type().is_symlink());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn long_file_name_still_saves() {
        let d = tmpdir("longname");
        let p = d.join(format!("{}.fcproj", "a".repeat(245)));
        fs::write(&p, b"old").unwrap();
        atomic_write(&p, b"new").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"new");
        let multi = d.join(format!("{}.fcproj", "é".repeat(120)));
        assert!(temp_path(&multi).file_name().unwrap().len() <= 255);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn replaces_and_leaves_no_temp_files() {
        let d = tmpdir("replace");
        let p = d.join("a.fcproj");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn failed_write_keeps_old_file() {
        let d = tmpdir("fail");
        let p = d.join("a.fcproj");
        atomic_write(&p, b"good").unwrap();
        // Renaming a file over a non-empty directory fails: the original target must survive.
        let dir_target = d.join("sub");
        fs::create_dir_all(dir_target.join("x")).unwrap();
        assert!(atomic_write(&dir_target, b"bad").is_err());
        assert_eq!(fs::read(&p).unwrap(), b"good");
        let tmp_left = fs::read_dir(&d).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".tmp")).count();
        assert_eq!(tmp_left, 0);
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let d = tmpdir("permissions");
        let p = d.join("private.fcproj");
        fs::write(&p, b"before").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        atomic_write(&p, b"after").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"after");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn new_file_is_published_complete_without_clobbering_or_temp_leaks() {
        let d = tmpdir("new");
        let p = d.join("audio.wav");
        atomic_write_new(&p, b"complete").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"complete");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let reference = d.join("normal-create");
            OpenOptions::new().write(true).create_new(true).open(&reference).unwrap();
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, fs::metadata(&reference).unwrap().permissions().mode() & 0o777);
            fs::remove_file(reference).unwrap();
        }
        assert_eq!(atomic_write_new(&p, b"replacement").unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&p).unwrap(), b"complete");
        #[cfg(unix)]
        {
            let link = d.join("link.wav");
            std::os::unix::fs::symlink(&p, &link).unwrap();
            assert_eq!(atomic_write_new(&link, b"replacement").unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(fs::read(&p).unwrap(), b"complete");
        }
        let expected = if cfg!(unix) { 2 } else { 1 };
        assert_eq!(fs::read_dir(&d).unwrap().count(), expected, "staging file remained after publication");
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn new_file_can_publish_in_a_directory_without_read_permission() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("write-only-directory");
        let original_permissions = fs::metadata(&d).unwrap().permissions();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o300)).unwrap();
        let p = d.join("audio.wav");
        let result = atomic_write_new(&p, b"complete");
        fs::set_permissions(&d, original_permissions).unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fs::read(&p).unwrap(), b"complete");
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn failed_staging_leaves_no_partial_destination() {
        let d = tmpdir("failed-new");
        let p = d.join("audio.wav");
        let error = atomic_write_new_with(&p, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("simulated full disk"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "simulated full disk");
        assert!(!p.exists());
        assert_eq!(fs::read_dir(&d).unwrap().count(), 0);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn missing_directory_is_an_error() {
        let d = tmpdir("missing");
        assert!(atomic_write(&d.join("nope/a.fcproj"), b"x").is_err());
        fs::remove_dir_all(&d).unwrap();
    }
}
