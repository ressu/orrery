//! D-Bus messages and the values in them, after the "Message Protocol" part of the D-Bus
//! specification. Values are encoded from a [`Value`] tree by one function, so a new call adds
//! no code of its own.

use std::fmt;

/// A D-Bus value. Arrays carry their element's signature, so an empty one still has a type.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    Path(String),
    Signature(String),
    Array(String, Vec<Value>),
    /// An array of dict entries, `a{kv}`, with the key's and the value's signatures.
    Dict(String, String, Vec<(Value, Value)>),
    Struct(Vec<Value>),
    Variant(Box<Value>),
}

impl Value {
    pub fn str(s: &str) -> Self {
        Value::Str(s.to_owned())
    }

    pub fn path(s: &str) -> Self {
        Value::Path(s.to_owned())
    }

    /// An array of strings, `as`.
    pub fn strs(items: &[&str]) -> Self {
        Value::Array("s".into(), items.iter().map(|s| Value::str(s)).collect())
    }

    pub fn signature(&self) -> String {
        match self {
            Value::Byte(_) => "y".into(),
            Value::Bool(_) => "b".into(),
            Value::I16(_) => "n".into(),
            Value::U16(_) => "q".into(),
            Value::I32(_) => "i".into(),
            Value::U32(_) => "u".into(),
            Value::I64(_) => "x".into(),
            Value::U64(_) => "t".into(),
            Value::F64(_) => "d".into(),
            Value::Str(_) => "s".into(),
            Value::Path(_) => "o".into(),
            Value::Signature(_) => "g".into(),
            Value::Array(elem, _) => format!("a{elem}"),
            Value::Dict(k, v, _) => format!("a{{{k}{v}}}"),
            Value::Struct(fields) => format!("({})", fields.iter().map(Value::signature).collect::<String>()),
            Value::Variant(_) => "v".into(),
        }
    }

    /// The string inside a string, object path, or signature, looking through variants.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Path(s) | Value::Signature(s) => Some(s),
            Value::Variant(v) => v.as_str(),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Value::U32(n) => Some(*n),
            Value::Variant(v) => v.as_u32(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Variant(v) => v.as_bool(),
            _ => None,
        }
    }

    /// The items of an array, or the fields of a struct, looking through variants.
    pub fn items(&self) -> Option<&[Value]> {
        match self {
            Value::Array(_, items) | Value::Struct(items) => Some(items),
            Value::Variant(v) => v.items(),
            _ => None,
        }
    }

    /// The value for `key` in a dict with string keys, such as `a{sv}`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(_, _, entries) => entries.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v),
            Value::Variant(v) => v.get(key),
            _ => None,
        }
    }
}

/// A message that breaks the format, or that orrery can't take apart.
#[derive(Debug, Clone, PartialEq)]
pub struct FormatError(pub String);

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad D-Bus message: {}", self.0)
    }
}

fn bad(msg: impl Into<String>) -> FormatError {
    FormatError(msg.into())
}

/// The specification's limits: a message is at most 128 MiB, and an array 64 MiB.
const MAX_MESSAGE: usize = 128 << 20;
const MAX_ARRAY: usize = 64 << 20;
/// Containers nest at most this deep, so a hostile message can't exhaust the stack.
const MAX_DEPTH: usize = 64;

fn alignment(code: u8) -> usize {
    match code {
        b'y' | b'g' | b'v' => 1,
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b's' | b'o' | b'a' => 4,
        _ => 8, // x, t, d, struct, and dict entry
    }
}

/// Splits the first complete type off a signature.
fn split_type(sig: &str) -> Result<(&str, &str), FormatError> {
    let bytes = sig.as_bytes();
    let end = match bytes.first() {
        None => return Err(bad("missing type in signature")),
        Some(b'a') => 1 + split_type(&sig[1..])?.0.len(),
        Some(&open @ (b'(' | b'{')) => {
            let close = if open == b'(' { b')' } else { b'}' };
            let mut i = 1;
            while bytes.get(i) != Some(&close) {
                if i >= bytes.len() {
                    return Err(bad(format!("unclosed {} in signature {sig}", open as char)));
                }
                i += split_type(&sig[i..])?.0.len();
            }
            i + 1
        }
        Some(c) if b"ybnqiuxtdsogv".contains(c) => 1,
        Some(c) => return Err(bad(format!("unknown type {} in signature {sig}", *c as char))),
    };
    Ok(sig.split_at(end))
}

/// Encodes values, little-endian.
#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    fn align(&mut self, n: usize) {
        while !self.buf.len().is_multiple_of(n) {
            self.buf.push(0);
        }
    }

    fn u32(&mut self, n: u32) {
        self.align(4);
        self.buf.extend_from_slice(&n.to_le_bytes());
    }

    fn string(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    pub fn put(&mut self, v: &Value) {
        match v {
            Value::Byte(n) => self.buf.push(*n),
            Value::Bool(b) => self.u32(u32::from(*b)),
            Value::I16(n) => {
                self.align(2);
                self.buf.extend_from_slice(&n.to_le_bytes());
            }
            Value::U16(n) => {
                self.align(2);
                self.buf.extend_from_slice(&n.to_le_bytes());
            }
            Value::I32(n) => {
                self.align(4);
                self.buf.extend_from_slice(&n.to_le_bytes());
            }
            Value::U32(n) => self.u32(*n),
            Value::I64(n) => {
                self.align(8);
                self.buf.extend_from_slice(&n.to_le_bytes());
            }
            Value::U64(n) => {
                self.align(8);
                self.buf.extend_from_slice(&n.to_le_bytes());
            }
            Value::F64(n) => {
                self.align(8);
                self.buf.extend_from_slice(&n.to_le_bytes());
            }
            Value::Str(s) | Value::Path(s) => self.string(s),
            Value::Signature(s) => {
                self.buf.push(s.len() as u8);
                self.buf.extend_from_slice(s.as_bytes());
                self.buf.push(0);
            }
            Value::Array(elem, items) => {
                let start = self.array_start(elem.as_bytes()[0]);
                for item in items {
                    self.put(item);
                }
                self.array_end(start);
            }
            Value::Dict(_, _, entries) => {
                let start = self.array_start(b'{');
                for (k, v) in entries {
                    self.align(8);
                    self.put(k);
                    self.put(v);
                }
                self.array_end(start);
            }
            Value::Struct(fields) => {
                self.align(8);
                for f in fields {
                    self.put(f);
                }
            }
            Value::Variant(inner) => {
                self.put(&Value::Signature(inner.signature()));
                self.put(inner);
            }
        }
    }

    /// Writes a placeholder length and the padding before the first element. Returns where the
    /// length is, and where the elements start; the padding doesn't count toward the length.
    fn array_start(&mut self, elem: u8) -> (usize, usize) {
        self.u32(0);
        let len_at = self.buf.len() - 4;
        self.align(alignment(elem));
        (len_at, self.buf.len())
    }

    fn array_end(&mut self, (len_at, start): (usize, usize)) {
        let len = (self.buf.len() - start) as u32;
        self.buf[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    }
}

/// Decodes values, in either byte order. Positions count from the start of the message, which
/// alignment is relative to.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    big: bool,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], big: bool) -> Self {
        Reader { buf, pos: 0, big }
    }

    fn align(&mut self, n: usize) -> Result<(), FormatError> {
        let next = self.pos.next_multiple_of(n);
        if next > self.buf.len() {
            return Err(bad("message ends in padding"));
        }
        self.pos = next;
        Ok(())
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], FormatError> {
        self.align(N)?;
        let bytes = self.buf.get(self.pos..self.pos + N).ok_or_else(|| bad("message ends early"))?;
        self.pos += N;
        Ok(bytes.try_into().expect("N bytes"))
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        let b = self.take::<4>()?;
        Ok(if self.big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    }

    fn text(&mut self, len: usize) -> Result<String, FormatError> {
        let bytes = self.buf.get(self.pos..self.pos + len + 1).ok_or_else(|| bad("string runs past the end"))?;
        self.pos += len + 1;
        if bytes[len] != 0 {
            return Err(bad("string isn't nul-terminated"));
        }
        String::from_utf8(bytes[..len].to_vec()).map_err(|_| bad("string isn't UTF-8"))
    }

    /// Reads the values of a signature with any number of complete types.
    pub fn values(&mut self, mut sig: &str) -> Result<Vec<Value>, FormatError> {
        let mut out = Vec::new();
        while !sig.is_empty() {
            let (first, rest) = split_type(sig)?;
            out.push(self.value(first, 0)?);
            sig = rest;
        }
        Ok(out)
    }

    fn value(&mut self, sig: &str, depth: usize) -> Result<Value, FormatError> {
        if depth > MAX_DEPTH {
            return Err(bad("containers nest too deep"));
        }
        let big = self.big;
        macro_rules! num {
            ($t:ty, $n:literal) => {{
                let b = self.take::<$n>()?;
                if big { <$t>::from_be_bytes(b) } else { <$t>::from_le_bytes(b) }
            }};
        }
        Ok(match sig.as_bytes()[0] {
            b'y' => {
                let b = *self.buf.get(self.pos).ok_or_else(|| bad("message ends early"))?;
                self.pos += 1;
                Value::Byte(b)
            }
            b'b' => match self.u32()? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                n => return Err(bad(format!("boolean {n}"))),
            },
            b'n' => Value::I16(num!(i16, 2)),
            b'q' => Value::U16(num!(u16, 2)),
            b'i' => Value::I32(num!(i32, 4)),
            b'u' => Value::U32(self.u32()?),
            b'x' => Value::I64(num!(i64, 8)),
            b't' => Value::U64(num!(u64, 8)),
            b'd' => Value::F64(num!(f64, 8)),
            b's' | b'o' => {
                let len = self.u32()? as usize;
                let s = self.text(len)?;
                if sig.starts_with('s') { Value::Str(s) } else { Value::Path(s) }
            }
            b'g' => {
                let len = usize::from(*self.buf.get(self.pos).ok_or_else(|| bad("message ends early"))?);
                self.pos += 1;
                Value::Signature(self.text(len)?)
            }
            b'v' => {
                let Value::Signature(inner) = self.value("g", depth)? else { unreachable!() };
                let (one, rest) = split_type(&inner)?;
                if !rest.is_empty() {
                    return Err(bad(format!("variant holds more than one type: {inner}")));
                }
                Value::Variant(Box::new(self.value(one, depth + 1)?))
            }
            b'(' => {
                self.align(8)?;
                let fields = self.values_nested(&sig[1..sig.len() - 1], depth)?;
                Value::Struct(fields)
            }
            b'a' => {
                let len = self.u32()? as usize;
                if len > MAX_ARRAY {
                    return Err(bad(format!("array of {len} bytes")));
                }
                let elem = &sig[1..];
                self.align(alignment(elem.as_bytes()[0]))?;
                let end = self.pos + len;
                if end > self.buf.len() {
                    return Err(bad("array runs past the end"));
                }
                if elem.starts_with('{') {
                    let (k, v) = split_type(&elem[1..elem.len() - 1])?;
                    let mut entries = Vec::new();
                    while self.pos < end {
                        self.align(8)?;
                        let key = self.value(k, depth + 1)?;
                        let val = self.value(v, depth + 1)?;
                        entries.push((key, val));
                    }
                    Value::Dict(k.into(), v.into(), entries)
                } else {
                    let mut items = Vec::new();
                    while self.pos < end {
                        items.push(self.value(elem, depth + 1)?);
                    }
                    Value::Array(elem.into(), items)
                }
            }
            c => return Err(bad(format!("unknown type {}", c as char))),
        })
    }

    fn values_nested(&mut self, mut sig: &str, depth: usize) -> Result<Vec<Value>, FormatError> {
        let mut out = Vec::new();
        while !sig.is_empty() {
            let (first, rest) = split_type(sig)?;
            out.push(self.value(first, depth + 1)?);
            sig = rest;
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    MethodCall = 1,
    Return = 2,
    Error = 3,
    Signal = 4,
}

/// The caller doesn't want a reply.
pub const NO_REPLY_EXPECTED: u8 = 0x1;

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub kind: Kind,
    pub flags: u8,
    pub serial: u32,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub body: Vec<Value>,
}

impl Message {
    pub fn new(kind: Kind) -> Self {
        Message {
            kind,
            flags: 0,
            serial: 0,
            path: None,
            interface: None,
            member: None,
            error_name: None,
            reply_serial: None,
            destination: None,
            sender: None,
            body: Vec::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut fields = Vec::new();
        let mut field =
            |code: u8, v: Value| fields.push(Value::Struct(vec![Value::Byte(code), Value::Variant(Box::new(v))]));
        let strings = [
            (1, self.path.as_ref().map(|s| Value::path(s))),
            (2, self.interface.as_ref().map(|s| Value::str(s))),
            (3, self.member.as_ref().map(|s| Value::str(s))),
            (4, self.error_name.as_ref().map(|s| Value::str(s))),
            (5, self.reply_serial.map(Value::U32)),
            (6, self.destination.as_ref().map(|s| Value::str(s))),
            (7, self.sender.as_ref().map(|s| Value::str(s))),
        ];
        for (code, v) in strings {
            if let Some(v) = v {
                field(code, v);
            }
        }
        let sig: String = self.body.iter().map(Value::signature).collect();
        if !sig.is_empty() {
            field(8, Value::Signature(sig));
        }
        let mut body = Writer::new();
        for v in &self.body {
            body.put(v);
        }
        let mut w = Writer::new();
        w.buf.extend_from_slice(&[b'l', self.kind as u8, self.flags, 1]);
        w.u32(body.buf.len() as u32);
        w.u32(self.serial);
        w.put(&Value::Array("(yv)".into(), fields));
        w.align(8);
        w.buf.extend_from_slice(&body.buf);
        w.buf
    }

    /// The whole message's length, from its first 16 bytes.
    pub fn length(head: &[u8; 16]) -> Result<usize, FormatError> {
        let big = match head[0] {
            b'l' => false,
            b'B' => true,
            c => return Err(bad(format!("byte order {c:#x}"))),
        };
        let word = |i: usize| {
            let b: [u8; 4] = head[i..i + 4].try_into().expect("4 bytes");
            (if big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) }) as usize
        };
        let len = (16 + word(12)).next_multiple_of(8) + word(4);
        if len > MAX_MESSAGE {
            return Err(bad(format!("message of {len} bytes")));
        }
        Ok(len)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, FormatError> {
        let head: &[u8; 16] = buf.get(..16).and_then(|h| h.try_into().ok()).ok_or_else(|| bad("short header"))?;
        if Self::length(head)? != buf.len() {
            return Err(bad("length doesn't match the header"));
        }
        let kind = match buf[1] {
            1 => Kind::MethodCall,
            2 => Kind::Return,
            3 => Kind::Error,
            4 => Kind::Signal,
            n => return Err(bad(format!("message type {n}"))),
        };
        let mut r = Reader::new(buf, buf[0] == b'B');
        r.pos = 8;
        let serial = r.u32()?;
        let mut msg = Message::new(kind);
        msg.flags = buf[2];
        msg.serial = serial;
        let mut sig = String::new();
        let Value::Array(_, fields) = r.value("a(yv)", 0)? else { unreachable!() };
        for f in fields {
            let Value::Struct(f) = f else { continue };
            let (Value::Byte(code), value) = (&f[0], &f[1]) else { continue };
            let text = value.as_str().map(str::to_owned);
            match code {
                1 => msg.path = text,
                2 => msg.interface = text,
                3 => msg.member = text,
                4 => msg.error_name = text,
                5 => msg.reply_serial = value.as_u32(),
                6 => msg.destination = text,
                7 => msg.sender = text,
                8 => sig = text.unwrap_or_default(),
                _ => {}
            }
        }
        r.align(8)?;
        msg.body = r.values(&sig)?;
        if r.pos != buf.len() {
            return Err(bad("body is longer than its signature"));
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(v: &Value) -> Vec<u8> {
        let mut w = Writer::new();
        w.put(v);
        w.buf
    }

    #[test]
    fn encodes_like_the_specification() {
        assert_eq!(bytes(&Value::str("ab")), [2, 0, 0, 0, b'a', b'b', 0]);
        // The padding before the first element doesn't count toward the array's length.
        assert_eq!(
            bytes(&Value::Array("t".into(), vec![Value::U64(1)])),
            [8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(bytes(&Value::Variant(Box::new(Value::U32(7)))), [1, b'u', 0, 0, 7, 0, 0, 0]);
    }

    #[test]
    fn round_trips() {
        let mut msg = Message::new(Kind::Signal);
        msg.serial = 9;
        msg.path = Some("/org/orrery".into());
        msg.interface = Some("org.orrery.Test".into());
        msg.member = Some("Changed".into());
        msg.body = vec![
            Value::str("org.freedesktop.systemd1.Unit"),
            Value::Dict(
                "s".into(),
                "v".into(),
                vec![
                    (Value::str("ActiveState"), Value::Variant(Box::new(Value::str("active")))),
                    (Value::str("Size"), Value::Variant(Box::new(Value::U64(1 << 40)))),
                ],
            ),
            Value::strs(&[]),
            Value::Struct(vec![Value::Byte(1), Value::Bool(true), Value::I16(-2), Value::F64(0.5), Value::path("/")]),
            Value::Array(
                "(sv)".into(),
                vec![Value::Struct(vec![Value::str("x"), Value::Variant(Box::new(Value::I64(-1)))])],
            ),
        ];
        let encoded = msg.encode();
        assert_eq!(Message::length(encoded[..16].try_into().unwrap()).unwrap(), encoded.len());
        assert_eq!(Message::decode(&encoded).unwrap(), msg);
        assert_eq!(msg.body[1].get("ActiveState").and_then(Value::as_str), Some("active"));
    }

    #[test]
    fn refuses_broken_messages() {
        let mut msg = Message::new(Kind::MethodCall);
        msg.member = Some("Hello".into());
        msg.body = vec![Value::strs(&["a", "b"])];
        let good = msg.encode();
        for cut in 16..good.len() {
            assert!(Message::decode(&good[..cut]).is_err());
        }
        assert!(split_type("a(su").is_err());
        assert!(split_type("z").is_err());
        // A variant inside a variant, and so on: refused past the depth limit.
        let mut v = Value::U32(1);
        for _ in 0..100 {
            v = Value::Variant(Box::new(v));
        }
        assert!(Reader::new(&bytes(&v), false).values("v").is_err());
    }
}
