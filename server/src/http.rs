//! The read side over HTTP: the web view, and SQL for it and for agents.
//!
//! - `GET /health`: `ok`, for the orchestrator's check.
//! - `GET /api/index`: every project's runs, each with its manifest, its sites' files, when it last
//!   wrote a file (`last`) and its size on disk (`bytes`).
//! - `POST /api/sql?[project=<p>][&run=<id>][&format=text][&max_rows=<n>]`, the SQL as the body:
//!   its result as JSON (`columns`, `types`, `rows`, `truncated`, `ms`), or as an aligned text table.
//!   See [`crate::query`]: read-only, SELECT only, the views `queries`, and with a project `runs`
//!   and one per site. The `X-Prow-Source` header names who asks, for the query log (default `api`).
//! - `GET /api/flame?project=<p>&run=<id>&from=<µs>&to=<µs>`: the flame graph of the run's zones in
//!   that range, exact at any range (see [`crate::query::Engine::flame`]).
//! - anything else: the web app's files from `PROW_WEB`, `index.html` for `/`.
//!
//! One request a connection. Paths are checked to stay inside their root.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value as Json};

use crate::query::{Table, MAX_ROWS};
use crate::worker::Queries;

pub struct Roots {
    pub data: PathBuf,
    pub web: PathBuf,
    pub queries: Arc<Queries>,
}

/// The largest SQL body accepted.
const MAX_BODY: usize = 1 << 20;

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
    // of the headers, only a body's length and who asks matter here
    let mut length = 0usize;
    let mut source = String::from("api");
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0);
            } else if k.trim().eq_ignore_ascii_case("x-prow-source") {
                source = v.trim().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).take(32).collect();
            }
        }
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let head = method == "HEAD";
    let mut out = stream;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = decode(path);
    if method == "GET" && path == "/api/flame" {
        return flame(&mut out, &roots.queries, query);
    }
    if method == "POST" && path == "/api/sql" {
        if length > MAX_BODY {
            return respond(&mut out, 413, "text/plain", b"the SQL is longer than 1 MB\n", false);
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        return sql(&mut out, &roots.queries, &source, query, &String::from_utf8_lossy(&body));
    }
    if method != "GET" && !head {
        return respond(&mut out, 405, "text/plain", b"GET, HEAD, or POST /api/sql\n", head);
    }
    if path == "/health" {
        return respond(&mut out, 200, "text/plain", b"ok\n", head);
    }
    if path == "/api/index" {
        let body = serde_json::to_vec(&index(&roots.data)).map_err(io::Error::other)?;
        return respond(&mut out, 200, "application/json", &body, head);
    }
    let rel = if path == "/" { "index.html".to_string() } else { path.trim_start_matches('/').to_string() };
    let Some(file) = inside(&roots.web, &rel) else {
        return respond(&mut out, 404, "text/plain", b"not found\n", head);
    };
    match fs::read(&file) {
        Ok(body) => respond(&mut out, 200, content_type(&file), &body, head),
        Err(_) => respond(&mut out, 404, "text/plain", b"not found\n", head),
    }
}

/// `GET /api/flame?project=<p>&run=<id>&from=<µs>&to=<µs>`: the flame graph of the run's zones in
/// [from, to) (µs since the epoch), as nested frames; see [`Engine::flame`].
fn flame(out: &mut TcpStream, queries: &Queries, query: &str) -> io::Result<()> {
    let mut p = HashMap::new();
    for kv in query.split('&') {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        p.insert(k.to_string(), decode(v));
    }
    let num = |k: &str| p.get(k).and_then(|v| v.parse::<f64>().ok()).map(|v| v as i64);
    let result = match (p.get("project"), p.get("run"), num("from"), num("to")) {
        (Some(project), Some(run), Some(from), Some(to)) if to > from => queries.flame(project, run, from, to),
        _ => Err("project, run, from and to (µs since the epoch, from < to) are required".into()),
    };
    match result {
        Ok(a) => respond(out, 200, "application/json", &a.body, false),
        Err(e) => respond(out, 400, "application/json", &serde_json::to_vec(&json!({ "error": e })).map_err(io::Error::other)?, false),
    }
}

/// `POST /api/sql`: the query string names the project (and run, format, row limit), the body is the SQL.
fn sql(out: &mut TcpStream, queries: &Queries, source: &str, query: &str, sql: &str) -> io::Result<()> {
    let mut project = String::new();
    let mut run = None;
    let mut text = false;
    let mut max_rows = MAX_ROWS;
    for kv in query.split('&') {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let v = decode(&v.replace('+', " "));
        match k {
            "project" => project = v,
            "run" if !v.is_empty() => run = Some(v),
            "format" => text = v == "text",
            "max_rows" => max_rows = v.parse().unwrap_or(MAX_ROWS).min(MAX_ROWS),
            _ => {}
        }
    }
    match queries.sql(source, &project, run.as_deref(), sql, max_rows) {
        Ok(a) if text => {
            let t = Table::from_json(&serde_json::from_slice(&a.body).map_err(io::Error::other)?);
            respond(out, 200, "text/plain; charset=utf-8", t.to_text().as_bytes(), false)
        }
        Ok(a) => respond(out, 200, "application/json", &a.body, false),
        Err(e) if text => respond(out, 400, "text/plain; charset=utf-8", format!("{e}\n").as_bytes(), false),
        Err(e) => respond(out, 400, "application/json", &serde_json::to_vec(&json!({ "error": e })).map_err(io::Error::other)?, false),
    }
}

fn respond(out: &mut TcpStream, status: u16, ctype: &str, body: &[u8], head: bool) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
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

/// `{"projects": [{"name", "runs": [{"manifest", "sites": {site: [files]}, "last", "bytes"}]}]}`,
/// newest run first. `last`: the newest file's write time, so a run that stopped without `END`
/// (its producer's machine went down) shows when its data stopped.
fn index(data: &Path) -> Json {
    let mut projects = Vec::new();
    for p in dirs(data, "project=") {
        let name = p.file_name().unwrap().to_string_lossy()["project=".len()..].to_string();
        let mut runs = Vec::new();
        for r in dirs(&p, "run=") {
            let manifest: Map<String, Json> = fs::read(r.join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            let mut sites = Map::new();
            let (mut last, mut bytes) = (std::time::UNIX_EPOCH, 0u64);
            for s in dirs(&r, "") {
                let mut files: Vec<String> = fs::read_dir(&s)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".parquet") && !n.starts_with('.'))
                    .collect();
                files.sort();
                for f in &files {
                    if let Ok(m) = fs::metadata(s.join(f)) {
                        bytes += m.len();
                        last = last.max(m.modified().unwrap_or(std::time::UNIX_EPOCH));
                    }
                }
                let rel = |f: &String| format!("{}/{}/{}/{f}", p.file_name().unwrap().to_string_lossy(), r.file_name().unwrap().to_string_lossy(), s.file_name().unwrap().to_string_lossy());
                sites.insert(s.file_name().unwrap().to_string_lossy().into_owned(), files.iter().map(rel).collect::<Vec<_>>().into());
            }
            let last = last.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as i64;
            runs.push(json!({ "manifest": manifest, "sites": sites, "last": (last > 0).then(|| crate::iso(last)), "bytes": bytes }));
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
