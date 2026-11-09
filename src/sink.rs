//! Where rows go: a buffer per recording thread, and the run's sender thread.
//!
//! A hook appends its row to the calling thread's buffer: the site's id, `ts`, then each value
//! with its type, a label as its bytes. That is a lock nobody else holds (the sender takes it
//! for a swap every flush) and a copy; nothing is allocated once the buffer has grown. The
//! sender swaps every buffer out each flush, interns the labels, fixes each site's schema from
//! its first row, and sends `SCHEMA`, `STRING` and `ROWS` frames. It keeps every `ROWS` frame
//! until the server's `ACK` says it is on disk (at most [`MAX_PENDING`] bytes). When the
//! connection drops it reconnects with the same run id, sends the schemas and strings again, and
//! then every frame not yet acknowledged.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, OnceLock};
use std::time::{Duration, Instant};

use crate::protocol::{Attr, Frame, Hello, Out, Schema, Type, Wire, VERSION};
use crate::{clock, warn, Run, Site, Timestamp, Value};

/// Most bytes a thread's buffer holds between flushes; rows past it are dropped and counted.
pub const MAX_THREAD_BUFFER: usize = 8 << 20;
/// Most bytes of frames kept until acknowledged; the oldest are dropped past it.
pub const MAX_PENDING: usize = 64 << 20;
/// A `ROWS` frame is cut at about this many bytes.
const ROWS_FRAME: usize = 1 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every site this process has recorded at, by id - 1.
static SITES: Mutex<Vec<&'static Site>> = Mutex::new(Vec::new());
/// Every recording thread's buffer.
static BUFFERS: Mutex<Vec<Arc<Buffer>>> = Mutex::new(Vec::new());
/// Rows dropped anywhere: a full thread buffer, a full pending queue, a type mismatch.
static DROPPED: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Buffer {
    bytes: Mutex<Vec<u8>>,
}

/// Give `site` its id: the next one, or that of an earlier site with the same name and fields.
/// Two sites with one name and different fields: the later one is refused (`u16::MAX`).
pub(crate) fn register(site: &'static Site) -> u16 {
    let mut sites = lock(&SITES);
    let cell = site.id_cell();
    let id = cell.load(Ordering::Acquire);
    if id != 0 {
        return id;
    }
    let id = match sites.iter().position(|s| s.name() == site.name()) {
        Some(i) if sites[i].fields() == site.fields() => (i + 1) as u16,
        Some(i) => {
            warn(&format!(
                "two sites named '{}' with different fields ({:?} and {:?}); the second records nothing",
                site.name(),
                sites[i].fields(),
                site.fields()
            ));
            u16::MAX
        }
        None if sites.len() + 1 >= u16::MAX as usize => {
            warn("more than 65534 sites; further sites record nothing");
            u16::MAX
        }
        None => {
            sites.push(site);
            sites.len() as u16
        }
    };
    cell.store(id, Ordering::Release);
    drop(sites);
    spawn_if_ready();
    id
}

thread_local! {
    static LOCAL: Arc<Buffer> = {
        let b = Arc::new(Buffer::default());
        lock(&BUFFERS).push(Arc::clone(&b));
        b
    };
}

/// Append one row to the calling thread's buffer.
pub(crate) fn record(id: u16, ts: Timestamp, values: &[Value<'_>]) {
    let _ = LOCAL.try_with(|b| {
        let mut buf = lock(&b.bytes);
        if buf.len() >= MAX_THREAD_BUFFER {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut o = Out(&mut buf);
        o.u16(id);
        o.i64(ts.unix_nanos());
        for v in values {
            match *v {
                Value::I64(x) => {
                    o.u8(Type::I64 as u8);
                    o.i64(x)
                }
                Value::U64(x) => {
                    o.u8(Type::U64 as u8);
                    o.u64(x)
                }
                Value::F64(x) => {
                    o.u8(Type::F64 as u8);
                    o.f64(x)
                }
                Value::F32(x) => {
                    o.u8(Type::F32 as u8);
                    o.f32(x)
                }
                Value::Bool(x) => {
                    o.u8(Type::Bool as u8);
                    o.u8(x as u8)
                }
                Value::Label(s) => {
                    o.u8(Type::Label as u8);
                    o.str(s)
                }
                Value::Ts(t) => {
                    o.u8(Type::Ts as u8);
                    o.i64(t.unix_nanos())
                }
            }
        }
    });
}

/// The run's control block: the sender waits on it between flushes, `finish` signals it.
struct Control {
    state: Mutex<CtlState>,
    cv: Condvar,
}

#[derive(Default)]
struct CtlState {
    /// `finish` asked for the end of the run, by this time.
    stop: Option<Instant>,
    /// The sender has ended the run.
    done: bool,
}

static CONTROL: OnceLock<Arc<Control>> = OnceLock::new();
/// The run [`start`] configured, until its first row starts the sender.
static CONFIGURED: Mutex<Option<Run>> = Mutex::new(None);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Configure the run. Its sender starts with the first site that records a row, so a build
/// with no categories compiled in never starts a thread or opens a connection.
pub(crate) fn start(run: Run) {
    if STARTED.swap(true, Ordering::AcqRel) {
        warn("Run::start was called again; the first run continues");
        return;
    }
    *lock(&CONFIGURED) = Some(run);
    spawn_if_ready();
}

/// Start the sender once a run is configured and some site has recorded.
fn spawn_if_ready() {
    if lock(&SITES).is_empty() {
        return;
    }
    let Some(run) = lock(&CONFIGURED).take() else { return };
    let ctl = Arc::new(Control { state: Mutex::new(CtlState::default()), cv: Condvar::new() });
    let _ = CONTROL.set(Arc::clone(&ctl));
    let spawned = std::thread::Builder::new().name("prow-sender".into()).spawn(move || {
        if let Some(core) = run.core {
            pin(core);
        }
        let mut s = Sender::new(run);
        s.run(&ctl);
    });
    if let Err(e) = spawned {
        warn(&format!("the sender thread could not start: {e}; rows will be dropped"));
        return;
    }
    register_atexit();
}

pub(crate) fn finish(budget: Duration) {
    let Some(ctl) = CONTROL.get() else { return };
    let deadline = Instant::now() + budget;
    let mut st = lock(&ctl.state);
    if st.stop.is_none() {
        st.stop = Some(deadline);
    }
    ctl.cv.notify_all();
    while !st.done {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        st = ctl.cv.wait_timeout(st, deadline - now).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
    }
}

fn register_atexit() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        extern "C" fn at_exit() {
            let _ = std::panic::catch_unwind(|| finish(Duration::from_secs(2)));
        }
        // SAFETY: the C library's atexit; the callback cannot unwind (panics are caught).
        if unsafe { libc::atexit(at_exit) } != 0 {
            warn("the exit hook could not be registered; call prow::finish() before exiting");
        }
    });
}

fn pin(core: usize) {
    // SAFETY: a zeroed cpu_set_t is the empty set; CPU_SET writes within it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            warn(&format!("the sender could not be pinned to core {core}: {}", io::Error::last_os_error()));
        }
    }
}

fn run_id() -> [u8; 16] {
    let mut id = [0u8; 16];
    // SAFETY: getrandom writes at most `id.len()` bytes into `id`.
    let n = unsafe { libc::getrandom(id.as_mut_ptr().cast(), id.len(), 0) };
    if n != id.len() as isize {
        // no entropy source: the time and pid are unique enough for a run id
        let t = Timestamp::now().unix_nanos() as u64;
        id[..8].copy_from_slice(&t.to_le_bytes());
        id[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    id
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes a NUL-terminated name into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return "unknown".into();
    }
    let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// The run's sender: the only thread that touches the network.
struct Sender {
    hello: Hello,
    server: Option<String>,
    flush: Duration,
    /// Each site's field types, fixed by its first row; by id - 1.
    types: Vec<Option<Vec<Type>>>,
    /// Schemas in the order their sites first sent rows.
    schemas: Vec<Schema>,
    strings: HashMap<String, u32>,
    string_list: Vec<String>,
    /// `ROWS` frames, encoded, not yet acknowledged: (sequence, frame, rows).
    pending: VecDeque<(u64, Vec<u8>, u32)>,
    pending_bytes: usize,
    next_seq: u64,
    conn: Option<Conn>,
    retry_at: Instant,
    backoff: Duration,
    /// The server refused the run (version, token, project name): retried with backoff, never at the end.
    refused: bool,
    /// Rows of the frame being built, by site.
    building: HashMap<u16, (Vec<u8>, u32)>,
    spare: Vec<u8>,
    values: Vec<(Type, Wire)>,
}

struct Conn {
    stream: TcpStream,
    sent_schemas: usize,
    sent_strings: usize,
    /// How many of the pending frames this connection has written.
    written: usize,
    /// Bytes from the server not yet a whole frame.
    inbox: Vec<u8>,
}

impl Sender {
    fn new(run: Run) -> Sender {
        let health = clock::health();
        let mut attrs: Vec<(String, Attr)> = vec![
            ("host".into(), Attr::Label(hostname())),
            ("pid".into(), Attr::U64(std::process::id() as u64)),
            ("started".into(), Attr::Ts(Timestamp::now().unix_nanos())),
            ("clock.synced".into(), Attr::Bool(health.synced)),
            ("clock.maxerror_us".into(), Attr::I64(health.maxerror_us)),
            ("clock.esterror_us".into(), Attr::I64(health.esterror_us)),
            ("prow.version".into(), Attr::Label(env!("CARGO_PKG_VERSION").into())),
        ];
        attrs.extend(run.attrs);
        if run.server.is_none() {
            warn("no server (Run::server or PROW_SERVER): rows are recorded and dropped");
        }
        Sender {
            hello: Hello { version: VERSION, token: run.token.unwrap_or_default(), project: run.project, run: run_id(), attrs },
            server: run.server,
            flush: run.flush,
            types: Vec::new(),
            schemas: Vec::new(),
            strings: HashMap::new(),
            string_list: Vec::new(),
            pending: VecDeque::new(),
            pending_bytes: 0,
            next_seq: 1,
            conn: None,
            retry_at: Instant::now(),
            backoff: Duration::from_millis(500),
            refused: false,
            building: HashMap::new(),
            spare: Vec::new(),
            values: Vec::new(),
        }
    }

    fn run(&mut self, ctl: &Control) {
        loop {
            let stop = {
                let st = lock(&ctl.state);
                let st = match st.stop {
                    Some(_) => st,
                    None => ctl.cv.wait_timeout(st, self.flush).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0),
                };
                st.stop
            };
            // a (raw, real) pair a flush, for converting raw-clock readings later (clock::from_raw)
            clock::sample();
            self.collect();
            match stop {
                None => self.send(),
                Some(deadline) => {
                    self.end(deadline);
                    break;
                }
            }
        }
        let dropped = DROPPED.load(Ordering::Relaxed);
        if dropped > 0 {
            eprintln!("prow: dropped {dropped} rows");
        }
        let mut st = lock(&ctl.state);
        st.done = true;
        ctl.cv.notify_all();
    }

    /// Swap every thread's buffer out and turn its rows into `ROWS` frames.
    fn collect(&mut self) {
        let sites: Vec<&'static Site> = lock(&SITES).clone();
        let buffers: Vec<Arc<Buffer>> = {
            let mut all = lock(&BUFFERS);
            // a buffer only this list holds belongs to a thread that has ended
            all.retain(|b| Arc::strong_count(b) > 1 || !lock(&b.bytes).is_empty());
            all.clone()
        };
        for b in buffers {
            let mut data = std::mem::take(&mut self.spare);
            std::mem::swap(&mut data, &mut *lock(&b.bytes));
            if let Err(e) = self.parse(&data, &sites) {
                warn(&format!("a thread's buffer could not be read ({e}); its remaining rows were dropped"));
            }
            data.clear();
            self.spare = data;
        }
        let building = std::mem::take(&mut self.building);
        for (site, (rows, count)) in building {
            self.queue(site, rows, count);
        }
        if self.server.is_none() {
            let n: u64 = self.pending.drain(..).map(|(_, _, c)| c as u64).sum();
            self.pending_bytes = 0;
            DROPPED.fetch_add(n, Ordering::Relaxed);
        }
    }

    fn parse(&mut self, data: &[u8], sites: &[&'static Site]) -> io::Result<()> {
        let mut i = crate::protocol::In::new(data);
        while !i.is_empty() {
            let id = i.u16()?;
            let ts = i.i64()?;
            let site = *sites.get(id as usize - 1).ok_or_else(|| io::Error::other("a row of an unknown site"))?;
            self.values.clear();
            for _ in 0..site.fields().len() {
                let t = Type::from_u8(i.u8()?)?;
                let w = match t {
                    Type::Label => {
                        let s = i.str()?;
                        Wire::Label(self.intern(s))
                    }
                    _ => i.wire(t)?,
                };
                self.values.push((t, w));
            }
            if self.types.len() < id as usize {
                self.types.resize(id as usize, None);
            }
            let slot = &mut self.types[id as usize - 1];
            match slot {
                None => {
                    let types: Vec<Type> = self.values.iter().map(|(t, _)| *t).collect();
                    self.schemas.push(Schema {
                        site: id,
                        name: site.name().into(),
                        category: site.category().into(),
                        fields: site.fields().iter().map(|f| f.to_string()).zip(types.iter().copied()).collect(),
                    });
                    *slot = Some(types);
                }
                Some(types) if types.iter().zip(&self.values).any(|(t, (v, _))| t != v) => {
                    warn(&format!("site '{}' recorded a row with other field types than its first; such rows are dropped", site.name()));
                    DROPPED.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Some(_) => {}
            }
            let (rows, count) = self.building.entry(id).or_default();
            let mut o = Out(rows);
            o.i64(ts);
            for (_, w) in &self.values {
                o.wire(*w);
            }
            *count += 1;
            if rows.len() >= ROWS_FRAME {
                let (rows, count) = self.building.remove(&id).unwrap();
                self.queue(id, rows, count);
            }
        }
        Ok(())
    }

    fn intern(&mut self, s: String) -> u32 {
        if let Some(&id) = self.strings.get(&s) {
            return id;
        }
        let id = self.string_list.len() as u32;
        self.string_list.push(s.clone());
        self.strings.insert(s, id);
        id
    }

    fn queue(&mut self, site: u16, rows: Vec<u8>, count: u32) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut frame = Vec::with_capacity(rows.len() + 24);
        Frame::Rows { seq, site, count, data: rows }.encode(&mut frame);
        self.pending_bytes += frame.len();
        self.pending.push_back((seq, frame, count));
        while self.pending_bytes > MAX_PENDING {
            let Some((_, f, n)) = self.pending.pop_front() else { break };
            self.pending_bytes -= f.len();
            DROPPED.fetch_add(n as u64, Ordering::Relaxed);
            if let Some(c) = self.conn.as_mut() {
                c.written = c.written.saturating_sub(1);
            }
        }
    }

    /// Forget the frames an `ACK` covers.
    fn acked(&mut self, seq: u64) {
        while self.pending.front().is_some_and(|(s, _, _)| *s <= seq) {
            let (_, f, _) = self.pending.pop_front().unwrap();
            self.pending_bytes -= f.len();
            if let Some(c) = self.conn.as_mut() {
                c.written = c.written.saturating_sub(1);
            }
        }
    }

    /// Take the `ACK`s that have arrived, without waiting.
    fn read_acks(&mut self) -> io::Result<()> {
        let conn = self.conn.as_mut().unwrap();
        conn.stream.set_nonblocking(true)?;
        let mut chunk = [0u8; 4096];
        let read = loop {
            match conn.stream.read(&mut chunk) {
                Ok(0) => break Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the server closed the connection")),
                Ok(n) => conn.inbox.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        conn.stream.set_nonblocking(false)?;
        read?;
        while let Some(frame) = self.next_reply()? {
            match frame {
                Frame::Ack { seq } => self.acked(seq),
                other => return Err(io::Error::other(format!("unexpected frame from the server: {other:?}"))),
            }
        }
        Ok(())
    }

    /// The next whole frame in the connection's inbox.
    fn next_reply(&mut self) -> io::Result<Option<Frame>> {
        let conn = self.conn.as_mut().unwrap();
        match Frame::parse(&conn.inbox)? {
            Some((f, n)) => {
                conn.inbox.drain(..n);
                Ok(Some(f))
            }
            None => Ok(None),
        }
    }

    /// Write what is pending, connecting first if needed. Errors drop the connection.
    fn send(&mut self) {
        if self.pending.is_empty() && self.conn.is_none() {
            return;
        }
        if self.conn.is_none() && !self.connect() {
            return;
        }
        if let Err(e) = self.write_pending() {
            self.lost(&e);
        }
    }

    fn connect(&mut self) -> bool {
        let Some(server) = self.server.clone() else { return false };
        if Instant::now() < self.retry_at {
            return false;
        }
        match self.open(&server) {
            Ok(stream) => {
                self.conn = Some(Conn { stream, sent_schemas: 0, sent_strings: 0, written: 0, inbox: Vec::new() });
                self.backoff = Duration::from_millis(500);
                self.refused = false;
                true
            }
            Err(e) => {
                self.refused = e.kind() == io::ErrorKind::PermissionDenied;
                warn(&format!("cannot reach the server at {server} ({e}); retrying in the background"));
                self.retry_at = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                false
            }
        }
    }

    fn open(&self, server: &str) -> io::Result<TcpStream> {
        let addr = server.to_socket_addrs()?.next().ok_or_else(|| io::Error::other("no address"))?;
        let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        Frame::Hello(self.hello.clone()).write_to(&mut stream)?;
        match Frame::read_from(&mut stream)? {
            Some(Frame::Ok) => Ok(stream),
            Some(Frame::Err(reason)) => Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("refused: {reason}"))),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }

    fn write_pending(&mut self) -> io::Result<()> {
        let conn = self.conn.as_mut().unwrap();
        let mut buf = Vec::new();
        for s in &self.schemas[conn.sent_schemas..] {
            Frame::Schema(s.clone()).encode(&mut buf);
        }
        for (id, s) in self.string_list.iter().enumerate().skip(conn.sent_strings) {
            Frame::String { id: id as u32, value: s.clone() }.encode(&mut buf);
        }
        conn.stream.write_all(&buf)?;
        conn.sent_schemas = self.schemas.len();
        conn.sent_strings = self.string_list.len();
        while let Some((_, frame, _)) = self.pending.get(conn.written) {
            conn.stream.write_all(frame)?;
            conn.written += 1;
        }
        self.read_acks()
    }

    fn lost(&mut self, e: &io::Error) {
        warn(&format!("lost the server connection ({e}); reconnecting in the background"));
        self.conn = None;
        self.retry_at = Instant::now() + self.backoff;
    }

    /// The end of the run: send everything, `END`, and wait for the server's `OK`.
    fn end(&mut self, deadline: Instant) {
        if self.server.is_none() {
            return;
        }
        while Instant::now() < deadline && !self.refused {
            if self.conn.is_none() {
                self.retry_at = self.retry_at.min(Instant::now() + Duration::from_millis(200));
                if !self.connect() {
                    std::thread::sleep(Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())));
                    continue;
                }
            }
            let result = self.write_pending().and_then(|()| {
                let conn = self.conn.as_mut().unwrap();
                Frame::End.write_to(&mut conn.stream)?;
                conn.stream.set_read_timeout(Some(deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))))?;
                // ACKs may come before the OK; the OK covers everything
                loop {
                    while let Some(frame) = self.next_reply()? {
                        match frame {
                            Frame::Ack { seq } => self.acked(seq),
                            Frame::Ok => return Ok(()),
                            other => return Err(io::Error::other(format!("unexpected reply to END: {other:?}"))),
                        }
                    }
                    let conn = self.conn.as_mut().unwrap();
                    let mut chunk = [0u8; 4096];
                    match conn.stream.read(&mut chunk)? {
                        0 => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the server closed the connection")),
                        n => conn.inbox.extend_from_slice(&chunk[..n]),
                    }
                }
            });
            match result {
                Ok(()) => {
                    self.pending.clear();
                    self.pending_bytes = 0;
                    return;
                }
                Err(e) => self.lost(&e),
            }
        }
        let unsent: u64 = self.pending.iter().map(|(_, _, n)| *n as u64).sum();
        DROPPED.fetch_add(unsent, Ordering::Relaxed);
        warn(if self.refused { "the server refused the run; its rows were not stored" } else { "the run could not be ended with the server in time" });
    }
}
