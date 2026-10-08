//! The native `.designcraft` format: a zip archive holding
//!
//! - `mimetype` (stored first, uncompressed): `application/vnd.designcraft+zip`
//! - `document.json`: the [`Document`] (pretty JSON, documented by the serde model in `designcraft-doc`)
//! - `assets/<id>.<ext>`: embedded placed files, byte-for-byte
//! - `meta.json`: format version and generator
//!
//! Older single-file JSON documents (assets as base64 in `assetData`) still open.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use std::sync::Arc;

use designcraft_doc::{AssetId, Document};
use serde_json::{Value, json};

pub const MIME: &str = "application/vnd.designcraft+zip";
pub const VERSION: u32 = 1;

const MAX_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4_096;
const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_META_BYTES: usize = 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
struct ArchiveLimits {
    entries: usize,
    entry_bytes: u64,
    expanded_bytes: u64,
}

const ARCHIVE_LIMITS: ArchiveLimits =
    ArchiveLimits { entries: MAX_ARCHIVE_ENTRIES, entry_bytes: MAX_ENTRY_BYTES, expanded_bytes: MAX_EXPANDED_BYTES };

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("not a DesignCraft document: {0}")]
    NotOurs(String),
    #[error("document is from a newer DesignCraft (format {0}); please update")]
    TooNew(u32),
    #[error("{0}")]
    Io(String),
}

fn ext_for(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/tiff" => "tif",
        "application/pdf" => "pdf",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

/// Serialize a document to `.designcraft` bytes.
pub fn save(doc: &Document) -> Result<Vec<u8>, FormatError> {
    let io = |e: zip::result::ZipError| FormatError::Io(e.to_string());
    let mut buf = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let deflate = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        z.start_file("mimetype", stored).map_err(io)?;
        z.write_all(MIME.as_bytes()).map_err(|e| FormatError::Io(e.to_string()))?;
        z.start_file("meta.json", deflate).map_err(io)?;
        let meta = json!({"format": "designcraft", "version": VERSION, "generator": concat!("DesignCraft ", env!("CARGO_PKG_VERSION"))});
        z.write_all(meta.to_string().as_bytes()).map_err(|e| FormatError::Io(e.to_string()))?;
        z.start_file("document.json", deflate).map_err(io)?;
        let json = serde_json::to_vec_pretty(doc).map_err(|e| FormatError::Io(e.to_string()))?;
        z.write_all(&json).map_err(|e| FormatError::Io(e.to_string()))?;
        for (id, a) in &doc.assets {
            if a.data.is_empty() {
                continue;
            }
            // Already-compressed images are stored as-is.
            let opts = if matches!(a.mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp") { stored } else { deflate };
            z.start_file(format!("assets/{}.{}", id.0, ext_for(&a.mime)), opts).map_err(io)?;
            z.write_all(&a.data).map_err(|e| FormatError::Io(e.to_string()))?;
        }
        z.finish().map_err(io)?;
    }
    Ok(buf.into_inner())
}

/// Read `.designcraft` bytes (zip, or the legacy single JSON file).
pub fn load(bytes: &[u8]) -> Result<Document, FormatError> {
    if bytes.starts_with(b"PK") {
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(FormatError::NotOurs(format!("file exceeds the {MAX_ARCHIVE_BYTES}-byte size limit")));
        }
        return load_zip(bytes);
    }
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(FormatError::NotOurs(format!("legacy document exceeds the {MAX_DOCUMENT_BYTES}-byte size limit")));
    }
    load_legacy_json(bytes)
}

fn validate_archive(z: &mut zip::ZipArchive<Cursor<&[u8]>>, limits: ArchiveLimits) -> Result<(), FormatError> {
    if z.len() > limits.entries {
        return Err(FormatError::NotOurs(format!("archive contains more than {} entries", limits.entries)));
    }
    let mut expanded = 0u64;
    for i in 0..z.len() {
        let f = z.by_index(i).map_err(|e| FormatError::NotOurs(e.to_string()))?;
        let size = f.size();
        if size > limits.entry_bytes {
            return Err(FormatError::NotOurs(format!("archive entry exceeds the {}-byte limit", limits.entry_bytes)));
        }
        expanded = expanded.checked_add(size).ok_or_else(|| FormatError::NotOurs("archive expanded size is too large".into()))?;
        if expanded > limits.expanded_bytes {
            return Err(FormatError::NotOurs(format!("archive expands beyond the {}-byte limit", limits.expanded_bytes)));
        }
    }
    Ok(())
}

fn read_entry(z: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str, max_bytes: usize) -> Result<Option<Vec<u8>>, FormatError> {
    let mut f = match z.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(FormatError::NotOurs(e.to_string())),
    };
    if f.size() > max_bytes as u64 {
        return Err(FormatError::NotOurs(format!("{name} exceeds the {max_bytes}-byte limit")));
    }
    let capacity = usize::try_from(f.size()).unwrap_or(max_bytes).min(max_bytes);
    let mut v = Vec::with_capacity(capacity);
    (&mut f).take(max_bytes.saturating_add(1) as u64).read_to_end(&mut v).map_err(|e| FormatError::NotOurs(format!("can't read {name}: {e}")))?;
    if v.len() > max_bytes {
        return Err(FormatError::NotOurs(format!("{name} exceeds the {max_bytes}-byte limit")));
    }
    Ok(Some(v))
}

fn load_zip(bytes: &[u8]) -> Result<Document, FormatError> {
    let mut z = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| FormatError::NotOurs(e.to_string()))?;
    validate_archive(&mut z, ARCHIVE_LIMITS)?;
    if let Some(meta) = read_entry(&mut z, "meta.json", MAX_META_BYTES)?
        && let Ok(m) = serde_json::from_slice::<Value>(&meta)
        && let Some(v) = m.get("version").and_then(Value::as_u64)
        && v as u32 > VERSION
    {
        return Err(FormatError::TooNew(v as u32));
    }
    let json = read_entry(&mut z, "document.json", MAX_DOCUMENT_BYTES)?.ok_or_else(|| FormatError::NotOurs("missing document.json".into()))?;
    let mut doc: Document = serde_json::from_slice(&json).map_err(|e| FormatError::NotOurs(e.to_string()))?;
    let names: HashSet<String> = z.file_names().filter(|name| name.starts_with("assets/")).map(str::to_string).collect();
    for name in names {
        let Some(stem) = name.strip_prefix("assets/") else { continue };
        let Some(id) = stem.split('.').next().and_then(|s| s.parse::<u64>().ok()) else { continue };
        if let (Some(data), Some(a)) = (read_entry(&mut z, &name, MAX_ENTRY_BYTES as usize)?, doc.assets.get_mut(&AssetId(id))) {
            Arc::make_mut(a).data = Arc::new(data);
        }
    }
    doc.check().map_err(|e| FormatError::NotOurs(e.to_string()))?;
    Ok(doc)
}

fn load_legacy_json(bytes: &[u8]) -> Result<Document, FormatError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| FormatError::NotOurs(e.to_string()))?;
    let mut doc: Document = serde_json::from_value(v.clone()).map_err(|e| FormatError::NotOurs(e.to_string()))?;
    if let Some(data) = v.get("assetData").and_then(Value::as_object) {
        for (k, s) in data {
            if let (Ok(id), Some(s)) = (k.parse::<u64>(), s.as_str())
                && let Some(a) = doc.assets.get_mut(&AssetId(id))
            {
                Arc::make_mut(a).data = Arc::new(base64_decode(s));
            }
        }
    }
    doc.check().map_err(|e| FormatError::NotOurs(e.to_string()))?;
    Ok(doc)
}

fn base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => continue,
        } as u32;
        buf = buf << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_doc::build::NewDocument;
    use designcraft_doc::geom::Rect;
    use designcraft_doc::{Asset, ParaFormat, SpreadRef};

    #[test]
    fn roundtrip_with_assets() {
        let mut d = Document::new(&NewDocument { pages: 3, ..Default::default() });
        let lid = d.default_layer();
        d.add_text_frame(SpreadRef::Doc(0), Rect::new(10.0, 10.0, 200.0, 200.0), lid, "héllo\nworld", ParaFormat::default()).unwrap();
        let aid = AssetId(d.alloc());
        d.assets.insert(
            aid,
            Arc::new(Asset {
                page: 0,
                id: aid,
                name: "x.png".into(),
                mime: "image/png".into(),
                link: None,
                data: Arc::new(vec![1, 2, 3, 4]),
                pixels: Some((1, 1)),
            }),
        );
        let bytes = save(&d).unwrap();
        assert!(bytes.starts_with(b"PK"));
        let back = load(&bytes).unwrap();
        assert_eq!(back.page_count(), 3);
        assert_eq!(*back.assets[&aid].data, vec![1, 2, 3, 4]);
        assert_eq!(back.stories, d.stories);
    }

    #[test]
    fn rejects_garbage_and_newer_versions() {
        assert!(load(b"nope").is_err());
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            z.start_file("meta.json", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(br#"{"version": 99}"#).unwrap();
            z.finish().unwrap();
        }
        assert!(matches!(load(&buf.into_inner()), Err(FormatError::TooNew(99))));
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            for (name, body) in entries {
                z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                z.write_all(body).unwrap();
            }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn archive_resource_limits_cover_count_entry_and_total_sizes() {
        let bytes = archive(&[("one", b"123"), ("two", b"456")]);
        let limits = |entries, entry_bytes, expanded_bytes| ArchiveLimits { entries, entry_bytes, expanded_bytes };

        let mut z = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        assert!(validate_archive(&mut z, limits(1, 10, 10)).is_err());
        let mut z = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        assert!(validate_archive(&mut z, limits(2, 2, 10)).is_err());
        let mut z = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        assert!(validate_archive(&mut z, limits(2, 10, 5)).is_err());
        let mut z = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        assert!(validate_archive(&mut z, limits(2, 3, 6)).is_ok());
    }
}
