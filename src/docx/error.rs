use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLimitKind {
    ArchiveBytes,
    PartCount,
    PartBytes,
    TotalUncompressedBytes,
    CompressionRatio,
}

impl std::fmt::Display for ResourceLimitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::ArchiveBytes => "archive bytes",
            Self::PartCount => "part count",
            Self::PartBytes => "part bytes",
            Self::TotalUncompressedBytes => "total uncompressed bytes",
            Self::CompressionRatio => "compression ratio",
        };
        f.write_str(name)
    }
}

/// All errors that can occur during DOCX parsing.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("failed to read ZIP archive: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("failed to deserialize XML: {0}")]
    XmlDeserialize(#[from] quick_xml::DeError),

    #[error("invalid UTF-8 in XML content: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("missing required part: {0}")]
    MissingPart(String),

    #[error("invalid attribute value '{value}' for '{attr}': {reason}")]
    InvalidAttributeValue {
        attr: String,
        value: String,
        reason: String,
    },

    #[error("invalid integer: {0}")]
    ParseInt(#[from] std::num::ParseIntError),

    #[error(
        "DOCX resource limit exceeded ({kind}){part}: actual {actual}, limit {limit}",
        part = part.as_deref().map(|name| format!(" for '{name}'")).unwrap_or_default()
    )]
    ResourceLimit {
        kind: ResourceLimitKind,
        part: Option<String>,
        actual: u64,
        limit: u64,
    },
}

pub type Result<T> = std::result::Result<T, ParseError>;
