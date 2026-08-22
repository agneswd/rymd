//! Duplicate file detection.
//!
//! Three passes over the already-scanned tree, so no extra directory
//! traversal happens: group by logical size, then partial hash (head and
//! tail), then full BLAKE3 for groups that still collide. Hard links are
//! not waste; files sharing one inode collapse into a single identity.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::model::{NodeId, NodeKind, ScanModel, TOMBSTONED};

/// Files smaller than this never pay off to hash or delete.
pub const MIN_SIZE: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DuplicateFile {
    pub node_id: NodeId,
    pub path: PathBuf,
    /// True when this path shares an inode with another listed path.
    pub hard_linked: bool,
}

#[derive(Clone, Debug)]
pub struct DuplicateGroup {
    /// Size of one copy in bytes.
    pub size: u64,
    pub files: Vec<DuplicateFile>,
    /// Bytes wasted assuming exactly one identity per group survives.
    pub reclaimable: u64,
}

pub struct FinderProgress {
    pub files_scanned: AtomicU64,
}

/// One hashable file snapshot; Send so detection can run off-thread.
#[derive(Clone, Debug)]
pub struct CandidateFile {
    pub node_id: NodeId,
    pub path: PathBuf,
    pub size: u64,
    pub identity: (u64, u64),
}

/// Walk a finished model and gather hashable candidates.
pub fn collect_candidates(model: &ScanModel) -> Vec<CandidateFile> {
    let mut out = Vec::new();
    let mut stack = vec![model.root()];
    while let Some(id) = stack.pop() {
        let n = model.node(id);
        if n.has_flag(TOMBSTONED) {
            continue;
        }
        match n.kind() {
            NodeKind::Directory => stack.extend(n.children.iter().copied()),
            NodeKind::File if n.own_logical >= MIN_SIZE => out.push(CandidateFile {
                node_id: id,
                path: model.path_of(id),
                size: n.own_logical,
                identity: (n.device, n.inode),
            }),
            _ => {}
        }
    }
    out
}

/// Run the three-pass detection over a finished scan. Convenience wrapper
/// used by tests; the UI path snapshots candidates first and calls
/// [`detect`] off-thread.
#[allow(dead_code)]
pub fn find_duplicates(
    model: &ScanModel,
    cancelled: &AtomicBool,
    progress: &FinderProgress,
) -> Vec<DuplicateGroup> {
    detect(collect_candidates(model), cancelled, progress)
}

/// Detection over pre-collected candidates. Cancellation stops hashing
/// early and returns whatever was found so far.
pub fn detect(
    candidates: Vec<CandidateFile>,
    cancelled: &AtomicBool,
    progress: &FinderProgress,
) -> Vec<DuplicateGroup> {
    let mut by_size: HashMap<u64, Vec<NodeId>> = HashMap::new();
    let mut sizes: HashMap<NodeId, (PathBuf, (u64, u64))> =
        HashMap::with_capacity(candidates.len());
    for c in candidates {
        by_size.entry(c.size).or_default().push(c.node_id);
        sizes.insert(c.node_id, (c.path, c.identity));
    }

    // Pass 1: only sizes with more than one candidate can contain dups.
    let candidates: Vec<(u64, Vec<NodeId>)> =
        by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();

    let mut partial_groups: Vec<(u64, Vec<NodeId>)> = Vec::new();
    for (size, ids) in candidates {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let mut seen: HashMap<u64, NodeId> = HashMap::new();
        let mut dup_ids: Vec<NodeId> = Vec::new();
        for id in ids {
            progress.files_scanned.fetch_add(1, Ordering::Relaxed);
            let Some((path, _)) = sizes.get(&id) else {
                continue;
            };
            match crate::duplicates::hashing::partial_hash(path, size) {
                Ok(h) => match seen.get(&h) {
                    Some(_) => dup_ids.push(id),
                    None => {
                        seen.insert(h, id);
                    }
                },
                Err(_) => continue,
            }
        }
        if !dup_ids.is_empty() {
            let firsts: Vec<NodeId> = seen.into_values().collect();
            let mut all = firsts;
            all.extend(dup_ids);
            partial_groups.push((size, all));
        }
    }

    // Pass 2: full hash inside surviving groups, then split by identity.
    let mut out: Vec<DuplicateGroup> = Vec::new();
    for (size, ids) in partial_groups {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let mut by_hash: HashMap<[u8; 16], Vec<NodeId>> = HashMap::new();
        for id in ids {
            progress.files_scanned.fetch_add(1, Ordering::Relaxed);
            let Some((path, _)) = sizes.get(&id) else {
                continue;
            };
            match crate::duplicates::hashing::full_hash(path) {
                Ok(h) => by_hash.entry(h).or_default().push(id),
                Err(_) => continue,
            }
        }

        for (_, group_ids) in by_hash {
            if group_ids.len() < 2 {
                continue;
            }
            // Collapse identical inodes into one identity.
            let mut identities: HashMap<(u64, u64), Vec<NodeId>> = HashMap::new();
            for id in &group_ids {
                let ident = sizes.get(id).map(|(_, i)| *i).unwrap_or((0, 0));
                identities.entry(ident).or_default().push(*id);
            }

            let mut files: Vec<DuplicateFile> = Vec::new();
            for members in identities.values() {
                let shared = members.len() > 1;
                for (ix, id) in members.iter().enumerate() {
                    files.push(DuplicateFile {
                        node_id: *id,
                        path: sizes.get(id).map(|(p, _)| p.clone()).unwrap_or_default(),
                        hard_linked: shared && ix + 1 < members.len(),
                    });
                }
            }
            files.sort_by(|a, b| a.path.cmp(&b.path));
            // Keeping one identity preserves the content; every further
            // identity is waste. Hard-link siblings never add waste.
            let distinct = identities.len();
            let reclaimable = if distinct > 1 {
                size * (distinct - 1) as u64
            } else {
                0
            };

            out.push(DuplicateGroup {
                size,
                files,
                reclaimable,
            });
        }
    }

    out.sort_by_key(|g| std::cmp::Reverse(g.reclaimable));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::options::ScanOptions;
    use crate::scan::scanner::{ScanOutcome, spawn_scan};
    use std::fs;
    use std::time::Duration;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("rymd-dups-{}-{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scan(p: &std::path::Path) -> ScanModel {
        match spawn_scan(p.to_path_buf(), ScanOptions::default())
            .rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
        {
            ScanOutcome::Completed { model, .. } => *model,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn finds_duplicates_and_skips_unique_small_hardlinks() {
        let td = TempDir::new("basic");
        let root = td.0.clone();

        // Two identical 2 MiB copies.
        let payload: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        fs::write(root.join("a.bin"), &payload).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/b.bin"), &payload).unwrap();

        // A different large file: unique content, must not join a group.
        let other: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 253) as u8).collect();
        fs::write(root.join("c.bin"), &other).unwrap();

        // Small duplicate below the size floor: ignored entirely.
        fs::write(root.join("tiny.txt"), b"same").unwrap();
        fs::write(root.join("tiny2.txt"), b"same").unwrap();

        // Hard link pair of unique content: one identity, no waste.
        let big: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 249) as u8).collect();
        fs::write(root.join("orig.dat"), &big).unwrap();
        fs::hard_link(root.join("orig.dat"), root.join("alias.dat")).unwrap();

        let model = scan(&root);
        let groups = find_duplicates(
            &model,
            &AtomicBool::new(false),
            &FinderProgress {
                files_scanned: AtomicU64::new(0),
            },
        );

        assert_eq!(groups.len(), 1, "exactly one duplicate group: {groups:?}");
        let g = &groups[0];
        assert_eq!(g.size, 2 * 1024 * 1024);
        assert_eq!(g.files.len(), 2);
        assert_eq!(g.reclaimable, 2 * 1024 * 1024);
        let names: Vec<_> = g
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"a.bin".to_string()) && names.contains(&"b.bin".to_string()));
    }

    #[test]
    fn hard_link_pairs_are_not_waste() {
        let td = TempDir::new("hardlink");
        let root = td.0.clone();
        let payload: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 61) as u8).collect();
        fs::write(root.join("one.img"), &payload).unwrap();
        fs::hard_link(root.join("one.img"), root.join("two.img")).unwrap();

        let model = scan(&root);
        let groups = find_duplicates(
            &model,
            &AtomicBool::new(false),
            &FinderProgress {
                files_scanned: AtomicU64::new(0),
            },
        );
        assert!(groups.iter().all(|g| g.reclaimable == 0), "{groups:?}");
    }

    #[test]
    fn cancellation_stops_early_without_panic() {
        let td = TempDir::new("cancel");
        let payload: Vec<u8> = vec![7u8; 3 * 1024 * 1024];
        for i in 0..6 {
            fs::write(td.0.join(format!("f{i}.bin")), &payload).unwrap();
        }
        let model = scan(&td.0);
        let flag = AtomicBool::new(true); // cancelled before we start
        let groups = find_duplicates(
            &model,
            &flag,
            &FinderProgress {
                files_scanned: AtomicU64::new(0),
            },
        );
        // Either empty or partial; must simply terminate.
        let _ = groups;
    }
}
