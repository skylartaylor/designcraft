use std::io::{Cursor, Read};

use crate::ImportError;

const MAX_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4_096;
const MAX_ENTRY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_XML_PART_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
struct ArchiveLimits {
    entries: usize,
    entry_bytes: u64,
    expanded_bytes: u64,
}

const ARCHIVE_LIMITS: ArchiveLimits =
    ArchiveLimits { entries: MAX_ARCHIVE_ENTRIES, entry_bytes: MAX_ENTRY_BYTES, expanded_bytes: MAX_EXPANDED_BYTES };

pub(crate) fn open(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, ImportError> {
    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(ImportError::Corrupt(format!("file exceeds the {MAX_ARCHIVE_BYTES}-byte size limit")));
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| ImportError::Corrupt(e.to_string()))?;
    validate(&mut zip, ARCHIVE_LIMITS)?;
    Ok(zip)
}

fn validate(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, limits: ArchiveLimits) -> Result<(), ImportError> {
    if zip.len() > limits.entries {
        return Err(ImportError::Corrupt(format!("archive contains more than {} entries", limits.entries)));
    }
    let mut expanded = 0u64;
    for i in 0..zip.len() {
        let f = zip.by_index(i).map_err(|e| ImportError::Corrupt(e.to_string()))?;
        let size = f.size();
        if size > limits.entry_bytes {
            return Err(ImportError::Corrupt(format!("archive entry exceeds the {}-byte limit", limits.entry_bytes)));
        }
        expanded = expanded.checked_add(size).ok_or_else(|| ImportError::Corrupt("archive expanded size is too large".into()))?;
        if expanded > limits.expanded_bytes {
            return Err(ImportError::Corrupt(format!("archive expands beyond the {}-byte limit", limits.expanded_bytes)));
        }
    }
    Ok(())
}

pub(crate) fn part(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<Option<String>, ImportError> {
    let mut f = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(ImportError::Corrupt(e.to_string())),
    };
    if f.size() > MAX_XML_PART_BYTES as u64 {
        return Err(ImportError::Corrupt(format!("{name} exceeds the {MAX_XML_PART_BYTES}-byte XML part limit")));
    }
    let capacity = usize::try_from(f.size()).unwrap_or(MAX_XML_PART_BYTES).min(MAX_XML_PART_BYTES);
    let mut s = String::with_capacity(capacity);
    (&mut f)
        .take(MAX_XML_PART_BYTES.saturating_add(1) as u64)
        .read_to_string(&mut s)
        .map_err(|e| ImportError::Corrupt(format!("can't read {name}: {e}")))?;
    if s.len() > MAX_XML_PART_BYTES {
        return Err(ImportError::Corrupt(format!("{name} exceeds the {MAX_XML_PART_BYTES}-byte XML part limit")));
    }
    Ok(Some(s))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

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
        assert!(validate(&mut z, limits(1, 10, 10)).is_err());
        let mut z = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        assert!(validate(&mut z, limits(2, 2, 10)).is_err());
        let mut z = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        assert!(validate(&mut z, limits(2, 10, 5)).is_err());
        let mut z = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        assert!(validate(&mut z, limits(2, 3, 6)).is_ok());
    }
}
