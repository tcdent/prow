//! SQL over the data tree: DuckDB embedded in the server, the one query path for the web view and
//! for agents (`POST /api/sql`, `bin/prow-sql`).
//!
//! - **UTC.** With the ICU extension loaded (`PROW_EXTENSIONS`), `TIMESTAMPTZ` arithmetic,
//!   `date_trunc` and `AT TIME ZONE` work; the session's time zone is UTC wherever the server runs.
//! - **Read-only, inside `PROW_DATA`.** The database is in memory. At open, file access is limited
//!   to the data directory, extensions are neither installed nor loaded, and the configuration is
//!   locked. Every request's SQL must parse as `SELECT` statements only (DuckDB's own parser,
//!   through `json_serialize_sql`), so `COPY ... TO`, `ATTACH` and DDL are refused even inside the
//!   data directory.
//! - **Views:** `queries` (the server's query log: `ts`, `source`, `project`, `run`, `sql`, `ms`,
//!   `rows`, `truncated`, `error`), and with a project named, `runs` (one row per run: its
//!   manifest) and one view per site (`step`, `zones`, ...) with `ts`, `run` and the site's fields.
//!   With a run named, the views cover that run's files only; without, every run of the project
//!   (fields added in later runs read NULL in earlier ones).
//! - **A time limit:** a query running past [`TIME_LIMIT`] is interrupted.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use duckdb::types::{TimeUnit, Value};
use duckdb::Connection;
use serde_json::{json, Map, Value as Json};

/// Rows a query returns at most; past it the result is cut and says so.
pub const MAX_ROWS: usize = 200_000;

/// How long a query may run.
pub const TIME_LIMIT: Duration = Duration::from_secs(60);

pub struct Engine {
    base: Mutex<Connection>,
    data: PathBuf,
}

/// A query's result.
pub struct Table {
    pub columns: Vec<String>,
    pub types: Vec<String>,
    pub rows: Vec<Vec<Json>>,
    pub truncated: bool,
    pub ms: f64,
}

impl Engine {
    /// `extensions`: a directory of DuckDB extensions (`*.duckdb_extension`, built for this DuckDB's
    /// version and platform) loaded before the lock-down; DuckDB checks their signatures. ICU is the
    /// one we need: TIMESTAMPTZ arithmetic, `date_trunc`, `AT TIME ZONE`.
    pub fn open(data: &Path, memory: &str, threads: usize, extensions: &Path) -> Result<Engine, String> {
        let data = fs::canonicalize(data).map_err(|e| format!("{}: {e}", data.display()))?;
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        let mut found: Vec<PathBuf> = fs::read_dir(extensions)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "duckdb_extension"))
            .collect();
        found.sort();
        for ext in found {
            let path = ext.to_string_lossy().replace('\'', "''");
            conn.execute_batch(&format!("LOAD '{path}';")).map_err(|e| format!("loading {}: {e}", ext.display()))?;
            eprintln!("prow-server: loaded {}", ext.display());
        }
        let dir = data.to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!(
            "SET GLOBAL TimeZone = 'UTC';
             SET memory_limit = '{memory}';
             SET threads = {threads};
             SET parquet_metadata_cache = true;
             SET autoinstall_known_extensions = false;
             SET autoload_known_extensions = false;
             SET allowed_directories = ['{dir}/'];
             SET enable_external_access = false;
             SET lock_configuration = true;"
        ))
        .map_err(|e| format!("configuring DuckDB: {e}"))?;
        Ok(Engine { base: Mutex::new(conn), data })
    }

    /// A connection with the views in place: the query log's, and `project`'s (and `run`'s, when
    /// given) unless `project` is empty.
    fn connect(&self, project: &str, run: Option<&str>) -> Result<Connection, String> {
        let conn = self.base.lock().unwrap_or_else(|e| e.into_inner()).try_clone().map_err(|e| e.to_string())?;
        let log = self.data.join("queries.jsonl").to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!(
            "CREATE OR REPLACE TEMP VIEW queries AS SELECT * FROM read_json('{log}', format = 'newline_delimited', ignore_errors = true,
               columns = {{ts: 'TIMESTAMPTZ', source: 'VARCHAR', project: 'VARCHAR', run: 'VARCHAR', sql: 'VARCHAR', ms: 'DOUBLE', rows: 'BIGINT', truncated: 'BOOLEAN', error: 'VARCHAR'}});"
        ))
        .map_err(|e| e.to_string())?;
        if project.is_empty() && run.is_none() {
            return Ok(conn);
        }
        if !name_ok(project) || run.is_some_and(|r| !r.bytes().all(|c| c.is_ascii_hexdigit())) {
            return Err("a project uses a-z, 0-9, '_', '.' and '-'; a run is its hex id".into());
        }
        let root = self.data.join(format!("project={project}"));
        if !root.is_dir() {
            return Err(format!("no project '{project}'"));
        }
        let runs = match run {
            Some(r) => {
                let d = root.join(format!("run={r}"));
                if !d.is_dir() {
                    return Err(format!("no run '{r}' in '{project}'"));
                }
                format!("run={r}")
            }
            None => "run=*".into(),
        };
        let root_s = root.to_string_lossy().replace('\'', "''");
        let mut views = format!(
            "CREATE OR REPLACE TEMP VIEW runs AS SELECT * FROM read_json('{root_s}/{runs}/manifest.json', union_by_name = true, format = 'auto');"
        );
        for site in sites(&root, run) {
            views.push_str(&format!(
                " CREATE OR REPLACE TEMP VIEW \"{site}\" AS SELECT * EXCLUDE (project) FROM read_parquet('{root_s}/{runs}/{site}/*.parquet', hive_partitioning = true, union_by_name = true);"
            ));
        }
        conn.execute_batch(&views).map_err(|e| e.to_string())?;
        Ok(conn)
    }

    /// Run `sql` in `project` (and `run`, when given) with the views in place.
    pub fn query(&self, project: &str, run: Option<&str>, sql: &str, max_rows: usize) -> Result<Table, String> {
        let conn = self.connect(project, run)?;
        select_only(&conn, sql)?;
        let limit = Limit::arm(&conn);
        limit.check(run_query(&conn, sql, max_rows))
    }
}

fn run_query(conn: &Connection, sql: &str, max_rows: usize) -> Result<Table, String> {
    let t0 = Instant::now();
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let s = rows.as_ref().ok_or("no statement")?;
    let n = s.column_count();
    let columns = s.column_names();
    let types = (0..n).map(|i| format!("{}", s.column_type(i))).collect();
    let utc: Vec<bool> = (0..n).map(|i| matches!(s.column_type(i), duckdb::arrow::datatypes::DataType::Timestamp(_, Some(_)))).collect();
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if out.len() == max_rows {
            truncated = true;
            break;
        }
        let mut r = Vec::with_capacity(n);
        for i in 0..n {
            r.push(json_of(row.get::<_, Value>(i).map_err(|e| e.to_string())?, utc[i]));
        }
        out.push(r);
    }
    Ok(Table { columns, types, rows: out, truncated, ms: t0.elapsed().as_secs_f64() * 1e3 })
}

/// A frame of a flame graph: its name, its time inside the view (µs), its zones, its frames.
pub struct Frame {
    pub name: String,
    pub total: f64,
    pub count: u64,
    children: HashMap<String, usize>,
}

impl Engine {
    /// The flame graph of a run's zones in [from, to) (µs since the epoch): every zone's stack, by
    /// containment on its lane, merged by path; each frame's time clipped to the range. Exact at any
    /// range, however many zones: DuckDB streams them in order, one pass walks the stacks.
    pub fn flame(&self, project: &str, run: &str, from: i64, to: i64) -> Result<(Vec<Frame>, f64), String> {
        let conn = self.connect(project, Some(run))?;
        let limit = Limit::arm(&conn);
        limit.check(walk(&conn, from, to))
    }
}

fn walk(conn: &Connection, from: i64, to: i64) -> Result<(Vec<Frame>, f64), String> {
    let t0 = Instant::now();
    let longest: i64 = conn
        .query_row("SELECT (coalesce(max(dur_ns), 0) // 1000)::BIGINT FROM zones", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let at = |us: i64| format!("to_timestamp({})", us as f64 / 1e6);
    let sql = format!(
        "SELECT lane::VARCHAR AS lane, name::VARCHAR AS name, epoch_us(ts)::BIGINT AS s, (epoch_us(ts) + (dur_ns // 1000)::BIGINT)::BIGINT AS e FROM zones
         WHERE ts >= {} AND ts < {} ORDER BY lane, s, e DESC",
        at(from - longest - 1),
        at(to)
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let arrow = stmt.query_arrow([]).map_err(|e| e.to_string())?;

    // frames[0] is the root ("all lanes"); a lane's frame is the root's child
    let mut frames = vec![Frame { name: "all".into(), total: 0.0, count: 0, children: HashMap::new() }];
    let child = |frames: &mut Vec<Frame>, parent: usize, name: &str| -> usize {
        if let Some(&i) = frames[parent].children.get(name) {
            return i;
        }
        frames.push(Frame { name: name.to_string(), total: 0.0, count: 0, children: HashMap::new() });
        let i = frames.len() - 1;
        frames[parent].children.insert(name.to_string(), i);
        i
    };
    let mut lane = String::new();
    let mut lane_frame = 0usize;
    let mut stack: Vec<(i64, usize)> = Vec::new(); // (end, frame)
    for batch in arrow {
        use duckdb::arrow::array::{Array, AsArray};
        use duckdb::arrow::datatypes::Int64Type;
        let lanes = batch.column(0).as_string::<i32>();
        let names = batch.column(1).as_string::<i32>();
        let starts = batch.column(2).as_primitive::<Int64Type>();
        let ends = batch.column(3).as_primitive::<Int64Type>();
        for i in 0..batch.num_rows() {
            if lanes.is_null(i) || names.is_null(i) {
                continue;
            }
            let (l, n, s, e) = (lanes.value(i), names.value(i), starts.value(i), ends.value(i));
            if l != lane {
                lane = l.to_string();
                lane_frame = child(&mut frames, 0, l);
                stack.clear();
            }
            while stack.last().is_some_and(|&(end, _)| end <= s) {
                stack.pop();
            }
            let parent = stack.last().map_or(lane_frame, |&(_, f)| f);
            let f = child(&mut frames, parent, n);
            let clipped = (e.min(to) - s.max(from)).max(0) as f64;
            if clipped > 0.0 {
                frames[f].total += clipped;
                frames[f].count += 1;
                if parent == lane_frame {
                    frames[lane_frame].total += clipped;
                }
            }
            stack.push((e, f));
        }
    }
    frames[0].total = frames[0].children.values().map(|&i| frames[i].total).sum();
    Ok((frames, t0.elapsed().as_secs_f64() * 1e3))
}

/// [`TIME_LIMIT`] on a connection's query: past it, the query is interrupted.
struct Limit {
    _stop: mpsc::Sender<()>,
    fired: Arc<AtomicBool>,
}

impl Limit {
    fn arm(conn: &Connection) -> Limit {
        let handle = conn.interrupt_handle();
        let (stop, wait) = mpsc::channel::<()>();
        let fired = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&fired);
        // ends when the limit is dropped (the sender with it) or the time is up
        std::thread::spawn(move || {
            if wait.recv_timeout(TIME_LIMIT) == Err(mpsc::RecvTimeoutError::Timeout) {
                f.store(true, Ordering::Relaxed);
                handle.interrupt();
            }
        });
        Limit { _stop: stop, fired }
    }

    fn check<T>(&self, r: Result<T, String>) -> Result<T, String> {
        match r {
            Err(_) if self.fired.load(Ordering::Relaxed) => Err(format!("stopped at the {} s limit", TIME_LIMIT.as_secs())),
            r => r,
        }
    }
}

/// A flame graph as nested JSON: `{name, total, count, children: [...]}`, frames without time left out.
pub fn flame_json(frames: &[Frame], i: usize) -> Json {
    let f = &frames[i];
    let mut kids: Vec<&usize> = f.children.values().filter(|&&c| frames[c].total > 0.0).collect();
    kids.sort_by(|a, b| frames[**a].name.cmp(&frames[**b].name));
    json!({ "name": f.name, "total": f.total, "count": f.count, "children": kids.into_iter().map(|&c| flame_json(frames, c)).collect::<Vec<_>>() })
}

fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-'))
}

/// Refuse anything but SELECT statements, by DuckDB's own parser.
fn select_only(conn: &Connection, sql: &str) -> Result<(), String> {
    let parsed: String = conn
        .query_row("SELECT json_serialize_sql(?::VARCHAR)", [sql], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let v: Json = serde_json::from_str(&parsed).map_err(|e| e.to_string())?;
    if v.get("error").and_then(Json::as_bool) == Some(true) {
        let msg = v.get("error_message").and_then(Json::as_str).unwrap_or("not a SELECT");
        return Err(format!("only SELECT statements are run here: {msg}"));
    }
    Ok(())
}

/// The sites with files: under one run, or under every run of the project.
fn sites(root: &Path, run: Option<&str>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let runs: Vec<PathBuf> = match run {
        Some(r) => vec![root.join(format!("run={r}"))],
        None => fs::read_dir(root).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
    };
    for r in runs {
        for e in fs::read_dir(r).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let has_parquet = fs::read_dir(&p).into_iter().flatten().flatten().any(|f| {
                let n = f.file_name();
                let n = n.to_string_lossy();
                n.ends_with(".parquet") && !n.starts_with('.')
            });
            if p.is_dir() && name_ok(&name) && has_parquet {
                out.insert(name);
            }
        }
    }
    out
}

/// Integers past 2^53 as strings: JSON readers in browsers would round them.
fn int(v: i128) -> Json {
    if v.unsigned_abs() <= (1u128 << 53) {
        json!(v as i64)
    } else {
        json!(v.to_string())
    }
}

/// A value as JSON; `utc`: a timestamp is TIMESTAMPTZ (shown with `Z`), else wall time (without).
fn json_of(v: Value, utc: bool) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Boolean(b) => json!(b),
        Value::TinyInt(x) => json!(x),
        Value::SmallInt(x) => json!(x),
        Value::Int(x) => json!(x),
        Value::BigInt(x) => int(x as i128),
        Value::HugeInt(x) => int(x),
        Value::UHugeInt(x) => if x <= (1u128 << 53) { json!(x as u64) } else { json!(x.to_string()) },
        Value::UTinyInt(x) => json!(x),
        Value::USmallInt(x) => json!(x),
        Value::UInt(x) => json!(x),
        Value::UBigInt(x) => int(x as i128),
        Value::Float(x) => float(x as f64),
        Value::Double(x) => float(x),
        Value::Decimal(d) => d.to_string().parse::<f64>().map(float).unwrap_or(Json::Null),
        Value::Timestamp(unit, x) => {
            let iso = crate::iso(match unit {
                TimeUnit::Second => x.saturating_mul(1_000_000_000),
                TimeUnit::Millisecond => x.saturating_mul(1_000_000),
                TimeUnit::Microsecond => x.saturating_mul(1_000),
                TimeUnit::Nanosecond => x,
            });
            json!(if utc { iso } else { iso.trim_end_matches('Z').to_string() })
        }
        Value::Text(s) | Value::Enum(s) => json!(s),
        Value::Blob(b) | Value::Geometry(b) => json!(b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        Value::Date32(d) => json!(crate::iso(d as i64 * 86_400_000_000_000)[..10].to_string()),
        Value::Time64(unit, x) => json!(format!("{x} {unit:?}")),
        Value::Interval { months, days, nanos } => json!(interval(months, days, nanos)),
        Value::List(xs) | Value::Array(xs) => Json::Array(xs.into_iter().map(|x| json_of(x, utc)).collect()),
        Value::Struct(m) => Json::Object(m.iter().map(|(k, v)| (k.clone(), json_of(v.clone(), utc))).collect::<Map<_, _>>()),
        Value::Map(m) => Json::Array(m.iter().map(|(k, v)| json!([json_of(k.clone(), utc), json_of(v.clone(), utc)])).collect()),
        Value::Union(v) => json_of(*v, utc),
        other => json!(format!("{other:?}")),
    }
}

/// An interval as DuckDB prints it: `1 month 2 days 03:04:05.678901`, the parts that are there.
fn interval(months: i32, days: i32, nanos: i64) -> String {
    let mut parts = Vec::new();
    let (y, mo) = (months / 12, months % 12);
    if y != 0 {
        parts.push(format!("{y} year{}", if y.abs() == 1 { "" } else { "s" }));
    }
    if mo != 0 {
        parts.push(format!("{mo} month{}", if mo.abs() == 1 { "" } else { "s" }));
    }
    if days != 0 {
        parts.push(format!("{days} day{}", if days.abs() == 1 { "" } else { "s" }));
    }
    if nanos != 0 || parts.is_empty() {
        let sign = if nanos < 0 { "-" } else { "" };
        let a = nanos.unsigned_abs();
        let (h, m, sec, us) = (a / 3_600_000_000_000, a / 60_000_000_000 % 60, a / 1_000_000_000 % 60, a / 1000 % 1_000_000);
        parts.push(if us > 0 { format!("{sign}{h:02}:{m:02}:{sec:02}.{us:06}") } else { format!("{sign}{h:02}:{m:02}:{sec:02}") });
    }
    parts.join(" ")
}

fn float(x: f64) -> Json {
    if x.is_finite() {
        json!(x)
    } else {
        json!(if x.is_nan() { "NaN" } else if x > 0.0 { "Infinity" } else { "-Infinity" })
    }
}

impl Table {
    /// A table from [`Table::to_json`]'s form.
    pub fn from_json(v: &Json) -> Table {
        let strings = |k: &str| v[k].as_array().map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect()).unwrap_or_default();
        Table {
            columns: strings("columns"),
            types: strings("types"),
            rows: v["rows"].as_array().map(|rs| rs.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect()).unwrap_or_default(),
            truncated: v["truncated"].as_bool().unwrap_or(false),
            ms: v["ms"].as_f64().unwrap_or(0.0),
        }
    }

    pub fn to_json(&self) -> Json {
        json!({ "columns": self.columns, "types": self.types, "rows": self.rows, "truncated": self.truncated, "ms": self.ms })
    }

    /// An aligned text table, numbers to the right, for a terminal.
    pub fn to_text(&self) -> String {
        let cell = |v: &Json| -> String {
            let s = match v {
                Json::Null => "NULL".to_string(),
                Json::String(s) => s.clone(),
                Json::Number(n) => match n.as_f64() {
                    Some(f) if n.is_f64() => {
                        let a = f.abs();
                        if a != 0.0 && (a >= 1e9 || a < 1e-3) { format!("{f:.4e}") } else { format!("{:.4}", f).trim_end_matches('0').trim_end_matches('.').to_string() }
                    }
                    _ => n.to_string(),
                },
                other => other.to_string(),
            };
            if s.chars().count() > 80 { format!("{}…", s.chars().take(79).collect::<String>()) } else { s }
        };
        let cells: Vec<Vec<String>> = self.rows.iter().map(|r| r.iter().map(cell).collect()).collect();
        let numeric: Vec<bool> = (0..self.columns.len()).map(|i| self.rows.iter().all(|r| matches!(r[i], Json::Number(_) | Json::Null))).collect();
        let width: Vec<usize> = (0..self.columns.len())
            .map(|i| cells.iter().map(|r| r[i].chars().count()).chain([self.columns[i].chars().count()]).max().unwrap_or(0))
            .collect();
        let line = |vals: &[String], right: &[bool]| -> String {
            vals.iter()
                .enumerate()
                .map(|(i, v)| if right[i] { format!("{v:>w$}", w = width[i]) } else { format!("{v:<w$}", w = width[i]) })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        };
        let mut out = String::new();
        out.push_str(&line(&self.columns, &numeric));
        out.push('\n');
        out.push_str(&width.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("  "));
        out.push('\n');
        for r in &cells {
            out.push_str(&line(r, &numeric));
            out.push('\n');
        }
        out.push_str(&format!(
            "({} row{}{}, {:.1} ms)\n",
            self.rows.len(),
            if self.rows.len() == 1 { "" } else { "s" },
            if self.truncated { ", cut at the row limit" } else { "" },
            self.ms
        ));
        out
    }
}
