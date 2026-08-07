//! ZIP extraction and OOXML package-path resolution.

use std::collections::HashMap;
use std::io::Read;

use crate::docx::error::{ParseError, ResourceLimitKind, Result};
use crate::docx::whitespace_workaround::substitute_whitespace_only_runs;

const MIB: u64 = 1024 * 1024;

/// Resource limits applied before and during OOXML ZIP extraction.
///
/// Defaults are deliberately generous for large business documents while
/// bounding the memory amplification possible from an untrusted upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageLimits {
    pub max_archive_bytes: u64,
    pub max_parts: usize,
    pub max_part_uncompressed_bytes: u64,
    pub max_total_uncompressed_bytes: u64,
    pub max_compression_ratio: u64,
    pub compression_ratio_min_uncompressed_bytes: u64,
}

impl Default for PackageLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 256 * MIB,
            max_parts: 4096,
            max_part_uncompressed_bytes: 256 * MIB,
            max_total_uncompressed_bytes: 512 * MIB,
            max_compression_ratio: 1000,
            compression_ratio_min_uncompressed_bytes: MIB,
        }
    }
}

/// The contents of a DOCX package, extracted from the ZIP archive.
pub struct PackageContents {
    /// All files in the ZIP, keyed by normalized path (no leading slash).
    pub parts: HashMap<String, Vec<u8>>,
}

impl PackageContents {
    /// Extract all parts from a DOCX ZIP archive.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Self::from_bytes_with_limits(data, &PackageLimits::default())
    }

    /// Extract all parts while enforcing caller-supplied resource limits.
    pub fn from_bytes_with_limits(data: &[u8], limits: &PackageLimits) -> Result<Self> {
        if data.len() as u64 > limits.max_archive_bytes {
            return Err(ParseError::ResourceLimit {
                kind: ResourceLimitKind::ArchiveBytes,
                part: None,
                actual: data.len() as u64,
                limit: limits.max_archive_bytes,
            });
        }
        let cursor = std::io::Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor)?;
        if archive.len() > limits.max_parts {
            return Err(ParseError::ResourceLimit {
                kind: ResourceLimitKind::PartCount,
                part: None,
                actual: archive.len() as u64,
                limit: limits.max_parts as u64,
            });
        }
        let mut parts = HashMap::with_capacity(archive.len());
        let mut total_uncompressed = 0_u64;

        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let name = normalize_path(file.name());
            let declared_size = file.size();
            if declared_size > limits.max_part_uncompressed_bytes {
                return Err(ParseError::ResourceLimit {
                    kind: ResourceLimitKind::PartBytes,
                    part: Some(name),
                    actual: declared_size,
                    limit: limits.max_part_uncompressed_bytes,
                });
            }
            let declared_total = total_uncompressed
                .checked_add(declared_size)
                .unwrap_or(u64::MAX);
            if declared_total > limits.max_total_uncompressed_bytes {
                return Err(ParseError::ResourceLimit {
                    kind: ResourceLimitKind::TotalUncompressedBytes,
                    part: Some(name),
                    actual: declared_total,
                    limit: limits.max_total_uncompressed_bytes,
                });
            }
            let compressed_size = file.compressed_size();
            if declared_size >= limits.compression_ratio_min_uncompressed_bytes
                && (compressed_size == 0
                    || declared_size > compressed_size.saturating_mul(limits.max_compression_ratio))
            {
                let actual_ratio = if compressed_size == 0 {
                    u64::MAX
                } else {
                    declared_size
                        .saturating_add(compressed_size - 1)
                        .saturating_div(compressed_size)
                };
                return Err(ParseError::ResourceLimit {
                    kind: ResourceLimitKind::CompressionRatio,
                    part: Some(name),
                    actual: actual_ratio,
                    limit: limits.max_compression_ratio,
                });
            }

            // Do not trust ZIP metadata as the only guard. Cap the decompressor
            // itself so a malformed local/central-header size mismatch cannot
            // allocate past the configured entry or cumulative limit.
            let remaining_total = limits
                .max_total_uncompressed_bytes
                .saturating_sub(total_uncompressed);
            let read_limit = limits.max_part_uncompressed_bytes.min(remaining_total);
            let initial_capacity = declared_size.min(MIB) as usize;
            let mut buf = Vec::with_capacity(initial_capacity);
            (&mut file)
                .take(read_limit.saturating_add(1))
                .read_to_end(&mut buf)?;
            if buf.len() as u64 > read_limit {
                let kind = if remaining_total <= limits.max_part_uncompressed_bytes {
                    ResourceLimitKind::TotalUncompressedBytes
                } else {
                    ResourceLimitKind::PartBytes
                };
                return Err(ParseError::ResourceLimit {
                    kind,
                    part: Some(name),
                    actual: total_uncompressed.saturating_add(buf.len() as u64),
                    limit: if matches!(kind, ResourceLimitKind::TotalUncompressedBytes) {
                        limits.max_total_uncompressed_bytes
                    } else {
                        limits.max_part_uncompressed_bytes
                    },
                });
            }
            total_uncompressed = total_uncompressed.saturating_add(buf.len() as u64);
            // Apply the whitespace workaround only to XML parts. Binary parts
            // (images, fonts, embedded OLE) must not be touched.
            // See `whitespace_workaround` module docs for the rationale.
            if name.ends_with(".xml") || name.ends_with(".rels") {
                buf = substitute_whitespace_only_runs(&buf);
            }
            parts.insert(name, buf);
        }

        Ok(Self { parts })
    }

    /// Get the bytes for a part, case-insensitively.
    pub fn get_part(&self, path: &str) -> Option<&[u8]> {
        let normalized = normalize_path(path);
        self.parts.get(&normalized).map(|v| v.as_slice())
    }

    /// Get part bytes, or return MissingPart error.
    pub fn require_part(&self, path: &str) -> Result<&[u8]> {
        self.get_part(path)
            .ok_or_else(|| ParseError::MissingPart(path.to_string()))
    }

    /// Remove and return the owned bytes for a part. Avoids cloning.
    pub fn take_part(&mut self, path: &str) -> Option<Vec<u8>> {
        let normalized = normalize_path(path);
        self.parts.remove(&normalized)
    }
}

fn normalize_path(path: &str) -> String {
    path.trim_start_matches('/').to_lowercase()
}

/// Resolve a relationship target to an absolute path within the package.
/// base_dir is the directory containing the source part (e.g., "word" for "word/document.xml").
pub fn resolve_target(base_dir: &str, target: &str) -> String {
    if target.starts_with('/') {
        // Absolute path within the package
        normalize_path(target)
    } else {
        // Relative to base_dir
        let mut path = if base_dir.is_empty() {
            target.to_string()
        } else {
            format!("{}/{}", base_dir, target)
        };
        // Drop a leading "./" and collapse interior "/./" no-op segments so a
        // target like "./media/x.png" resolves the same as "media/x.png".
        if let Some(stripped) = path.strip_prefix("./") {
            path = stripped.to_string();
        }
        while let Some(pos) = path.find("/./") {
            path = format!("{}{}", &path[..pos], &path[pos + 2..]);
        }
        // Simplify "../" sequences
        while let Some(pos) = path.find("/../") {
            if let Some(parent_start) = path[..pos].rfind('/') {
                path = format!("{}{}", &path[..parent_start], &path[pos + 3..]);
            } else {
                path = path[pos + 4..].to_string();
            }
        }
        normalize_path(&path)
    }
}

/// Get the .rels path for a given part path.
/// e.g., "word/document.xml" → "word/_rels/document.xml.rels"
pub fn rels_path_for(part_path: &str) -> String {
    let normalized = normalize_path(part_path);
    if let Some(slash_pos) = normalized.rfind('/') {
        format!(
            "{}/_rels/{}.rels",
            &normalized[..slash_pos],
            &normalized[slash_pos + 1..]
        )
    } else {
        format!("_rels/{}.rels", normalized)
    }
}

/// Get the directory portion of a part path.
pub fn part_directory(part_path: &str) -> &str {
    match part_path.rfind('/') {
        Some(pos) => &part_path[..pos],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn make_zip(parts: &[(&str, &[u8])], method: zip::CompressionMethod) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(method);
        for (name, bytes) in parts {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn tiny_limits() -> PackageLimits {
        PackageLimits {
            max_archive_bytes: 1024 * 1024,
            max_parts: 10,
            max_part_uncompressed_bytes: 1024,
            max_total_uncompressed_bytes: 2048,
            max_compression_ratio: 100,
            compression_ratio_min_uncompressed_bytes: 1024,
        }
    }

    #[test]
    fn package_limits_accept_a_small_archive() {
        let data = make_zip(
            &[("word/document.xml", b"<document/>")],
            zip::CompressionMethod::Stored,
        );
        let package = PackageContents::from_bytes_with_limits(&data, &tiny_limits()).unwrap();
        assert_eq!(
            package.get_part("word/document.xml"),
            Some(&b"<document/>"[..])
        );
    }

    #[test]
    fn package_limits_reject_archive_bytes_before_opening() {
        let data = make_zip(&[("a.bin", b"1234")], zip::CompressionMethod::Stored);
        let mut limits = tiny_limits();
        limits.max_archive_bytes = data.len() as u64 - 1;
        assert!(matches!(
            PackageContents::from_bytes_with_limits(&data, &limits),
            Err(ParseError::ResourceLimit {
                kind: ResourceLimitKind::ArchiveBytes,
                ..
            })
        ));
    }

    #[test]
    fn package_limits_reject_too_many_parts() {
        let data = make_zip(
            &[("a.bin", b"a"), ("b.bin", b"b")],
            zip::CompressionMethod::Stored,
        );
        let mut limits = tiny_limits();
        limits.max_parts = 1;
        assert!(matches!(
            PackageContents::from_bytes_with_limits(&data, &limits),
            Err(ParseError::ResourceLimit {
                kind: ResourceLimitKind::PartCount,
                ..
            })
        ));
    }

    #[test]
    fn package_limits_reject_an_oversized_part() {
        let data = make_zip(&[("large.bin", b"12345")], zip::CompressionMethod::Stored);
        let mut limits = tiny_limits();
        limits.max_part_uncompressed_bytes = 4;
        assert!(matches!(
            PackageContents::from_bytes_with_limits(&data, &limits),
            Err(ParseError::ResourceLimit {
                kind: ResourceLimitKind::PartBytes,
                ..
            })
        ));
    }

    #[test]
    fn package_limits_reject_excessive_total_uncompressed_bytes() {
        let data = make_zip(
            &[("a.bin", b"123"), ("b.bin", b"456")],
            zip::CompressionMethod::Stored,
        );
        let mut limits = tiny_limits();
        limits.max_total_uncompressed_bytes = 5;
        assert!(matches!(
            PackageContents::from_bytes_with_limits(&data, &limits),
            Err(ParseError::ResourceLimit {
                kind: ResourceLimitKind::TotalUncompressedBytes,
                ..
            })
        ));
    }

    #[test]
    fn package_limits_reject_an_extreme_compression_ratio() {
        let repeated = vec![b'x'; 64 * 1024];
        let data = make_zip(
            &[("repeated.bin", repeated.as_slice())],
            zip::CompressionMethod::Deflated,
        );
        let mut limits = tiny_limits();
        limits.max_part_uncompressed_bytes = 128 * 1024;
        limits.max_total_uncompressed_bytes = 128 * 1024;
        limits.compression_ratio_min_uncompressed_bytes = 1024;
        limits.max_compression_ratio = 10;
        assert!(matches!(
            PackageContents::from_bytes_with_limits(&data, &limits),
            Err(ParseError::ResourceLimit {
                kind: ResourceLimitKind::CompressionRatio,
                ..
            })
        ));
    }

    #[test]
    fn normalize_strips_leading_slash_and_lowercases() {
        assert_eq!(normalize_path("/Word/Document.XML"), "word/document.xml");
        assert_eq!(
            normalize_path("word/media/Image1.PNG"),
            "word/media/image1.png"
        );
    }

    #[test]
    fn resolve_relative_target_joins_base_dir() {
        assert_eq!(
            resolve_target("word", "media/image1.png"),
            "word/media/image1.png"
        );
    }

    #[test]
    fn resolve_absolute_target_ignores_base_dir() {
        assert_eq!(
            resolve_target("word", "/word/media/image1.png"),
            "word/media/image1.png"
        );
    }

    #[test]
    fn resolve_empty_base_dir_uses_target_as_is() {
        assert_eq!(resolve_target("", "document.xml"), "document.xml");
    }

    #[test]
    fn resolve_collapses_parent_sequences() {
        // A theme rel from word/ pointing at word/theme/theme1.xml via "../".
        assert_eq!(
            resolve_target("word/theme", "../media/image1.png"),
            "word/media/image1.png"
        );
        // Multiple "../" segments.
        assert_eq!(resolve_target("a/b/c", "../../x.xml"), "a/x.xml");
    }

    #[test]
    fn resolve_parent_past_root_leaves_residual_dotdot() {
        // Traversing above the package root is malformed. One "../" is consumed
        // against "word"; the second has no parent left, so a residual "../"
        // remains — a path that matches no part, which is the safe outcome for
        // malformed input (the image is simply not found).
        assert_eq!(resolve_target("word", "../../x.xml"), "../x.xml");
    }

    #[test]
    fn resolve_collapses_current_dir_segments() {
        // Leading "./" and interior "/./" are no-ops and must normalize away,
        // otherwise the part lookup (an exact map key) would miss.
        assert_eq!(resolve_target("word", "./media/x.png"), "word/media/x.png");
        assert_eq!(resolve_target("", "./document.xml"), "document.xml");
        assert_eq!(resolve_target("word", "media/./x.png"), "word/media/x.png");
    }

    #[test]
    fn rels_path_for_builds_sibling_rels_file() {
        assert_eq!(
            rels_path_for("word/document.xml"),
            "word/_rels/document.xml.rels"
        );
        // A root-level part has no directory prefix.
        assert_eq!(
            rels_path_for("[Content_Types].xml"),
            "_rels/[content_types].xml.rels"
        );
    }

    #[test]
    fn part_directory_returns_dir_or_empty() {
        assert_eq!(part_directory("word/media/image1.png"), "word/media");
        assert_eq!(part_directory("document.xml"), "");
    }

    // KNOWN GAP: percent-encoded relationship targets (e.g. spaces as "%20")
    // are not decoded, so a target like "media/my%20image.png" resolves to a
    // key that won't match the ZIP part "media/my image.png". Word does not
    // percent-encode internal media names in practice, so this is left as a
    // documented limitation rather than pulling in a percent-decoder.
}
