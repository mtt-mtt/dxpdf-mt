//! Filesystem helpers for paths that cross the legacy Windows `MAX_PATH` limit.
//!
//! The byte conversion API is path-independent. The CLI and Python
//! `convert_file` entry points pass through here so they share one narrowly
//! scoped Windows boundary without changing path semantics on other hosts.

use std::io;
use std::path::{Path, PathBuf};

/// Read a complete file, using an extended-length path on Windows when needed.
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    std::fs::read(io_path(path.as_ref())?)
}

/// Write a complete file, using an extended-length path on Windows when needed.
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    std::fs::write(io_path(path.as_ref())?, contents)
}

#[cfg(not(windows))]
fn io_path(path: &Path) -> io::Result<PathBuf> {
    Ok(path.to_path_buf())
}

#[cfg(windows)]
fn io_path(path: &Path) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Prefix};

    let existing_prefix = path
        .components()
        .next()
        .and_then(|component| match component {
            Component::Prefix(prefix) => Some(prefix.kind()),
            _ => None,
        });
    if matches!(
        existing_prefix,
        Some(
            Prefix::Verbatim(_)
                | Prefix::VerbatimUNC(_, _)
                | Prefix::VerbatimDisk(_)
                | Prefix::DeviceNS(_)
        )
    ) {
        return Ok(path.to_path_buf());
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let absolute = lexical_normalize(&absolute);
    let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();

    // Leave ordinary paths untouched. 248 is the traditional maximum
    // directory-path boundary and gives the final filename room below 260.
    if wide.len() < 248 {
        return Ok(path.to_path_buf());
    }

    let is_unc = matches!(
        absolute.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::UNC(_, _))
    );
    let mut extended: Vec<u16> = if is_unc {
        r"\\?\UNC\".encode_utf16().collect()
    } else {
        r"\\?\".encode_utf16().collect()
    };
    if is_unc {
        // A normal UNC path starts with two separators. The extended UNC form
        // replaces those with `\\?\UNC\`.
        extended.extend_from_slice(wide.get(2..).unwrap_or_default());
    } else {
        extended.extend_from_slice(&wide);
    }
    Ok(PathBuf::from(OsString::from_wide(&extended)))
}

#[cfg(windows)]
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn long_tail() -> String {
        ["a".repeat(90), "目录".repeat(45), "b".repeat(90)].join("\\")
    }

    #[test]
    fn long_drive_path_gets_verbatim_prefix() {
        let path = PathBuf::from(format!(r"C:\{}\file.docx", long_tail()));
        let converted = io_path(&path).unwrap();
        assert!(converted
            .as_os_str()
            .to_string_lossy()
            .starts_with(r"\\?\C:\"));
    }

    #[test]
    fn long_unc_path_gets_extended_unc_prefix() {
        let path = PathBuf::from(format!(r"\\server\share\{}\file.docx", long_tail()));
        let converted = io_path(&path).unwrap();
        assert!(converted
            .as_os_str()
            .to_string_lossy()
            .starts_with(r"\\?\UNC\server\share\"));
    }

    #[test]
    fn existing_verbatim_path_is_preserved() {
        let path = PathBuf::from(r"\\?\C:\already\verbatim.docx");
        assert_eq!(io_path(&path).unwrap(), path);
    }
}
