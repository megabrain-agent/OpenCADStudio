// One system font database for the whole app, built from a cached index.
//
// A full `fontdb` scan opens every installed font file to read its names. On
// Windows that is close to a thousand files, and after a pause the virus
// scanner checks each one again before it opens: the first start of the day
// spent about eight seconds there before any window appeared. The index keeps
// the parsed face list in the settings folder, keyed by each file's path, size
// and modification time. A start then only lists the font directories and
// parses the files that are new or changed; font data itself is still opened
// lazily, when a face is first used.
//
// Every system font consumer starts from `system_database`: iced's UI text
// and SVG text (handed over in `run`), the drawing text engine, the TrueType
// glyph shaper and the view cube labels. A consumer that scanned on its own
// would bring the cold cost back at its first use.

// Only Windows reads the index; elsewhere it is built just by the tests.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

mod adapters;
mod domain;
mod ports;

use std::sync::OnceLock;

use domain::Index;
use ports::{FaceReader, FontFiles, IndexStore};

/// Index layout and the parser that produced its faces. Changing either
/// (including a fontdb upgrade) must change this, which discards old caches.
const FORMAT: &str = "ocs-font-index/1 fontdb-0.23";

/// The installed system fonts: the faces `fontdb::Database::load_system_fonts`
/// would add, in the same order. Built once per process; each call returns a
/// copy the caller may extend.
pub fn system_database() -> fontdb::Database {
    static SYSTEM: OnceLock<fontdb::Database> = OnceLock::new();
    SYSTEM.get_or_init(scan_system).clone()
}

/// The system database with the generic families cosmic-text's
/// `FontSystem::new` sets, for consumers that used to call it.
pub fn cosmic_database() -> fontdb::Database {
    let mut database = system_database();
    database.set_monospace_family("Noto Sans Mono");
    database.set_sans_serif_family("Open Sans");
    database.set_serif_family("DejaVu Serif");
    database
}

/// The locale cosmic-text's `FontSystem::new` would pick.
pub fn locale() -> String {
    sys_locale::get_locale().unwrap_or_else(|| String::from("en-US"))
}

#[cfg(target_os = "windows")]
fn scan_system() -> fontdb::Database {
    let Some(dir) = crate::config::config_dir() else {
        return full_scan();
    };
    let store = adapters::JsonStore::new(dir.join("font-index.json"));
    database(&refresh(
        &adapters::FontDirectories::windows_system(),
        &adapters::FontdbReader,
        &store,
    ))
}

// Elsewhere the system scan stays fontdb's own (fontconfig on Linux); the
// shared database still saves the repeated scans.
#[cfg(not(target_os = "windows"))]
fn scan_system() -> fontdb::Database {
    full_scan()
}

fn full_scan() -> fontdb::Database {
    let mut database = fontdb::Database::new();
    database.load_system_fonts();
    database
}

/// Bring the stored index up to date with the listed files and store it
/// again if anything changed.
fn refresh(files: &impl FontFiles, reader: &impl FaceReader, store: &impl IndexStore) -> Index {
    let listing = files.list();
    let cached = store.load().unwrap_or_default();
    let index = domain::assemble(&domain::plan(&listing, &cached, FORMAT), FORMAT, |stamp| {
        reader.read(stamp)
    });
    if index != cached {
        store.save(&index);
    }
    index
}

fn database(index: &Index) -> fontdb::Database {
    let mut database = fontdb::Database::new();
    for file in &index.files {
        for face in &file.faces {
            database.push_face_info(adapters::face_info(&file.stamp.path, face));
        }
    }
    database
}

#[cfg(test)]
mod tests {
    use super::adapters::{FontDirectories, FontdbReader, JsonStore};
    use super::domain::{FileStamp, Parsed};
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct CountingReader(Cell<usize>);

    impl FaceReader for CountingReader {
        fn read(&self, stamp: &FileStamp) -> Parsed {
            self.0.set(self.0.get() + 1);
            FontdbReader.read(stamp)
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ocs-font-index-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_fonts(dir: &Path) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("web/fonts");
        fs::create_dir_all(dir.join("nested")).unwrap();
        for (name, target) in [
            ("latin.ttf", "latin.ttf"),
            ("greek.ttf", "GREEK.TTF"),
            ("hebrew.ttf", "nested/hebrew.ttf"),
            ("OFL.txt", "OFL.txt"),
        ] {
            fs::copy(source.join(name), dir.join(target)).unwrap();
        }
    }

    type FaceKey = (
        PathBuf,
        u32,
        Vec<String>,
        String,
        fontdb::Style,
        u16,
        u16,
        bool,
    );

    fn faces(database: &fontdb::Database) -> Vec<FaceKey> {
        database
            .faces()
            .map(|face| {
                let fontdb::Source::File(path) = &face.source else {
                    panic!("system faces come from files");
                };
                (
                    path.clone(),
                    face.index,
                    face.families.iter().map(|(name, _)| name.clone()).collect(),
                    face.post_script_name.clone(),
                    face.style,
                    face.weight.0,
                    face.stretch.to_number(),
                    face.monospaced,
                )
            })
            .collect()
    }

    #[test]
    fn index_matches_a_full_scan_and_a_warm_start_opens_no_font() {
        let fonts = scratch("fonts");
        sample_fonts(&fonts);
        let store = JsonStore::new(scratch("store").join("font-index.json"));
        let files = FontDirectories::new(vec![fonts.clone()]);

        let mut full = fontdb::Database::new();
        full.load_fonts_dir(&fonts);
        assert_eq!(full.len(), 3);

        let cold = CountingReader(Cell::new(0));
        let first = database(&refresh(&files, &cold, &store));
        assert_eq!(cold.0.get(), 3);
        assert_eq!(faces(&first), faces(&full));

        let warm = CountingReader(Cell::new(0));
        let second = database(&refresh(&files, &warm, &store));
        assert_eq!(warm.0.get(), 0);
        assert_eq!(faces(&second), faces(&full));

        // Replacing one file reparses that file only.
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("web/fonts/thai.ttf"),
            fonts.join("latin.ttf"),
        )
        .unwrap();
        let changed = CountingReader(Cell::new(0));
        let third = database(&refresh(&files, &changed, &store));
        assert_eq!(changed.0.get(), 1);
        let mut rescanned = fontdb::Database::new();
        rescanned.load_fonts_dir(&fonts);
        assert_eq!(faces(&third), faces(&rescanned));
    }

    #[test]
    fn a_damaged_store_rebuilds_from_the_fonts() {
        let fonts = scratch("damaged-fonts");
        sample_fonts(&fonts);
        let path = scratch("damaged-store").join("font-index.json");
        fs::write(&path, b"{ not json").unwrap();
        let store = JsonStore::new(path.clone());
        let index = refresh(&FontDirectories::new(vec![fonts]), &FontdbReader, &store);
        assert_eq!(index.files.len(), 3);
        assert_eq!(store.load(), Some(index));
    }
}
