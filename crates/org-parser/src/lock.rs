use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Emacs lockfile path for `target` (`path/to/foo.org` -> `path/to/.#foo.org`).
fn lockfile_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lock_name = format!(".#{name}");
    match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(lock_name),
        _ => PathBuf::from(lock_name),
    }
}

/// Best-effort read of the lock symlink target (`user@host.pid[:boottime]`).
/// Never errors — returns `None` if unreadable or empty.
fn describe_lock_owner(lock_path: &Path) -> Option<String> {
    let s = std::fs::read_link(lock_path).ok()?.to_string_lossy().into_owned();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Abort with an error if `target` is currently locked by Emacs (a `.#name`
/// lock symlink exists alongside it), unless `force` is true. Uses
/// `symlink_metadata` (does not follow the link — Emacs lock targets need not
/// resolve to a real file).
pub fn check_not_locked(target: &Path, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    let lock_path = lockfile_path(target);
    if std::fs::symlink_metadata(&lock_path).is_err() {
        return Ok(());
    }
    match describe_lock_owner(&lock_path) {
        Some(owner) => bail!(
            "{} is locked by Emacs (lock owner: {owner}) — pass --force to override",
            target.display()
        ),
        None => bail!(
            "{} is locked by Emacs (lock file present, owner unreadable) — pass --force to override",
            target.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "org-parser-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_lockfile_is_ok() {
        let dir = tempdir();
        let target = dir.join("foo.org");
        std::fs::write(&target, "* heading\n").unwrap();
        assert!(check_not_locked(&target, false).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn lockfile_present_blocks() {
        let dir = tempdir();
        let target = dir.join("foo.org");
        std::fs::write(&target, "* heading\n").unwrap();
        let lock = dir.join(".#foo.org");
        symlink("alice@host.42317:1234567890", &lock).unwrap();

        let err = check_not_locked(&target, false).unwrap_err();
        assert!(err.to_string().contains("locked by Emacs"));
        assert!(err.to_string().contains("alice@host.42317:1234567890"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn lockfile_present_with_force_is_ok() {
        let dir = tempdir();
        let target = dir.join("foo.org");
        std::fs::write(&target, "* heading\n").unwrap();
        let lock = dir.join(".#foo.org");
        symlink("alice@host.42317:1234567890", &lock).unwrap();

        assert!(check_not_locked(&target, true).is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn malformed_lock_target_does_not_panic() {
        let dir = tempdir();
        let target = dir.join("foo.org");
        std::fs::write(&target, "* heading\n").unwrap();
        let lock = dir.join(".#foo.org");
        symlink("", &lock).ok(); // empty target may fail to create on some platforms
        if std::fs::symlink_metadata(&lock).is_ok() {
            let err = check_not_locked(&target, false).unwrap_err();
            assert!(err.to_string().contains("locked by Emacs"));
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
