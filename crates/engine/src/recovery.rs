//! Crash recovery (InDesign's automatic recovery data): unsaved changes of every open document are
//! written to a recovery folder (`file.recovery.save`, called on a timer by the app); saving or
//! closing a document removes its file; after a crash `file.recovery.open` reopens what was left
//! there as unsaved documents that remember where they were saved.
//!
//! A recovery entry is `<uid>.designcraft` (the document) plus `<uid>.json` (`{"path", "title",
//! "saved"}`: the original file, the title, and when it was written).

#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{DocState, EngineError, Result, Session};

/// The platform's per-user recovery folder.
pub fn default_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Application Support/DesignCraft/Recovery"));
    }
    if cfg!(target_os = "windows") {
        return std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("DesignCraft").join("Recovery"));
    }
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| home.map(|h| h.join(".local/share"))).map(|d| d.join("designcraft/recovery"))
}

fn files(dir: &Path, uid: u64) -> (PathBuf, PathBuf) {
    (dir.join(format!("{uid}.designcraft")), dir.join(format!("{uid}.json")))
}

/// Remove a document's recovery entry (it was saved or closed).
pub fn discard(dir: &Path, uid: u64) {
    let (d, m) = files(dir, uid);
    let _ = std::fs::remove_file(d);
    let _ = std::fs::remove_file(m);
}

/// Write recovery entries for the dirty documents (and drop those of clean ones). Returns how
/// many were written.
pub fn save(s: &Session, dir: &Path) -> Result<usize> {
    secure_recovery_dir(dir)?;
    let mut n = 0;
    for d in &s.docs {
        if !d.is_dirty() {
            discard(dir, d.uid);
            continue;
        }
        let (doc, meta) = files(dir, d.uid);
        let bytes = crate::cmd::to_bytes(&d.doc);
        // Write then rename, so a crash mid-write never leaves a torn file.
        write_recovery_file(&doc, &bytes)?;
        let m = json!({"path": d.path, "title": d.title(), "saved": designcraft_doc::vars::now()});
        write_recovery_file(&meta, m.to_string().as_bytes())?;
        n += 1;
    }
    Ok(n)
}

#[cfg(not(target_arch = "wasm32"))]
fn secure_recovery_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| EngineError::Other(format!("{}: {e}", dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| EngineError::Other(format!("{}: {e}", dir.display())))?;
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn secure_recovery_dir(_dir: &Path) -> Result<()> {
    Err(EngineError::Other("recovery files are unavailable on the web".into()))
}

#[cfg(not(target_arch = "wasm32"))]
fn write_recovery_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(dir) = path.parent() else {
        return Err(EngineError::Other(format!("{} has no parent folder", path.display())));
    };
    let mut tmp = tempfile::Builder::new()
        .prefix(".designcraft-recovery-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?;
    }
    tmp.as_file_mut().write_all(bytes).map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?;
    tmp.as_file_mut().sync_all().map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?;
    tmp.persist(path).map_err(|e| EngineError::Other(format!("{}: {}", path.display(), e.error)))?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn write_recovery_file(_path: &Path, _bytes: &[u8]) -> Result<()> {
    Err(EngineError::Other("recovery files are unavailable on the web".into()))
}

/// Recovery entries in `dir`: (uid, metadata).
pub fn list(dir: &Path) -> Vec<(u64, Value)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("designcraft") {
            continue;
        }
        let Some(uid) = p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()) else { continue };
        // Metadata is a file on disk: anything but an object (corrupt, hand-edited) counts as none.
        let meta = std::fs::read_to_string(p.with_extension("json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .filter(Value::is_object)
            .unwrap_or(json!({}));
        out.push((uid, meta));
    }
    out.sort_by_key(|e| e.0);
    out
}

/// Reopen every recovery entry as an unsaved document (and remove the entries: the new session
/// writes its own). Returns the opened documents' indices.
pub fn open(s: &mut Session, dir: &Path) -> Result<Vec<usize>> {
    let mut opened = Vec::new();
    for (uid, meta) in list(dir) {
        let (doc, _) = files(dir, uid);
        let Ok(bytes) = std::fs::read(&doc) else { continue };
        let d = match crate::cmd::from_bytes(&bytes) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let path = meta.get("path").and_then(Value::as_str).map(str::to_string);
        let mut st = DocState::new(d, path);
        // Unsaved: the copy on disk (if any) is older than what was recovered.
        st.saved_doc = std::sync::Arc::new((*st.doc).clone());
        st.revision += 1;
        opened.push(s.add_document(st));
        discard(dir, uid);
    }
    Ok(opened)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_unsaved_documents() {
        let dir = std::env::temp_dir().join(format!("dc-recovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("file.new", &json!({})).unwrap();
        // Only the edited document is written.
        s.execute("frame.create", &json!({"rect": [10, 10, 100, 100]})).unwrap();
        assert_eq!(save(&s, &dir).unwrap(), 1);
        assert_eq!(list(&dir).len(), 1);
        // "Crash": a new session reopens it, unsaved, with the item.
        let mut s2 = Session::new();
        let opened = open(&mut s2, &dir).unwrap();
        assert_eq!(opened.len(), 1);
        let st = s2.doc().unwrap();
        assert!(st.is_dirty());
        assert_eq!(st.doc.spreads[0].items.len(), 1);
        assert!(list(&dir).is_empty(), "entries are consumed");
        // Saving a document drops its entry.
        s.recovery_dir = Some(dir.clone());
        assert_eq!(save(&s, &dir).unwrap(), 1);
        let path = dir.join("saved.designcraft");
        s.execute("file.saveAs", &json!({"path": path.to_string_lossy()})).unwrap();
        assert!(list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_data_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("recovery");
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect": [10, 10, 100, 100]})).unwrap();
        assert_eq!(save(&s, &dir).unwrap(), 1);

        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 2);
        for entry in entries {
            let entry = entry.unwrap();
            assert_eq!(entry.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    /// `file.recovery.list` adds `uid` to each entry's metadata: metadata that wasn't an object
    /// (a corrupt `.json` beside the recovery file) panicked on `m["uid"] = …`.
    #[test]
    fn corrupt_recovery_metadata_is_ignored() {
        let dir = std::env::temp_dir().join(format!("dc-recovery-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("7.designcraft"), b"not a document").unwrap();
        std::fs::write(dir.join("7.json"), b"[1, 2]").unwrap();
        let mut s = Session::new();
        s.recovery_dir = Some(dir.clone());
        let r = s.execute("file.recovery.list", &json!({})).unwrap();
        assert_eq!(r[0]["uid"], json!(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revert_and_save_a_copy() {
        let dir = std::env::temp_dir().join(format!("dc-revert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, copy) = (dir.join("a.designcraft"), dir.join("copy.designcraft"));
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        assert!(s.execute("file.revert", &json!({})).is_err(), "never saved");
        s.execute("file.saveAs", &json!({"path": a.to_string_lossy()})).unwrap();
        s.execute("frame.create", &json!({"rect": [10, 10, 100, 100]})).unwrap();
        // A copy keeps the document unsaved and on its own file.
        s.execute("file.saveACopy", &json!({"path": copy.to_string_lossy()})).unwrap();
        assert!(copy.exists());
        assert!(s.doc().unwrap().is_dirty());
        assert_eq!(s.doc().unwrap().path.as_deref(), Some(a.to_string_lossy().as_ref()));
        // Revert drops the frame.
        s.execute("file.revert", &json!({})).unwrap();
        assert!(s.doc().unwrap().doc.spreads[0].items.is_empty());
        assert!(!s.doc().unwrap().is_dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
