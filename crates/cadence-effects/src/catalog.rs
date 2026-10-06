//! Bounded indexing of CLAP installations, following Resonara's catalog.
//! A macOS bundle is one library; do not descend into it.
use std::{collections::BTreeSet, path::PathBuf};

#[cfg(any(test, target_os = "linux", target_os = "scarlet"))]
use std::path::Path;

const MAX_ENTRIES: usize = 16_384;
const MAX_LIBRARIES: usize = 1024;

pub(crate) fn roots() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(extra) = std::env::var_os("CLAP_PATH") {
        paths.extend(std::env::split_paths(&extra).filter(|p| p.is_absolute()));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        paths.push(parent.join("plugins"));
    }
    #[cfg(target_os = "macos")]
    {
        paths.push("/Library/Audio/Plug-Ins/CLAP".into());
        if let Some(home) = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            paths.push(home.join("Library/Audio/Plug-Ins/CLAP"));
        }
    }
    #[cfg(any(target_os = "linux", target_os = "scarlet"))]
    {
        paths.extend(unix_roots(
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        ));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(common) = std::env::var_os("COMMONPROGRAMFILES") {
            paths.push(PathBuf::from(common).join("CLAP"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            paths.push(PathBuf::from(local).join("Programs/Common/CLAP"));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(any(test, target_os = "linux", target_os = "scarlet"))]
fn unix_roots(home: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = vec!["/usr/lib/clap".into(), "/usr/local/lib/clap".into()];
    if let Some(home) = home.filter(|p| p.is_absolute()) {
        paths.push(home.join(".clap"));
        paths.push(home.join(".local/lib/clap"));
    }
    paths
}

pub(crate) fn index(roots: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<String>) {
    let mut pending: Vec<_> = roots.into_iter().map(|path| (path, 0)).collect();
    let mut visited = BTreeSet::new();
    let mut libraries = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut entries = 0;
    while let Some((directory, depth)) = pending.pop() {
        let Ok(canonical) = directory.canonicalize() else {
            continue;
        };
        if !visited.insert(canonical) {
            continue;
        }
        let children = match std::fs::read_dir(&directory) {
            Ok(children) => children,
            Err(error) => {
                warnings.push(format!("{}: {error}", directory.display()));
                continue;
            }
        };
        for child in children {
            entries += 1;
            if entries > MAX_ENTRIES || libraries.len() >= MAX_LIBRARIES {
                warnings.push("CLAP search limit reached; narrow CLAP_PATH".into());
                return (libraries.into_iter().collect(), warnings);
            }
            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    warnings.push(error.to_string());
                    continue;
                }
            };
            let path = child.path();
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("clap"))
            {
                if let Ok(canonical) = path.canonicalize() {
                    libraries.insert(canonical);
                }
            } else if path.is_dir() {
                if depth < 32 {
                    pending.push((path, depth + 1));
                } else {
                    warnings.push(format!("CLAP search depth exceeded: {}", path.display()));
                }
            }
        }
    }
    (libraries.into_iter().collect(), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unix_locations_match_resonara_without_relative_home() {
        assert_eq!(
            unix_roots(Some(Path::new("/home/music"))),
            vec![
                PathBuf::from("/usr/lib/clap"),
                "/usr/local/lib/clap".into(),
                "/home/music/.clap".into(),
                "/home/music/.local/lib/clap".into()
            ]
        );
        assert_eq!(unix_roots(Some(Path::new("relative"))), unix_roots(None));
    }
    #[test]
    fn nested_libraries_and_bundles_are_deduplicated_without_entering_bundles() {
        let root = std::env::temp_dir().join(format!("cadence-clap-index-{}", std::process::id()));
        std::fs::create_dir_all(root.join("vendor/Bundle.clap/Contents")).unwrap();
        std::fs::write(
            root.join("vendor/Bundle.clap/Contents/hidden.clap"),
            b"hidden",
        )
        .unwrap();
        std::fs::write(root.join("effect.clap"), b"effect").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, root.join("vendor/loop")).unwrap();
        let (libraries, warnings) =
            index(vec![root.clone(), root.join("vendor"), root.join("absent")]);
        assert!(warnings.is_empty());
        assert_eq!(libraries.len(), 2);
        assert!(libraries.iter().any(|p| p.ends_with("Bundle.clap")));
        assert!(libraries.iter().any(|p| p.ends_with("effect.clap")));
        std::fs::remove_dir_all(root).unwrap();
    }
}
