//! Attachments — image (and future binary) blobs that ride along with a
//! [`ChatMessageRequest`].
//!
//! What this is:
//! - A small struct + helpers for loading an image from a local path or
//!   a `data:` / `http(s)` URL, validating it (size cap, MIME whitelist),
//!   and packaging it as a base64 payload that providers can serialize
//!   into their wire format.
//! - Provider-agnostic. Each provider (OpenAI, Anthropic, ZAI) translates
//!   `Attachment` into its own multipart-content shape.
//!
//! What this isn't:
//! - An image processor. We don't resize/compress; if a file is too
//!   large we reject it with a clear error and let the user shrink it.
//! - A general file embedder. Today only images are supported. PDFs and
//!   other binary types are intentionally out of scope until at least
//!   one provider has stable support across the board.
//!
//! Default caps:
//! - Max file size: 20 MB (matches OpenAI / Anthropic vision limits).
//! - Max attachments per message: 10.
//! - Allowed MIME types: image/png, image/jpeg, image/gif, image/webp.
//!
//! Caps live on [`AttachmentLimits`] so callers can tighten them per
//! provider or per session.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

/// Maximum payload size (bytes) for a single image. 20 MB matches the
/// public limits published by OpenAI and Anthropic at the time of
/// writing. Providers may reject smaller payloads; we surface their
/// errors verbatim.
pub const DEFAULT_MAX_BYTES: usize = 20 * 1024 * 1024;

/// Maximum number of attachments per chat message. Hard cap to prevent
/// runaway prompts; well above any documented provider limit.
pub const DEFAULT_MAX_PER_MESSAGE: usize = 10;

/// Allowed MIME types for image attachments. Wire formats vary; the
/// providers only accept these four.
pub const ALLOWED_IMAGE_MIME: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Per-message constraints. Construct with [`AttachmentLimits::default`]
/// and override fields as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentLimits {
    pub max_bytes: usize,
    pub max_per_message: usize,
}

impl Default for AttachmentLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_per_message: DEFAULT_MAX_PER_MESSAGE,
        }
    }
}

/// Where the attachment came from. Kept around for diagnostics + the
/// CLI status bar; providers don't see this directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttachmentSource {
    /// Loaded from a local filesystem path.
    FilePath { path: String },
    /// Inlined `data:image/...;base64,...` URL.
    DataUrl,
    /// Remote `http(s)://...` URL. Some providers fetch this themselves;
    /// we keep the URL verbatim and let them handle it.
    RemoteUrl { url: String },
}

/// What flavour of attachment this is. Today only images are supported,
/// but the enum is open-ended so future kinds (e.g. PDFs) don't force a
/// breaking change to consumers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
}

/// One attachment riding along with a chat message.
///
/// `data_base64` is the raw payload in standard base64 (no `data:`
/// prefix). For [`AttachmentSource::RemoteUrl`] the field stays empty —
/// the URL is the payload. Providers either inline the base64 (OpenAI:
/// `image_url.url = "data:<mime>;base64,<data>"`; Anthropic: a
/// `source.type = "base64"` block) or pass the remote URL through.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub kind: AttachmentKind,
    /// MIME type, e.g. `"image/png"`. Always lowercase.
    pub mime_type: String,
    /// Base64-encoded payload. Empty when `source` is `RemoteUrl`.
    pub data_base64: String,
    /// Origin of the attachment. Useful for the CLI to surface where a
    /// payload came from in confirmation prompts and audit logs.
    pub source: AttachmentSource,
    /// Original byte length pre-encoding. Empty for remote URLs.
    pub size_bytes: usize,
}

impl Attachment {
    /// Convenience: render the attachment as a `data:` URL string. Used
    /// by the OpenAI-compatible providers.
    pub fn as_data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.data_base64)
    }

    /// `true` when the payload travels by URL reference rather than as
    /// inline base64.
    pub fn is_remote(&self) -> bool {
        matches!(self.source, AttachmentSource::RemoteUrl { .. })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("Failed to read file {path:?}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Unsupported MIME type {mime_type:?} (allowed: {allowed:?})")]
    UnsupportedMime {
        mime_type: String,
        allowed: Vec<String>,
    },
    #[error("Cannot detect image format from {path:?}: file isn't a recognized image")]
    UnrecognizedFormat { path: String },
    #[error("Attachment too large: {size} bytes > limit {limit} bytes")]
    TooLarge { size: usize, limit: usize },
    #[error("Invalid data URL: {reason}")]
    InvalidDataUrl { reason: String },
    #[error("Invalid URL scheme {scheme:?}; only http(s) and data: are allowed")]
    InvalidUrlScheme { scheme: String },
}

/// Load an image attachment from a local filesystem path. Detects the
/// MIME type from magic bytes (not extension), validates against the
/// allowlist, and base64-encodes the payload.
pub fn load_image_from_path(
    path: impl AsRef<Path>,
    limits: AttachmentLimits,
) -> Result<Attachment, AttachmentError> {
    let path_ref = path.as_ref();
    let path_str = path_ref.to_string_lossy().to_string();

    let bytes = std::fs::read(path_ref).map_err(|e| AttachmentError::Io {
        path: path_str.clone(),
        source: e,
    })?;

    if bytes.len() > limits.max_bytes {
        return Err(AttachmentError::TooLarge {
            size: bytes.len(),
            limit: limits.max_bytes,
        });
    }

    let mime_type =
        detect_image_mime(&bytes).ok_or_else(|| AttachmentError::UnrecognizedFormat {
            path: path_str.clone(),
        })?;

    if !ALLOWED_IMAGE_MIME.contains(&mime_type) {
        return Err(AttachmentError::UnsupportedMime {
            mime_type: mime_type.to_string(),
            allowed: ALLOWED_IMAGE_MIME.iter().map(|s| s.to_string()).collect(),
        });
    }

    let size_bytes = bytes.len();
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(Attachment {
        kind: AttachmentKind::Image,
        mime_type: mime_type.to_string(),
        data_base64,
        source: AttachmentSource::FilePath { path: path_str },
        size_bytes,
    })
}

/// Build an attachment from a URL. Two shapes are recognised:
///
/// 1. `data:<mime>;base64,<payload>` — decoded inline. The MIME must be
///    in `ALLOWED_IMAGE_MIME`.
/// 2. `http(s)://...` — kept verbatim as a `RemoteUrl` source. The
///    `data_base64` field is empty; providers that support remote URLs
///    (OpenAI, Anthropic) pass the URL through.
///
/// Other schemes (`file:`, `ftp:`, etc.) are rejected.
pub fn load_image_from_url(
    url: &str,
    limits: AttachmentLimits,
) -> Result<Attachment, AttachmentError> {
    if let Some(rest) = url.strip_prefix("data:") {
        return parse_data_url(rest, limits);
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(Attachment {
            kind: AttachmentKind::Image,
            // We don't know the MIME without fetching; providers that
            // accept remote URLs rely on the server's content-type
            // header. Leave empty so consumers can detect this case.
            mime_type: String::new(),
            data_base64: String::new(),
            source: AttachmentSource::RemoteUrl {
                url: url.to_string(),
            },
            size_bytes: 0,
        });
    }
    let scheme = url.split(':').next().unwrap_or(url).to_string();
    Err(AttachmentError::InvalidUrlScheme { scheme })
}

fn parse_data_url(rest: &str, limits: AttachmentLimits) -> Result<Attachment, AttachmentError> {
    // Expected: `<mime>;base64,<payload>`.
    let comma = rest
        .find(',')
        .ok_or_else(|| AttachmentError::InvalidDataUrl {
            reason: "missing comma between metadata and payload".into(),
        })?;
    let (meta, payload) = rest.split_at(comma);
    let payload = &payload[1..]; // skip the comma itself

    let mut parts = meta.split(';');
    let mime_type = parts
        .next()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AttachmentError::InvalidDataUrl {
            reason: "missing MIME type".into(),
        })?;

    let is_base64 = parts.any(|p| p.trim().eq_ignore_ascii_case("base64"));
    if !is_base64 {
        return Err(AttachmentError::InvalidDataUrl {
            reason: "only ;base64 data URLs are supported".into(),
        });
    }

    if !ALLOWED_IMAGE_MIME.contains(&mime_type.as_str()) {
        return Err(AttachmentError::UnsupportedMime {
            mime_type,
            allowed: ALLOWED_IMAGE_MIME.iter().map(|s| s.to_string()).collect(),
        });
    }

    let raw = base64::engine::general_purpose::STANDARD
        .decode(payload.as_bytes())
        .map_err(|e| AttachmentError::InvalidDataUrl {
            reason: format!("base64 decode failed: {}", e),
        })?;

    if raw.len() > limits.max_bytes {
        return Err(AttachmentError::TooLarge {
            size: raw.len(),
            limit: limits.max_bytes,
        });
    }

    Ok(Attachment {
        kind: AttachmentKind::Image,
        mime_type,
        data_base64: payload.to_string(),
        source: AttachmentSource::DataUrl,
        size_bytes: raw.len(),
    })
}

/// Detect image MIME type from magic bytes. Returns one of
/// [`ALLOWED_IMAGE_MIME`] or `None` if unrecognised.
///
/// We sniff bytes rather than trusting file extensions because the
/// agent is non-interactive: a `.png` that's actually a JPEG (or worse,
/// a renamed binary) should not slip through.
pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // WebP: "RIFF....WEBP"
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

impl fmt::Display for AttachmentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachmentSource::FilePath { path } => write!(f, "{}", path),
            AttachmentSource::DataUrl => write!(f, "<data url>"),
            AttachmentSource::RemoteUrl { url } => write!(f, "{}", url),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes() -> Vec<u8> {
        // Minimal valid PNG signature + IHDR header (1×1 pixel, 8-bit RGBA).
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            b'I', b'H', b'D', b'R', // chunk type
            0x00, 0x00, 0x00, 0x01, // width
            0x00, 0x00, 0x00, 0x01, // height
            0x08, 0x06, 0x00, 0x00, 0x00, // bit depth, color type, etc.
            0x1F, 0x15, 0xC4, 0x89, // CRC (not validated)
        ]);
        bytes
    }

    fn jpeg_bytes() -> Vec<u8> {
        b"\xff\xd8\xff\xe0\x00\x10JFIF\x00".to_vec()
    }

    fn gif_bytes() -> Vec<u8> {
        b"GIF89a\x01\x00\x01\x00".to_vec()
    }

    fn webp_bytes() -> Vec<u8> {
        let mut b = b"RIFF\x24\x00\x00\x00WEBP".to_vec();
        b.extend_from_slice(b"VP8 ");
        b
    }

    fn write_temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("agentic-attach-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn detect_mime_known_formats() {
        assert_eq!(detect_image_mime(&png_bytes()), Some("image/png"));
        assert_eq!(detect_image_mime(&jpeg_bytes()), Some("image/jpeg"));
        assert_eq!(detect_image_mime(&gif_bytes()), Some("image/gif"));
        assert_eq!(detect_image_mime(&webp_bytes()), Some("image/webp"));
    }

    #[test]
    fn detect_mime_unknown_returns_none() {
        assert_eq!(detect_image_mime(b"plain text"), None);
        assert_eq!(detect_image_mime(b""), None);
        assert_eq!(detect_image_mime(b"\x00\x01\x02"), None);
    }

    #[test]
    fn load_from_path_png_success() {
        let path = write_temp_file("a.png", &png_bytes());
        let att = load_image_from_path(&path, AttachmentLimits::default()).unwrap();
        assert_eq!(att.mime_type, "image/png");
        assert_eq!(att.kind, AttachmentKind::Image);
        assert!(!att.data_base64.is_empty());
        assert!(matches!(att.source, AttachmentSource::FilePath { .. }));
    }

    #[test]
    fn load_from_path_rejects_oversize() {
        // Pretend we have a 1-byte limit; the real PNG is bigger.
        let path = write_temp_file("big.png", &png_bytes());
        let limits = AttachmentLimits {
            max_bytes: 4,
            max_per_message: 10,
        };
        let err = load_image_from_path(&path, limits).unwrap_err();
        assert!(matches!(err, AttachmentError::TooLarge { .. }));
    }

    #[test]
    fn load_from_path_rejects_non_image() {
        let path = write_temp_file("not-image.txt", b"hello world");
        let err = load_image_from_path(&path, AttachmentLimits::default()).unwrap_err();
        assert!(matches!(err, AttachmentError::UnrecognizedFormat { .. }));
    }

    #[test]
    fn load_from_path_rejects_renamed_extension() {
        // .png extension but the bytes are a JPEG. We sniff bytes, so
        // this should still be detected as image/jpeg (allowed) — the
        // important property is that we don't trust the extension.
        let path = write_temp_file("liar.png", &jpeg_bytes());
        let att = load_image_from_path(&path, AttachmentLimits::default()).unwrap();
        assert_eq!(att.mime_type, "image/jpeg");
    }

    #[test]
    fn load_from_data_url_success() {
        let raw = png_bytes();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let url = format!("data:image/png;base64,{}", b64);
        let att = load_image_from_url(&url, AttachmentLimits::default()).unwrap();
        assert_eq!(att.mime_type, "image/png");
        assert_eq!(att.source, AttachmentSource::DataUrl);
        assert_eq!(att.size_bytes, raw.len());
    }

    #[test]
    fn load_from_data_url_rejects_unsupported_mime() {
        let url = "data:application/pdf;base64,JVBERi0=";
        let err = load_image_from_url(url, AttachmentLimits::default()).unwrap_err();
        assert!(matches!(err, AttachmentError::UnsupportedMime { .. }));
    }

    #[test]
    fn load_from_data_url_rejects_non_base64() {
        let url = "data:image/png,raw-text-not-base64";
        let err = load_image_from_url(url, AttachmentLimits::default()).unwrap_err();
        assert!(matches!(err, AttachmentError::InvalidDataUrl { .. }));
    }

    #[test]
    fn load_from_remote_url_keeps_url_verbatim() {
        let url = "https://example.com/cat.png";
        let att = load_image_from_url(url, AttachmentLimits::default()).unwrap();
        assert!(att.is_remote());
        assert!(att.data_base64.is_empty());
        match att.source {
            AttachmentSource::RemoteUrl { url: u } => assert_eq!(u, url),
            _ => panic!("expected RemoteUrl"),
        }
    }

    #[test]
    fn load_from_url_rejects_other_schemes() {
        for url in ["file:///etc/passwd", "ftp://example.com/x", "ssh://x"] {
            let err = load_image_from_url(url, AttachmentLimits::default()).unwrap_err();
            assert!(
                matches!(err, AttachmentError::InvalidUrlScheme { .. }),
                "expected scheme rejection for {:?}, got: {:?}",
                url,
                err
            );
        }
    }

    #[test]
    fn as_data_url_round_trip() {
        let path = write_temp_file("rt.png", &png_bytes());
        let att = load_image_from_path(&path, AttachmentLimits::default()).unwrap();
        let url = att.as_data_url();
        assert!(url.starts_with("data:image/png;base64,"));
        // Round-trip through the URL parser.
        let again = load_image_from_url(&url, AttachmentLimits::default()).unwrap();
        assert_eq!(att.mime_type, again.mime_type);
        assert_eq!(att.data_base64, again.data_base64);
    }
}
