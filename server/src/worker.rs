//! DuckDB in its own process: a crash in a query (DuckDB has segfaulted on valid SQL) takes the
//! worker down, not the server. The server starts itself as `prow-server query-worker`, sends it
//! requests over its stdin, reads answers from its stdout, and starts a new one when it dies; the
//! queries in flight then fail with the crash, and the next ones go to the new worker.
//!
//! The pipe: a request is one JSON line (`{"id", "op": "sql" | "flame", ...}`); an answer is one
//! JSON header line (`{"id", "len", "rows", "truncated"}` or `{"id", "error"}`) and, after a header
//! with `len`, that many bytes of the result's JSON.
//!
//! Every SQL request is appended to the query log, `$PROW_DATA/queries.jsonl` (the `queries` view).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value as Json};

use crate::query::{self, Engine};

/// An answer: the result's JSON bytes, with its row count and whether it was cut.
pub struct Answer {
    pub body: Vec<u8>,
    pub rows: u64,
    pub truncated: bool,
}

type Reply = mpsc::Sender<Result<Answer, String>>;

/// The server's side: the worker of the moment, and the requests waiting on it.
pub struct Queries {
    stdin: Mutex<Option<ChildStdin>>,
    ready: Condvar,
    pending: Mutex<HashMap<u64, Reply>>,
    next: AtomicU64,
    log: Mutex<File>,
}

impl Queries {
    /// Start the worker, and a thread that starts a new one whenever it dies.
    pub fn start(data: &Path) -> Arc<Queries> {
        let log = OpenOptions::new().create(true).append(true).open(data.join("queries.jsonl")).unwrap_or_else(|e| {
            eprintln!("prow-server: cannot open the query log in {}: {e}", data.display());
            std::process::exit(1);
        });
        let q = Arc::new(Queries { stdin: Mutex::new(None), ready: Condvar::new(), pending: Mutex::new(HashMap::new()), next: AtomicU64::new(1), log: Mutex::new(log) });
        let s = Arc::clone(&q);
        std::thread::spawn(move || s.supervise());
        q
    }

    fn supervise(&self) {
        let exe = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
        loop {
            let began = Instant::now();
            let mut cmd = Command::new(&exe);
            cmd.arg("query-worker").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
            // the worker goes with the server
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                });
            }
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("prow-server: cannot start the query worker: {e}");
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };
            let stdout = child.stdout.take().unwrap();
            *self.stdin.lock().unwrap() = child.stdin.take();
            self.ready.notify_all();
            let _ = self.read(BufReader::new(stdout));

            *self.stdin.lock().unwrap() = None;
            let status = child.wait().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
            let lost: Vec<Reply> = self.pending.lock().unwrap().drain().map(|(_, r)| r).collect();
            eprintln!("prow-server: the query worker died ({status}) with {} queries in flight; starting another", lost.len());
            for r in lost {
                let _ = r.send(Err(format!("the query engine crashed ({status}) and has restarted; a query in flight crashed it")));
            }
            // a worker that dies at once (a bad configuration) is retried slowly
            if began.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    /// Answers from the worker, to their requests, until its stdout ends.
    fn read(&self, mut r: BufReader<impl Read>) -> io::Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let head: Json = serde_json::from_str(&line).map_err(io::Error::other)?;
            let id = head["id"].as_u64().unwrap_or(0);
            let answer = match head["error"].as_str() {
                Some(e) => Err(e.to_string()),
                None => {
                    let mut body = vec![0u8; head["len"].as_u64().unwrap_or(0) as usize];
                    r.read_exact(&mut body)?;
                    Ok(Answer { body, rows: head["rows"].as_u64().unwrap_or(0), truncated: head["truncated"].as_bool().unwrap_or(false) })
                }
            };
            if let Some(reply) = self.pending.lock().unwrap().remove(&id) {
                let _ = reply.send(answer);
            }
        }
    }

    /// Send `request` (without its id) to the worker and wait for the answer.
    fn call(&self, mut request: Json) -> Result<Answer, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        request["id"] = json!(id);
        let mut line = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        line.push(b'\n');
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        {
            // a worker being replaced is back in milliseconds
            let (mut stdin, _) = self.ready.wait_timeout_while(self.stdin.lock().unwrap(), Duration::from_secs(10), |s| s.is_none()).unwrap();
            let sent = match stdin.as_mut() {
                Some(s) => s.write_all(&line).and_then(|_| s.flush()).map_err(|e| e.to_string()),
                None => Err("the query engine is not running".into()),
            };
            if let Err(e) = sent {
                self.pending.lock().unwrap().remove(&id);
                return Err(e);
            }
        }
        // the worker stops a query at its time limit; this only guards against a worker that hangs
        rx.recv_timeout(query::TIME_LIMIT + Duration::from_secs(30)).unwrap_or_else(|_| {
            self.pending.lock().unwrap().remove(&id);
            Err("the query engine did not answer".into())
        })
    }

    /// SQL in `project` (and `run`), logged with `source` (who asked: `page`, `console`, ...).
    pub fn sql(&self, source: &str, project: &str, run: Option<&str>, sql: &str, max_rows: usize) -> Result<Answer, String> {
        let t0 = Instant::now();
        let r = self.call(json!({ "op": "sql", "project": project, "run": run, "sql": sql, "max_rows": max_rows }));
        let (rows, truncated, error) = match &r {
            Ok(a) => (Some(a.rows), Some(a.truncated), None),
            Err(e) => (None, None, Some(e.as_str())),
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as i64;
        let entry = json!({
            "ts": crate::iso(now), "source": source, "project": (!project.is_empty()).then_some(project), "run": run, "sql": sql,
            "ms": t0.elapsed().as_secs_f64() * 1e3, "rows": rows, "truncated": truncated, "error": error,
        });
        let mut line = serde_json::to_vec(&entry).unwrap_or_default();
        line.push(b'\n');
        // one write a line, so a reader never sees half of one
        let _ = self.log.lock().unwrap().write_all(&line);
        r
    }

    /// The flame graph of a run's zones in [from, to) (µs since the epoch); see [`Engine::flame`].
    pub fn flame(&self, project: &str, run: &str, from: i64, to: i64) -> Result<Answer, String> {
        self.call(json!({ "op": "flame", "project": project, "run": run, "from": from, "to": to }))
    }
}

/// The worker: `prow-server query-worker`. Each request on its own thread; it exits when the
/// server's end of stdin closes.
pub fn main(data: &Path, memory: &str, threads: usize, extensions: &Path) {
    let engine = Arc::new(Engine::open(data, memory, threads, extensions).unwrap_or_else(|e| {
        eprintln!("prow-server: query worker: {e}");
        std::process::exit(1);
    }));
    let out = Arc::new(Mutex::new(io::stdout()));
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Json>(&line) else { continue };
        let (engine, out) = (Arc::clone(&engine), Arc::clone(&out));
        std::thread::spawn(move || {
            let id = req["id"].as_u64().unwrap_or(0);
            let s = |k: &str| req[k].as_str().unwrap_or("").to_string();
            let answer: Result<(Json, u64, bool), String> = match req["op"].as_str() {
                Some("sql") => {
                    let run = req["run"].as_str();
                    let max = req["max_rows"].as_u64().map_or(query::MAX_ROWS, |m| m as usize);
                    engine.query(&s("project"), run, &s("sql"), max).map(|t| (t.to_json(), t.rows.len() as u64, t.truncated))
                }
                Some("flame") => engine
                    .flame(&s("project"), &s("run"), req["from"].as_i64().unwrap_or(0), req["to"].as_i64().unwrap_or(0))
                    .map(|(frames, ms)| (json!({ "root": query::flame_json(&frames, 0), "ms": ms }), 0, false)),
                _ => Err("unknown request".into()),
            };
            let mut o = out.lock().unwrap();
            let _ = match answer {
                Ok((body, rows, truncated)) => {
                    let body = serde_json::to_vec(&body).unwrap_or_default();
                    writeln!(o, "{}", json!({ "id": id, "len": body.len(), "rows": rows, "truncated": truncated })).and_then(|_| o.write_all(&body))
                }
                Err(e) => writeln!(o, "{}", json!({ "id": id, "error": e })),
            }
            .and_then(|_| o.flush());
        });
    }
    std::process::exit(0);
}
