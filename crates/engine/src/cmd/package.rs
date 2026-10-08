//! File › Package and Links › Copy Links To: gather a document with the files it uses.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use designcraft_doc::{AssetId, Document};
use serde_json::{Value, json};

use super::{CommandSpec, bad, cmd, has_doc, str_param};
use crate::{EngineError, Result, Session};

const MAX_OUTPUT_COMPONENT_BYTES: usize = 200;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            noundo "file.package",
            "Package…",
            ["File"],
            None,
            "{dir: folder to create, idml?: true, pdf?: false, instructions?: text} — the document (its placed files relinked to Links/), Links/, an IDML copy, an optional PDF and a report → {dir, files, report}",
            has_doc,
            package
        ),
        cmd!(
            "links.copyTo",
            "Copy Link(s) To…",
            [],
            None,
            "{dir, assets?: [asset ids] (default: all)} — write the placed files to `dir` and relink to the copies → {copied}",
            has_doc,
            copy_links
        ),
    ]
}

fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches([' ', '.']).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
        || stem.strip_prefix("LPT").is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
}

/// A portable single path component made from document metadata.
fn safe_component(name: &str, fallback: &str) -> String {
    let leaf = name.rsplit(['/', '\\']).find(|part| !part.is_empty()).unwrap_or("");
    let mut safe = String::new();
    for ch in leaf.trim().chars() {
        let replacement = ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\');
        let ch = if replacement { '_' } else { ch };
        if safe.len() + ch.len_utf8() > MAX_OUTPUT_COMPONENT_BYTES {
            break;
        }
        safe.push(ch);
    }
    safe.truncate(safe.trim_end_matches([' ', '.']).len());
    if safe.is_empty() || safe == "." || safe == ".." {
        safe = fallback.to_string();
    }
    if is_reserved_windows_name(&safe) {
        safe.insert(0, '_');
    }
    safe
}

fn package_output_path(dir: &Path, title: &str, extension: &str) -> Result<PathBuf> {
    let path = dir.join(format!("{title}.{extension}"));
    if path.parent() != Some(dir) {
        return Err(bad("file.package", "an output filename would be outside the selected directory"));
    }
    Ok(path)
}

/// A file name for asset `a` in a Links folder, unique among `taken`.
fn link_name(name: &str, id: AssetId, taken: &mut Vec<String>) -> String {
    let fallback = format!("asset-{}", id.0);
    let base = safe_component(name, &fallback);
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (base.clone(), String::new()),
    };
    let mut n = base;
    let mut k = 2;
    while taken.iter().any(|t| t.eq_ignore_ascii_case(&n)) {
        n = format!("{stem} {k}{ext}");
        k += 1;
    }
    taken.push(n.clone());
    n
}

/// Write each asset's bytes into `dir`; returns the relinked document and the paths written.
fn write_links(d: &Document, dir: &Path, only: Option<&[AssetId]>) -> Result<(Document, Vec<PathBuf>)> {
    std::fs::create_dir_all(dir).map_err(|e| EngineError::Other(format!("{}: {e}", dir.display())))?;
    let mut out = d.clone();
    let mut taken = Vec::new();
    let mut written = Vec::new();
    let mut ids: Vec<AssetId> = d.assets.keys().copied().collect();
    ids.sort_by_key(|i| i.0);
    for id in ids {
        if only.is_some_and(|o| !o.contains(&id)) {
            continue;
        }
        let a = &d.assets[&id];
        let name = link_name(a.link.as_deref().unwrap_or(&a.name), id, &mut taken);
        let path = dir.join(&name);
        std::fs::write(&path, a.data.as_slice()).map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?;
        let mut na = (**a).clone();
        na.link = Some(path.to_string_lossy().to_string());
        out.assets.insert(id, Arc::new(na));
        written.push(path);
    }
    Ok((out, written))
}

fn package(s: &mut Session, p: &Value) -> Result<Value> {
    let dir = PathBuf::from(str_param(p, "dir").ok_or_else(|| bad("file.package", "missing `dir`"))?);
    let st = s.doc()?;
    let display_title = st.doc.title.clone();
    let title = safe_component(&display_title, "Untitled");
    let fonts = s.execute("font.list", &json!({}))?;
    let links = s.execute("links.list", &json!({}))?;
    let pre = s.execute("preflight.run", &json!({})).unwrap_or(Value::Null);
    let d = s.doc()?.doc.clone();
    let (packed, mut files) = write_links(&d, &dir.join("Links"), None)?;
    let doc_path = package_output_path(&dir, &title, "designcraft")?;
    std::fs::write(&doc_path, super::to_bytes(&packed)).map_err(|e| EngineError::Other(format!("{}: {e}", doc_path.display())))?;
    files.insert(0, doc_path);
    if p.get("idml").and_then(Value::as_bool).unwrap_or(true) {
        let path = package_output_path(&dir, &title, "idml")?;
        std::fs::write(&path, designcraft_idml::export_idml(&packed)).map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?;
        files.push(path);
    }
    if p.get("pdf").and_then(Value::as_bool).unwrap_or(false) {
        let path = package_output_path(&dir, &title, "pdf")?;
        let bytes = designcraft_pdf::export_pdf(&packed, &s.cache, &Default::default()).map_err(|e| EngineError::Other(e.to_string()))?;
        std::fs::write(&path, bytes).map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?;
        files.push(path);
    }
    // The report: fonts (missing first), links, preflight, the user's instructions.
    let mut r = format!("Package report: {display_title}\n\n");
    if let Some(t) = str_param(p, "instructions").filter(|t| !t.trim().is_empty()) {
        r += &format!("Instructions\n{t}\n\n");
    }
    r += "Fonts\n";
    for f in fonts.as_array().into_iter().flatten() {
        let missing = f["missing"].as_bool().unwrap_or(false) || f["styleMissing"].as_bool().unwrap_or(false);
        r += &format!("  {} {}{}\n", f["family"].as_str().unwrap_or(""), f["style"].as_str().unwrap_or(""), if missing { " — MISSING" } else { "" });
    }
    r += "\nLinks\n";
    for l in links.as_array().into_iter().flatten() {
        r += &format!("  {} ({})\n", l["name"].as_str().unwrap_or(""), l["status"].as_str().unwrap_or(""));
    }
    if let Some(issues) = pre.get("issues").and_then(Value::as_array) {
        r += &format!("\nPreflight: {} issue(s)\n", issues.len());
        for i in issues {
            r += &format!("  {}\n", i.get("message").and_then(Value::as_str).unwrap_or(""));
        }
    }
    let report = dir.join("Instructions.txt");
    std::fs::write(&report, &r).map_err(|e| EngineError::Other(format!("{}: {e}", report.display())))?;
    files.push(report);
    Ok(json!({"dir": dir.to_string_lossy(), "files": files.iter().map(|f| f.to_string_lossy().to_string()).collect::<Vec<_>>(), "report": r}))
}

fn copy_links(s: &mut Session, p: &Value) -> Result<Value> {
    let dir = PathBuf::from(str_param(p, "dir").ok_or_else(|| bad("links.copyTo", "missing `dir`"))?);
    let only: Option<Vec<AssetId>> = p.get("assets").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(AssetId).collect());
    let d = s.doc()?.doc.clone();
    let (relinked, written) = write_links(&d, &dir, only.as_deref())?;
    s.edit(|doc, _| {
        doc.assets = relinked.assets;
        Ok(())
    })?;
    Ok(json!({"copied": written.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_writes_document_links_and_report() {
        let dir = std::env::temp_dir().join(format!("dc-package-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let svg = dir.join("logo.svg");
        std::fs::write(&svg, r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40"><rect width="40" height="40"/></svg>"##).unwrap();
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("file.place", &json!({"path": svg.to_string_lossy(), "x": 100, "y": 100})).unwrap();
        s.execute("frame.create", &json!({"rect": [72, 300, 400, 400], "content": "text", "text": "Hello"})).unwrap();
        let out = dir.join("Pkg");
        let r = s.execute("file.package", &json!({"dir": out.to_string_lossy(), "pdf": true, "instructions": "Print on matte."})).unwrap();
        let files: Vec<String> = r["files"].as_array().unwrap().iter().map(|f| f.as_str().unwrap().to_string()).collect();
        assert!(files.iter().any(|f| f.ends_with(".designcraft")));
        assert!(files.iter().any(|f| f.ends_with(".idml")));
        assert!(files.iter().any(|f| f.ends_with(".pdf")));
        assert!(out.join("Links").join("logo.svg").exists());
        let report = std::fs::read_to_string(out.join("Instructions.txt")).unwrap();
        assert!(report.contains("Print on matte.") && report.contains("logo.svg"), "{report}");
        assert!(report.contains("Preflight:"), "{report}");
        // The packaged document links to its Links folder.
        let doc_file = files.iter().find(|f| f.ends_with(".designcraft")).unwrap();
        let packed = super::super::from_bytes(&std::fs::read(doc_file).unwrap()).unwrap();
        let link = packed.assets.values().next().unwrap().link.clone().unwrap();
        assert!(link.contains("Links"), "{link}");
        // Copy Links To relinks the open document.
        let r = s.execute("links.copyTo", &json!({"dir": dir.join("Copies").to_string_lossy()})).unwrap();
        assert_eq!(r["copied"], 1);
        assert!(s.doc().unwrap().doc.assets.values().next().unwrap().link.as_deref().unwrap().contains("Copies"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_uses_portable_names_inside_the_selected_directory() {
        assert_eq!(safe_component("Archive/Annual: Review", "Untitled"), "Annual_ Review");
        assert_eq!(safe_component(".", "Untitled"), "Untitled");
        assert_eq!(safe_component("CON", "Untitled"), "_CON");

        let dir = std::env::temp_dir().join(format!("dc-package-safe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::new();
        s.execute("file.new", &json!({"title": "Archive/Annual: Review"})).unwrap();
        let r = s.execute("file.package", &json!({"dir": dir.to_string_lossy(), "idml": false})).unwrap();
        let files = r["files"].as_array().unwrap();
        assert!(files.iter().all(|path| Path::new(path.as_str().unwrap()).starts_with(&dir)));
        assert!(dir.join("Annual_ Review.designcraft").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
