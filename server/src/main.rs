//! prow-server: receives runs over the prow protocol and writes them as Parquet.
//!
//! ```text
//! $PROW_DATA/project=<project>/run=<run id>/manifest.json
//! $PROW_DATA/project=<project>/run=<run id>/<site>/part-00001.parquet   (while the run is live)
//! $PROW_DATA/project=<project>/run=<run id>/<site>/data.parquet         (after END)
//! ```
//!
//! Configuration, from the environment: `PROW_DATA` (default `/data`), `PROW_LISTEN` (default
//! `0.0.0.0:7380`), `PROW_TOKEN` (optional: when set, a `HELLO` must carry it), `PROW_HTTP`
//! (default `0.0.0.0:7381`: the web view and its files, see [`http`]), `PROW_WEB` (default
//! `/web`: the built web app).
//!
//! Rows are buffered and written every 5 s or 100,000 rows: a part for each site with rows, so a
//! reader sees a live run within seconds. A part is written to a hidden temporary file, synced
//! and renamed into place, so a reader never sees a partial file. Once every part of a flush is
//! on disk, the manifest records the flush's last `ROWS` sequence (`durable_seq`) and the server
//! sends `ACK` with it; the producer forgets those frames. A crash before that leaves parts newer
//! than `durable_seq`: reopening the run deletes them, the producer sends their frames again, and
//! frames at or below `durable_seq` are skipped. So a server crash neither loses nor repeats rows.
//!
//! `END` compacts each site's parts into `data.parquet` and marks the run complete. A run whose
//! connection ends without `END` is marked `disconnected`; the producer's reconnection (the same
//! run id) reopens it and its parts continue.

mod http;

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow_array::{
    ArrayRef, BooleanArray, Float32Array, Float64Array, Int64Array, RecordBatch, StringArray, TimestampNanosecondArray, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema as ArrowSchema, TimeUnit};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use prow::protocol::{Attr, Frame, Hello, Rows, Schema, Type, Wire, VERSION};
use serde_json::{Map, Value as Json};

const FLUSH_ROWS: usize = 100_000;
const FLUSH_AGE: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_secs(1);

struct Config {
    data: PathBuf,
    token: Option<String>,
}

/// Exit on SIGTERM and SIGINT. As a container's PID 1 the server would otherwise ignore both. Exiting
/// at once loses nothing: rows not yet in a part are unacknowledged, and their producers send them again.
fn exit_on_signals() {
    extern "C" fn quit(_: libc::c_int) {
        // SAFETY: _exit is async-signal-safe.
        unsafe { libc::_exit(0) }
    }
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: installs a handler that only calls _exit.
        unsafe { libc::signal(sig, quit as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    }
}

fn main() {
    exit_on_signals();
    let data = PathBuf::from(std::env::var("PROW_DATA").unwrap_or_else(|_| "/data".into()));
    let listen = std::env::var("PROW_LISTEN").unwrap_or_else(|_| "0.0.0.0:7380".into());
    let token = std::env::var("PROW_TOKEN").ok().filter(|t| !t.is_empty());
    if let Err(e) = fs::create_dir_all(&data) {
        eprintln!("prow-server: cannot create {}: {e}", data.display());
        std::process::exit(1);
    }
    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| {
        eprintln!("prow-server: cannot listen on {listen}: {e}");
        std::process::exit(1)
    });
    eprintln!(
        "prow-server: listening on {listen}, data in {}, {}",
        data.display(),
        if token.is_some() { "token required" } else { "no token" }
    );
    let http_listen = std::env::var("PROW_HTTP").unwrap_or_else(|_| "0.0.0.0:7381".into());
    let roots = http::Roots { data: data.clone(), web: PathBuf::from(std::env::var("PROW_WEB").unwrap_or_else(|_| "/web".into())) };
    std::thread::spawn(move || http::serve(&http_listen, roots));
    let cfg = Arc::new(Config { data, token });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let cfg = Arc::clone(&cfg);
        std::thread::spawn(move || {
            let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            if let Err(e) = serve(stream, &cfg) {
                eprintln!("prow-server: {peer}: {e}");
            }
        });
    }
}

/// One connection: frames arrive from a reader thread, so flushing on a timer never splits a frame.
fn serve(stream: TcpStream, cfg: &Config) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<io::Result<Option<Frame>>>();
    let mut reader = BufReader::new(stream.try_clone()?);
    std::thread::spawn(move || loop {
        let f = Frame::read_from(&mut reader);
        let last = !matches!(f, Ok(Some(_)));
        if tx.send(f).is_err() || last {
            break;
        }
    });
    let mut out = stream;

    let hello = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(Some(Frame::Hello(h)))) => h,
        Ok(Ok(Some(f))) => return refuse(&mut out, &format!("the first frame must be HELLO, not kind {}", f.kind())),
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => return Err(e),
        Err(_) => return refuse(&mut out, "no HELLO within 10 s"),
    };

    if hello.version != VERSION {
        return refuse(&mut out, &format!("protocol version {} (this server speaks {VERSION})", hello.version));
    }
    if let Some(t) = &cfg.token {
        if hello.token != *t {
            return refuse(&mut out, "wrong or missing token");
        }
    }
    if !valid_name(&hello.project) {
        return refuse(&mut out, "a project name uses a-z, 0-9, '_', '.' and '-'");
    }
    let mut run = RunDir::open(&cfg.data, &hello)?;
    Frame::Ok.write_to(&mut out)?;
    eprintln!("prow-server: run {} of {} open ({} connection)", run.id, hello.project, ordinal(run.connections));

    let mut strings: HashMap<u32, String> = HashMap::new();
    let mut sites: HashMap<u16, SiteBuf> = HashMap::new();
    let mut values = Vec::new();
    let result = loop {
        let frame = match rx.recv_timeout(TICK) {
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) => break Ok(false),
            Ok(Err(e)) => break Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if run.due(&sites) {
                    run.flush_all(&mut sites, Some(&mut out))?;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(false),
        };
        match frame {
            Frame::String { id, value } => {
                strings.insert(id, value);
            }
            Frame::Schema(s) => {
                if !valid_name(&s.name) || s.fields.iter().any(|(f, _)| !valid_name(f) || f == "ts") {
                    break Err(io::Error::other(format!("site '{}': invalid site or field name", s.name)));
                }
                let same = sites.get(&s.site).is_some_and(|b| b.schema == s);
                if !same {
                    if sites.get(&s.site).is_some_and(|b| b.rows() > 0) {
                        run.flush_all(&mut sites, Some(&mut out))?;
                    }
                    sites.insert(s.site, SiteBuf::new(s));
                }
            }
            Frame::Rows { seq, site, count, data } => {
                if seq <= run.durable_seq {
                    continue; // written before a reconnect; the producer had not seen the ACK
                }
                run.received_seq = run.received_seq.max(seq);
                let Some(buf) = sites.get_mut(&site) else {
                    break Err(io::Error::other(format!("rows of site {site} before its SCHEMA")));
                };
                let fields = buf.schema.fields.clone();
                let mut rows = Rows::new(&data, count, &fields);
                while let Some(ts) = rows.next_into(&mut values)? {
                    buf.push(ts, &values, &strings)?;
                }
            }
            Frame::End => {
                run.flush_all(&mut sites, None)?;
                run.complete()?;
                Frame::Ok.write_to(&mut out)?;
                break Ok(true);
            }
            other => break Err(io::Error::other(format!("unexpected frame kind {} from a producer", other.kind()))),
        }
        if run.due(&sites) {
            run.flush_all(&mut sites, Some(&mut out))?;
        }
    };
    let ended = matches!(result, Ok(true));
    if !ended {
        run.flush_all(&mut sites, None)?;
        run.disconnected()?;
    }
    eprintln!("prow-server: run {} {}", run.id, if ended { "complete" } else { "disconnected" });
    result.map(|_| ())
}

fn refuse(out: &mut TcpStream, reason: &str) -> io::Result<()> {
    let _ = Frame::Err(reason.into()).write_to(out);
    Err(io::Error::other(format!("refused: {reason}")))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-'))
}

fn ordinal(n: u64) -> String {
    match n {
        1 => "first".into(),
        2 => "second".into(),
        3 => "third".into(),
        n => format!("{n}th"),
    }
}

enum Col {
    I64(Vec<i64>),
    U64(Vec<u64>),
    F64(Vec<f64>),
    F32(Vec<f32>),
    Bool(Vec<bool>),
    Label(Vec<String>),
    Ts(Vec<i64>),
}

/// One site's rows since its last part.
struct SiteBuf {
    schema: Schema,
    ts: Vec<i64>,
    cols: Vec<Col>,
    since: Option<Instant>,
}

impl SiteBuf {
    fn new(schema: Schema) -> SiteBuf {
        let cols = schema
            .fields
            .iter()
            .map(|(_, t)| match t {
                Type::I64 => Col::I64(Vec::new()),
                Type::U64 => Col::U64(Vec::new()),
                Type::F64 => Col::F64(Vec::new()),
                Type::F32 => Col::F32(Vec::new()),
                Type::Bool => Col::Bool(Vec::new()),
                Type::Label => Col::Label(Vec::new()),
                Type::Ts => Col::Ts(Vec::new()),
            })
            .collect();
        SiteBuf { schema, ts: Vec::new(), cols, since: None }
    }

    fn rows(&self) -> usize {
        self.ts.len()
    }

    fn push(&mut self, ts: i64, values: &[Wire], strings: &HashMap<u32, String>) -> io::Result<()> {
        self.since.get_or_insert_with(Instant::now);
        self.ts.push(ts);
        for (c, v) in self.cols.iter_mut().zip(values) {
            match (c, *v) {
                (Col::I64(c), Wire::I64(x)) => c.push(x),
                (Col::U64(c), Wire::U64(x)) => c.push(x),
                (Col::F64(c), Wire::F64(x)) => c.push(x),
                (Col::F32(c), Wire::F32(x)) => c.push(x),
                (Col::Bool(c), Wire::Bool(x)) => c.push(x),
                (Col::Ts(c), Wire::Ts(x)) => c.push(x),
                (Col::Label(c), Wire::Label(id)) => {
                    let s = strings.get(&id).ok_or_else(|| io::Error::other(format!("label {id} before its STRING")))?;
                    c.push(s.clone());
                }
                _ => return Err(io::Error::other("a value of another type than its field")),
            }
        }
        Ok(())
    }

    fn batch(self) -> io::Result<RecordBatch> {
        let ts_type = DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()));
        let mut fields = vec![Field::new("ts", ts_type.clone(), false)];
        let mut arrays: Vec<ArrayRef> = vec![Arc::new(TimestampNanosecondArray::from(self.ts).with_timezone("UTC"))];
        for ((name, _), col) in self.schema.fields.iter().zip(self.cols) {
            let (dt, a): (DataType, ArrayRef) = match col {
                Col::I64(v) => (DataType::Int64, Arc::new(Int64Array::from(v))),
                Col::U64(v) => (DataType::UInt64, Arc::new(UInt64Array::from(v))),
                Col::F64(v) => (DataType::Float64, Arc::new(Float64Array::from(v))),
                Col::F32(v) => (DataType::Float32, Arc::new(Float32Array::from(v))),
                Col::Bool(v) => (DataType::Boolean, Arc::new(BooleanArray::from(v))),
                Col::Label(v) => (DataType::Utf8, Arc::new(StringArray::from(v))),
                Col::Ts(v) => (ts_type.clone(), Arc::new(TimestampNanosecondArray::from(v).with_timezone("UTC"))),
            };
            fields.push(Field::new(name, dt, false));
            arrays.push(a);
        }
        RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), arrays).map_err(io::Error::other)
    }
}

/// A run's directory and manifest.
struct RunDir {
    id: String,
    dir: PathBuf,
    manifest: Map<String, Json>,
    connections: u64,
    /// The next part number, by site name.
    parts: HashMap<String, u32>,
    /// Every `ROWS` frame up to this is on disk.
    durable_seq: u64,
    /// The last `ROWS` frame taken in.
    received_seq: u64,
}

/// Manifest keys of the server's own; an attribute with one of these names is stored as `attr.<name>`.
const RESERVED: [&str; 6] = ["project", "run", "state", "opened", "ended", "connections"];

impl RunDir {
    fn open(data: &Path, hello: &Hello) -> io::Result<RunDir> {
        let id: String = hello.run.iter().map(|b| format!("{b:02x}")).collect();
        let dir = data.join(format!("project={}", hello.project)).join(format!("run={id}"));
        fs::create_dir_all(&dir)?;
        let path = dir.join("manifest.json");
        let mut manifest = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Map<String, Json>>(&bytes).unwrap_or_default(),
            Err(_) => Map::new(),
        };
        let connections = manifest.get("connections").and_then(Json::as_u64).unwrap_or(0) + 1;
        if manifest.is_empty() {
            manifest.insert("project".into(), hello.project.clone().into());
            manifest.insert("run".into(), id.clone().into());
            manifest.insert("opened".into(), iso(now_ns()).into());
            for (k, v) in &hello.attrs {
                let key = if RESERVED.contains(&k.as_str()) { format!("attr.{k}") } else { k.clone() };
                manifest.insert(key, attr_json(v));
            }
        }
        manifest.insert("state".into(), "open".into());
        manifest.insert("connections".into(), connections.into());
        manifest.remove("ended");
        let durable_seq = manifest.get("durable_seq").and_then(Json::as_u64).unwrap_or(0);
        manifest.insert("durable_seq".into(), durable_seq.into());
        drop_parts_after(&dir, durable_seq)?;
        let run = RunDir { id, dir, manifest, connections, parts: HashMap::new(), durable_seq, received_seq: durable_seq };
        run.write_manifest()?;
        Ok(run)
    }

    /// A flush is due: rows waiting longer than 5 s, or 100,000 of them.
    fn due(&self, sites: &HashMap<u16, SiteBuf>) -> bool {
        let rows: usize = sites.values().map(SiteBuf::rows).sum();
        rows >= FLUSH_ROWS || sites.values().any(|b| b.since.is_some_and(|t| t.elapsed() >= FLUSH_AGE))
    }

    /// Write every site's rows as parts, then record and acknowledge the sequence they reach.
    fn flush_all(&mut self, sites: &mut HashMap<u16, SiteBuf>, ack: Option<&mut TcpStream>) -> io::Result<()> {
        if self.received_seq == self.durable_seq {
            return Ok(());
        }
        let seq = self.received_seq;
        for b in sites.values_mut() {
            let full = std::mem::replace(b, SiteBuf::new(b.schema.clone()));
            self.flush(full, seq)?;
        }
        self.durable_seq = seq;
        self.manifest.insert("durable_seq".into(), seq.into());
        self.write_manifest()?;
        if let Some(out) = ack {
            Frame::Ack { seq }.write_to(out)?;
        }
        Ok(())
    }

    fn write_manifest(&self) -> io::Result<()> {
        let tmp = self.dir.join(".manifest.json.tmp");
        let mut f = fs::File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut f, &self.manifest).map_err(io::Error::other)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(tmp, self.dir.join("manifest.json"))?;
        fs::File::open(&self.dir)?.sync_all()
    }

    /// Write a site's buffered rows as its next part, marked with the flush's sequence.
    fn flush(&mut self, buf: SiteBuf, seq: u64) -> io::Result<()> {
        if buf.rows() == 0 {
            return Ok(());
        }
        let site_dir = self.dir.join(&buf.schema.name);
        fs::create_dir_all(&site_dir)?;
        let n = match self.parts.get(&buf.schema.name) {
            Some(n) => *n,
            None => next_part(&site_dir)?,
        };
        self.parts.insert(buf.schema.name.clone(), n + 1);
        let batch = buf.batch()?;
        write_parquet(&site_dir.join(format!("part-{n:05}.parquet")), &[batch], seq)
    }

    fn complete(&mut self) -> io::Result<()> {
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Err(e) = compact(&entry.path()) {
                    eprintln!("prow-server: {}: parts left as they are: {e}", entry.path().display());
                }
            }
        }
        self.manifest.insert("state".into(), "complete".into());
        self.manifest.insert("ended".into(), iso(now_ns()).into());
        self.write_manifest()
    }

    fn disconnected(&mut self) -> io::Result<()> {
        self.manifest.insert("state".into(), "disconnected".into());
        self.manifest.insert("ended".into(), iso(now_ns()).into());
        self.write_manifest()
    }
}

fn next_part(site_dir: &Path) -> io::Result<u32> {
    let mut next = 1;
    for entry in fs::read_dir(site_dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if let Some(n) = name.strip_prefix("part-").and_then(|s| s.strip_suffix(".parquet")).and_then(|s| s.parse::<u32>().ok()) {
            next = next.max(n + 1);
        }
    }
    Ok(next)
}

/// Parquet key-value metadata: the last `ROWS` sequence a file's rows come from.
const SEQ_KEY: &str = "prow.seq";

/// Write `batches` to `path` through a synced temporary file and a rename.
fn write_parquet(path: &Path, batches: &[RecordBatch], seq: u64) -> io::Result<()> {
    let tmp = path.with_file_name(format!(".{}.tmp", path.file_name().unwrap().to_string_lossy()));
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_key_value_metadata(Some(vec![KeyValue::new(SEQ_KEY.into(), seq.to_string())]))
        .build();
    let file = fs::File::create(&tmp)?;
    let mut w = ArrowWriter::try_new(file, batches[0].schema(), Some(props)).map_err(io::Error::other)?;
    for b in batches {
        w.write(b).map_err(io::Error::other)?;
    }
    w.into_inner().map_err(io::Error::other)?.sync_all()?;
    fs::rename(tmp, path)?;
    fs::File::open(path.parent().unwrap())?.sync_all()
}

/// A Parquet file's `prow.seq`.
fn file_seq(path: &Path) -> io::Result<u64> {
    let b = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(path)?).map_err(io::Error::other)?;
    let kv = b.metadata().file_metadata().key_value_metadata();
    Ok(kv.and_then(|kv| kv.iter().find(|k| k.key == SEQ_KEY)).and_then(|k| k.value.as_deref()).and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Delete the parts of a flush that never reached the manifest: the producer sends their rows again.
fn drop_parts_after(run_dir: &Path, durable_seq: u64) -> io::Result<()> {
    for site in fs::read_dir(run_dir)? {
        let site = site?;
        if !site.file_type()?.is_dir() {
            continue;
        }
        for f in fs::read_dir(site.path())? {
            let p = f?.path();
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.starts_with(".") && name.ends_with(".tmp") {
                fs::remove_file(&p)?;
            } else if name.starts_with("part-") && name.ends_with(".parquet") && file_seq(&p)? > durable_seq {
                eprintln!("prow-server: {}: newer than the run's durable sequence {durable_seq}; removed", p.display());
                fs::remove_file(&p)?;
            }
        }
    }
    Ok(())
}

/// A site's parts (and an earlier `data.parquet`) into one `data.parquet`, a batch at a time, so a
/// long run compacts in the memory of one batch.
fn compact(site_dir: &Path) -> io::Result<()> {
    let data = site_dir.join("data.parquet");
    let mut parts: Vec<PathBuf> = fs::read_dir(site_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            n.starts_with("part-") && n.ends_with(".parquet")
        })
        .collect();
    parts.sort();
    if parts.is_empty() {
        return Ok(());
    }
    if !data.exists() && parts.len() == 1 {
        return fs::rename(&parts[0], &data);
    }
    let inputs: Vec<&PathBuf> = std::iter::once(&data).filter(|d| d.exists()).chain(parts.iter()).collect();
    let mut seq = 0;
    let mut schema = None;
    for p in &inputs {
        seq = seq.max(file_seq(p)?);
        let s = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(p)?).map_err(io::Error::other)?.schema().clone();
        if schema.get_or_insert_with(|| s.clone()) != &s {
            return Err(io::Error::other("the parts have different schemas"));
        }
    }
    let tmp = data.with_file_name(".data.parquet.tmp");
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_key_value_metadata(Some(vec![KeyValue::new(SEQ_KEY.into(), seq.to_string())]))
        .build();
    let mut w = ArrowWriter::try_new(fs::File::create(&tmp)?, schema.unwrap(), Some(props)).map_err(io::Error::other)?;
    for p in &inputs {
        let reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(p)?).map_err(io::Error::other)?.build().map_err(io::Error::other)?;
        for b in reader {
            w.write(&b.map_err(io::Error::other)?).map_err(io::Error::other)?;
        }
    }
    w.into_inner().map_err(io::Error::other)?.sync_all()?;
    fs::rename(&tmp, &data)?;
    fs::File::open(site_dir)?.sync_all()?;
    for p in parts {
        fs::remove_file(p)?;
    }
    Ok(())
}

fn attr_json(v: &Attr) -> Json {
    match v {
        Attr::I64(x) => (*x).into(),
        Attr::U64(x) => (*x).into(),
        Attr::F64(x) => serde_json::Number::from_f64(*x).map(Json::Number).unwrap_or(Json::Null),
        Attr::Bool(x) => (*x).into(),
        Attr::Label(s) => s.clone().into(),
        Attr::Ts(ns) => iso(*ns).into(),
    }
}

fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

/// UTC, ISO 8601 to the nanosecond: `2026-10-18T17:37:48.123456789Z`.
fn iso(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let frac = ns.rem_euclid(1_000_000_000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:09}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (H. Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_to_the_nanosecond() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(iso(1_792_242_425_000_042_999), "2026-10-17T13:07:05.000042999Z");
    }

    #[test]
    fn parts_past_the_durable_sequence_are_dropped() {
        let dir = std::env::temp_dir().join(format!("prow-test-{}", std::process::id()));
        let site = dir.join("step");
        fs::create_dir_all(&site).unwrap();
        let schema = Schema { site: 1, name: "step".into(), category: "c".into(), fields: vec![("ms".into(), Type::F64)] };
        for (n, seq) in [(1, 4u64), (2, 9)] {
            let mut b = SiteBuf::new(schema.clone());
            b.push(1, &[Wire::F64(0.5)], &HashMap::new()).unwrap();
            write_parquet(&site.join(format!("part-{n:05}.parquet")), &[b.batch().unwrap()], seq).unwrap();
        }
        fs::write(site.join(".part-00003.parquet.tmp"), b"partial").unwrap();
        assert_eq!(file_seq(&site.join("part-00002.parquet")).unwrap(), 9);
        drop_parts_after(&dir, 5).unwrap();
        let mut left: Vec<String> = fs::read_dir(&site).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["part-00001.parquet"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names() {
        assert!(valid_name("step.prefill-2"));
        assert!(!valid_name("Step") && !valid_name("a/b") && !valid_name(""));
    }
}
