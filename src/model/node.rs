/// Arena node types for the filesystem tree.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

pub const HARDLINK_SHARED: u16 = 1 << 0;
pub const MOUNT_BOUNDARY: u16 = 1 << 1;
pub const TOMBSTONED: u16 = 1 << 2;
pub const UNREADABLE: u16 = 1 << 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub parent: Option<NodeId>,
    pub name: std::ffi::OsString,
    pub kind: NodeKind,

    /// Bytes attributed to this node alone (directories own their entry blocks).
    pub own_logical: u64,
    pub own_allocated: u64,

    /// Own + all descendants.
    pub agg_logical: u64,
    pub agg_allocated: u64,

    pub file_count: u64,
    pub dir_count: u64,

    /// Modification time in unix milliseconds.
    pub modified_ms: Option<i64>,

    pub device: u64,
    pub inode: u64,

    pub children: Vec<NodeId>,
    pub flags: u16,
}

impl Node {
    pub fn kind(&self) -> NodeKind {
        self.kind
    }

    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Directory
    }

    pub fn has_flag(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }
}
