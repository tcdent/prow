//! The read side over HTTP, for the web view: no query engine, only files.
//!
//! - `GET /health`: `ok`, for the orchestrator's check.
//! - `GET /api/index`: every project's runs, each with its manifest and its sites' files.
//! - `GET /data/<project=..>/<run=..>/<site>/<file>.parquet` and `.../manifest.json`: the files.
//! - anything else: the web app's files from `PROW_WEB`, `index.html` for `/`.
//!
//! One request a connection, `GET` and `HEAD` only. Paths are checked to stay inside their root.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value as Json};

pub struct Roots {
    pub data: PathBuf,
    pub web: PathBuf,
}

pub fn serve(listen: &str, roots: Roots) {
    let listener = match TcpListener::bind(listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("prow-server: cannot serve HTTP on {listen}: {e}");
            return;
        }
    };
    eprintln!("prow-server: web view on http://{listen}, app from {}", roots.web.display());
    let roots = Arc::new(roots);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let roots = Arc::clone(&roots);
        std::thread::spawn(move || {
            let _ = handle(stream, &roots);
        });
    }
}

fn handle(stream: TcpStream, roots: &Roots) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    // the headers carry nothing this server needs; read past them
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let head = method == "HEAD";
    let mut out = stream;
    if method != "GET" && !head {
        return respond(&mut out, 405, "text/plain", b"GET or HEAD only\n", head);
    }
    let path = decode(target.split('?').next().unwrap_or("/"));
    if path == "/health" {
        return respond(&mut out, 200, "text/plain", b"ok\n", head);
    }
    if path == "/api/index" {
        let body = serde_json::to_vec(&index(&roots.data)).map_err(io::Error::other)?;
        return respond(&mut out, 200, "application/json", &body, head);
    }
    let (root, rel) = match path.strip_prefix("/data/") {
        Some(rel) => (&roots.data, rel.to_string()),
        None if path == "/" => (&roots.web, "index.html".to_string()),
        None => (&roots.web, path.trim_start_matches('/').to_string()),
    };
    let Some(file) = inside(root, &rel) else {
        return respond(&mut out, 404, "text/plain", b"not found\n", head);
    };
    match fs::read(&file) {
        Ok(body) => respond(&mut out, 200, content_type(&file), &body, head),
        Err(_) => respond(&mut out, 404, "text/plain", b"not found\n", head),
    }
}

fn respond(out: &mut TcpStream, status: u16, ctype: &str, body: &[u8], head: bool) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if !head {
        out.write_all(body)?;
    }
    out.flush()
}

/// `rel` under `root`, if it names a file there: no `..`, no absolute parts.
fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let p = root.join(rel);
    p.is_file().then_some(p)
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn content_type(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// `{"projects": [{"name", "runs": [{"manifest", "sites": {site: [files]}}]}]}`, newest run first.
fn index(data: &Path) -> Json {
    let mut projects = Vec::new();
    for p in dirs(data, "project=") {
        let name = p.file_name().unwrap().to_string_lossy()["project=".len()..].to_string();
        let mut runs = Vec::new();
        for r in dirs(&p, "run=") {
            let manifest: Map<String, Json> = fs::read(r.join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            let mut sites = Map::new();
            for s in dirs(&r, "") {
                let mut files: Vec<String> = fs::read_dir(&s)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".parquet") && !n.starts_with('.'))
                    .collect();
                files.sort();
                let rel = |f: &String| format!("{}/{}/{}/{f}", p.file_name().unwrap().to_string_lossy(), r.file_name().unwrap().to_string_lossy(), s.file_name().unwrap().to_string_lossy());
                sites.insert(s.file_name().unwrap().to_string_lossy().into_owned(), files.iter().map(rel).collect::<Vec<_>>().into());
            }
            runs.push(json!({ "manifest": manifest, "sites": sites }));
        }
        runs.sort_by(|a, b| b["manifest"]["opened"].as_str().cmp(&a["manifest"]["opened"].as_str()));
        projects.push(json!({ "name": name, "runs": runs }));
    }
    json!({ "projects": projects })
}

fn dirs(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix) && !n.to_string_lossy().starts_with('.')))
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside() {
        let root = std::env::temp_dir();
        assert!(inside(&root, "../etc/passwd").is_none());
        assert!(inside(&root, "/etc/passwd").is_none());
        assert_eq!(decode("/data/project%3Ddemo/a%20b"), "/data/project=demo/a b");
    }
}
