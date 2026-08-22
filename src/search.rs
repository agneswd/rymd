//! Global file search over a completed [`ScanModel`].
//!
//! Names are normalized once at index build time (lossy UTF-8, lowercased)
//! and kept next to their node ids, so every keystroke is a substring scan
//! over flat byte arrays instead of per-node OsString conversion.
//!
//! Queries grow one character at a time most of the time, so the UI keeps
//! the previous result set and refines it instead of re-scanning the whole
//! model; a changed (non-extending) query falls back to a full pass. Full
//! passes run off the GPUI thread through a generation counter so an
//! older search can never overwrite newer results.

use crate::model::{NodeId, ScanModel};

/// One normalized name per node, indexed by `NodeId`.
pub struct SearchIndex {
    names: Vec<Box<[u8]>>,
}

impl SearchIndex {
    /// Build from a completed model. One allocation per node plus one big
    /// normalization pass; runs off-thread in production.
    pub fn build(model: &ScanModel) -> Self {
        let mut names = Vec::with_capacity(model.len());
        for ix in 0..model.len() {
            let n = model.node(NodeId(ix as u32));
            names.push(normalize(n.name.as_encoded_bytes().to_vec()).into_boxed_slice());
        }
        Self { names }
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// All node ids whose normalized name contains `query` (already
    /// normalized). Order follows arena order, which callers sort for
    /// presentation.
    pub fn find(&self, query: &[u8]) -> Vec<NodeId> {
        if query.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (ix, name) in self.names.iter().enumerate() {
            if memmem(name, query) {
                out.push(NodeId(ix as u32));
                if out.len() == usize::MAX / 2 {
                    break; // absurd cap; never hit in practice
                }
            }
        }
        out
    }

    /// Refine a previous result set for a longer query without touching
    /// the full index.
    pub fn refine(&self, previous: &[NodeId], query: &[u8]) -> Vec<NodeId> {
        if query.is_empty() {
            return Vec::new();
        }
        previous
            .iter()
            .copied()
            .filter(|id| memmem(&self.names[id.index()], query))
            .collect()
    }
}

/// Normalize raw name bytes: lossy UTF-8 then lowercase (ASCII fast path,
/// Unicode-aware otherwise).
fn normalize(raw: Vec<u8>) -> Vec<u8> {
    if raw.iter().all(|b| !b.is_ascii_uppercase()) {
        return raw;
    }
    match String::from_utf8(raw) {
        Ok(s) => s.to_lowercase().into_bytes(),
        Err(e) => String::from_utf8_lossy(e.as_bytes())
            .to_lowercase()
            .into_bytes(),
    }
}

/// Lowercase a query with the same rules as the index.
pub fn normalize_query(q: &str) -> Vec<u8> {
    q.to_lowercase().into_bytes()
}

/// Plain substring scan with a first-byte skip loop. Short needles are
/// the common case; no extra dependency.
fn memmem(haystack: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if haystack.len() < n {
        return false;
    }
    let first = needle[0];
    let last = haystack.len() - n;
    let mut start = 0usize;
    while start <= last {
        let Some(rel) = haystack[start..].iter().position(|&b| b == first) else {
            return false;
        };
        let i = start + rel;
        if i > last {
            return false;
        }
        if &haystack[i..i + n] == needle {
            return true;
        }
        start = i + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, NodeKind};

    fn model_with_names(names: &[&str]) -> ScanModel {
        let mut m = ScanModel::new(std::path::PathBuf::from("/t"), 0);
        m.nodes_mut().reserve(names.len() + 1);
        m.nodes_mut().push(Node {
            parent: None,
            name: "/t".into(),
            kind: NodeKind::Directory,
            own_logical: 0,
            own_allocated: 0,
            agg_logical: 0,
            agg_allocated: 0,
            file_count: 0,
            dir_count: 0,
            modified_ms: None,
            device: 0,
            inode: 0,
            children: Vec::new(),
            flags: 0,
        });
        let root = NodeId(0);
        for (ix, n) in names.iter().enumerate() {
            let id = NodeId(ix as u32 + 1);
            m.nodes_mut().push(Node {
                parent: Some(root),
                name: (*n).into(),
                kind: NodeKind::File,
                own_logical: ix as u64,
                own_allocated: ix as u64,
                agg_logical: ix as u64,
                agg_allocated: ix as u64,
                file_count: 1,
                dir_count: 0,
                modified_ms: None,
                device: 0,
                inode: id.0 as u64,
                children: Vec::new(),
                flags: 0,
            });
            m.nodes_mut()[root.index()].children.push(id);
        }
        m.aggregate();
        m
    }

    #[test]
    fn finds_substrings_case_insensitively() {
        let m = model_with_names(&["Report.PDF", "notes.txt", "vacation.jpg"]);
        let idx = SearchIndex::build(&m);
        assert_eq!(
            idx.find(&normalize_query("pdf")),
            vec![NodeId(1)],
            "case-insensitive match"
        );
        assert_eq!(idx.find(&normalize_query("vacation")).len(), 1);
        assert_eq!(idx.find(&normalize_query("e")).len(), 2); // report, notes
        assert!(idx.find(&normalize_query("zzz")).is_empty());
        assert!(idx.find(&normalize_query("")).is_empty());
    }

    #[test]
    fn refinement_is_consistent_with_full_search() {
        let names: Vec<String> = (0..10_000).map(|i| format!("file{i}.bin")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let m = model_with_names(&refs);
        let idx = SearchIndex::build(&m);

        let all = idx.find(&normalize_query("file"));
        let refined = idx.refine(&all, &normalize_query("file42"));
        let direct = idx.find(&normalize_query("file42"));
        assert_eq!(refined, direct);
        // 42 itself, x420..x429, and 4200..4299.
        assert_eq!(refined.len(), 111);
    }

    #[test]
    fn unicode_names_normalize() {
        let m = model_with_names(&["ÄPFEL.txt", "straße.doc"]);
        let idx = SearchIndex::build(&m);
        assert_eq!(idx.find(&normalize_query("äpfel")).len(), 1);
        assert_eq!(idx.find(&normalize_query("stra")).len(), 1);
        // Documented limitation: plain lowercase keeps 'ß', so ASCII
        // "strasse" does not match it.
        assert!(idx.find(&normalize_query("strasse")).is_empty());
    }

    #[test]
    fn memmem_basics() {
        assert!(memmem(b"hello world", b"o w"));
        assert!(!memmem(b"short", b"longer needle"));
        assert!(!memmem(b"abc", b"abcd"));
        assert!(memmem(b"abc", b""));
        assert!(!memmem(b"", b"a"));
    }

    #[test]
    fn query_normalization_and_zero_matches_behavior() {
        let m = model_with_names(&["budget.xlsx", "notes.md", "README.txt"]);
        let idx = SearchIndex::build(&m);

        // Empty and whitespace queries
        assert!(normalize_query("").is_empty());
        assert_eq!(idx.find(&normalize_query("")), Vec::<NodeId>::new());

        // Non-matching query returns 0 matches
        let no_match = normalize_query("nonexistent");
        assert!(!no_match.is_empty());
        let results = idx.find(&no_match);
        assert!(results.is_empty(), "query is active but matches 0 items");

        // Refine with zero matching subset
        let initial = idx.find(&normalize_query("note"));
        assert_eq!(initial.len(), 1);
        let refined_empty = idx.refine(&initial, &normalize_query("notexyz"));
        assert!(refined_empty.is_empty());
    }
}
