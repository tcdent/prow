//! prow: profiling rows. Structured, timestamped measurements from a program and its devices,
//! sent to `prow-server`, stored as one columnar table per site per run, and queried with SQL.
//!
//! ```ignore
//! prow::row!("track-step", "step", { "kind" => "prefill", "width" => 512u32, "ms" => ms });
//! prow::zone!("track-zones", "attend");           // a span, to the end of the scope
//! prow::plot!("track-store", "store.bytes", used);
//! prow::message!("track-store", "spilled to NVMe");
//! ```
//!
//! **A hook exists only if its category is compiled in.** The first argument names a Cargo
//! feature *of the crate that calls the hook* (`[features] track-step = []`). With the feature
//! off, the hook expands to nothing: no code, no check, and its arguments are not evaluated.
//! A misspelled category is a compiler warning (`unexpected cfg condition value`), because Cargo
//! checks feature names. There is no runtime switch.
//!
//! **A run starts with [`Run::start`]**, once, early in `main`: the project, the run's
//! attributes (build identity from [`stamp`], anything else that distinguishes the run) and the
//! server. Nothing happens until a hook records its first row: then the sender thread starts
//! and connects. Rows recorded before `start` wait in memory (at most
//! [`sink::MAX_THREAD_BUFFER`] a thread). A build with no categories never starts a thread.
//!
//! **The hooks** (Tracy's, adapted to rows):
//! - [`row!`]: a row of named fields at one site. Each site is its own table.
//! - [`zone!`]: a span from here to the end of the scope, on the calling thread's lane.
//! - [`zone_at!`]: a span with given start and end, on a given lane: a card's work, through
//!   [`clock::Card::at`].
//! - [`plot!`]: one number over time, by name.
//! - [`message!`]: a line of text at a time.
//!
//! Zones, plots and messages go to three built-in sites: `zones`, `plots`, `messages`.

pub mod clock;
pub mod protocol;
pub mod sink;
pub mod stamp;

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

pub use clock::Timestamp;
use protocol::Attr;

/// One field's value at the moment it is recorded. Built by the hooks from the field's
/// expression through [`ToValue`]; a label borrows its string and is copied into the buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    I64(i64),
    U64(u64),
    F64(f64),
    F32(f32),
    Bool(bool),
    Label(&'a str),
    Ts(Timestamp),
}

/// Anything a field can hold: integers, floats, `bool`, strings (labels) and [`Timestamp`].
pub trait ToValue {
    fn to_value(&self) -> Value<'_>;
}

macro_rules! to_value {
    ($variant:ident as $as:ty: $($t:ty),*) => {$(
        impl ToValue for $t {
            #[inline(always)]
            fn to_value(&self) -> Value<'_> { Value::$variant(*self as $as) }
        }
    )*};
}
to_value!(I64 as i64: i8, i16, i32, i64, isize);
to_value!(U64 as u64: u8, u16, u32, u64, usize);
to_value!(F64 as f64: f64);
to_value!(F32 as f32: f32);

impl ToValue for bool {
    fn to_value(&self) -> Value<'_> {
        Value::Bool(*self)
    }
}
impl ToValue for str {
    fn to_value(&self) -> Value<'_> {
        Value::Label(self)
    }
}
impl ToValue for String {
    fn to_value(&self) -> Value<'_> {
        Value::Label(self)
    }
}
impl ToValue for std::borrow::Cow<'_, str> {
    fn to_value(&self) -> Value<'_> {
        Value::Label(self)
    }
}
impl ToValue for Timestamp {
    fn to_value(&self) -> Value<'_> {
        Value::Ts(*self)
    }
}
impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value<'_> {
        (**self).to_value()
    }
}

/// A place in the code that records rows: one table. Made by the hooks as a `static`.
pub struct Site {
    category: &'static str,
    name: &'static str,
    fields: &'static [&'static str],
    /// The id this process gave the site, 0 until its first row; `u16::MAX` if it was refused.
    id: AtomicU16,
}

/// Site and field names: `a-z`, `0-9`, `_`, `.` and `-`, so they are paths and SQL names as they are.
const fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'.' || c == b'-') {
            return false;
        }
        i += 1;
    }
    true
}

const fn is_ts(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 2 && b[0] == b't' && b[1] == b's'
}

impl Site {
    /// A site. Checked at compile time when used in a `static`: names use `a-z 0-9 _ . -`, and
    /// no field is called `ts` (every row's implicit first column).
    pub const fn new(category: &'static str, name: &'static str, fields: &'static [&'static str]) -> Site {
        assert!(valid_name(name), "prow: a site name uses a-z, 0-9, '_', '.' and '-'");
        let mut i = 0;
        while i < fields.len() {
            assert!(valid_name(fields[i]), "prow: a field name uses a-z, 0-9, '_', '.' and '-'");
            assert!(!is_ts(fields[i]), "prow: 'ts' is every row's time; name the field something else");
            i += 1;
        }
        Site { category, name, fields, id: AtomicU16::new(0) }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn category(&self) -> &'static str {
        self.category
    }

    pub fn fields(&self) -> &'static [&'static str] {
        self.fields
    }

    /// Record one row at `ts`. `values` follow the site's fields, in order.
    #[inline]
    pub fn row(&'static self, ts: Timestamp, values: &[Value<'_>]) {
        debug_assert_eq!(values.len(), self.fields.len(), "prow: site '{}' has {} fields", self.name, self.fields.len());
        let id = match self.id.load(Ordering::Acquire) {
            0 => sink::register(self),
            id => id,
        };
        if id != u16::MAX {
            sink::record(id, ts, values);
        }
    }

    pub(crate) fn id_cell(&self) -> &AtomicU16 {
        &self.id
    }
}

/// The built-in sites the zone, plot and message hooks record to.
pub mod builtin {
    use super::Site;
    pub static ZONES: Site = Site::new("prow", "zones", &["category", "name", "lane", "dur_ns"]);
    pub static PLOTS: Site = Site::new("prow", "plots", &["category", "name", "value"]);
    pub static MESSAGES: Site = Site::new("prow", "messages", &["category", "text"]);
}

/// The calling thread's lane: its name, or `tid-<n>`.
pub fn lane() -> &'static str {
    thread_local! {
        static LANE: &'static str = Box::leak(
            std::thread::current()
                .name()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("tid-{}", unsafe { libc::gettid() }))
                .into_boxed_str(),
        );
    }
    LANE.with(|l| *l)
}

/// A span that records itself to `zones` when dropped. Made by [`zone!`].
pub struct Zone {
    category: &'static str,
    name: &'static str,
    start: Timestamp,
}

impl Zone {
    #[inline]
    pub fn begin(category: &'static str, name: &'static str) -> Zone {
        Zone { category, name, start: Timestamp::now() }
    }
}

impl Drop for Zone {
    #[inline]
    fn drop(&mut self) {
        let dur = (Timestamp::now().unix_nanos() - self.start.unix_nanos()).max(0) as u64;
        builtin::ZONES.row(
            self.start,
            &[Value::Label(self.category), Value::Label(self.name), Value::Label(lane()), Value::U64(dur)],
        );
    }
}

/// `row!(category, site, { field => value, .. })`: one row at `site`, now.
/// `row!(category, site, at = ts, { .. })`: at a given [`Timestamp`].
///
/// Fields are string literals; values are any [`ToValue`]. A site's fields and their types are
/// fixed by its first row.
#[macro_export]
macro_rules! row {
    ($cat:literal, $site:literal, at = $at:expr, { $($field:literal => $value:expr),* $(,)? }) => {{
        #[cfg(feature = $cat)]
        {
            static SITE: $crate::Site = $crate::Site::new($cat, $site, &[$($field),*]);
            SITE.row($at, &[$($crate::ToValue::to_value(&$value)),*]);
        }
        #[cfg(not(feature = $cat))]
        {
            if false {
                let _ = (&$at, $(&$value),*);
            }
        }
    }};
    ($cat:literal, $site:literal, { $($field:literal => $value:expr),* $(,)? }) => {
        $crate::row!($cat, $site, at = $crate::Timestamp::now(), { $($field => $value),* })
    };
}

/// `zone!(category, name);`: a span from here to the end of the enclosing scope, recorded to
/// `zones` on the calling thread's lane. `name` is a `&'static str`.
#[macro_export]
macro_rules! zone {
    ($cat:literal, $name:expr) => {
        #[cfg(feature = $cat)]
        let _prow_zone = $crate::Zone::begin($cat, $name);
    };
}

/// `zone_at!(category, name, lane, start, end)`: a span timed elsewhere (a card's completion
/// stamps through [`clock::Card::at`]), on a named lane (`"card0"`).
#[macro_export]
macro_rules! zone_at {
    ($cat:literal, $name:expr, $lane:expr, $start:expr, $end:expr $(,)?) => {{
        #[cfg(feature = $cat)]
        {
            let (start, end): ($crate::Timestamp, $crate::Timestamp) = ($start, $end);
            let dur = (end.unix_nanos() - start.unix_nanos()).max(0) as u64;
            $crate::builtin::ZONES.row(
                start,
                &[
                    $crate::Value::Label($cat),
                    $crate::ToValue::to_value(&$name),
                    $crate::ToValue::to_value(&$lane),
                    $crate::Value::U64(dur),
                ],
            );
        }
        #[cfg(not(feature = $cat))]
        {
            if false {
                let _ = (&$name, &$lane, &$start, &$end);
            }
        }
    }};
}

/// `plot!(category, name, value)`: one number now, recorded to `plots` as f64.
#[macro_export]
macro_rules! plot {
    ($cat:literal, $name:expr, $value:expr $(,)?) => {{
        #[cfg(feature = $cat)]
        {
            $crate::builtin::PLOTS.row(
                $crate::Timestamp::now(),
                &[$crate::Value::Label($cat), $crate::ToValue::to_value(&$name), $crate::Value::F64(($value) as f64)],
            );
        }
        #[cfg(not(feature = $cat))]
        {
            if false {
                let _ = (&$name, &$value);
            }
        }
    }};
}

/// `message!(category, text)`: a line of text now, recorded to `messages`.
#[macro_export]
macro_rules! message {
    ($cat:literal, $text:expr $(,)?) => {{
        #[cfg(feature = $cat)]
        {
            $crate::builtin::MESSAGES.row(
                $crate::Timestamp::now(),
                &[$crate::Value::Label($cat), $crate::ToValue::to_value(&$text)],
            );
        }
        #[cfg(not(feature = $cat))]
        {
            if false {
                let _ = &$text;
            }
        }
    }};
}

/// The build identity [`stamp::emit`] put into the calling crate's environment, as attributes:
/// `build.commit`, `build.dirty`, `build.features`. `Run::new(..).attrs(prow::build_attrs!())`.
#[macro_export]
macro_rules! build_attrs {
    () => {
        [
            ("build.commit", $crate::protocol::Attr::Label(option_env!("PROW_COMMIT").unwrap_or("unknown").into())),
            ("build.dirty", $crate::protocol::Attr::Bool(option_env!("PROW_DIRTY") == Some("1"))),
            ("build.features", $crate::protocol::Attr::Label(option_env!("PROW_FEATURES").unwrap_or("").into())),
        ]
    };
}

/// A run: one producer's rows under one project, with the attributes that identify it.
/// `Run::new("myapp").attrs(prow::build_attrs!()).server("10.0.0.2:7380").start()`.
pub struct Run {
    pub(crate) project: String,
    pub(crate) attrs: Vec<(String, Attr)>,
    pub(crate) server: Option<String>,
    pub(crate) token: Option<String>,
    pub(crate) core: Option<usize>,
    pub(crate) flush: Duration,
}

impl Run {
    /// A run of `project`. The server and token default to `PROW_SERVER` and `PROW_TOKEN`.
    pub fn new(project: impl Into<String>) -> Run {
        Run {
            project: project.into(),
            attrs: Vec::new(),
            server: std::env::var("PROW_SERVER").ok().filter(|s| !s.is_empty()),
            token: std::env::var("PROW_TOKEN").ok().filter(|s| !s.is_empty()),
            core: None,
            flush: Duration::from_millis(100),
        }
    }

    /// One attribute of the run: what distinguishes it (a setting, a host, a workload).
    pub fn attr(mut self, name: impl Into<String>, value: impl Into<Attr>) -> Run {
        self.attrs.push((name.into(), value.into()));
        self
    }

    pub fn attrs<N: Into<String>>(mut self, attrs: impl IntoIterator<Item = (N, Attr)>) -> Run {
        self.attrs.extend(attrs.into_iter().map(|(n, v)| (n.into(), v)));
        self
    }

    /// The server's `host:port`.
    pub fn server(mut self, addr: impl Into<String>) -> Run {
        self.server = Some(addr.into());
        self
    }

    /// The server's token, when it requires one.
    pub fn token(mut self, token: impl Into<String>) -> Run {
        self.token = Some(token.into());
        self
    }

    /// Pin the run's sender thread to this core (it inherits the starting thread's otherwise).
    pub fn core(mut self, core: usize) -> Run {
        self.core = Some(core);
        self
    }

    /// How often recorded rows are collected and sent (100 ms by default).
    pub fn flush_every(mut self, every: Duration) -> Run {
        self.flush = every;
        self
    }

    /// Start the run. Its sender thread starts with the first row any site records, connects,
    /// and sends every row recorded since the process began; with no category compiled in, no
    /// row is ever recorded, and nothing starts. A second `start` in a process is ignored with a
    /// warning. The process's normal exit ends the run (at most 2 s), as does [`finish`].
    pub fn start(self) {
        sink::start(self);
    }
}

impl From<i64> for Attr {
    fn from(v: i64) -> Attr {
        Attr::I64(v)
    }
}
impl From<u64> for Attr {
    fn from(v: u64) -> Attr {
        Attr::U64(v)
    }
}
impl From<usize> for Attr {
    fn from(v: usize) -> Attr {
        Attr::U64(v as u64)
    }
}
impl From<f64> for Attr {
    fn from(v: f64) -> Attr {
        Attr::F64(v)
    }
}
impl From<bool> for Attr {
    fn from(v: bool) -> Attr {
        Attr::Bool(v)
    }
}
impl From<&str> for Attr {
    fn from(v: &str) -> Attr {
        Attr::Label(v.into())
    }
}
impl From<String> for Attr {
    fn from(v: String) -> Attr {
        Attr::Label(v)
    }
}
impl From<Timestamp> for Attr {
    fn from(v: Timestamp) -> Attr {
        Attr::Ts(v.unix_nanos())
    }
}

/// End the run now: send what is recorded, then `END`, and wait at most 5 s for the server.
/// Rows recorded afterwards are not sent. Optional: a normal exit does this within 2 s.
pub fn finish() {
    sink::finish(Duration::from_secs(5));
}

/// A warning, once per distinct message, on stderr.
pub(crate) fn warn(message: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    if seen.lock().unwrap_or_else(|e| e.into_inner()).insert(message.to_string()) {
        eprintln!("prow: {message}");
    }
}
