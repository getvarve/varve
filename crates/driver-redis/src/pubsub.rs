//! Pub/Sub on a dedicated connection (design doc 4.2).
//!
//! The console refuses `SUBSCRIBE` (it would leave the session connection
//! in subscriber mode). A subscription gets its own connection instead,
//! set up like the session's (same server, user, transport and init
//! commands; client name `<name>-pubsub`), split in a [`PubSubReader`]
//! and a [`PubSubWriter`] so one task can read messages while another
//! subscribes and unsubscribes. Messages are read as a stream: a payload
//! longer than the caller's limit keeps its first bytes and its size, the
//! rest is dropped unread.

use tokio::io::{AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use varve_driver::{DriverError, ErrorKind};

use crate::conn::{Endpoint, Stream};
use crate::measure::{self, Head, RawConn, encode};
use crate::parse::ParsedCommand;
use crate::session::RedisSession;

/// Suffix of the client name of Pub/Sub connections.
pub const PUBSUB_NAME_SUFFIX: &str = "-pubsub";

/// Bytes of a channel or pattern name kept (64 KiB); longer names are
/// shortened.
pub const MAX_NAME_BYTES: u64 = 64 * 1024;

/// A channel or a pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NameKind {
    /// `SUBSCRIBE`.
    Channel,
    /// `PSUBSCRIBE`.
    Pattern,
    /// `SSUBSCRIBE`: a sharded channel (Redis 7.0 and newer, Valkey);
    /// `SPUBLISH` reaches only these.
    ShardChannel,
}

impl NameKind {
    fn commands(self) -> (&'static [u8], &'static [u8]) {
        match self {
            Self::Channel => (b"SUBSCRIBE", b"UNSUBSCRIBE"),
            Self::Pattern => (b"PSUBSCRIBE", b"PUNSUBSCRIBE"),
            Self::ShardChannel => (b"SSUBSCRIBE", b"SUNSUBSCRIBE"),
        }
    }
}

/// What the server says about dropping subscribers that fall behind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputBufferReport {
    /// `client_output_buffer_limit_disconnections` from `INFO stats`
    /// (Redis 7.0 and newer, Valkey); `None` when the server does not
    /// report it.
    pub disconnections: Option<u64>,
    /// The `pubsub` part of `client-output-buffer-limit` (hard limit, soft
    /// limit, soft seconds), when `CONFIG GET` is allowed.
    pub pubsub_limit: Option<String>,
}

/// Reads the `pubsub` class of a `client-output-buffer-limit` value.
#[must_use]
pub fn pubsub_limit(config: &str) -> Option<String> {
    let words: Vec<&str> = config.split_whitespace().collect();
    words
        .iter()
        .position(|w| w.eq_ignore_ascii_case("pubsub"))
        .and_then(|at| words.get(at + 1..at + 4))
        .map(|limit| limit.join(" "))
}

/// One thing read from a subscriber connection.
#[derive(Debug, Clone, PartialEq)]
pub enum PubSubItem {
    /// A published message.
    Message {
        /// The pattern it matched, for pattern subscriptions.
        pattern: Option<Vec<u8>>,
        /// Channel.
        channel: Vec<u8>,
        /// The payload, at most the limit passed to [`PubSubReader::next`].
        payload: Vec<u8>,
        /// The full payload size in bytes.
        size: u64,
    },
    /// The server confirmed a subscription.
    Subscribed {
        /// Channel or pattern.
        kind: NameKind,
        /// The name.
        name: Vec<u8>,
        /// Names this connection is subscribed to now.
        count: i64,
    },
    /// The server confirmed an unsubscription.
    Unsubscribed {
        /// Channel or pattern.
        kind: NameKind,
        /// The name (empty when there was nothing to unsubscribe from).
        name: Vec<u8>,
        /// Names this connection is subscribed to now.
        count: i64,
    },
    /// The server refused something (for example `NOPERM` for a channel
    /// the ACL user may not read); the connection goes on.
    Error(DriverError),
    /// The answer to [`PubSubWriter::ping`].
    Pong,
}

/// Opens Pub/Sub connections for a session, without holding the session.
#[derive(Clone)]
pub struct PubSubOpener {
    endpoint: Endpoint,
    init: Vec<ParsedCommand>,
    db: u16,
    compat: (u32, u32),
}

impl std::fmt::Debug for PubSubOpener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PubSubOpener")
            .field("endpoint", &self.endpoint.display())
            .finish_non_exhaustive()
    }
}

impl PubSubOpener {
    /// Asks the server, on a short-lived connection, how many clients it
    /// dropped for exceeding their output buffer limit and what the
    /// `pubsub` limit is. Parts the server does not give are `None`.
    ///
    /// # Errors
    ///
    /// Connect errors.
    pub async fn output_buffer_report(&self) -> Result<OutputBufferReport, DriverError> {
        let mut raw =
            measure::open_named(&self.endpoint, self.db, &self.init, PUBSUB_NAME_SUFFIX).await?;
        self.endpoint.bounded(read_report(&mut raw)).await?
    }

    /// The connect timeout of the session: [`Self::open`] and
    /// [`Self::output_buffer_report`] each take at most this long.
    #[must_use]
    pub fn connect_timeout(&self) -> std::time::Duration {
        self.endpoint.connect_timeout()
    }

    /// True when the server has sharded Pub/Sub (Redis 7.0 and newer,
    /// Valkey).
    #[must_use]
    pub fn has_sharded(&self) -> bool {
        self.compat >= (7, 0)
    }

    /// Opens a connection within the connect timeout and splits it.
    ///
    /// # Errors
    ///
    /// Connect, Tls, Auth or Timeout errors, and failing init commands.
    pub async fn open(&self) -> Result<(PubSubReader, PubSubWriter), DriverError> {
        let raw =
            measure::open_named(&self.endpoint, self.db, &self.init, PUBSUB_NAME_SUFFIX).await?;
        Self::split(raw)
    }

    /// [`Self::open`], and first asks the server on the new connection
    /// what [`Self::output_buffer_report`] asks (no second connection), so
    /// a later loss can be compared with the count at this moment.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub async fn open_with_report(
        &self,
    ) -> Result<(PubSubReader, PubSubWriter, OutputBufferReport), DriverError> {
        let mut raw =
            measure::open_named(&self.endpoint, self.db, &self.init, PUBSUB_NAME_SUFFIX).await?;
        let report = self.endpoint.bounded(read_report(&mut raw)).await??;
        let (reader, writer) = Self::split(raw)?;
        Ok((reader, writer, report))
    }

    fn split(raw: RawConn) -> Result<(PubSubReader, PubSubWriter), DriverError> {
        if !raw.io.buffer().is_empty() {
            return Err(measure::unexpected("data after the connection setup"));
        }
        let (read, write) = tokio::io::split(raw.io.into_inner());
        Ok((
            PubSubReader {
                raw: RawConn {
                    io: BufReader::with_capacity(measure::SKIP_CHUNK, read),
                },
            },
            PubSubWriter { io: write },
        ))
    }
}

/// Reads `client_output_buffer_limit_disconnections` and the `pubsub`
/// limit on a connection that is not subscribed. A command the server
/// refuses (ACL, renamed) leaves its part `None`.
async fn read_report(raw: &mut RawConn) -> Result<OutputBufferReport, DriverError> {
    raw.send(&[b"INFO", b"stats"]).await?;
    let disconnections = match raw.read_resp(1024 * 1024).await {
        Ok(info) => {
            let text = crate::reply::as_text(&info).unwrap_or_default();
            crate::conn::info_fields(&text)
                .into_iter()
                .find(|(k, _)| *k == "client_output_buffer_limit_disconnections")
                .and_then(|(_, v)| v.trim().parse().ok())
        }
        // A server error line was read: the connection is in step.
        Err(error) if error.code.is_some() => None,
        Err(error) => return Err(error),
    };
    raw.send(&[b"CONFIG", b"GET", b"client-output-buffer-limit"])
        .await?;
    let pubsub_limit = match raw.read_resp(64 * 1024).await {
        Ok(redis::Value::Array(items)) => items
            .get(1)
            .and_then(crate::reply::as_text)
            .and_then(|v| pubsub_limit(&v)),
        Ok(_) => None,
        // CONFIG renamed away or not permitted.
        Err(error) if error.code.is_some() => None,
        Err(error) => return Err(error),
    };
    Ok(OutputBufferReport {
        disconnections,
        pubsub_limit,
    })
}

/// The reading half of a Pub/Sub connection.
pub struct PubSubReader {
    raw: RawConn<ReadHalf<Box<dyn Stream>>>,
}

impl PubSubReader {
    /// Waits for the next item. Message payloads keep at most
    /// `max_payload` bytes. Not cancel safe: dropping the future in the
    /// middle of a message leaves the reader out of step, so read from one
    /// task and close the connection to stop.
    ///
    /// # Errors
    ///
    /// ConnectionLost when the connection closes, Protocol for a reply of
    /// an unexpected shape.
    pub async fn next(&mut self, max_payload: u64) -> Result<PubSubItem, DriverError> {
        let raw = &mut self.raw;
        let parts = match raw.head().await? {
            Head::Array(Some(n)) if n >= 2 => n,
            Head::Error(line) => return Ok(PubSubItem::Error(measure::server_error(&line))),
            other => return Err(measure::unexpected(&format!("{other:?}"))),
        };
        let kind = raw.short().await?.to_ascii_lowercase();
        match (kind.as_str(), parts) {
            ("message" | "smessage", 3) => {
                let channel = name(raw).await?;
                let (payload, size) = payload(raw, max_payload).await?;
                Ok(PubSubItem::Message {
                    pattern: None,
                    channel,
                    payload,
                    size,
                })
            }
            ("pmessage", 4) => {
                let pattern = name(raw).await?;
                let channel = name(raw).await?;
                let (payload, size) = payload(raw, max_payload).await?;
                Ok(PubSubItem::Message {
                    pattern: Some(pattern),
                    channel,
                    payload,
                    size,
                })
            }
            (
                "subscribe" | "psubscribe" | "ssubscribe" | "unsubscribe" | "punsubscribe"
                | "sunsubscribe",
                3,
            ) => {
                let name = name(raw).await?;
                let count = raw.int_reply().await?;
                let kind_of = match kind.as_str() {
                    "psubscribe" | "punsubscribe" => NameKind::Pattern,
                    "ssubscribe" | "sunsubscribe" => NameKind::ShardChannel,
                    _ => NameKind::Channel,
                };
                Ok(if kind.contains("unsub") {
                    PubSubItem::Unsubscribed {
                        kind: kind_of,
                        name,
                        count,
                    }
                } else {
                    PubSubItem::Subscribed {
                        kind: kind_of,
                        name,
                        count,
                    }
                })
            }
            ("pong", 2) => {
                raw.skip_value().await?;
                Ok(PubSubItem::Pong)
            }
            _ => {
                for _ in 1..parts {
                    raw.skip_value().await?;
                }
                Err(measure::unexpected(&format!("a Pub/Sub {kind} push")))
            }
        }
    }
}

/// A channel or pattern name, shortened to [`MAX_NAME_BYTES`]; empty for
/// nil.
async fn name<S: tokio::io::AsyncRead + Unpin>(
    raw: &mut RawConn<S>,
) -> Result<Vec<u8>, DriverError> {
    match raw.next().await? {
        Head::Bulk(Some(len)) => raw.keep_prefix(len, MAX_NAME_BYTES).await,
        Head::Bulk(None) => Ok(Vec::new()),
        other => Err(measure::unexpected(&format!("{other:?}"))),
    }
}

/// A payload: its first `max` bytes and its size.
async fn payload<S: tokio::io::AsyncRead + Unpin>(
    raw: &mut RawConn<S>,
    max: u64,
) -> Result<(Vec<u8>, u64), DriverError> {
    match raw.next().await? {
        Head::Bulk(Some(len)) => Ok((raw.keep_prefix(len, max).await?, len)),
        Head::Bulk(None) => Ok((Vec::new(), 0)),
        other => Err(measure::unexpected(&format!("{other:?}"))),
    }
}

/// The writing half of a Pub/Sub connection.
pub struct PubSubWriter {
    io: WriteHalf<Box<dyn Stream>>,
}

impl PubSubWriter {
    async fn send(&mut self, args: &[&[u8]]) -> Result<(), DriverError> {
        let mut out = Vec::new();
        encode(args, &mut out);
        self.write(&out).await
    }

    async fn write(&mut self, out: &[u8]) -> Result<(), DriverError> {
        let lost = |e: std::io::Error| {
            DriverError::new(
                ErrorKind::ConnectionLost,
                format!("The Pub/Sub connection to the server was lost: {e}"),
            )
        };
        self.io.write_all(out).await.map_err(lost)?;
        self.io.flush().await.map_err(lost)
    }

    /// `SUBSCRIBE` or `PSUBSCRIBE`; the server confirms each name with a
    /// [`PubSubItem::Subscribed`] on the reader.
    ///
    /// # Errors
    ///
    /// ConnectionLost.
    pub async fn subscribe(
        &mut self,
        kind: NameKind,
        names: &[Vec<u8>],
    ) -> Result<(), DriverError> {
        if names.is_empty() {
            return Ok(());
        }
        let command = kind.commands().0;
        let mut args = vec![command];
        args.extend(names.iter().map(Vec::as_slice));
        self.send(&args).await
    }

    /// One `SUBSCRIBE`, `PSUBSCRIBE` or `SSUBSCRIBE` per name, written
    /// together. The server answers each on its own: a confirmation, or an
    /// error (for example `NOPERM` for a channel the ACL user may not
    /// read), in the order sent. (One command with several names is
    /// refused as a whole when one of them is not permitted.)
    ///
    /// # Errors
    ///
    /// ConnectionLost.
    pub async fn subscribe_each(
        &mut self,
        kind: NameKind,
        names: &[Vec<u8>],
    ) -> Result<(), DriverError> {
        if names.is_empty() {
            return Ok(());
        }
        let command = kind.commands().0;
        let mut out = Vec::new();
        for name in names {
            encode(&[command, name.as_slice()], &mut out);
        }
        self.write(&out).await
    }

    /// `UNSUBSCRIBE` or `PUNSUBSCRIBE` of `names`; nothing for an empty
    /// list (the commands without names would drop every subscription).
    ///
    /// # Errors
    ///
    /// ConnectionLost.
    pub async fn unsubscribe(
        &mut self,
        kind: NameKind,
        names: &[Vec<u8>],
    ) -> Result<(), DriverError> {
        if names.is_empty() {
            return Ok(());
        }
        let command = kind.commands().1;
        let mut args = vec![command];
        args.extend(names.iter().map(Vec::as_slice));
        self.send(&args).await
    }

    /// `PING`; answered with [`PubSubItem::Pong`].
    ///
    /// # Errors
    ///
    /// ConnectionLost.
    pub async fn ping(&mut self) -> Result<(), DriverError> {
        self.send(&[b"PING"]).await
    }
}

impl RedisSession {
    /// An opener for Pub/Sub connections like this session's (on the
    /// database the session is on now; Pub/Sub ignores it).
    pub async fn pubsub_opener(&self) -> PubSubOpener {
        PubSubOpener {
            endpoint: self.endpoint().clone(),
            init: self.init_commands().to_vec(),
            db: self.current_db().await,
            compat: self.compat_version(),
        }
    }

    /// Publishes `message` on `channel` on the session connection and
    /// returns how many clients received it.
    ///
    /// # Errors
    ///
    /// ReadOnly on a read-only session, `MULTI_OPEN` while a console
    /// `MULTI` block is open, server and connection errors, Cancelled.
    pub async fn publish(&self, channel: &[u8], message: &[u8]) -> Result<i64, DriverError> {
        if self.is_read_only() {
            return Err(DriverError::new(
                ErrorKind::ReadOnly,
                "This connection is read-only, so it cannot publish",
            ));
        }
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        let reply = call
            .query(redis::cmd("PUBLISH").arg(channel).arg(message))
            .await?;
        crate::reply::as_int(&reply)
            .ok_or_else(|| crate::error::protocol("PUBLISH did not return an integer"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pubsub_limits_are_read_from_the_config() {
        assert_eq!(
            pubsub_limit("normal 0 0 0 slave 268435456 67108864 60 pubsub 33554432 8388608 60")
                .as_deref(),
            Some("33554432 8388608 60")
        );
        assert_eq!(pubsub_limit("normal 0 0 0"), None);
    }
}
