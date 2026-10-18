//! The wire between a producer and `prow-server`: one TCP connection is one run.
//!
//! A frame is `u32 length (LE) · u8 kind · payload`, the length counting the kind and the
//! payload. Integers and floats are little-endian; a string is `u32 length · UTF-8 bytes`.
//!
//! | kind | direction | payload |
//! |---|---|---|
//! | `HELLO` | producer → server | version `u16`, token (string, empty = none), project, run id (16 bytes), attribute count `u16`, attributes `[name, value]` |
//! | `STRING` | producer → server | id `u32`, string: rows carry the id of a label |
//! | `SCHEMA` | producer → server | site `u16`, name, category, field count `u16`, fields `[name, type u8]` |
//! | `ROWS` | producer → server | sequence `u64`, site `u16`, row count `u32`, rows: `ts i64`, then each field's value by its type |
//! | `END` | producer → server | (empty): the run is complete |
//! | `ACK` | server → producer | sequence `u64`: every `ROWS` frame up to it is on disk |
//! | `OK` | server → producer | (empty): after `HELLO`, accepted; after `END`, the run is written |
//! | `ERR` | server → producer | a reason (string); the server closes the connection |
//!
//! An attribute value is a `u8` type followed by the value; a label attribute is a string,
//! not a `STRING` id. A site's schema and the strings are per connection: a producer that
//! reconnects sends `HELLO` with the same run id, then its schemas and strings again before
//! rows that use them. Every row's first column is `ts` (UTC, ns), implicit in the schema.
//!
//! `ROWS` frames are numbered from 1 within a run. The producer keeps each frame until an `ACK`
//! covers it and sends the unacknowledged ones again after a reconnect; the server skips those it
//! already wrote. A server crash loses nothing that the producer still holds.

use std::io::{self, Read, Write};

pub const VERSION: u16 = 1;

pub mod kind {
    pub const HELLO: u8 = 1;
    pub const STRING: u8 = 2;
    pub const SCHEMA: u8 = 3;
    pub const ROWS: u8 = 4;
    pub const END: u8 = 5;
    pub const OK: u8 = 6;
    pub const ERR: u8 = 7;
    pub const ACK: u8 = 8;
}

/// Largest frame either side accepts.
pub const MAX_FRAME: usize = 64 << 20;

/// A field's type: everything a hook can record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Type {
    I64 = 1,
    U64 = 2,
    F64 = 3,
    F32 = 4,
    Bool = 5,
    /// A string, sent once as `STRING` and referenced by id.
    Label = 6,
    /// UTC, ns since the Unix epoch.
    Ts = 7,
}

impl Type {
    pub fn from_u8(t: u8) -> io::Result<Type> {
        Ok(match t {
            1 => Type::I64,
            2 => Type::U64,
            3 => Type::F64,
            4 => Type::F32,
            5 => Type::Bool,
            6 => Type::Label,
            7 => Type::Ts,
            _ => return Err(bad(format!("unknown field type {t}"))),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Type::I64 => "i64",
            Type::U64 => "u64",
            Type::F64 => "f64",
            Type::F32 => "f32",
            Type::Bool => "bool",
            Type::Label => "label",
            Type::Ts => "ts",
        }
    }
}

/// A value on the wire: a label is its `STRING` id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Wire {
    I64(i64),
    U64(u64),
    F64(f64),
    F32(f32),
    Bool(bool),
    Label(u32),
    Ts(i64),
}

/// An attribute value in `HELLO`: a label is the string itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Attr {
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    Label(String),
    Ts(i64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    pub version: u16,
    pub token: String,
    pub project: String,
    pub run: [u8; 16],
    pub attrs: Vec<(String, Attr)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    pub site: u16,
    pub name: String,
    pub category: String,
    pub fields: Vec<(String, Type)>,
}

/// A decoded frame. `Rows` keeps its rows encoded; [`Rows`] decodes them against the schema.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello(Hello),
    String { id: u32, value: String },
    Schema(Schema),
    Rows { seq: u64, site: u16, count: u32, data: Vec<u8> },
    End,
    Ack { seq: u64 },
    Ok,
    Err(String),
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Appends little-endian primitives and strings.
pub struct Out<'a>(pub &'a mut Vec<u8>);

impl Out<'_> {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    pub fn wire(&mut self, v: Wire) {
        match v {
            Wire::I64(x) | Wire::Ts(x) => self.i64(x),
            Wire::U64(x) => self.u64(x),
            Wire::F64(x) => self.f64(x),
            Wire::F32(x) => self.f32(x),
            Wire::Bool(x) => self.u8(x as u8),
            Wire::Label(x) => self.u32(x),
        }
    }
    pub fn attr(&mut self, v: &Attr) {
        match v {
            Attr::I64(x) => {
                self.u8(Type::I64 as u8);
                self.i64(*x)
            }
            Attr::U64(x) => {
                self.u8(Type::U64 as u8);
                self.u64(*x)
            }
            Attr::F64(x) => {
                self.u8(Type::F64 as u8);
                self.f64(*x)
            }
            Attr::Bool(x) => {
                self.u8(Type::Bool as u8);
                self.u8(*x as u8)
            }
            Attr::Label(x) => {
                self.u8(Type::Label as u8);
                self.str(x)
            }
            Attr::Ts(x) => {
                self.u8(Type::Ts as u8);
                self.i64(*x)
            }
        }
    }
}

/// Reads little-endian primitives and strings from a payload.
pub struct In<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> In<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        In { buf, at: 0 }
    }
    pub fn is_empty(&self) -> bool {
        self.at == self.buf.len()
    }
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.buf.len() - self.at < n {
            return Err(bad("truncated payload"));
        }
        let s = &self.buf[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn f64(&mut self) -> io::Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn f32(&mut self) -> io::Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn str(&mut self) -> io::Result<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| bad("string is not UTF-8"))
    }
    pub fn wire(&mut self, t: Type) -> io::Result<Wire> {
        Ok(match t {
            Type::I64 => Wire::I64(self.i64()?),
            Type::U64 => Wire::U64(self.u64()?),
            Type::F64 => Wire::F64(self.f64()?),
            Type::F32 => Wire::F32(self.f32()?),
            Type::Bool => Wire::Bool(self.u8()? != 0),
            Type::Label => Wire::Label(self.u32()?),
            Type::Ts => Wire::Ts(self.i64()?),
        })
    }
    pub fn attr(&mut self) -> io::Result<Attr> {
        Ok(match Type::from_u8(self.u8()?)? {
            Type::I64 => Attr::I64(self.i64()?),
            Type::U64 => Attr::U64(self.u64()?),
            Type::F64 => Attr::F64(self.f64()?),
            Type::F32 => return Err(bad("an attribute is never f32")),
            Type::Bool => Attr::Bool(self.u8()? != 0),
            Type::Label => Attr::Label(self.str()?),
            Type::Ts => Attr::Ts(self.i64()?),
        })
    }
}

impl Frame {
    pub fn kind(&self) -> u8 {
        match self {
            Frame::Hello(_) => kind::HELLO,
            Frame::String { .. } => kind::STRING,
            Frame::Schema(_) => kind::SCHEMA,
            Frame::Rows { .. } => kind::ROWS,
            Frame::End => kind::END,
            Frame::Ack { .. } => kind::ACK,
            Frame::Ok => kind::OK,
            Frame::Err(_) => kind::ERR,
        }
    }

    /// Append the whole frame (length, kind, payload) to `buf`.
    pub fn encode(&self, buf: &mut Vec<u8>) {
        let start = buf.len();
        buf.extend_from_slice(&[0; 4]);
        buf.push(self.kind());
        let mut o = Out(buf);
        match self {
            Frame::Hello(h) => {
                o.u16(h.version);
                o.str(&h.token);
                o.str(&h.project);
                o.0.extend_from_slice(&h.run);
                o.u16(h.attrs.len() as u16);
                for (k, v) in &h.attrs {
                    o.str(k);
                    o.attr(v);
                }
            }
            Frame::String { id, value } => {
                o.u32(*id);
                o.str(value);
            }
            Frame::Schema(s) => {
                o.u16(s.site);
                o.str(&s.name);
                o.str(&s.category);
                o.u16(s.fields.len() as u16);
                for (name, t) in &s.fields {
                    o.str(name);
                    o.u8(*t as u8);
                }
            }
            Frame::Rows { seq, site, count, data } => {
                o.u64(*seq);
                o.u16(*site);
                o.u32(*count);
                o.0.extend_from_slice(data);
            }
            Frame::Ack { seq } => o.u64(*seq),
            Frame::End | Frame::Ok => {}
            Frame::Err(reason) => o.str(reason),
        }
        let len = (buf.len() - start - 4) as u32;
        buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
    }

    pub fn decode(kind: u8, payload: &[u8]) -> io::Result<Frame> {
        let mut i = In::new(payload);
        let f = match kind {
            kind::HELLO => {
                let version = i.u16()?;
                let token = i.str()?;
                let project = i.str()?;
                let run = i.take(16)?.try_into().unwrap();
                let n = i.u16()?;
                let mut attrs = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    attrs.push((i.str()?, i.attr()?));
                }
                Frame::Hello(Hello { version, token, project, run, attrs })
            }
            kind::STRING => Frame::String { id: i.u32()?, value: i.str()? },
            kind::SCHEMA => {
                let site = i.u16()?;
                let name = i.str()?;
                let category = i.str()?;
                let n = i.u16()?;
                let mut fields = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    fields.push((i.str()?, Type::from_u8(i.u8()?)?));
                }
                Frame::Schema(Schema { site, name, category, fields })
            }
            kind::ROWS => {
                let seq = i.u64()?;
                let site = i.u16()?;
                let count = i.u32()?;
                return Ok(Frame::Rows { seq, site, count, data: payload[14..].to_vec() });
            }
            kind::END => Frame::End,
            kind::ACK => Frame::Ack { seq: i.u64()? },
            kind::OK => Frame::Ok,
            kind::ERR => Frame::Err(i.str()?),
            k => return Err(bad(format!("unknown frame kind {k}"))),
        };
        if !i.is_empty() {
            return Err(bad(format!("{} trailing bytes in frame kind {kind}", payload.len() - i.at)));
        }
        Ok(f)
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let mut buf = Vec::new();
        self.encode(&mut buf);
        w.write_all(&buf)
    }

    /// The first whole frame in `buf`, and the bytes it took; `None` until one has arrived.
    pub fn parse(buf: &[u8]) -> io::Result<Option<(Frame, usize)>> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(bad(format!("frame length {len}")));
        }
        if buf.len() < 4 + len {
            return Ok(None);
        }
        Frame::decode(buf[4], &buf[5..4 + len]).map(|f| Some((f, 4 + len)))
    }

    /// Read one frame; `Ok(None)` at a clean end of stream (before a frame starts).
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Frame>> {
        let mut len = [0u8; 4];
        match r.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(bad(format!("frame length {len}")));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body)?;
        Frame::decode(body[0], &body[1..]).map(Some)
    }
}

/// Decodes the rows of a `ROWS` frame against the site's fields.
pub struct Rows<'a> {
    input: In<'a>,
    fields: &'a [(String, Type)],
    left: u32,
}

impl<'a> Rows<'a> {
    pub fn new(data: &'a [u8], count: u32, fields: &'a [(String, Type)]) -> Self {
        Rows { input: In::new(data), fields, left: count }
    }

    /// The next row: its `ts`, with its values in `out` (cleared first). `Ok(None)` after the last.
    pub fn next_into(&mut self, out: &mut Vec<Wire>) -> io::Result<Option<i64>> {
        if self.left == 0 {
            if !self.input.is_empty() {
                return Err(bad("trailing bytes after the last row"));
            }
            return Ok(None);
        }
        self.left -= 1;
        let ts = self.input.i64()?;
        out.clear();
        for (_, t) in self.fields {
            out.push(self.input.wire(*t)?);
        }
        Ok(Some(ts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(f: Frame) {
        let mut buf = Vec::new();
        f.encode(&mut buf);
        let back = Frame::read_from(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn frames_round_trip() {
        round_trip(Frame::Hello(Hello {
            version: VERSION,
            token: "t".into(),
            project: "myapp".into(),
            run: [7; 16],
            attrs: vec![
                ("commit".into(), Attr::Label("971f5df-dirty".into())),
                ("cores".into(), Attr::U64(96)),
                ("synced".into(), Attr::Bool(true)),
                ("maxerror_us".into(), Attr::I64(-1)),
                ("at".into(), Attr::Ts(1_792_242_425_000_042_000)),
                ("rate".into(), Attr::F64(0.5)),
            ],
        }));
        round_trip(Frame::String { id: 3, value: "prefill".into() });
        round_trip(Frame::Schema(Schema {
            site: 1,
            name: "step".into(),
            category: "track-step".into(),
            fields: vec![("kind".into(), Type::Label), ("width".into(), Type::U64), ("ms".into(), Type::F64)],
        }));
        round_trip(Frame::End);
        round_trip(Frame::Ack { seq: 41 });
        round_trip(Frame::Ok);
        round_trip(Frame::Err("bad token".into()));
    }

    #[test]
    fn rows_decode_against_their_schema() {
        let fields = vec![("kind".to_string(), Type::Label), ("width".to_string(), Type::U64), ("ok".to_string(), Type::Bool)];
        let mut data = Vec::new();
        let mut o = Out(&mut data);
        for (ts, w) in [(10i64, 512u64), (20, 1)] {
            o.i64(ts);
            o.wire(Wire::Label(3));
            o.wire(Wire::U64(w));
            o.wire(Wire::Bool(w > 1));
        }
        let f = Frame::Rows { seq: 9, site: 1, count: 2, data };
        let mut buf = Vec::new();
        f.encode(&mut buf);
        let Some(Frame::Rows { seq: 9, count, data, .. }) = Frame::read_from(&mut &buf[..]).unwrap() else { panic!() };
        assert!(Frame::parse(&buf[..buf.len() - 1]).unwrap().is_none());
        assert_eq!(Frame::parse(&buf).unwrap().unwrap().1, buf.len());
        let mut rows = Rows::new(&data, count, &fields);
        let mut v = Vec::new();
        assert_eq!(rows.next_into(&mut v).unwrap(), Some(10));
        assert_eq!(v, [Wire::Label(3), Wire::U64(512), Wire::Bool(true)]);
        assert_eq!(rows.next_into(&mut v).unwrap(), Some(20));
        assert_eq!(v, [Wire::Label(3), Wire::U64(1), Wire::Bool(false)]);
        assert_eq!(rows.next_into(&mut v).unwrap(), None);
    }

    #[test]
    fn malformed_frames_are_errors() {
        assert!(Frame::decode(99, &[]).is_err());
        assert!(Frame::decode(kind::STRING, &[1, 0]).is_err());
        assert!(Frame::decode(kind::END, &[0]).is_err());
        assert!(Frame::read_from(&mut &[0u8, 0, 0, 0][..]).is_err());
        assert!(Frame::read_from(&mut &[][..]).unwrap().is_none());
    }
}
