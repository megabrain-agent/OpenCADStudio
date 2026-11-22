// The three things the font index needs from the outside world.

use super::domain::{FileStamp, Index, Parsed};

/// Lists the installed font files, in the order a full fontdb scan visits
/// them. Listing reads directory entries only and opens no font file.
pub trait FontFiles {
    fn list(&self) -> Vec<FileStamp>;
}

/// Parses the faces of one font file.
pub trait FaceReader {
    fn read(&self, stamp: &FileStamp) -> Parsed;
}

/// Keeps the index between runs. A missing or unreadable store is an empty
/// cache, never an error: the index then rebuilds from the fonts.
pub trait IndexStore {
    fn load(&self) -> Option<Index>;
    fn save(&self, index: &Index);
}
