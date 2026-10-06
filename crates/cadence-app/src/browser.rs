use crate::state;
use scarlet_ui::State;
use std::{cell::RefCell, rc::Rc};

/// State and pending directory job belong to the browser pane.
#[derive(Clone)]
pub struct BrowserState {
    pub visible: State<bool>,
    pub directory: State<String>,
    pub entries: State<Vec<Entry>>,
    pub selection: State<Option<usize>>,
    pub status: State<String>,
    pub listing: Rc<RefCell<Option<std::sync::mpsc::Receiver<Listing>>>>,
}
impl BrowserState {
    pub fn new() -> Self {
        Self {
            visible: state(false),
            directory: state(initial_directory().to_string_lossy().into()),
            entries: state(vec![]),
            selection: state(None),
            status: state(String::new()),
            listing: Rc::new(RefCell::new(None)),
        }
    }
}

// Portable, non-recursive browser used when the native picker is unavailable.
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

pub use cadence_core::EXTENSIONS;
#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub directory: bool,
}
pub type Listing = Result<(PathBuf, Vec<Entry>), String>;
pub fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}
pub fn initial_directory() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .map(|p| {
            if p.join("Music").is_dir() {
                p.join("Music")
            } else {
                p
            }
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
}
pub fn read(path: PathBuf) -> mpsc::Receiver<Listing> {
    let (tx, rx) = mpsc::channel();
    let sender = tx.clone();
    if let Err(error) = thread::Builder::new()
        .name("cadence-browser".into())
        .spawn(move || {
            let result = (|| {
                let path = path.canonicalize().map_err(|e| e.to_string())?;
                let directory = std::fs::read_dir(&path).map_err(|e| e.to_string())?;
                let mut entries = Vec::new();
                for entry in directory {
                    // A broken link or a file disappearing during enumeration
                    // must not hide the other music in this folder.
                    let Ok(entry) = entry else { continue };
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let entry_path = entry.path();
                    let Ok(metadata) = entry_path.metadata() else {
                        continue;
                    };
                    if metadata.is_dir() || (metadata.is_file() && supported(&entry_path)) {
                        entries.push(Entry {
                            name,
                            path: entry_path,
                            directory: metadata.is_dir(),
                        });
                    }
                    if entries.len() > 20_000 {
                        return Err(
                            "This folder has too many entries; open a smaller subfolder.".into(),
                        );
                    }
                }
                entries.sort_by(|a, b| {
                    b.directory
                        .cmp(&a.directory)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                Ok((path, entries))
            })();
            let _ = sender.send(result);
        })
    {
        let _ = tx.send(Err(error.to_string()));
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_lists_music_and_directories_despite_a_broken_link() {
        let root = std::env::temp_dir().join(format!("cadence-browser-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Album")).unwrap();
        std::fs::write(root.join("日本語.FLAC"), b"fixture").unwrap();
        std::fs::write(root.join("notes.txt"), b"not audio").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("missing"), root.join("broken.wav")).unwrap();
        let (_, entries) = read(root.clone())
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Album");
        assert!(entries[0].directory);
        assert_eq!(entries[1].name, "日本語.FLAC");
        assert!(!entries[1].directory);
        std::fs::remove_dir_all(root).unwrap();
    }
}
