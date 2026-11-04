//! A minimal client for the D-Bus session bus.
//!
//! Enough for what sequencer asks of a desktop: call a method and read its reply, hear
//! the signals it subscribed to, and answer the method calls a KWin script makes to report
//! focus ([`crate::kde`]). It speaks the wire format of the D-Bus specification — protocol
//! version 1, little-endian out, either byte order in — over a Unix socket, and signs in
//! with EXTERNAL, the mechanism session buses use: the bus checks the socket's credentials
//! against the user id sent.
//!
//! Written by hand rather than taken as a dependency (developer's call, 2026-10-27). The
//! maintained pure-Rust client, zbus, is async and adds fifteen crates and up to two
//! megabytes to the binary; rustbus has had no release since 2023 and would bring a second
//! copy of `nix`. The part of the protocol used here has not changed since 2008.

use std::collections::VecDeque;
use std::io::{self, Read as _, Write as _};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The bus daemon itself, as a peer.
const BUS_NAME: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const BUS_INTERFACE: &str = "org.freedesktop.DBus";

/// The specification's limit on one message: 128 MiB. A peer claiming more is broken,
/// and believing it would mean allocating whatever it claims.
const MAX_MESSAGE: usize = 1 << 27;

/// How many unread incoming messages a connection keeps before dropping the oldest. A
/// connection that only makes calls never reads what others send it, and must not grow
/// without bound if someone keeps sending.
const BACKLOG_LIMIT: usize = 256;

/// How deep signatures may nest. The specification allows 32 levels each of arrays and
/// structs; the limit keeps a hostile message from recursing the reader off the stack.
const MAX_DEPTH: usize = 64;

/// Header flag: the caller wants no reply.
const NO_REPLY_EXPECTED: u8 = 0x1;

/// How long to wait on the bus daemon itself, which answers at once: five seconds of
/// silence means it is wedged, not busy.
const BUS_TIMEOUT: Duration = Duration::from_secs(5);

/// What can go wrong talking to the bus.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// No address to connect to.
    #[error("no session bus: $DBUS_SESSION_BUS_ADDRESS is unset and no bus socket was found")]
    NoBus,
    /// The address names no transport this client speaks.
    #[error("no unix socket in the D-Bus address {0:?}")]
    Address(String),
    /// The socket failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The bus refused to sign us in.
    #[error("the bus refused authentication: {0}")]
    Auth(String),
    /// A message broke the wire format.
    #[error("malformed D-Bus message: {0}")]
    Malformed(&'static str),
    /// A value cannot be sent as given.
    #[error("cannot send this value: {0}")]
    Unsendable(&'static str),
    /// The peer answered with an error.
    #[error("{name}: {message}")]
    Remote {
        /// The D-Bus error name, such as `org.freedesktop.DBus.Error.ServiceUnknown`.
        name: String,
        /// Its message, when it sent one.
        message: String,
    },
    /// No answer in time.
    #[error("no reply from the bus within {0:?}")]
    Timeout(Duration),
    /// The bus closed the connection, or it was shut down here.
    #[error("the D-Bus connection closed")]
    Closed,
}

/// One D-Bus value, in or out.
///
/// An array carries its element signature, so an empty one still knows its type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    /// `y`
    Byte(u8),
    /// `b`
    Bool(bool),
    /// `n`
    I16(i16),
    /// `q`
    U16(u16),
    /// `i`
    I32(i32),
    /// `u`
    U32(u32),
    /// `x`
    I64(i64),
    /// `t`
    U64(u64),
    /// `d`
    F64(f64),
    /// `s`
    Str(String),
    /// `o`
    ObjectPath(String),
    /// `g`
    Signature(String),
    /// `h`: an index into the file descriptors sent alongside, which this client never
    /// receives — it is read so a message carrying one still parses.
    UnixFd(u32),
    /// `a…`: the element signature, then the elements.
    Array(String, Vec<Value>),
    /// `(…)`
    Struct(Vec<Value>),
    /// `v`
    Variant(Box<Value>),
    /// `{…}`, only ever inside an array.
    DictEntry(Box<Value>, Box<Value>),
}

impl Value {
    /// A string value.
    pub(crate) fn str(text: &str) -> Self {
        Self::Str(text.to_owned())
    }

    /// The value's D-Bus signature.
    pub(crate) fn signature(&self) -> String {
        match self {
            Self::Byte(_) => "y".into(),
            Self::Bool(_) => "b".into(),
            Self::I16(_) => "n".into(),
            Self::U16(_) => "q".into(),
            Self::I32(_) => "i".into(),
            Self::U32(_) => "u".into(),
            Self::I64(_) => "x".into(),
            Self::U64(_) => "t".into(),
            Self::F64(_) => "d".into(),
            Self::Str(_) => "s".into(),
            Self::ObjectPath(_) => "o".into(),
            Self::Signature(_) => "g".into(),
            Self::UnixFd(_) => "h".into(),
            Self::Array(item, _) => format!("a{item}"),
            Self::Struct(fields) => format!(
                "({})",
                fields.iter().map(Self::signature).collect::<String>()
            ),
            Self::Variant(_) => "v".into(),
            Self::DictEntry(key, value) => format!("{{{}{}}}", key.signature(), value.signature()),
        }
    }

    /// The text of a string-like value.
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(text) | Self::ObjectPath(text) | Self::Signature(text) => Some(text),
            _ => None,
        }
    }

    /// A `u`.
    pub(crate) const fn as_u32(&self) -> Option<u32> {
        match *self {
            Self::U32(number) => Some(number),
            _ => None,
        }
    }

    /// An `i`.
    pub(crate) const fn as_i32(&self) -> Option<i32> {
        match *self {
            Self::I32(number) => Some(number),
            _ => None,
        }
    }

    /// The elements of an array.
    pub(crate) fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(_, items) => Some(items),
            _ => None,
        }
    }

    /// The fields of a struct.
    pub(crate) fn as_struct(&self) -> Option<&[Self]> {
        match self {
            Self::Struct(fields) => Some(fields),
            _ => None,
        }
    }
}

/// What a message is (header byte 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A request.
    MethodCall,
    /// A request's answer.
    MethodReturn,
    /// A request's failure.
    Error,
    /// A broadcast.
    Signal,
}

impl Kind {
    const fn code(self) -> u8 {
        match self {
            Self::MethodCall => 1,
            Self::MethodReturn => 2,
            Self::Error => 3,
            Self::Signal => 4,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::MethodCall),
            2 => Some(Self::MethodReturn),
            3 => Some(Self::Error),
            4 => Some(Self::Signal),
            _ => None,
        }
    }
}

/// A message, as received or about to be sent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Message {
    /// What it is.
    pub(crate) kind: Kind,
    /// Header flags.
    pub(crate) flags: u8,
    /// The sender's number for it; replies quote it.
    pub(crate) serial: u32,
    /// The object it concerns (calls and signals).
    pub(crate) path: Option<String>,
    /// The interface of the member.
    pub(crate) interface: Option<String>,
    /// The method or signal name.
    pub(crate) member: Option<String>,
    /// The error's name (errors).
    pub(crate) error_name: Option<String>,
    /// The call this answers (replies and errors).
    pub(crate) reply_serial: Option<u32>,
    /// Who it is for.
    pub(crate) destination: Option<String>,
    /// Who sent it, as the bus stamps it.
    pub(crate) sender: Option<String>,
    /// The arguments.
    pub(crate) body: Vec<Value>,
}

impl Message {
    /// A message of `kind` with every header field unset and no body.
    const fn new(kind: Kind) -> Self {
        Self {
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

    /// A method call with no header fields beyond its address.
    pub(crate) fn call(
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        body: Vec<Value>,
    ) -> Self {
        Self {
            path: Some(path.to_owned()),
            interface: Some(interface.to_owned()),
            member: Some(member.to_owned()),
            destination: Some(destination.to_owned()),
            body,
            ..Self::new(Kind::MethodCall)
        }
    }

    /// The answer to `call`, a return or an error, addressed back to its sender.
    fn answer(call: &Self, kind: Kind, body: Vec<Value>) -> Self {
        Self {
            reply_serial: Some(call.serial),
            destination: call.sender.clone(),
            body,
            ..Self::new(kind)
        }
    }

    /// Whether this is a call of `interface.member` on `path`.
    pub(crate) fn is_call(&self, path: &str, interface: &str, member: &str) -> bool {
        self.kind == Kind::MethodCall
            && self.path.as_deref() == Some(path)
            && self.interface.as_deref() == Some(interface)
            && self.member.as_deref() == Some(member)
    }

    /// Whether this is the signal `interface.member`.
    pub(crate) fn is_signal(&self, interface: &str, member: &str) -> bool {
        self.kind == Kind::Signal
            && self.interface.as_deref() == Some(interface)
            && self.member.as_deref() == Some(member)
    }

    /// The bytes on the wire, numbered `serial`.
    fn encode(&self, serial: u32) -> Result<Vec<u8>, Error> {
        let mut body = Writer::default();
        for value in &self.body {
            body.value(value, 0)?;
        }
        let signature: String = self.body.iter().map(Value::signature).collect();
        if signature.len() > 255 {
            return Err(Error::Unsendable(
                "a body signature longer than 255 characters",
            ));
        }

        let mut fields: Vec<(u8, Value)> = Vec::new();
        let strings = [
            (
                1,
                self.path
                    .as_ref()
                    .map(|path| Value::ObjectPath(path.clone())),
            ),
            (
                2,
                self.interface.as_ref().map(|name| Value::Str(name.clone())),
            ),
            (3, self.member.as_ref().map(|name| Value::Str(name.clone()))),
            (
                4,
                self.error_name
                    .as_ref()
                    .map(|name| Value::Str(name.clone())),
            ),
            (5, self.reply_serial.map(Value::U32)),
            (
                6,
                self.destination
                    .as_ref()
                    .map(|name| Value::Str(name.clone())),
            ),
            (7, self.sender.as_ref().map(|name| Value::Str(name.clone()))),
        ];
        fields.extend(
            strings
                .into_iter()
                .filter_map(|(code, value)| Some((code, value?))),
        );
        if !signature.is_empty() {
            fields.push((8, Value::Signature(signature)));
        }

        let mut out = Writer::default();
        out.byte(b'l');
        out.byte(self.kind.code());
        out.byte(self.flags);
        out.byte(1);
        let body_len =
            u32::try_from(body.bytes.len()).map_err(|_| Error::Unsendable("a body over 4 GiB"))?;
        out.u32(body_len);
        out.u32(serial);
        let header_fields = Value::Array(
            "(yv)".into(),
            fields
                .into_iter()
                .map(|(code, value)| {
                    Value::Struct(vec![Value::Byte(code), Value::Variant(Box::new(value))])
                })
                .collect(),
        );
        out.value(&header_fields, 0)?;
        out.pad(8);
        out.bytes.extend_from_slice(&body.bytes);
        if out.bytes.len() > MAX_MESSAGE {
            return Err(Error::Unsendable("a message over the 128 MiB limit"));
        }
        Ok(out.bytes)
    }

    /// Parses one whole message.
    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let big_endian = match bytes.first() {
            Some(b'l') => false,
            Some(b'B') => true,
            _ => return Err(Error::Malformed("unknown byte order")),
        };
        let mut header = Reader::new(bytes, big_endian);
        header.skip(1)?;
        let kind =
            Kind::from_code(header.byte()?).ok_or(Error::Malformed("unknown message type"))?;
        let flags = header.byte()?;
        if header.byte()? != 1 {
            return Err(Error::Malformed("not protocol version 1"));
        }
        let body_len = header.u32()? as usize;
        let serial = header.u32()?;
        let fields = header.value("a(yv)", 0)?;

        let mut message = Self {
            flags,
            serial,
            ..Self::new(kind)
        };
        let mut signature = String::new();
        for field in fields.as_array().unwrap_or_default() {
            let Some([Value::Byte(code), Value::Variant(value)]) = field.as_struct() else {
                return Err(Error::Malformed("a header field is not (yv)"));
            };
            let text = value.as_str().map(str::to_owned);
            match code {
                1 => message.path = text,
                2 => message.interface = text,
                3 => message.member = text,
                4 => message.error_name = text,
                5 => message.reply_serial = value.as_u32(),
                6 => message.destination = text,
                7 => message.sender = text,
                8 => signature = text.unwrap_or_default(),
                // UNIX_FDS, and codes from later versions: not needed here, and the
                // specification says to ignore what is not understood.
                _ => {}
            }
        }
        header.align(8)?;
        let body_start = header.pos;
        let body_bytes = bytes
            .get(body_start..body_start + body_len)
            .ok_or(Error::Malformed(
                "the body is shorter than its declared length",
            ))?;
        let mut reader = Reader::new(body_bytes, big_endian);
        let mut rest = signature.as_str();
        while !rest.is_empty() {
            let (single, tail) = split_type(rest)?;
            message.body.push(reader.value(single, 0)?);
            rest = tail;
        }
        Ok(message)
    }
}

/// Marshals values in this client's byte order, little-endian.
#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// Zero bytes up to the next multiple of `align` — measured from the start of the
    /// buffer, which is always 8-aligned within the message.
    fn pad(&mut self, align: usize) {
        while !self.bytes.len().is_multiple_of(align) {
            self.bytes.push(0);
        }
    }

    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.pad(2);
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.pad(4);
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.pad(8);
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, text: &str) -> Result<(), Error> {
        if text.contains('\0') {
            return Err(Error::Unsendable("a string containing NUL"));
        }
        let len =
            u32::try_from(text.len()).map_err(|_| Error::Unsendable("a string over 4 GiB"))?;
        self.u32(len);
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes.push(0);
        Ok(())
    }

    fn signature(&mut self, text: &str) -> Result<(), Error> {
        let len = u8::try_from(text.len())
            .map_err(|_| Error::Unsendable("a signature over 255 bytes"))?;
        self.byte(len);
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes.push(0);
        Ok(())
    }

    fn value(&mut self, value: &Value, depth: usize) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Unsendable("a value nested too deeply"));
        }
        match value {
            Value::Byte(byte) => self.byte(*byte),
            Value::Bool(flag) => self.u32(u32::from(*flag)),
            Value::I16(number) => self.u16(number.cast_unsigned()),
            Value::U16(number) => self.u16(*number),
            Value::I32(number) => self.u32(number.cast_unsigned()),
            Value::U32(number) | Value::UnixFd(number) => self.u32(*number),
            Value::I64(number) => self.u64(number.cast_unsigned()),
            Value::U64(number) => self.u64(*number),
            Value::F64(number) => self.u64(number.to_bits()),
            Value::Str(text) | Value::ObjectPath(text) => self.string(text)?,
            Value::Signature(text) => self.signature(text)?,
            Value::Array(item, items) => {
                self.u32(0);
                let len_at = self.bytes.len() - 4;
                self.pad(alignment(item));
                let start = self.bytes.len();
                for element in items {
                    if element.signature() != *item {
                        return Err(Error::Unsendable("an array element of the wrong type"));
                    }
                    self.value(element, depth + 1)?;
                }
                let len = u32::try_from(self.bytes.len() - start)
                    .map_err(|_| Error::Unsendable("an array over 4 GiB"))?;
                self.bytes[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
            }
            Value::Struct(fields) => {
                if fields.is_empty() {
                    return Err(Error::Unsendable("an empty struct"));
                }
                self.pad(8);
                for field in fields {
                    self.value(field, depth + 1)?;
                }
            }
            Value::Variant(inner) => {
                self.signature(&inner.signature())?;
                self.value(inner, depth + 1)?;
            }
            Value::DictEntry(key, inner) => {
                self.pad(8);
                self.value(key, depth + 1)?;
                self.value(inner, depth + 1)?;
            }
        }
        Ok(())
    }
}

/// Reads values in either byte order.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    big_endian: bool,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8], big_endian: bool) -> Self {
        Self {
            bytes,
            pos: 0,
            big_endian,
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let slice = self
            .bytes
            .get(self.pos..self.pos + len)
            .ok_or(Error::Malformed("a value runs past the end of the message"))?;
        self.pos += len;
        Ok(slice)
    }

    fn skip(&mut self, len: usize) -> Result<(), Error> {
        self.take(len).map(drop)
    }

    fn align(&mut self, align: usize) -> Result<(), Error> {
        let padding = self.pos.next_multiple_of(align) - self.pos;
        self.skip(padding)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        self.align(2)?;
        let raw: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| Error::Malformed("u16"))?;
        Ok(if self.big_endian {
            u16::from_be_bytes(raw)
        } else {
            u16::from_le_bytes(raw)
        })
    }

    fn u32(&mut self) -> Result<u32, Error> {
        self.align(4)?;
        let raw: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| Error::Malformed("u32"))?;
        Ok(if self.big_endian {
            u32::from_be_bytes(raw)
        } else {
            u32::from_le_bytes(raw)
        })
    }

    fn u64(&mut self) -> Result<u64, Error> {
        self.align(8)?;
        let raw: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| Error::Malformed("u64"))?;
        Ok(if self.big_endian {
            u64::from_be_bytes(raw)
        } else {
            u64::from_le_bytes(raw)
        })
    }

    fn text(&mut self, len: usize) -> Result<String, Error> {
        let raw = self.take(len)?;
        if self.byte()? != 0 {
            return Err(Error::Malformed("a string without its NUL"));
        }
        String::from_utf8(raw.to_vec()).map_err(|_| Error::Malformed("a string that is not UTF-8"))
    }

    fn string(&mut self) -> Result<String, Error> {
        let len = self.u32()? as usize;
        self.text(len)
    }

    fn signature(&mut self) -> Result<String, Error> {
        let len = usize::from(self.byte()?);
        self.text(len)
    }

    /// Reads one value of the single complete type `signature`.
    fn value(&mut self, signature: &str, depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Malformed("a value nested too deeply"));
        }
        let Some(code) = signature.as_bytes().first() else {
            return Err(Error::Malformed("an empty type"));
        };
        Ok(match code {
            b'y' => Value::Byte(self.byte()?),
            b'b' => match self.u32()? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return Err(Error::Malformed("a boolean that is neither 0 nor 1")),
            },
            b'n' => Value::I16(self.u16()?.cast_signed()),
            b'q' => Value::U16(self.u16()?),
            b'i' => Value::I32(self.u32()?.cast_signed()),
            b'u' => Value::U32(self.u32()?),
            b'h' => Value::UnixFd(self.u32()?),
            b'x' => Value::I64(self.u64()?.cast_signed()),
            b't' => Value::U64(self.u64()?),
            b'd' => Value::F64(f64::from_bits(self.u64()?)),
            b's' => Value::Str(self.string()?),
            b'o' => Value::ObjectPath(self.string()?),
            b'g' => Value::Signature(self.signature()?),
            b'v' => {
                let inner = self.signature()?;
                let (single, rest) = split_type(&inner)?;
                if !rest.is_empty() {
                    return Err(Error::Malformed("a variant holding more than one type"));
                }
                Value::Variant(Box::new(self.value(single, depth + 1)?))
            }
            b'a' => {
                let item = &signature[1..];
                let len = self.u32()? as usize;
                if len > MAX_MESSAGE {
                    return Err(Error::Malformed("an array longer than a message can be"));
                }
                self.align(alignment(item))?;
                let end = self.pos + len;
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.value(item, depth + 1)?);
                }
                if self.pos != end {
                    return Err(Error::Malformed(
                        "an array whose elements overrun its length",
                    ));
                }
                Value::Array(item.to_owned(), items)
            }
            b'(' => {
                self.align(8)?;
                let mut fields = Vec::new();
                let mut rest = &signature[1..signature.len() - 1];
                while !rest.is_empty() {
                    let (single, tail) = split_type(rest)?;
                    fields.push(self.value(single, depth + 1)?);
                    rest = tail;
                }
                Value::Struct(fields)
            }
            b'{' => {
                self.align(8)?;
                let (key, rest) = split_type(&signature[1..signature.len() - 1])?;
                let key = self.value(key, depth + 1)?;
                let (inner, _) = split_type(rest)?;
                Value::DictEntry(Box::new(key), Box::new(self.value(inner, depth + 1)?))
            }
            _ => return Err(Error::Malformed("an unknown type code")),
        })
    }
}

/// The alignment of a value whose signature starts with `signature`'s first code.
fn alignment(signature: &str) -> usize {
    match signature.as_bytes().first() {
        Some(b'n' | b'q') => 2,
        Some(b'b' | b'i' | b'u' | b'h' | b's' | b'o' | b'a') => 4,
        Some(b'x' | b't' | b'd' | b'(' | b'{') => 8,
        _ => 1,
    }
}

/// Splits the first single complete type off a signature: `a(ss)u` into `a(ss)` and `u`.
fn split_type(signature: &str) -> Result<(&str, &str), Error> {
    let bytes = signature.as_bytes();
    let mut end = 0;
    // Array prefixes, then one element.
    while bytes.get(end) == Some(&b'a') {
        end += 1;
        if end > MAX_DEPTH {
            return Err(Error::Malformed("a signature nested too deeply"));
        }
    }
    match bytes.get(end) {
        Some(b'(' | b'{') => {
            let mut depth = 0_usize;
            loop {
                match bytes.get(end) {
                    Some(b'(' | b'{') => depth += 1,
                    Some(b')' | b'}') => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Some(_) => {}
                    None => return Err(Error::Malformed("an unclosed struct in a signature")),
                }
                end += 1;
                if depth > MAX_DEPTH {
                    return Err(Error::Malformed("a signature nested too deeply"));
                }
            }
            end += 1;
        }
        Some(
            b'y' | b'b' | b'n' | b'q' | b'i' | b'u' | b'x' | b't' | b'd' | b'h' | b's' | b'o'
            | b'g' | b'v',
        ) => {
            end += 1;
        }
        _ => return Err(Error::Malformed("an invalid signature")),
    }
    Ok(signature.split_at(end))
}

/// Where a bus listens.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Endpoint {
    /// A socket file.
    Path(PathBuf),
    /// A Linux abstract socket name.
    Abstract(Vec<u8>),
}

/// The unix sockets a D-Bus address names, in the order to try them.
///
/// An address is `transport:key=value,…`, several separated by `;`, with values
/// percent-escaped. Only `unix:` with `path=` or `abstract=` is something a client can
/// connect to; the rest (`tcp:`, `launchd:`, `tmpdir=` — which is for listening) is
/// passed over.
fn endpoints(address: &str) -> Vec<Endpoint> {
    let mut found = Vec::new();
    for entry in address.split(';') {
        let Some(params) = entry.strip_prefix("unix:") else {
            continue;
        };
        for param in params.split(',') {
            let Some((key, value)) = param.split_once('=') else {
                continue;
            };
            let value = unescape(value);
            match key {
                "path" => found.push(Endpoint::Path(PathBuf::from(
                    String::from_utf8_lossy(&value).into_owned(),
                ))),
                "abstract" => found.push(Endpoint::Abstract(value)),
                _ => {}
            }
        }
    }
    found
}

/// Undoes D-Bus address escaping: `%xx` is a byte, everything else stands for itself.
fn unescape(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = value
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// This process's effective user id, from `/proc/self/status` — readable even after a
/// process drops root, when the ownership of the rest of `/proc/self` changes.
pub(crate) fn effective_uid() -> Result<u32, Error> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().nth(1))
        .and_then(|uid| uid.parse().ok())
        .ok_or(Error::Malformed("no effective uid in /proc/self/status"))
}

/// Where the session bus listens: `$DBUS_SESSION_BUS_ADDRESS`, else the socket systemd
/// and dbus-broker put in the user's runtime directory. The fallback matters under sudo,
/// which drops that variable but leaves the socket where it was.
fn session_endpoints() -> Result<Vec<Endpoint>, Error> {
    if let Some(address) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        let address = address.to_string_lossy();
        let found = endpoints(&address);
        return if found.is_empty() {
            Err(Error::Address(address.into_owned()))
        } else {
            Ok(found)
        };
    }
    let socket = runtime_dir()?.join("bus");
    if socket.exists() {
        Ok(vec![Endpoint::Path(socket)])
    } else {
        Err(Error::NoBus)
    }
}

/// Where the system bus listens: `$DBUS_SYSTEM_BUS_ADDRESS`, else the socket the D-Bus
/// specification names.
#[cfg(feature = "evdev")]
fn system_endpoints() -> Result<Vec<Endpoint>, Error> {
    if let Some(address) = std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS") {
        let address = address.to_string_lossy();
        let found = endpoints(&address);
        return if found.is_empty() {
            Err(Error::Address(address.into_owned()))
        } else {
            Ok(found)
        };
    }
    Ok(vec![Endpoint::Path(PathBuf::from(
        "/var/run/dbus/system_bus_socket",
    ))])
}

/// The user's runtime directory: `$XDG_RUNTIME_DIR`, else `/run/user/<uid>`, where
/// systemd makes it — the same place, when sudo has dropped the variable.
pub(crate) fn runtime_dir() -> Result<PathBuf, Error> {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(PathBuf::from(format!("/run/user/{}", effective_uid()?))),
    }
}

fn connect(endpoint: &Endpoint) -> io::Result<UnixStream> {
    match endpoint {
        Endpoint::Path(path) => UnixStream::connect(path),
        Endpoint::Abstract(name) => {
            use std::os::linux::net::SocketAddrExt as _;
            let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            UnixStream::connect_addr(&address)
        }
    }
}

/// A way to close a connection from another thread, waking whatever is blocked reading
/// it.
#[derive(Debug)]
pub(crate) struct Closer(UnixStream);

impl Closer {
    /// Shuts the socket down; the connection's next read reports [`Error::Closed`].
    pub(crate) fn close(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// One connection to a bus, signed in and named.
#[derive(Debug)]
pub(crate) struct Connection {
    stream: UnixStream,
    /// Bytes read but not yet a whole message.
    inbuf: Vec<u8>,
    /// Messages that arrived while a call waited for its reply — calls to us and
    /// signals — handed out in order by [`Self::next_message`].
    backlog: VecDeque<Message>,
    next_serial: u32,
    unique_name: String,
}

impl Connection {
    /// Connects to the session bus.
    pub(crate) fn session() -> Result<Self, Error> {
        Self::open(&session_endpoints()?)
    }

    /// Connects to the system bus, where logind answers.
    #[cfg(feature = "evdev")]
    pub(crate) fn system() -> Result<Self, Error> {
        Self::open(&system_endpoints()?)
    }

    /// Connects to the bus at a D-Bus address.
    #[cfg(test)]
    pub(crate) fn at(address: &str) -> Result<Self, Error> {
        let found = endpoints(address);
        if found.is_empty() {
            return Err(Error::Address(address.to_owned()));
        }
        Self::open(&found)
    }

    fn open(endpoints: &[Endpoint]) -> Result<Self, Error> {
        let mut last = Error::NoBus;
        for endpoint in endpoints {
            match connect(endpoint) {
                Ok(stream) => return Self::sign_in(stream),
                Err(err) => last = Error::Io(err),
            }
        }
        Err(last)
    }

    /// Authenticates and says Hello, which is what makes the bus give this connection a
    /// name and deliver to it.
    fn sign_in(stream: UnixStream) -> Result<Self, Error> {
        stream.set_write_timeout(Some(BUS_TIMEOUT))?;
        let mut connection = Self {
            stream,
            inbuf: Vec::new(),
            backlog: VecDeque::new(),
            next_serial: 1,
            unique_name: String::new(),
        };
        let uid = effective_uid()?;
        let hex = uid
            .to_string()
            .bytes()
            .fold(String::new(), |mut hex, byte| {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
                hex
            });
        connection.stream.write_all(b"\0")?;
        connection
            .stream
            .write_all(format!("AUTH EXTERNAL {hex}\r\n").as_bytes())?;
        let answer = connection.read_line(Instant::now() + BUS_TIMEOUT)?;
        if !answer.starts_with("OK ") {
            return Err(Error::Auth(answer));
        }
        connection.stream.write_all(b"BEGIN\r\n")?;
        let name = connection.bus("Hello", Vec::new())?;
        name.first()
            .and_then(Value::as_str)
            .ok_or(Error::Malformed("Hello answered without a name"))?
            .clone_into(&mut connection.unique_name);
        Ok(connection)
    }

    /// The name the bus gave this connection, such as `:1.42`.
    pub(crate) fn unique_name(&self) -> &str {
        &self.unique_name
    }

    /// A [`Closer`] for this connection.
    pub(crate) fn closer(&self) -> Result<Closer, Error> {
        Ok(Closer(self.stream.try_clone()?))
    }

    /// Calls `interface.member` on `path` at `destination` and waits for the answer.
    ///
    /// Whatever else arrives meanwhile — calls to this connection, signals — is kept for
    /// [`Self::next_message`].
    pub(crate) fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> Result<Vec<Value>, Error> {
        let serial = self.send(&Message::call(destination, path, interface, member, args))?;
        let deadline = Instant::now() + timeout;
        loop {
            let Some(message) = self.read_message(Some(deadline))? else {
                return Err(Error::Timeout(timeout));
            };
            if message.reply_serial == Some(serial) {
                return match message.kind {
                    Kind::MethodReturn => Ok(message.body),
                    _ => Err(Error::Remote {
                        name: message.error_name.unwrap_or_default(),
                        message: message
                            .body
                            .first()
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    }),
                };
            }
            if matches!(message.kind, Kind::MethodCall | Kind::Signal) {
                if self.backlog.len() == BACKLOG_LIMIT {
                    self.backlog.pop_front();
                }
                self.backlog.push_back(message);
            }
        }
    }

    /// The next call to this connection or signal it subscribed to, waiting up to
    /// `timeout` (`None`: until one arrives, or the connection closes).
    pub(crate) fn next_message(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<Message>, Error> {
        if let Some(message) = self.backlog.pop_front() {
            return Ok(Some(message));
        }
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            let Some(message) = self.read_message(deadline)? else {
                return Ok(None);
            };
            // A reply here answers a call that already gave up waiting.
            if matches!(message.kind, Kind::MethodCall | Kind::Signal) {
                return Ok(Some(message));
            }
        }
    }

    /// Answers `call` with `args`, unless it asked for no answer.
    pub(crate) fn reply(&mut self, call: &Message, args: Vec<Value>) -> Result<(), Error> {
        if call.flags & NO_REPLY_EXPECTED != 0 {
            return Ok(());
        }
        self.send(&Message::answer(call, Kind::MethodReturn, args))
            .map(drop)
    }

    /// Answers a call this connection does not serve with the standard error, so the
    /// caller is told rather than left to time out. Signals need no answer.
    pub(crate) fn decline(&mut self, message: &Message) -> Result<(), Error> {
        if message.kind != Kind::MethodCall || message.flags & NO_REPLY_EXPECTED != 0 {
            return Ok(());
        }
        let text = vec![Value::str("sequencer serves no such method")];
        self.send(&Message {
            error_name: Some("org.freedesktop.DBus.Error.UnknownMethod".into()),
            ..Message::answer(message, Kind::Error, text)
        })
        .map(drop)
    }

    /// Asks the bus to deliver the signals `rule` matches.
    pub(crate) fn add_match(&mut self, rule: &str) -> Result<(), Error> {
        self.bus("AddMatch", vec![Value::str(rule)]).map(drop)
    }

    /// The unique name owning `name` right now, or `None` when nobody does.
    pub(crate) fn name_owner(&mut self, name: &str) -> Result<Option<String>, Error> {
        match self.bus("GetNameOwner", vec![Value::str(name)]) {
            Ok(reply) => Ok(reply.first().and_then(Value::as_str).map(str::to_owned)),
            Err(Error::Remote { name: error, .. })
                if error == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
            {
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Claims a well-known name, as a fake service in a test does.
    #[cfg(test)]
    pub(crate) fn request_name(&mut self, name: &str) -> Result<(), Error> {
        // Flags 0x4, DO_NOT_QUEUE: fail rather than wait in line.
        let reply = self.bus("RequestName", vec![Value::str(name), Value::U32(4)])?;
        match reply.first().and_then(Value::as_u32) {
            Some(1) => Ok(()),
            _ => Err(Error::Malformed("the name was not granted")),
        }
    }

    /// Broadcasts a signal, as a fake service in a test does.
    #[cfg(test)]
    pub(crate) fn emit(
        &mut self,
        path: &str,
        interface: &str,
        member: &str,
        args: Vec<Value>,
    ) -> Result<(), Error> {
        self.send(&Message {
            flags: NO_REPLY_EXPECTED,
            path: Some(path.to_owned()),
            interface: Some(interface.to_owned()),
            member: Some(member.to_owned()),
            body: args,
            ..Message::new(Kind::Signal)
        })
        .map(drop)
    }

    /// Calls a method of the bus daemon itself.
    fn bus(&mut self, member: &str, args: Vec<Value>) -> Result<Vec<Value>, Error> {
        self.call(BUS_NAME, BUS_PATH, BUS_INTERFACE, member, args, BUS_TIMEOUT)
    }

    /// Sends a message, numbering it, without waiting for anything; returns the number.
    pub(crate) fn send(&mut self, message: &Message) -> Result<u32, Error> {
        let serial = self.next_serial;
        // Zero is not a valid serial, and a connection that sent four billion messages
        // has long since had its early replies.
        self.next_serial = self.next_serial.checked_add(1).unwrap_or(1);
        let bytes = message.encode(serial)?;
        self.stream
            .write_all(&bytes)
            .map_err(|err| match err.kind() {
                io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected => Error::Closed,
                _ => Error::Io(err),
            })?;
        Ok(serial)
    }

    /// One whole message, or `None` once `deadline` passes first.
    fn read_message(&mut self, deadline: Option<Instant>) -> Result<Option<Message>, Error> {
        loop {
            if let Some(len) = whole_message_len(&self.inbuf)? {
                let bytes: Vec<u8> = self.inbuf.drain(..len).collect();
                match Message::decode(&bytes) {
                    Ok(message) => return Ok(Some(message)),
                    // Framing held, so the stream is still in step; only this message is
                    // lost, and it cannot have been one worth having.
                    Err(err) => {
                        log::debug!("dropped an unreadable D-Bus message err={err}");
                        continue;
                    }
                }
            }
            if !self.fill(deadline)? {
                return Ok(None);
            }
        }
    }

    /// One line of the sign-in conversation, without its CR LF.
    fn read_line(&mut self, deadline: Instant) -> Result<String, Error> {
        loop {
            if let Some(end) = self.inbuf.windows(2).position(|pair| pair == b"\r\n") {
                let line: Vec<u8> = self.inbuf.drain(..end + 2).collect();
                return Ok(String::from_utf8_lossy(&line[..end]).into_owned());
            }
            if self.inbuf.len() > 4096 {
                return Err(Error::Auth("an overlong line during sign-in".into()));
            }
            if !self.fill(Some(deadline))? {
                return Err(Error::Timeout(BUS_TIMEOUT));
            }
        }
    }

    /// Reads whatever has arrived into the buffer; `false` if `deadline` passed first.
    fn fill(&mut self, deadline: Option<Instant>) -> Result<bool, Error> {
        let timeout = match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(false);
                }
                Some(left)
            }
            None => None,
        };
        self.stream.set_read_timeout(timeout)?;
        let mut chunk = [0_u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(Error::Closed),
                Ok(len) => {
                    self.inbuf.extend_from_slice(&chunk[..len]);
                    return Ok(true);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(false);
                }
                Err(err) => return Err(Error::Io(err)),
            }
        }
    }
}

/// How long the message at the start of `buffer` is, once enough of it has arrived to
/// tell and all of it is there.
fn whole_message_len(buffer: &[u8]) -> Result<Option<usize>, Error> {
    let Some(fixed) = buffer.get(..16) else {
        return Ok(None);
    };
    let read = |at: usize| {
        let raw: [u8; 4] = fixed[at..at + 4].try_into().unwrap_or_default();
        match fixed[0] {
            b'B' => u32::from_be_bytes(raw),
            _ => u32::from_le_bytes(raw),
        }
    };
    if !matches!(fixed[0], b'l' | b'B') {
        return Err(Error::Malformed("unknown byte order"));
    }
    let body = read(4) as usize;
    let fields = read(12) as usize;
    let total = (16 + fields).next_multiple_of(8) + body;
    if total > MAX_MESSAGE {
        return Err(Error::Malformed("a message over the 128 MiB limit"));
    }
    Ok((buffer.len() >= total).then_some(total))
}

/// A private bus for tests: a bus daemon of its own in a temporary directory, so nothing
/// a test does reaches the user's session bus. Stopped when dropped.
#[cfg(test)]
pub(crate) mod test_bus {
    use std::io::BufRead as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    /// The daemon `SEQUENCER_TEST_BUS_DAEMON` names, else `dbus-daemon` from `PATH`.
    fn daemon() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("SEQUENCER_TEST_BUS_DAEMON") {
            return Some(PathBuf::from(path));
        }
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("dbus-daemon"))
                .find(|candidate| candidate.is_file())
        })
    }

    /// A running private bus.
    pub(crate) struct TestBus {
        child: Child,
        dir: PathBuf,
        /// Its D-Bus address.
        pub(crate) address: String,
    }

    impl TestBus {
        /// Starts one, or `None` with a note when there is no daemon to run — the tests
        /// that need it then pass vacuously, the way the uinput tests do without
        /// `/dev/uinput`.
        pub(crate) fn start() -> Option<Self> {
            let Some(daemon) = daemon() else {
                eprintln!(
                    "skipping: no dbus-daemon on PATH (SEQUENCER_TEST_BUS_DAEMON may name another bus daemon)"
                );
                return None;
            };
            let dir = std::env::temp_dir().join(format!(
                "sequencer-test-bus-{}-{}",
                std::process::id(),
                next_id()
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let config = dir.join("session.conf");
            std::fs::write(&config, config_text(&dir.join("bus"))).expect("bus config");
            // dbus-daemon needs --nofork to stay our child; busd never forks.
            let mut command = Command::new(&daemon);
            if daemon.file_name().is_some_and(|name| name == "dbus-daemon") {
                command
                    .arg(format!("--config-file={}", config.display()))
                    .arg("--nofork")
                    .arg("--print-address");
            } else {
                command.arg("--config").arg(&config).arg("--print-address");
            }
            let mut child = command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("the bus daemon starts");
            let mut line = String::new();
            let stdout = child.stdout.take().expect("piped stdout");
            std::io::BufReader::new(stdout)
                .read_line(&mut line)
                .expect("the daemon prints its address");
            assert!(!line.trim().is_empty(), "the bus daemon printed no address");
            Some(Self {
                child,
                dir,
                address: line.trim().to_owned(),
            })
        }

        /// A new connection to it.
        pub(crate) fn connect(&self) -> super::Connection {
            super::Connection::at(&self.address).expect("connects to the private bus")
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn next_id() -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    /// A session bus that lets everyone send to and own anything — it exists for one test.
    fn config_text(socket: &Path) -> String {
        format!(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\"\n \
             \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
             <busconfig>\n  <type>session</type>\n  <listen>unix:path={}</listen>\n  \
             <auth>EXTERNAL</auth>\n  <policy context=\"default\">\n    \
             <allow send_destination=\"*\" eavesdrop=\"true\"/>\n    <allow eavesdrop=\"true\"/>\n    \
             <allow own=\"*\"/>\n  </policy>\n</busconfig>\n",
            socket.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::test_bus::TestBus;
    use super::*;

    // ------------------------------------------------------------ the wire format

    /// The Hello every client sends first, as it appears in any trace of a libdbus
    /// client: header fields PATH, INTERFACE, MEMBER and DESTINATION, each 8-aligned,
    /// filling a 109-byte (`m`, 0x6d) field array. Written out here rather than built, so
    /// the encoder is checked against the specification's layout, not against itself.
    #[test]
    fn hello_is_the_bytes_every_client_sends() {
        let expected: &[u8] = b"l\x01\x00\x01\x00\x00\x00\x00\x01\x00\x00\x00m\x00\x00\x00\
            \x01\x01o\x00\x15\x00\x00\x00/org/freedesktop/DBus\x00\x00\x00\
            \x02\x01s\x00\x14\x00\x00\x00org.freedesktop.DBus\x00\x00\x00\x00\
            \x03\x01s\x00\x05\x00\x00\x00Hello\x00\x00\x00\
            \x06\x01s\x00\x14\x00\x00\x00org.freedesktop.DBus\x00\x00\x00\x00";
        let hello = Message::call(BUS_NAME, BUS_PATH, BUS_INTERFACE, "Hello", Vec::new());
        assert_eq!(hello.encode(1).unwrap(), expected);
    }

    /// A big-endian reply, assembled by hand: peers may send either byte order, and the
    /// bus passes theirs through. Also pins the alignment of a header field's value
    /// after its variant signature, and the padding before the body.
    #[test]
    fn a_big_endian_reply_decodes() {
        let bytes = [
            b'B', 2, 0, 1, 0, 0, 0, 4, 0, 0, 0, 7, 0, 0, 0, 15, // fixed part
            5, 1, b'u', 0, 0, 0, 0, 3, // REPLY_SERIAL = 3
            8, 1, b'g', 0, 1, b'u', 0, // SIGNATURE = "u"
            0, // padding to the body
            0, 0, 0, 42, // the body: 42
        ];
        assert_eq!(whole_message_len(&bytes).unwrap(), Some(bytes.len()));
        let message = Message::decode(&bytes).unwrap();
        assert_eq!(message.kind, Kind::MethodReturn);
        assert_eq!(message.serial, 7);
        assert_eq!(message.reply_serial, Some(3));
        assert_eq!(message.body, vec![Value::U32(42)]);
    }

    /// Every kind of value survives a round trip, nested ones and an empty array
    /// included — the shapes KDE's replies come in (`a(sss)`) among them.
    #[test]
    fn every_type_round_trips() {
        let body = vec![
            Value::Byte(7),
            Value::Bool(true),
            Value::I16(-2),
            Value::U16(3),
            Value::I32(-40_000),
            Value::U32(4_000_000_000),
            Value::I64(-5_000_000_000),
            Value::U64(u64::MAX),
            Value::F64(1.5),
            Value::str("hello"),
            Value::ObjectPath("/a/b".into()),
            Value::Signature("a(sss)".into()),
            Value::Array(
                "(sss)".into(),
                vec![
                    Value::Struct(vec![
                        Value::str("us"),
                        Value::str(""),
                        Value::str("English (US)"),
                    ]),
                    Value::Struct(vec![Value::str("il"), Value::str(""), Value::str("Hebrew")]),
                ],
            ),
            Value::Array(
                "{sv}".into(),
                vec![Value::DictEntry(
                    Box::new(Value::str("key")),
                    Box::new(Value::Variant(Box::new(Value::Struct(vec![
                        Value::Byte(1),
                        Value::U64(2),
                    ])))),
                )],
            ),
            Value::Array("s".into(), Vec::new()),
            Value::Byte(9),
        ];
        let mut message = Message::call(
            "org.example",
            "/org/example",
            "org.example.I",
            "M",
            body.clone(),
        );
        message.sender = Some(":1.5".into());
        let decoded = Message::decode(&message.encode(9).unwrap()).unwrap();
        assert_eq!(decoded.body, body);
        assert_eq!(decoded.sender.as_deref(), Some(":1.5"));
        assert_eq!(decoded.serial, 9);
        assert!(decoded.is_call("/org/example", "org.example.I", "M"));
    }

    #[test]
    fn signatures_split_one_complete_type_at_a_time() {
        assert_eq!(split_type("a(ss)u").unwrap(), ("a(ss)", "u"));
        assert_eq!(split_type("((s)s)i").unwrap(), ("((s)s)", "i"));
        assert_eq!(split_type("a{sv}").unwrap(), ("a{sv}", ""));
        assert_eq!(split_type("aai").unwrap(), ("aai", ""));
        for bad in ["", "(s", "z", "a"] {
            assert!(split_type(bad).is_err(), "{bad:?} is not a signature");
        }
    }

    /// What the reader refuses instead of misreading: no panic on anything a peer sends.
    #[test]
    fn malformed_messages_are_refused() {
        let good = Message::call("a.b", "/a", "a.b", "M", vec![Value::Bool(true)])
            .encode(1)
            .unwrap();
        let truncated = &good[..good.len() - 1];
        assert!(Message::decode(truncated).is_err(), "a short body");
        assert_eq!(
            whole_message_len(truncated).unwrap(),
            None,
            "still arriving"
        );

        let mut bad_bool = good.clone();
        let last = bad_bool.len() - 4;
        bad_bool[last] = 2;
        assert!(Message::decode(&bad_bool).is_err(), "a boolean of 2");

        let mut bad_order = good.clone();
        bad_order[0] = b'x';
        assert!(whole_message_len(&bad_order).is_err());
        assert!(Message::decode(&bad_order).is_err());

        let mut huge = good;
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(
            whole_message_len(&huge).is_err(),
            "a body length past the limit"
        );
    }

    #[test]
    fn unsendable_values_are_refused_before_anything_is_written() {
        let nul = Message::call("a.b", "/a", "a.b", "M", vec![Value::str("a\0b")]);
        assert!(nul.encode(1).is_err());
        let mixed = Message::call(
            "a.b",
            "/a",
            "a.b",
            "M",
            vec![Value::Array("s".into(), vec![Value::U32(1)])],
        );
        assert!(mixed.encode(1).is_err(), "a u32 in an array of strings");
    }

    #[test]
    fn addresses_name_the_unix_sockets_to_try() {
        assert_eq!(
            endpoints("unix:path=/run/user/1000/bus"),
            vec![Endpoint::Path("/run/user/1000/bus".into())]
        );
        assert_eq!(
            endpoints("unix:path=/tmp/a%20b,guid=0011"),
            vec![Endpoint::Path("/tmp/a b".into())],
            "values are percent-escaped, and the guid is not an endpoint"
        );
        assert_eq!(
            endpoints("unix:abstract=/tmp/dbus-x,guid=1;unix:path=/x"),
            vec![
                Endpoint::Abstract(b"/tmp/dbus-x".to_vec()),
                Endpoint::Path("/x".into())
            ]
        );
        assert!(endpoints("tcp:host=localhost,port=1").is_empty());
        assert!(
            endpoints("unix:tmpdir=/tmp").is_empty(),
            "tmpdir is for listening"
        );
    }

    #[test]
    fn the_effective_uid_is_readable() {
        effective_uid().expect("/proc/self/status names it");
    }

    // ------------------------------------------------------------ against a real bus

    /// The next method call to `connection`. Every new connection's first message is the
    /// bus's own `NameAcquired` signal, which is not what a test is waiting for.
    fn next_call(connection: &mut Connection) -> Message {
        loop {
            let message = connection
                .next_message(Some(Duration::from_secs(5)))
                .unwrap()
                .expect("a call arrives");
            if message.kind == Kind::MethodCall {
                return message;
            }
        }
    }

    #[test]
    fn each_connection_gets_its_own_name() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let (one, two) = (bus.connect(), bus.connect());
        assert!(one.unique_name().starts_with(':'), "{}", one.unique_name());
        assert_ne!(one.unique_name(), two.unique_name());
    }

    /// The path KWin's focus reports take: a call to this connection's own name, read
    /// with `next_message`, answered with `reply`.
    #[test]
    fn a_call_is_delivered_answered_and_its_reply_returned() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut client = bus.connect();
        let mut service = bus.connect();
        let name = service.unique_name().to_owned();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let call = next_call(&mut service);
                assert!(call.is_call("/probe", "org.sequencer.Test", "Echo"));
                assert_eq!(call.body, vec![Value::str("ping")]);
                service
                    .reply(&call, vec![Value::str("pong"), Value::U32(2)])
                    .unwrap();
            });
            let answer = client
                .call(
                    &name,
                    "/probe",
                    "org.sequencer.Test",
                    "Echo",
                    vec![Value::str("ping")],
                    Duration::from_secs(5),
                )
                .unwrap();
            assert_eq!(answer, vec![Value::str("pong"), Value::U32(2)]);
        });
    }

    #[test]
    fn an_unserved_call_is_declined_rather_than_left_to_time_out() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut client = bus.connect();
        let mut service = bus.connect();
        let name = service.unique_name().to_owned();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let call = next_call(&mut service);
                service.decline(&call).unwrap();
            });
            let err = client
                .call(
                    &name,
                    "/x",
                    "org.sequencer.Test",
                    "Nope",
                    Vec::new(),
                    Duration::from_secs(5),
                )
                .unwrap_err();
            assert!(
                matches!(&err, Error::Remote { name, .. } if name == "org.freedesktop.DBus.Error.UnknownMethod"),
                "{err:?}"
            );
        });
    }

    #[test]
    fn a_subscribed_signal_arrives_with_its_arguments() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut listener = bus.connect();
        let mut sender = bus.connect();
        listener
            .add_match("type='signal',interface='org.sequencer.Test'")
            .unwrap();
        sender
            .emit(
                "/layouts",
                "org.sequencer.Test",
                "Changed",
                vec![Value::U32(1)],
            )
            .unwrap();
        let signal = loop {
            let message = listener
                .next_message(Some(Duration::from_secs(5)))
                .unwrap()
                .expect("the signal arrives");
            // The bus announces the name it gave us; that is not the signal under test.
            if message.is_signal("org.sequencer.Test", "Changed") {
                break message;
            }
        };
        assert_eq!(signal.body, vec![Value::U32(1)]);
        assert_eq!(signal.sender.as_deref(), Some(sender.unique_name()));
    }

    #[test]
    fn a_name_has_an_owner_or_none() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut asker = bus.connect();
        let mut owner = bus.connect();
        owner.request_name("org.sequencer.Probe").unwrap();
        assert_eq!(
            asker.name_owner("org.sequencer.Probe").unwrap().as_deref(),
            Some(owner.unique_name())
        );
        assert_eq!(asker.name_owner("org.sequencer.Nobody").unwrap(), None);
    }

    #[test]
    fn a_call_nobody_answers_times_out() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut client = bus.connect();
        let silent = bus.connect();
        let err = client
            .call(
                silent.unique_name(),
                "/x",
                "org.sequencer.Test",
                "Wait",
                Vec::new(),
                Duration::from_millis(200),
            )
            .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err:?}");
    }

    /// How a listening thread is stopped: the read it is blocked in returns at once.
    #[test]
    fn closing_wakes_a_blocked_read() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut listener = bus.connect();
        let closer = listener.closer().unwrap();
        let reader = std::thread::spawn(move || {
            loop {
                match listener.next_message(None) {
                    Ok(_) => {}
                    Err(err) => return err,
                }
            }
        });
        std::thread::sleep(Duration::from_millis(100));
        closer.close();
        let err = reader.join().unwrap();
        assert!(matches!(err, Error::Closed), "{err:?}");
    }
}
