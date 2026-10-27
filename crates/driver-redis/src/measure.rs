//! Measuring entries too large to read, without holding them.
//!
//! The protocol library buffers whole replies, and a server script copies
//! every value it touches into the script engine. To get past an entry or a
//! scan step too large for one reply, the browser instead sends the command
//! on a short-lived side connection and reads the reply as a stream: small
//! parts (cursors, ids, scores, names that fit [`KEEP_LIMIT`]) are kept,
//! every other string is read in [`SKIP_CHUNK`] pieces and dropped, and
//! only its length is kept. Neither Varve nor the server's script engine
//! ever holds such a value; the server only sends its normal reply.
//!
//! The side connection is set up like the session's own: the same
//! transport (TLS or plain TCP, as the session negotiated it), user and
//! password, the client name `<name>-measure`, the session's init commands,
//! and the page's database. It is opened on the
//! first measurement of a page, used for every other one on that page, and
//! closed when the page is done (or after an error). Commands run under the
//! call's cancel and statement timeout, the opening under the connect
//! timeout too.

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use varve_driver::{DriverError, ErrorKind};

use crate::conn::{Endpoint, Stream};
use crate::error;
use crate::pagestate::Pending;
use crate::parse::ParsedCommand;
use crate::session::{Call, MAX_REPLY_BYTES};
use redis::Value as Resp;

/// Most bytes of names and members one measured reply keeps in memory.
pub(crate) const KEEP_LIMIT: u64 = MAX_REPLY_BYTES;

/// Pieces in which skipped strings are read and dropped (64 KiB).
pub(crate) const SKIP_CHUNK: usize = 64 * 1024;

/// Longest header line accepted (`$` or `*` with a length, `:` with a
/// number); also the longest kept cursor, id or score.
const LINE_LIMIT: usize = 512;

/// Longest error line (`-CODE detail`) read; module and ACL errors can be
/// long.
const ERROR_LINE_LIMIT: usize = 64 * 1024;

/// Bytes of an error line kept for the message.
const ERROR_KEEP: usize = 4 * 1024;

/// Deepest nesting of arrays accepted in a reply.
const MAX_DEPTH: usize = 32;

/// One RESP2 reply header.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Head {
    /// `*<n>`; `None` for a nil array.
    Array(Option<u64>),
    /// `$<n>`; `None` for a nil bulk string.
    Bulk(Option<u64>),
    /// `:<n>`.
    Int(i64),
    /// `+text`.
    Simple(String),
    /// `-CODE detail`.
    Error(String),
}

fn lost(e: &std::io::Error) -> DriverError {
    DriverError::new(
        ErrorKind::ConnectionLost,
        format!("The connection to the server was lost: {e}"),
    )
}

pub(crate) fn unexpected(what: &str) -> DriverError {
    error::protocol(format!("unexpected reply while measuring: {what}"))
}

/// The driver error for a `-CODE detail` line.
pub(crate) fn server_error(line: &str) -> DriverError {
    let code = line.split_whitespace().next().unwrap_or("ERR").to_string();
    let kind = if code == "WRONGPASS" || code == "NOAUTH" {
        ErrorKind::Auth
    } else {
        ErrorKind::Query
    };
    DriverError::new(kind, line.to_string()).with_code(code)
}

/// A side connection that reads replies as a stream. `S` is the stream:
/// a whole connection, or the read half of one ([`crate::pubsub`]).
pub(crate) struct RawConn<S = Box<dyn Stream>> {
    pub(crate) io: BufReader<S>,
}

impl RawConn {
    /// Opens and prepares the connection like the session's: authenticates,
    /// sets the client name `name`, runs the session's init commands (so
    /// it has the same identity and connection state) and selects `db`
    /// last, the database the page is on.
    async fn open(
        endpoint: &Endpoint,
        db: u16,
        name: &str,
        init: &[ParsedCommand],
    ) -> Result<Self, DriverError> {
        let stream = endpoint.open_stream().await?;
        let mut raw = Self {
            io: BufReader::with_capacity(SKIP_CHUNK, stream),
        };
        let password = endpoint.password().map(|p| p.expose().as_bytes());
        raw.prepare(endpoint.username(), password, db, name, init)
            .await?;
        Ok(raw)
    }

    async fn prepare(
        &mut self,
        username: Option<&str>,
        password: Option<&[u8]>,
        db: u16,
        name: &str,
        init: &[ParsedCommand],
    ) -> Result<(), DriverError> {
        let raw = self;
        match (username, password) {
            (Some(user), Some(password)) => {
                raw.expect_ok(&[b"AUTH", user.as_bytes(), password]).await?;
            }
            (None, Some(password)) => {
                raw.expect_ok(&[b"AUTH", password]).await?;
            }
            // An ACL user without a password ("nopass").
            (Some(user), None) => raw.expect_ok(&[b"AUTH", user.as_bytes(), b""]).await?,
            (None, None) => {}
        }
        for command in init {
            let args: Vec<&[u8]> = command.args.iter().map(Vec::as_slice).collect();
            raw.send(&args).await?;
            raw.skip_value().await.map_err(|mut e| {
                e.message = format!("Init command on line {}: {}", command.line, e.message);
                e.kind = ErrorKind::Query;
                e
            })?;
        }
        // After the init commands, so an init `CLIENT SETNAME` cannot rename
        // the side connection: `CLIENT LIST` always shows its purpose.
        if !name.is_empty() {
            raw.expect_ok(&[b"CLIENT", b"SETNAME", name.as_bytes()])
                .await?;
        }
        raw.expect_ok(&[b"SELECT", db.to_string().as_bytes()]).await
    }
}

/// One command in the RESP wire format.
pub(crate) fn encode(args: &[&[u8]], out: &mut Vec<u8>) {
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> RawConn<S> {
    pub(crate) async fn send(&mut self, args: &[&[u8]]) -> Result<(), DriverError> {
        let mut out = Vec::new();
        encode(args, &mut out);
        self.send_raw(&out).await
    }

    /// Writes commands already encoded with [`encode`] (a pipeline) and
    /// flushes once.
    pub(crate) async fn send_raw(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        let io = self.io.get_mut();
        io.write_all(bytes).await.map_err(|e| lost(&e))?;
        io.flush().await.map_err(|e| lost(&e))
    }

    pub(crate) async fn expect_ok(&mut self, args: &[&[u8]]) -> Result<(), DriverError> {
        self.send(args).await?;
        self.ok_reply().await
    }
}

impl<S: tokio::io::AsyncRead + Unpin> RawConn<S> {
    /// Reads the body of a bulk string of `len` bytes into `hasher`.
    pub(crate) async fn hash_body(
        &mut self,
        len: u64,
        hasher: &mut impl std::hash::Hasher,
    ) -> Result<(), DriverError> {
        let mut left = len;
        let mut scratch =
            vec![0; SKIP_CHUNK.min(usize::try_from(len).unwrap_or(SKIP_CHUNK).max(1))];
        while left > 0 {
            let want = usize::try_from(left.min(scratch.len() as u64)).unwrap_or(scratch.len());
            let read = self
                .io
                .read(&mut scratch[..want])
                .await
                .map_err(|e| lost(&e))?;
            if read == 0 {
                return Err(lost(&std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
            }
            hasher.write(&scratch[..read]);
            left -= read as u64;
        }
        let mut crlf = [0u8; 2];
        self.io.read_exact(&mut crlf).await.map_err(|e| lost(&e))?;
        Ok(())
    }

    /// Reads the body of a bulk string of `len` bytes, keeping at most
    /// `max` bytes of it and dropping the rest unread.
    pub(crate) async fn keep_prefix(&mut self, len: u64, max: u64) -> Result<Vec<u8>, DriverError> {
        if len <= max {
            return self.keep(len).await;
        }
        let size = usize::try_from(max).map_err(|_| unexpected("a prefix is too long"))?;
        let mut body = vec![0; size];
        self.io.read_exact(&mut body).await.map_err(|e| lost(&e))?;
        // The rest of the body and its CRLF.
        self.skip(len - max).await?;
        Ok(body)
    }

    /// Reads an integer reply.
    pub(crate) async fn int_reply(&mut self) -> Result<i64, DriverError> {
        match self.next().await? {
            Head::Int(n) => Ok(n),
            other => Err(unexpected(&format!("{other:?}"))),
        }
    }

    /// Reads a status reply (`+OK`, `+QUEUED`, ...); an error reply is an
    /// error.
    pub(crate) async fn ok_reply(&mut self) -> Result<(), DriverError> {
        match self.head().await? {
            Head::Simple(_) => Ok(()),
            Head::Error(line) => Err(server_error(&line)),
            other => Err(unexpected(&format!("{other:?}"))),
        }
    }

    /// Reads one header line: at most [`LINE_LIMIT`] bytes, or
    /// [`ERROR_LINE_LIMIT`] for an error line (of which [`ERROR_KEEP`]
    /// bytes are kept).
    pub(crate) async fn head(&mut self) -> Result<Head, DriverError> {
        let mut line = Vec::new();
        let mut total = 0usize;
        loop {
            let available = self.io.fill_buf().await.map_err(|e| lost(&e))?;
            if available.is_empty() {
                return Err(lost(&std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
            }
            crate::conn::mark_answered();
            let error_line = line.first() == Some(&b'-')
                || (line.is_empty() && available.first() == Some(&b'-'));
            let limit = if error_line {
                ERROR_LINE_LIMIT
            } else {
                LINE_LIMIT
            };
            let room = limit.saturating_sub(total);
            let (take, done) = match available.iter().take(room).position(|b| *b == b'\n') {
                Some(at) => (at + 1, true),
                None => (available.len().min(room), false),
            };
            let keep = ERROR_KEEP
                .max(LINE_LIMIT)
                .saturating_sub(line.len())
                .min(take);
            line.extend_from_slice(&available[..keep]);
            total += take;
            self.io.consume(take);
            if done {
                break;
            }
            if total >= limit {
                return Err(unexpected("a header line is too long"));
            }
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        let (kind, rest) = text.split_at(text.len().min(1));
        let number = || rest.parse::<i64>().map_err(|_| unexpected(text));
        let length = |n: i64| u64::try_from(n).ok();
        Ok(match kind {
            "*" => Head::Array(length(number()?)),
            "$" => Head::Bulk(length(number()?)),
            ":" => Head::Int(number()?),
            "+" => Head::Simple(rest.to_string()),
            "-" => Head::Error(rest.to_string()),
            _ => return Err(unexpected(text)),
        })
    }

    /// Reads the body of a bulk string of `len` bytes.
    pub(crate) async fn keep(&mut self, len: u64) -> Result<Vec<u8>, DriverError> {
        let size = usize::try_from(len).map_err(|_| unexpected("a string is too long"))?;
        let mut body = vec![0; size + 2];
        self.io.read_exact(&mut body).await.map_err(|e| lost(&e))?;
        body.truncate(size);
        Ok(body)
    }

    /// Reads and drops the body of a bulk string of `len` bytes.
    pub(crate) async fn skip(&mut self, len: u64) -> Result<(), DriverError> {
        let mut left = len.saturating_add(2);
        let mut scratch = vec![0; SKIP_CHUNK];
        while left > 0 {
            let want = usize::try_from(left.min(SKIP_CHUNK as u64)).unwrap_or(SKIP_CHUNK);
            let read = self
                .io
                .read(&mut scratch[..want])
                .await
                .map_err(|e| lost(&e))?;
            if read == 0 {
                return Err(lost(&std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
            }
            left -= read as u64;
        }
        Ok(())
    }

    /// The next reply part; a server error becomes an error.
    pub(crate) async fn next(&mut self) -> Result<Head, DriverError> {
        match self.head().await? {
            Head::Error(line) => Err(server_error(&line)),
            head => Ok(head),
        }
    }

    /// A short bulk string (cursor, id, score).
    pub(crate) async fn short(&mut self) -> Result<String, DriverError> {
        match self.next().await? {
            Head::Bulk(Some(len)) if len <= LINE_LIMIT as u64 => {
                Ok(String::from_utf8_lossy(&self.keep(len).await?).into_owned())
            }
            other => Err(unexpected(&format!("{other:?}"))),
        }
    }

    /// The length of a bulk string, whose body is dropped.
    pub(crate) async fn measure(&mut self) -> Result<u64, DriverError> {
        match self.next().await? {
            Head::Bulk(Some(len)) => {
                self.skip(len).await?;
                Ok(len)
            }
            other => Err(unexpected(&format!("{other:?}"))),
        }
    }

    /// Drops one value of any shape; returns the bytes of its strings.
    /// Iterative, with nesting limited to [`MAX_DEPTH`].
    pub(crate) async fn skip_value(&mut self) -> Result<u64, DriverError> {
        let mut bytes = 0u64;
        // Values still to read in each open array, outermost first.
        let mut open: Vec<u64> = Vec::new();
        let mut left = 1u64;
        loop {
            if left == 0 {
                match open.pop() {
                    Some(outer) => {
                        left = outer;
                        continue;
                    }
                    None => return Ok(bytes),
                }
            }
            left -= 1;
            match self.next().await? {
                Head::Bulk(Some(len)) => {
                    self.skip(len).await?;
                    bytes = bytes.saturating_add(len);
                }
                Head::Array(Some(n)) if n > 0 => {
                    if open.len() >= MAX_DEPTH {
                        return Err(unexpected("arrays nested too deeply"));
                    }
                    open.push(left);
                    left = n;
                }
                Head::Bulk(None) | Head::Array(_) | Head::Int(_) | Head::Simple(_) => {}
                Head::Error(line) => return Err(server_error(&line)),
            }
        }
    }

    /// Reads one whole reply of at most `max_bytes` bytes of strings, for
    /// small replies such as `COMMAND INFO`. Iterative, nesting limited to
    /// [`MAX_DEPTH`].
    pub(crate) async fn read_resp(&mut self, max_bytes: u64) -> Result<Resp, DriverError> {
        let mut budget = max_bytes;
        // Open arrays, outermost first: items so far and items still to come.
        let mut open: Vec<(Vec<Resp>, u64)> = Vec::new();
        loop {
            let mut value = match self.next().await? {
                Head::Bulk(Some(len)) => {
                    budget = budget
                        .checked_sub(len)
                        .ok_or_else(|| unexpected("a reply larger than expected"))?;
                    Resp::BulkString(self.keep(len).await?)
                }
                Head::Bulk(None) | Head::Array(None) => Resp::Nil,
                Head::Int(i) => Resp::Int(i),
                Head::Simple(text) => Resp::SimpleString(text),
                Head::Array(Some(0)) => Resp::Array(Vec::new()),
                Head::Array(Some(n)) => {
                    if open.len() >= MAX_DEPTH || n > budget {
                        return Err(unexpected("a reply larger than expected"));
                    }
                    let capacity = usize::try_from(n.min(1024)).unwrap_or(1024);
                    open.push((Vec::with_capacity(capacity), n));
                    continue;
                }
                Head::Error(line) => return Err(server_error(&line)),
            };
            // Attach the value to the open arrays, closing the full ones.
            loop {
                let Some((items, left)) = open.last_mut() else {
                    return Ok(value);
                };
                items.push(value);
                *left -= 1;
                if *left > 0 {
                    break;
                }
                let Some((items, _)) = open.pop() else {
                    return Err(unexpected("array state"));
                };
                value = Resp::Array(items);
            }
        }
    }

    /// `*<n>` with a length.
    pub(crate) async fn array(&mut self) -> Result<u64, DriverError> {
        match self.next().await? {
            Head::Array(Some(n)) => Ok(n),
            Head::Array(None) => Ok(0),
            other => Err(unexpected(&format!("{other:?}"))),
        }
    }

    /// The `[cursor, [...]]` frame of a scan reply: the next cursor and
    /// the number of items that follow.
    pub(crate) async fn scan_frame(&mut self) -> Result<(u64, u64), DriverError> {
        if self.array().await? != 2 {
            return Err(unexpected("a scan reply without two parts"));
        }
        let cursor = self.short().await?;
        let next = cursor
            .parse()
            .map_err(|_| unexpected(&format!("scan cursor {cursor}")))?;
        Ok((next, self.array().await?))
    }
}

/// Opens a side connection on the call's database within the connect
/// timeout.
async fn open_raw(
    endpoint: &Endpoint,
    db: u16,
    init: &[ParsedCommand],
) -> Result<RawConn, DriverError> {
    open_named(endpoint, db, init, SIDE_NAME_SUFFIX).await
}

/// Opens a side connection set up like the session's (see
/// [`RawConn`]), named `<session name><suffix>`, on database `db`, within
/// the connect timeout.
///
/// # Errors
///
/// Connect, Tls, Auth or Timeout errors, and failing init commands.
pub(crate) async fn open_named(
    endpoint: &Endpoint,
    db: u16,
    init: &[ParsedCommand],
    suffix: &str,
) -> Result<RawConn, DriverError> {
    let name = if endpoint.client_name().is_empty() {
        String::new()
    } else {
        format!("{}{suffix}", endpoint.client_name())
    };
    endpoint
        .bounded_connect(RawConn::open(endpoint, db, &name, init))
        .await?
}

/// Suffix of the client name of side connections (`CLIENT LIST` shows
/// `<session name>-measure`).
pub const SIDE_NAME_SUFFIX: &str = "-measure";

/// The call's side connection: the one already open for this page, or a
/// new one (under the call's cancel and the connect timeout).
async fn side(call: &mut Call<'_>) -> Result<RawConn, DriverError> {
    if let Some(raw) = call.side.take() {
        return Ok(raw);
    }
    let (endpoint, db, init) = (call.endpoint(), call.db(), call.init());
    let raw = call.watch(open_raw(endpoint, db, init)).await?;
    call.count_side_connection();
    Ok(raw)
}

/// A future borrowing a side connection.
pub(crate) type SideFuture<'r, T> =
    std::pin::Pin<Box<dyn Future<Output = Result<T, DriverError>> + Send + 'r>>;

/// Runs `work` on the call's side connection (opened on first use) under
/// the call's cancel and statement timeout; the connection is kept for the
/// rest of the call when `work` succeeds and closed otherwise.
///
/// # Errors
///
/// The error of `work`, connect errors, Cancelled, Timeout.
pub(crate) async fn with_side<T>(
    call: &mut Call<'_>,
    work: impl for<'r> FnOnce(&'r mut RawConn) -> SideFuture<'r, T>,
) -> Result<T, DriverError> {
    let mut raw = side(call).await?;
    let result = call.watch(work(&mut raw)).await;
    finish(call, raw, result)
}

/// Keeps the side connection for the rest of the page after a successful
/// command; after an error its position in the reply is unknown, so it is
/// closed.
fn finish<T>(
    call: &mut Call<'_>,
    raw: RawConn,
    result: Result<T, DriverError>,
) -> Result<T, DriverError> {
    if result.is_ok() {
        call.side = Some(raw);
    }
    result
}

/// The `SSCAN` step at `cursor` (`COUNT 1`): members while they fit
/// [`KEEP_LIMIT`] in total, the sizes of the rest. Returns the next cursor.
///
/// # Errors
///
/// Server errors (for example `WRONGTYPE`), connection errors, Cancelled,
/// Timeout.
pub(crate) async fn set_step(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: u64,
) -> Result<(u64, Vec<Pending>), DriverError> {
    let cursor = cursor.to_string();
    let mut raw = side(call).await?;
    let result = call
        .watch(async {
            let raw = &mut raw;
            raw.send(&[b"SSCAN", key, cursor.as_bytes(), b"COUNT", b"1"])
                .await?;
            let (next, n) = raw.scan_frame().await?;
            let mut left = KEEP_LIMIT;
            let mut pending = Vec::new();
            for _ in 0..n {
                match raw.next().await? {
                    Head::Bulk(Some(len)) if len <= left => {
                        left -= len;
                        pending.push(Pending::Member(raw.keep(len).await?));
                    }
                    Head::Bulk(Some(len)) => {
                        raw.skip(len).await?;
                        pending.push(Pending::OversizedMember(len));
                    }
                    other => return Err(unexpected(&format!("{other:?}"))),
                }
            }
            Ok((next, pending))
        })
        .await;
    finish(call, raw, result)
}

/// The `HSCAN` step at `cursor` (`COUNT 1`) without its values: field
/// names while they fit [`KEEP_LIMIT`] in total, the sizes of the rest and
/// of every value. Returns the next cursor.
///
/// # Errors
///
/// As [`set_step`].
pub(crate) async fn hash_step(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: u64,
) -> Result<(u64, Vec<Pending>), DriverError> {
    let cursor = cursor.to_string();
    let mut raw = side(call).await?;
    let result = call
        .watch(async {
            let raw = &mut raw;
            raw.send(&[b"HSCAN", key, cursor.as_bytes(), b"COUNT", b"1"])
                .await?;
            let (next, n) = raw.scan_frame().await?;
            let pairs = hash_pairs(n)?;
            let mut left = KEEP_LIMIT;
            let mut pending = Vec::new();
            for _ in 0..pairs {
                let field = match raw.next().await? {
                    Head::Bulk(Some(len)) if len <= left => {
                        left -= len;
                        Ok(raw.keep(len).await?)
                    }
                    Head::Bulk(Some(len)) => {
                        raw.skip(len).await?;
                        Err(len)
                    }
                    other => return Err(unexpected(&format!("{other:?}"))),
                };
                let value_bytes = raw.measure().await?;
                pending.push(match field {
                    Ok(name) => Pending::Field(name),
                    Err(field_bytes) => Pending::OversizedField {
                        field_bytes,
                        value_bytes: Some(value_bytes),
                    },
                });
            }
            Ok((next, pending))
        })
        .await;
    finish(call, raw, result)
}

/// Most bytes of strings in a `COMMAND INFO` reply read on the side
/// connection (1 MiB).
const COMMAND_INFO_LIMIT: u64 = 1024 * 1024;

/// `COMMAND INFO <name>` on the side connection, for lookups while a
/// console `MULTI` block is open on the session connection.
///
/// # Errors
///
/// Server errors (with the server's code, for example `NOPERM`),
/// connection errors, Cancelled, Timeout.
pub(crate) async fn command_info(call: &mut Call<'_>, name: &str) -> Result<Resp, DriverError> {
    side_query(
        call,
        &[b"COMMAND", b"INFO", name.as_bytes()],
        COMMAND_INFO_LIMIT,
    )
    .await
}

/// One command on the side connection, its reply read whole (at most
/// `limit` bytes of strings), for lookups while a console `MULTI` block is
/// open on the session connection.
///
/// # Errors
///
/// As [`command_info`].
pub(crate) async fn side_query(
    call: &mut Call<'_>,
    args: &[&[u8]],
    limit: u64,
) -> Result<Resp, DriverError> {
    let mut raw = side(call).await?;
    let result = call
        .watch(async {
            let raw = &mut raw;
            raw.send(args).await?;
            raw.read_resp(limit).await
        })
        .await;
    finish(call, raw, result)
}

/// `PING` on the side connection.
///
/// # Errors
///
/// Server, connection and protocol errors, Cancelled, Timeout.
pub(crate) async fn ping(call: &mut Call<'_>) -> Result<(), DriverError> {
    let mut raw = side(call).await?;
    let result = call
        .watch(async {
            let raw = &mut raw;
            raw.send(&[b"PING"]).await?;
            match raw.next().await? {
                Head::Simple(pong) if pong == "PONG" => Ok(()),
                other => Err(unexpected(&format!("PING returned {other:?}"))),
            }
        })
        .await;
    finish(call, raw, result)
}

/// A key name of a `SCAN` step: kept, or only its size.
pub(crate) type KeyName = Result<Vec<u8>, u64>;

/// The `SCAN` step at `cursor` (`MATCH pattern COUNT count`, and `TYPE`
/// when given), read as a stream. With `count` 1: key names while they fit
/// [`KEEP_LIMIT`] in total, the sizes of the rest. With a larger `count`,
/// `None` when the names do not all fit (the reply is read to its end, so
/// the connection stays usable and the caller can ask again with a smaller
/// count). Returns the next cursor.
///
/// # Errors
///
/// As [`set_step`].
pub(crate) async fn scan_step(
    call: &mut Call<'_>,
    cursor: u64,
    pattern: &str,
    type_name: Option<&str>,
    count: u32,
) -> Result<Option<(u64, Vec<KeyName>)>, DriverError> {
    let cursor = cursor.to_string();
    let count = count.to_string();
    let mut raw = side(call).await?;
    let result = call
        .watch(async {
            let raw = &mut raw;
            let mut args: Vec<&[u8]> = vec![
                b"SCAN",
                cursor.as_bytes(),
                b"MATCH",
                pattern.as_bytes(),
                b"COUNT",
                count.as_bytes(),
            ];
            let whole_step = count != "1";
            if let Some(type_name) = type_name {
                args.push(b"TYPE");
                args.push(type_name.as_bytes());
            }
            raw.send(&args).await?;
            let (next, n) = raw.scan_frame().await?;
            let mut left = KEEP_LIMIT;
            let mut names = Vec::new();
            let mut overflow = false;
            for _ in 0..n {
                match raw.next().await? {
                    Head::Bulk(Some(len)) if len <= left && !overflow => {
                        left -= len;
                        names.push(Ok(raw.keep(len).await?));
                    }
                    Head::Bulk(Some(len)) => {
                        raw.skip(len).await?;
                        overflow = whole_step;
                        names.push(Err(len));
                    }
                    other => return Err(unexpected(&format!("{other:?}"))),
                }
            }
            Ok((!overflow).then_some((next, names)))
        })
        .await;
    finish(call, raw, result)
}

/// Field and value pairs in an `HSCAN` reply of `n` parts; an odd count is
/// a protocol error (the connection is then closed, never reused with a
/// part left unread).
fn hash_pairs(n: u64) -> Result<u64, DriverError> {
    if n.is_multiple_of(2) {
        Ok(n / 2)
    } else {
        Err(unexpected("an HSCAN reply with an odd number of parts"))
    }
}

/// The size of the list element at `index`; `None` when there is none.
///
/// # Errors
///
/// As [`set_step`].
pub(crate) async fn list_element(
    call: &mut Call<'_>,
    key: &[u8],
    index: u64,
) -> Result<Option<u64>, DriverError> {
    let index = index.to_string();
    let mut raw = side(call).await?;
    let result = call
        .watch(async {
            let raw = &mut raw;
            raw.send(&[b"LRANGE", key, index.as_bytes(), index.as_bytes()])
                .await?;
            match raw.array().await? {
                0 => Ok(None),
                1 => Ok(Some(raw.measure().await?)),
                n => Err(unexpected(&format!("{n} list elements"))),
            }
        })
        .await;
    finish(call, raw, result)
}

/// The member size and score text of the sorted set member at `rank`;
/// `None` when there is none.
///
/// # Errors
///
/// As [`set_step`].
pub(crate) async fn zset_member(
    call: &mut Call<'_>,
    key: &[u8],
    rank: u64,
) -> Result<Option<(u64, String)>, DriverError> {
    let rank = rank.to_string();
    let mut raw = side(call).await?;
    let result = call
        .watch(async {
            let raw = &mut raw;
            raw.send(&[
                b"ZRANGE",
                key,
                rank.as_bytes(),
                rank.as_bytes(),
                b"WITHSCORES",
            ])
            .await?;
            match raw.array().await? {
                0 => Ok(None),
                2 => {
                    let size = raw.measure().await?;
                    Ok(Some((size, raw.short().await?)))
                }
                n => Err(unexpected(&format!("{n} sorted set parts"))),
            }
        })
        .await;
    finish(call, raw, result)
}

/// The id and field bytes of the first stream entry at or after `start`;
/// `None` when there is none.
///
/// # Errors
///
/// As [`set_step`].
pub(crate) async fn stream_entry(
    call: &mut Call<'_>,
    key: &[u8],
    start: &str,
) -> Result<Option<(String, u64)>, DriverError> {
    let start = start.to_string();
    let mut raw = side(call).await?;
    let result = call
        .watch(async {
            let raw = &mut raw;
            raw.send(&[b"XRANGE", key, start.as_bytes(), b"+", b"COUNT", b"1"])
                .await?;
            match raw.array().await? {
                0 => Ok(None),
                1 => {
                    if raw.array().await? != 2 {
                        return Err(unexpected("a stream entry without two parts"));
                    }
                    let id = raw.short().await?;
                    Ok(Some((id, raw.skip_value().await?)))
                }
                n => Err(unexpected(&format!("{n} stream entries"))),
            }
        })
        .await;
    finish(call, raw, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(bytes: &'static [u8]) -> RawConn {
        let (client, mut server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = server.write_all(bytes).await;
            // Keep the other side open until the test ends.
            let mut sink = Vec::new();
            let _ = server.read_to_end(&mut sink).await;
        });
        let stream: Box<dyn Stream> = Box::new(client);
        RawConn {
            io: BufReader::with_capacity(16, stream),
        }
    }

    #[tokio::test]
    async fn scan_frames_keep_small_parts_and_drop_the_rest() -> Result<(), DriverError> {
        let mut r = raw(b"*2\r\n$2\r\n17\r\n*3\r\n$1\r\na\r\n$40\r\n0123456789012345678901234567890123456789\r\n$-1\r\n");
        assert_eq!(r.scan_frame().await?, (17, 3));
        assert_eq!(r.next().await?, Head::Bulk(Some(1)));
        assert_eq!(r.keep(1).await?, b"a");
        assert_eq!(r.measure().await?, 40);
        assert_eq!(r.next().await?, Head::Bulk(None));
        Ok(())
    }

    /// A fake server: answers each command with the next canned reply and
    /// records the commands it got.
    fn scripted(
        replies: &'static [&'static [u8]],
    ) -> (RawConn, tokio::task::JoinHandle<Vec<String>>) {
        let (client, server) = tokio::io::duplex(1 << 16);
        let task = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut seen = Vec::new();
            for reply in replies {
                let mut header = String::new();
                if server.read_line(&mut header).await.unwrap_or(0) == 0 {
                    break;
                }
                let n: usize = header.trim().trim_start_matches('*').parse().unwrap_or(0);
                let mut words = Vec::new();
                for _ in 0..n {
                    let mut len = String::new();
                    let _ = server.read_line(&mut len).await;
                    let len: usize = len.trim().trim_start_matches('$').parse().unwrap_or(0);
                    let mut body = vec![0; len + 2];
                    let _ = server.read_exact(&mut body).await;
                    body.truncate(len);
                    words.push(String::from_utf8_lossy(&body).into_owned());
                }
                seen.push(words.join(" "));
                let _ = server.get_mut().write_all(reply).await;
            }
            seen
        });
        let stream: Box<dyn Stream> = Box::new(client);
        let raw = RawConn {
            io: BufReader::with_capacity(16, stream),
        };
        (raw, task)
    }

    #[tokio::test]
    async fn side_connections_are_set_up_like_the_session() -> Result<(), Box<dyn std::error::Error>>
    {
        let init = crate::parse::parse_script("CLIENT NO-TOUCH ON\nSELECT 3")?;
        let (mut raw, server) =
            scripted(&[b"+OK\r\n", b"+OK\r\n", b"+OK\r\n", b"+OK\r\n", b"+OK\r\n"]);
        raw.prepare(Some("alice"), Some(b"pw"), 5, "Varve-measure", &init)
            .await?;
        drop(raw);
        assert_eq!(
            server.await?,
            [
                "AUTH alice pw",
                "CLIENT NO-TOUCH ON",
                "SELECT 3",
                "CLIENT SETNAME Varve-measure",
                "SELECT 5"
            ]
        );
        // A failing init command fails the opening, with its line.
        let (mut raw, _server) = scripted(&[b"+OK\r\n", b"-ERR nope\r\n"]);
        let init = crate::parse::parse_script("\nBADCMD")?;
        let error = raw.prepare(None, Some(b"pw"), 0, "", &init).await.err();
        let error = error.ok_or("init error was ignored")?;
        assert!(
            error.message.starts_with("Init command on line 2:"),
            "{}",
            error.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn odd_hscan_replies_and_deep_nesting_are_protocol_errors() {
        let mut r = raw(b"*2\r\n$1\r\n0\r\n*3\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n");
        let frame = r.scan_frame().await;
        assert!(matches!(frame, Ok((0, 3))));
        assert_eq!(
            hash_pairs(3).err().map(|e| e.kind),
            Some(ErrorKind::Protocol)
        );
        assert_eq!(hash_pairs(4).ok(), Some(2));
        let deep: &'static [u8] = Box::leak(b"*1\r\n".repeat(10_000).into_boxed_slice());
        let mut r = raw(deep);
        let error = r.skip_value().await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Protocol));
        // Nesting within the limit is fine.
        let mut ok = b"*1\r\n".repeat(MAX_DEPTH);
        ok.extend_from_slice(b"$2\r\nok\r\n");
        let ok: &'static [u8] = Box::leak(ok.into_boxed_slice());
        let mut r = raw(ok);
        assert_eq!(r.skip_value().await.ok(), Some(2));
    }

    #[tokio::test]
    async fn long_error_lines_keep_the_server_message() {
        let mut line = b"-NOPERM ".to_vec();
        line.extend(std::iter::repeat_n(b'x', 20_000));
        line.extend_from_slice(b"\r\n");
        let line: &'static [u8] = Box::leak(line.into_boxed_slice());
        let mut r = raw(line);
        let error = r.next().await.err();
        let error = error.unwrap_or_else(|| panic!("no error"));
        assert_eq!(error.code.as_deref(), Some("NOPERM"));
        assert!(error.message.len() <= ERROR_KEEP);
        // A header line that is too long is still refused.
        let long: &'static [u8] = Box::leak(vec![b'$'; 4096].into_boxed_slice());
        let mut r = raw(long);
        assert_eq!(
            r.next().await.err().map(|e| e.kind),
            Some(ErrorKind::Protocol)
        );
    }

    #[tokio::test]
    async fn small_replies_are_read_whole() -> Result<(), DriverError> {
        let mut r = raw(b"*1\r\n*3\r\n$6\r\nconfig\r\n:-2\r\n*2\r\n+admin\r\n$-1\r\n");
        assert_eq!(
            r.read_resp(1024).await?,
            Resp::Array(vec![Resp::Array(vec![
                Resp::BulkString(b"config".to_vec()),
                Resp::Int(-2),
                Resp::Array(vec![Resp::SimpleString("admin".into()), Resp::Nil]),
            ])])
        );
        let mut r = raw(b"$10\r\n0123456789\r\n");
        assert!(r.read_resp(5).await.is_err());
        let mut r = raw(b"-NOPERM no\r\n");
        assert_eq!(
            r.read_resp(5).await.err().and_then(|e| e.code).as_deref(),
            Some("NOPERM")
        );
        Ok(())
    }

    #[tokio::test]
    async fn nested_values_are_measured() -> Result<(), DriverError> {
        let mut r = raw(b"*2\r\n$3\r\nabc\r\n*2\r\n:5\r\n$4\r\nwxyz\r\n");
        assert_eq!(r.skip_value().await?, 7);
        Ok(())
    }

    #[tokio::test]
    async fn errors_and_bad_lines_are_reported() {
        let mut r = raw(b"-WRONGTYPE Operation against a key\r\n");
        let error = r.next().await.err();
        assert_eq!(error.and_then(|e| e.code).as_deref(), Some("WRONGTYPE"));
        let mut r = raw(b"-WRONGPASS invalid\r\n");
        assert_eq!(r.next().await.err().map(|e| e.kind), Some(ErrorKind::Auth));
        let mut r = raw(b"?what\r\n");
        assert!(r.next().await.is_err());
        let long: &'static [u8] = Box::leak(vec![b'$'; 4096].into_boxed_slice());
        let mut r = raw(long);
        assert!(r.next().await.is_err());
    }
}
