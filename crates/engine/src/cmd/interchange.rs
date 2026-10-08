//! Interchange formats: IDML (InDesign Markup Language) import and export.

use designcraft_doc::Document;
use serde_json::{Value, json};

use super::file::{base64_decode, base64_encode};
use super::{CommandSpec, always, bad, cmd, has_doc, str_param};
use crate::{DocState, EngineError, Result, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "file.exportIdml", "Export IDML…", [], None,
            "{path?, embedImages?: true} — writes an IDML package to `path`, or returns {base64} without a path",
            has_doc, export_idml),
        cmd!(noundo "file.openIdml", "Open IDML", [], None,
            "{path | base64, name?} — opens an IDML package as a new document (linked images are read next to the file or from its Links/ folder)",
            always, open_idml),
    ]
}

fn export_idml(s: &mut Session, p: &Value) -> Result<Value> {
    let st = s.doc()?;
    let opts = designcraft_idml::ExportOptions { embed_images: p.get("embedImages").and_then(Value::as_bool).unwrap_or(true) };
    let bytes = designcraft_idml::export_idml_with(&st.doc, &opts);
    match str_param(p, "path") {
        Some(path) => {
            #[cfg(not(target_arch = "wasm32"))]
            std::fs::write(path, &bytes).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
            Ok(json!({"path": path, "bytes": bytes.len()}))
        }
        None => Ok(json!({"base64": base64_encode(&bytes), "bytes": bytes.len()})),
    }
}

/// Import IDML bytes; `dir` is the folder the package came from (for relative link lookup).
pub fn import(bytes: &[u8], dir: Option<&std::path::Path>) -> Result<Document> {
    let dir = dir.map(|d| d.to_path_buf());
    let read = move |link: &str| -> Option<Vec<u8>> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let dir = dir.as_ref()?;
            read_link_beside_idml(dir, link, designcraft_idml::MAX_LINKED_RESOURCE_BYTES)
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (link, &dir);
            None
        }
    };
    designcraft_idml::import_idml_with(bytes, &read).map_err(|e| EngineError::Other(e.to_string()))
}

#[cfg(not(target_arch = "wasm32"))]
fn read_link_beside_idml(dir: &std::path::Path, link: &str, max_bytes: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::path::{Component, Path};

    let relative = Path::new(link);
    if relative.as_os_str().is_empty() || !relative.components().all(|component| matches!(component, Component::Normal(_))) {
        return None;
    }
    let root = std::fs::canonicalize(dir).ok()?;
    let name = relative.file_name()?;
    let candidates = [root.join(relative), root.join("Links").join(relative), root.join("Links").join(name)];
    for candidate in candidates {
        let Ok(canonical) = std::fs::canonicalize(candidate) else {
            continue;
        };
        if !canonical.starts_with(&root) {
            continue;
        }
        let Ok(file) = std::fs::File::open(canonical) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > max_bytes {
            continue;
        }
        let capacity = usize::try_from(metadata.len()).ok()?;
        let mut data = Vec::with_capacity(capacity);
        if file.take(max_bytes.saturating_add(1)).read_to_end(&mut data).is_ok() && data.len() as u64 <= max_bytes {
            return Some(data);
        }
    }
    None
}

pub(crate) fn open_idml(s: &mut Session, p: &Value) -> Result<Value> {
    let (bytes, dir, name) = if let Some(b) = str_param(p, "base64") {
        (base64_decode(b), None, str_param(p, "name").map(|n| n.trim_end_matches(".idml").to_string()))
    } else if let Some(path) = str_param(p, "path") {
        #[cfg(not(target_arch = "wasm32"))]
        let b = std::fs::read(path).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
        #[cfg(target_arch = "wasm32")]
        let b: Vec<u8> = Vec::new();
        let pp = std::path::Path::new(path);
        (b, pp.parent().map(|d| d.to_path_buf()), pp.file_stem().map(|n| n.to_string_lossy().to_string()))
    } else {
        return Err(bad("file.openIdml", "missing `path` or `base64`"));
    };
    let mut d = import(&bytes, dir.as_deref())?;
    if let Some(n) = name {
        d.title = n;
    }
    // Never save over the .idml with the native format: the document starts unsaved.
    let i = s.add_document(DocState::new(d, None));
    Ok(json!({"index": i}))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::read_link_beside_idml;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn test_dir() -> std::path::PathBuf {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("dc-idml-links-{}-{id}", std::process::id()))
    }

    #[test]
    fn linked_resources_stay_beside_the_package() {
        let root = test_dir();
        let package = root.join("package");
        std::fs::create_dir_all(package.join("Links")).unwrap();
        std::fs::write(package.join("Links/photo.jpg"), b"jpeg").unwrap();
        std::fs::write(package.join("large.jpg"), b"12345").unwrap();
        assert_eq!(read_link_beside_idml(&package, "photo.jpg", 4).as_deref(), Some(b"jpeg".as_slice()));
        assert!(read_link_beside_idml(&package, "../photo.jpg", 4).is_none());
        assert!(read_link_beside_idml(&package, "large.jpg", 4).is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = root.join("outside.jpg");
            std::fs::write(&outside, b"data").unwrap();
            symlink(&outside, package.join("escape.jpg")).unwrap();
            assert!(read_link_beside_idml(&package, "escape.jpg", 4).is_none());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
