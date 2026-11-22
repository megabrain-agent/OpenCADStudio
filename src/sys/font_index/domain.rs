// What the font index knows and decides, without touching the disk.
//
// A font file is identified by its path, size and modification time. When all
// three match the cached entry, the faces recorded for it are reused and the
// file is not opened. Everything else (new, changed or unreadable files) is
// parsed again. Removed files simply drop out, because the plan follows the
// current listing.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Identity of a font file as the directory listing reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub path: PathBuf,
    pub len: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub modified_ns: u64,
}

/// One face as fontdb describes it, minus the database ID.
///
/// Family languages are reduced to "English (US) or not": fontdb only uses
/// the language to put the English name first, and that order is kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaceRecord {
    pub families: Vec<(String, bool)>,
    pub post_script_name: String,
    pub index: u32,
    /// 0 normal, 1 italic, 2 oblique.
    pub style: u8,
    pub weight: u16,
    /// 1 (ultra-condensed) to 9 (ultra-expanded).
    pub stretch: u16,
    pub monospaced: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedFile {
    pub stamp: FileStamp,
    pub faces: Vec<FaceRecord>,
}

/// The stored index. `format` names the index layout and the parser that
/// produced the faces; a different value discards the whole cache.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub format: String,
    pub files: Vec<IndexedFile>,
}

/// What to do with one listed file.
#[derive(Debug, PartialEq, Eq)]
pub enum Step<'a> {
    Reuse(&'a IndexedFile),
    Parse(&'a FileStamp),
}

/// One step per listed file, in listing order. The order matters: face IDs
/// follow it, and font matching breaks ties by ID, so the index must produce
/// the same order as a full scan.
pub fn plan<'a>(listing: &'a [FileStamp], cached: &'a Index, format: &str) -> Vec<Step<'a>> {
    let known: HashMap<&PathBuf, &IndexedFile> = if cached.format == format {
        cached
            .files
            .iter()
            .map(|file| (&file.stamp.path, file))
            .collect()
    } else {
        HashMap::new()
    };
    listing
        .iter()
        .map(|stamp| match known.get(&stamp.path) {
            Some(file) if file.stamp == *stamp => Step::Reuse(file),
            _ => Step::Parse(stamp),
        })
        .collect()
}

/// Outcome of parsing one file. `Unreadable` files are left out of the index
/// so the next start tries them again; files that open but hold no usable
/// face are recorded with no faces and are not reopened.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Faces(Vec<FaceRecord>),
    Unreadable,
}

/// Build the new index from a plan, parsing only where the plan says so.
pub fn assemble(
    steps: &[Step<'_>],
    format: &str,
    mut parse: impl FnMut(&FileStamp) -> Parsed,
) -> Index {
    let files = steps
        .iter()
        .filter_map(|step| match step {
            Step::Reuse(file) => Some((*file).clone()),
            Step::Parse(stamp) => match parse(stamp) {
                Parsed::Faces(faces) => Some(IndexedFile {
                    stamp: (*stamp).clone(),
                    faces,
                }),
                Parsed::Unreadable => None,
            },
        })
        .collect();
    Index {
        format: format.to_owned(),
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMAT: &str = "test/1";

    fn stamp(path: &str, len: u64, modified_ns: u64) -> FileStamp {
        FileStamp {
            path: PathBuf::from(path),
            len,
            modified_ns,
        }
    }

    fn face(name: &str) -> FaceRecord {
        FaceRecord {
            families: vec![(name.to_owned(), true)],
            post_script_name: name.replace(' ', ""),
            index: 0,
            style: 0,
            weight: 400,
            stretch: 5,
            monospaced: false,
        }
    }

    fn indexed(stamp: FileStamp, name: &str) -> IndexedFile {
        IndexedFile {
            stamp,
            faces: vec![face(name)],
        }
    }

    #[test]
    fn unchanged_files_are_reused_and_others_parsed() {
        let cached = Index {
            format: FORMAT.to_owned(),
            files: vec![
                indexed(stamp("a.ttf", 10, 1), "A"),
                indexed(stamp("b.ttf", 20, 2), "B"),
            ],
        };
        let listing = [
            stamp("a.ttf", 10, 1),
            stamp("b.ttf", 20, 3),
            stamp("c.ttf", 30, 4),
        ];
        let steps = plan(&listing, &cached, FORMAT);
        assert_eq!(
            steps,
            vec![
                Step::Reuse(&cached.files[0]),
                Step::Parse(&listing[1]),
                Step::Parse(&listing[2]),
            ]
        );
    }

    #[test]
    fn a_size_change_alone_forces_a_parse() {
        let cached = Index {
            format: FORMAT.to_owned(),
            files: vec![indexed(stamp("a.ttf", 10, 1), "A")],
        };
        let listing = [stamp("a.ttf", 11, 1)];
        assert_eq!(
            plan(&listing, &cached, FORMAT),
            vec![Step::Parse(&listing[0])]
        );
    }

    #[test]
    fn another_format_discards_the_cache() {
        let cached = Index {
            format: "old/0".to_owned(),
            files: vec![indexed(stamp("a.ttf", 10, 1), "A")],
        };
        let listing = [stamp("a.ttf", 10, 1)];
        assert_eq!(
            plan(&listing, &cached, FORMAT),
            vec![Step::Parse(&listing[0])]
        );
    }

    #[test]
    fn assembly_keeps_listing_order_and_drops_removed_and_unreadable_files() {
        let cached = Index {
            format: FORMAT.to_owned(),
            files: vec![
                indexed(stamp("gone.ttf", 1, 1), "Gone"),
                indexed(stamp("b.ttf", 20, 2), "B"),
            ],
        };
        let listing = [
            stamp("c.ttf", 30, 3),
            stamp("b.ttf", 20, 2),
            stamp("locked.ttf", 40, 4),
            stamp("empty.ttf", 50, 5),
        ];
        let steps = plan(&listing, &cached, FORMAT);
        let mut parsed = Vec::new();
        let index = assemble(&steps, FORMAT, |stamp| {
            parsed.push(stamp.path.clone());
            match stamp.path.to_str() {
                Some("c.ttf") => Parsed::Faces(vec![face("C")]),
                Some("empty.ttf") => Parsed::Faces(Vec::new()),
                _ => Parsed::Unreadable,
            }
        });
        assert_eq!(
            parsed,
            ["c.ttf", "locked.ttf", "empty.ttf"].map(PathBuf::from)
        );
        let paths: Vec<_> = index.files.iter().map(|f| f.stamp.path.clone()).collect();
        assert_eq!(paths, ["c.ttf", "b.ttf", "empty.ttf"].map(PathBuf::from));
        assert_eq!(index.files[1], cached.files[1]);
        assert!(index.files[2].faces.is_empty());
        assert_eq!(index.format, FORMAT);
    }

    #[test]
    fn a_warm_index_rebuilds_to_itself() {
        let cached = Index {
            format: FORMAT.to_owned(),
            files: vec![indexed(stamp("a.ttf", 10, 1), "A")],
        };
        let listing = [stamp("a.ttf", 10, 1)];
        let index = assemble(&plan(&listing, &cached, FORMAT), FORMAT, |_| {
            panic!("a warm index must not parse")
        });
        assert_eq!(index, cached);
    }
}
