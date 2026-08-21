pub mod finder;
pub mod hashing;

pub use finder::{find_duplicates, DuplicateFile, DuplicateGroup, FinderProgress, MIN_SIZE};
