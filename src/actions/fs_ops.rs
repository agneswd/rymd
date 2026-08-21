//! Filesystem mutation operations.
//!
//! Every destructive action goes through here so safety rules live in one
//! place. Operations are plain functions: they never touch GPUI and can be
//! unit tested against temporary directories.

use std::io;
use std::path::{Path, PathBuf};

use crate::model::{NodeId, NodeKind, ScanModel};

/// Re-read metadata and confirm the path still refers to the scanned object.
/// Guards against deleting something that was renamed, replaced or removed
/// after the scan ran.
pub fn verify_unchanged(model: &ScanModel, node: NodeId) -> bool {
    let n = model.node(node);
    verify_identity(&model.path_of(node), n.device, n.inode, n.kind)
}

/// Confirm `path` still refers to the recorded object. Used both right
/// after a scan (via [`verify_unchanged`]) and inside batch jobs that
/// cannot hold a model reference.
pub fn verify_identity(path: &Path, device: u64, inode: u64, kind: NodeKind) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(md) => {
            let dev_ok = dev_of(&md) == Some(device);
            let ino_ok = ino_of(&md) == Some(inode);
            let kind_ok = match kind {
                NodeKind::Directory => md.is_dir(),
                NodeKind::File => md.is_file(),
                NodeKind::Symlink => md.is_symlink(),
                NodeKind::Other => true,
            };
            dev_ok && ino_ok && kind_ok
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn dev_of(md: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(md.dev())
}
#[cfg(not(unix))]
fn dev_of(_: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn ino_of(md: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(md.ino())
}
#[cfg(not(unix))]
fn ino_of(md: &std::fs::Metadata) -> Option<u64> {
    // No stable file index without opening a handle here; fall back to a
    // weak identity so verification still catches renames and rewrites.
    Some(md.len())
}

/// Never allow these targets to be deleted.
pub fn is_protected(root_path: &Path, path: &Path) -> bool {
    if path == Path::new("/") {
        return true;
    }
    path == root_path
}

pub fn move_to_trash(path: &Path) -> io::Result<()> {
    trash::delete(path).map_err(|e| io::Error::other(e.to_string()))
}

/// Delete a file or a whole directory tree. Symlinks are removed as links;
/// their targets are never touched because `remove_dir_all` does not follow
/// symlinks on any supported platform.
pub fn delete_permanently(path: &Path, kind: NodeKind) -> io::Result<()> {
    match kind {
        NodeKind::Directory => std::fs::remove_dir_all(path),
        NodeKind::File | NodeKind::Other => std::fs::remove_file(path),
        NodeKind::Symlink => {
            #[cfg(unix)]
            {
                std::fs::remove_file(path)
            }
            #[cfg(not(unix))]
            {
                std::fs::remove_dir_all(path).or_else(|_| std::fs::remove_file(path))
            }
        }
    }
}

/// Remove every entry inside `dir`, keeping the directory itself.
/// Iterative so pathological nesting cannot overflow the stack.
/// Returns (entries_removed, bytes_estimated).
pub fn clear_directory(dir: &Path) -> io::Result<(u64, u64)> {
    let mut count = 0u64;
    let mut bytes = 0u64;
    // Post-order walk: deepest children are removed before their parents.
    let mut pending = vec![dir.to_path_buf()];
    let mut stack: Vec<(PathBuf, bool)> = Vec::new(); // (path, is_dir)
    while let Some(current) = pending.pop() {
        let md = std::fs::symlink_metadata(&current)?;
        if md.is_dir() && !md.is_symlink() {
            for entry in std::fs::read_dir(&current)? {
                pending.push(entry?.path());
            }
            // The directory itself stays; only its descendants get removed.
            if current != *dir {
                stack.push((current, true));
            }
        } else {
            bytes += md.len();
            stack.push((current, false));
        }
    }
    while let Some((path, is_dir)) = stack.pop() {
        let res = if is_dir {
            std::fs::remove_dir(&path)
        } else {
            std::fs::remove_file(&path)
        };
        res?;
        count += 1;
    }
    Ok((count, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::options::ScanOptions;
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("rymd-fsops-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scan(p: &Path) -> ScanModel {
        match crate::scan::scanner::spawn_scan(p.to_path_buf(), ScanOptions::default())
            .rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap()
        {
            crate::scan::scanner::ScanOutcome::Completed { model, .. } => *model,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn trash_and_verify_flow() {
        let td = TempDir::new("trash");
        let f = td.0.join("doomed.txt");
        std::fs::write(&f, b"data").unwrap();

        assert!(move_to_trash(&f).is_ok(), "trash failed");
        assert!(!f.exists());
    }

    #[test]
    fn permanent_delete_file_and_tree() {
        let td = TempDir::new("perm");
        let f = td.0.join("file.txt");
        std::fs::write(&f, b"x").unwrap();
        assert!(delete_permanently(&f, NodeKind::File).is_ok());

        let d = td.0.join("tree");
        std::fs::create_dir_all(d.join("nested")).unwrap();
        std::fs::write(d.join("nested/deep.txt"), b"deep").unwrap();
        assert!(delete_permanently(&d, NodeKind::Directory).is_ok());
        assert!(!d.exists());
    }

    #[test]
    fn clear_keeps_directory() {
        let td = TempDir::new("clear");
        let d = td.0.join("keep");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a"), vec![0; 100]).unwrap();
        std::fs::create_dir(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/b"), b"bb").unwrap();

        let (count, _) = clear_directory(&d).unwrap();
        assert_eq!(count, 3); // a, sub, sub/b
        assert!(d.is_dir());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    }

    #[test]
    fn refuses_scan_root() {
        let td = TempDir::new("root");
        assert!(is_protected(&td.0, &td.0));
        assert!(is_protected(&td.0, Path::new("/")));
        let inner = td.0.join("inner");
        std::fs::create_dir(&inner).unwrap();
        assert!(!is_protected(&td.0, &inner));
    }

    #[test]
    fn symlink_delete_removes_link_not_target() {
        let td = TempDir::new("symdel");
        let target = td.0.join("target.txt");
        std::fs::write(&target, b"precious").unwrap();
        let link = td.0.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(delete_permanently(&link, NodeKind::Symlink).is_ok());
        assert!(!link.exists());
        assert!(target.exists(), "symlink removal must not touch target");
    }

    #[test]
    fn inode_change_is_detected() {
        let td = TempDir::new("changed");
        let f = td.0.join("swap.txt");
        std::fs::write(&f, b"v1").unwrap();
        let model = scan(&td.0);
        let node = model.node(model.root()).children[0];

        // Replace the file with different content: same name, new inode.
        std::fs::remove_file(&f).unwrap();
        std::fs::write(&f, b"different and longer").unwrap();
        assert!(
            !verify_unchanged(&model, node),
            "inode change must be caught"
        );
    }

    #[test]
    fn disappeared_file_fails_verification() {
        let td = TempDir::new("gone");
        let f = td.0.join("vanish.txt");
        std::fs::write(&f, b"x").unwrap();
        let model = scan(&td.0);
        let node = model.node(model.root()).children[0];
        std::fs::remove_file(&f).unwrap();
        assert!(!verify_unchanged(&model, node));
    }
}
