// Disk-facing halves of the font index: directory listing, fontdb parsing,
// the JSON store, and the conversion back into fontdb faces.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::domain::{FaceRecord, FileStamp, Index, Parsed};
use super::ports::{FaceReader, FontFiles, IndexStore};

/// Font directories walked the way `fontdb::Database::load_fonts_dir` walks
/// them: recursively, in `read_dir` order, with the same extensions and the
/// same symlink handling. Sizes and times come from the directory entries,
/// which on Windows needs no open of the file itself.
pub struct FontDirectories {
    roots: Vec<PathBuf>,
}

impl FontDirectories {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// The directories `fontdb::Database::load_system_fonts` scans on Windows.
    #[cfg(target_os = "windows")]
    pub fn windows_system() -> Self {
        let mut roots = vec![match std::env::var_os("SYSTEMROOT") {
            Some(root) => Path::new(&root).join("Fonts"),
            None => PathBuf::from("C:\\Windows\\Fonts\\"),
        }];
        if let Ok(home) = std::env::var("USERPROFILE") {
            let home = Path::new(&home);
            roots.push(home.join("AppData\\Local\\Microsoft\\Windows\\Fonts"));
            roots.push(home.join("AppData\\Roaming\\Microsoft\\Windows\\Fonts"));
        }
        Self::new(roots)
    }
}

impl FontFiles for FontDirectories {
    fn list(&self) -> Vec<FileStamp> {
        let mut listing = Vec::new();
        let mut seen = HashSet::new();
        for root in &self.roots {
            walk(root, &mut seen, &mut listing);
        }
        listing
    }
}

fn walk(dir: &Path, seen: &mut HashSet<PathBuf>, listing: &mut Vec<FileStamp>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some((path, metadata)) = resolve(entry, seen, listing) else {
            continue;
        };
        if metadata.is_file() {
            if is_font_file(&path) {
                listing.push(file_stamp(path, &metadata));
            }
        } else if metadata.is_dir() {
            walk(&path, seen, listing);
        }
    }
}

/// fontdb's `canonicalize`: plain entries pass as they are (deduplicated only
/// once a symlink has started the `seen` set); symlinks resolve to their
/// canonical target, once each.
fn resolve(
    entry: fs::DirEntry,
    seen: &mut HashSet<PathBuf>,
    listing: &[FileStamp],
) -> Option<(PathBuf, fs::Metadata)> {
    let path = entry.path();
    let file_type = entry.file_type().ok()?;
    if !file_type.is_symlink() {
        if !seen.is_empty() {
            if seen.contains(&path) {
                return None;
            }
            seen.insert(path.clone());
        }
        return Some((path, entry.metadata().ok()?));
    }
    if seen.is_empty() && file_type.is_dir() {
        seen.extend(listing.iter().map(|stamp| stamp.path.clone()));
    }
    let metadata = fs::metadata(&path).ok()?;
    if metadata.is_symlink() {
        return None;
    }
    let canonical = fs::canonicalize(path).ok()?;
    if !seen.insert(canonical.clone()) {
        return None;
    }
    Some((canonical, metadata))
}

fn is_font_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ttf" | "ttc" | "TTF" | "TTC" | "otf" | "otc" | "OTF" | "OTC")
    )
}

fn file_stamp(path: PathBuf, metadata: &fs::Metadata) -> FileStamp {
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        });
    FileStamp {
        path,
        len: metadata.len(),
        modified_ns,
    }
}

/// Parses a file with fontdb itself, so indexed faces are exactly the faces a
/// full scan would add.
pub struct FontdbReader;

impl FaceReader for FontdbReader {
    fn read(&self, stamp: &FileStamp) -> Parsed {
        let mut database = fontdb::Database::new();
        match database.load_font_file(&stamp.path) {
            Ok(()) => Parsed::Faces(database.faces().map(face_record).collect()),
            Err(_) => Parsed::Unreadable,
        }
    }
}

fn face_record(face: &fontdb::FaceInfo) -> FaceRecord {
    FaceRecord {
        families: face
            .families
            .iter()
            .map(|(name, language)| {
                (
                    name.clone(),
                    *language == fontdb::Language::English_UnitedStates,
                )
            })
            .collect(),
        post_script_name: face.post_script_name.clone(),
        index: face.index,
        style: match face.style {
            fontdb::Style::Normal => 0,
            fontdb::Style::Italic => 1,
            fontdb::Style::Oblique => 2,
        },
        weight: face.weight.0,
        stretch: face.stretch.to_number(),
        monospaced: face.monospaced,
    }
}

/// The fontdb face for a stored record of the file at `path`.
pub fn face_info(path: &Path, record: &FaceRecord) -> fontdb::FaceInfo {
    fontdb::FaceInfo {
        id: fontdb::ID::dummy(),
        source: fontdb::Source::File(path.to_path_buf()),
        index: record.index,
        families: record
            .families
            .iter()
            .map(|(name, en_us)| {
                let language = if *en_us {
                    fontdb::Language::English_UnitedStates
                } else {
                    fontdb::Language::Unknown
                };
                (name.clone(), language)
            })
            .collect(),
        post_script_name: record.post_script_name.clone(),
        style: match record.style {
            1 => fontdb::Style::Italic,
            2 => fontdb::Style::Oblique,
            _ => fontdb::Style::Normal,
        },
        weight: fontdb::Weight(record.weight),
        stretch: stretch(record.stretch),
        monospaced: record.monospaced,
    }
}

fn stretch(number: u16) -> fontdb::Stretch {
    use fontdb::Stretch::*;
    match number {
        1 => UltraCondensed,
        2 => ExtraCondensed,
        3 => Condensed,
        4 => SemiCondensed,
        6 => SemiExpanded,
        7 => Expanded,
        8 => ExtraExpanded,
        9 => UltraExpanded,
        _ => Normal,
    }
}

/// The index as JSON in the OCS settings folder. Writes go to a sibling file
/// first and replace the index in one rename, so a crash mid-write leaves the
/// previous index intact.
pub struct JsonStore {
    path: PathBuf,
}

impl JsonStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl IndexStore for JsonStore {
    fn load(&self) -> Option<Index> {
        serde_json::from_slice(&fs::read(&self.path).ok()?).ok()
    }

    fn save(&self, index: &Index) {
        let Ok(bytes) = serde_json::to_vec(index) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let partial = self.path.with_extension("json.partial");
        if fs::write(&partial, bytes).is_ok() && fs::rename(&partial, &self.path).is_err() {
            let _ = fs::remove_file(&partial);
        }
    }
}
