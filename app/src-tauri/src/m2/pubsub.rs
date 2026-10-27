//! Redis Pub/Sub panel (design doc 4.2: "Subscribe panel, publish").
//!
//! Every subscription has its own dedicated connection, opened like the
//! tab's session (same server, user, transport and init commands, client
//! name `<name>-pubsub`), never the session connection. Three tasks serve
//! it:
//!
//! - the reader reads the connection all the time and only appends each
//!   message as received (raw bytes) to a bounded queue ([`QUEUE_MESSAGES`],
//!   [`QUEUE_BYTES`], names and payloads counted; beyond it the oldest are
//!   dropped and counted). Shortening, display text and serialization
//!   happen in the sender, off the socket's path;
//! - the sender hands messages to the UI as Tauri events ([`EVENT`]) in
//!   batches of at most [`MAX_BATCH_MESSAGES`] messages or
//!   [`MAX_BATCH_BYTES`], at most every [`BATCH_INTERVAL`], and only while
//!   fewer than [`MAX_IN_FLIGHT`] batches wait for the UI's acknowledgement
//!   (`redis_pubsub_ack`): that is the backpressure;
//! - the keepalive sends `PING` after [`KEEPALIVE_IDLE`] without traffic and
//!   treats the connection as dead when nothing arrives within
//!   [`PING_TIMEOUT`] (a half-open network never closes on its own).
//!
//! Reading fast does not make a subscriber safe from the server: when
//! messages are published faster than the network and Varve can take them,
//! the server drops the subscriber once its pending output passes
//! `client-output-buffer-limit pubsub` (32 MiB hard, or 8 MiB for 60 s, by
//! default). The reason given then says so when the server's count of such
//! drops (`client_output_buffer_limit_disconnections`, Redis 7.0 and newer)
//! rose while this subscription was receiving messages. When the count rose
//! but the subscription was quiet (a burst can get it dropped before its
//! first byte arrives, or another client was dropped), or the server does
//! not count, the cause is given as unknown and the limit is named as a
//! possibility; a keepalive timeout never mentions the buffer.
//!
//! Each name is subscribed with its own command, so the server confirms or
//! refuses it alone (a command with several names is refused as a whole
//! when one is not permitted). A refused name is dropped from the
//! subscription and reported with an `error` event that names it.
//!
//! With `resubscribe` (the default) a lost connection is opened again
//! ([`RESUBSCRIBE_ATTEMPTS`] attempts; see [`attempt_start`] for the
//! schedule) and subscribed to the same names; a `gap` event marks the
//! messages lost in between, sent once the server answered for every name.
//! When it refuses every name, the subscription closes. Subscriptions close with
//! the tab's session (the session close hook calls
//! [`PubSubHub::forget_session`]) and on `redis_pubsub_unsubscribe`; each
//! close sends a `closed` event. Publishing runs `PUBLISH` on the tab's
//! session.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::AbortHandle;
use varve_activity::{ActivityEvent, ErrorDetails, RowCounts, SqlDialect};
use varve_core::Engine;
use varve_driver::Value;
use varve_driver_redis::{
    NameKind, OutputBufferReport, PubSubItem, PubSubOpener, PubSubReader, PubSubWriter, key_display,
};

use super::redis_edit::{driver_err, name_bytes};
use super::{elapsed_ms, environment_label};
use crate::commands::{CmdError, Services};

type CmdResult<T> = Result<T, CmdError>;

/// The Tauri event every [`PubSubEvent`] is sent as.
pub const EVENT: &str = "redis-pubsub";

/// Payload bytes kept per message when the UI does not say.
pub const DEFAULT_PAYLOAD_BYTES: u64 = 64 * 1024;

/// Most payload bytes per message the UI may ask for (1 MiB).
pub const MAX_PAYLOAD_BYTES: u64 = 1024 * 1024;

/// Channels, patterns and sharded channels per subscription.
pub const MAX_NAMES: usize = 256;

/// Subscriptions per session.
pub const MAX_PER_SESSION: usize = 8;

/// Subscriptions in the app.
pub const MAX_TOTAL: usize = 32;

/// Most messages in one batch.
pub const MAX_BATCH_MESSAGES: usize = 500;

/// Most bytes (payloads and names) in one batch (1 MiB; a batch holds at
/// least one message).
pub const MAX_BATCH_BYTES: u64 = 1024 * 1024;

/// Batches sent and not yet acknowledged before sending pauses.
pub const MAX_IN_FLIGHT: u64 = 4;

/// Shortest time between two batches.
pub const BATCH_INTERVAL: Duration = Duration::from_millis(50);

/// Messages kept while the UI is behind.
pub const QUEUE_MESSAGES: usize = 10_000;

/// Bytes (payloads as kept plus channel and pattern names) kept while the
/// UI is behind (16 MiB).
pub const QUEUE_BYTES: u64 = 16 * 1024 * 1024;

/// Quiet time after which the keepalive sends `PING`.
pub const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);

/// How long the keepalive waits for anything after its `PING`.
pub const PING_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts to open a lost subscription again. They start 1, 3, 7, 15 and
/// 31 s after the loss (1, 2, 4, 8 and 16 s apart, see [`attempt_start`]).
pub const RESUBSCRIBE_ATTEMPTS: u32 = 5;

/// How recently a message must have arrived for a lost connection to be
/// put down to the output buffer limit without doubt (when the server's
/// count of such drops rose meanwhile).
pub const RECEIVING_WINDOW: Duration = Duration::from_secs(2);

/// When attempt `attempt` (1 to [`RESUBSCRIBE_ATTEMPTS`]) starts, counted
/// from the loss: 1, 3, 7, 15 and 31 s. Each attempt (opening the
/// connection, subscribing and waiting for the server's answers) is cut
/// off when the next one is due, the last one after twice the connect
/// timeout, so a server that does not answer cannot stretch the schedule.
/// The report on why the connection was lost is asked for within the first
/// second.
#[must_use]
pub fn attempt_start(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.min(32)) - 1)
}

/// A received message.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PubSubMessage {
    /// Channel, display text.
    pub channel: String,
    /// The pattern it matched, display text.
    pub pattern: Option<String>,
    /// The payload (its first bytes when `truncated`).
    pub payload: Value,
    /// The full payload size in bytes, decimal text.
    pub size: String,
    /// True when `payload` holds only the first bytes.
    pub truncated: bool,
    /// When it was read, unix ms.
    pub received_at: i64,
}

/// An event of one subscription, sent as [`EVENT`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PubSubEvent {
    /// Messages.
    #[serde(rename_all = "camelCase")]
    Batch {
        /// Subscription.
        subscription_id: String,
        /// 1, 2, 3, ...; acknowledge with `redis_pubsub_ack`.
        seq: u64,
        /// The messages.
        messages: Vec<PubSubMessage>,
        /// Messages dropped since the previous batch.
        dropped: u64,
    },
    /// The server confirmed a name.
    #[serde(rename_all = "camelCase")]
    Subscribed {
        /// Subscription.
        subscription_id: String,
        /// Channel, pattern or sharded channel.
        name_kind: NameKind,
        /// The name, display text.
        name: String,
        /// Names the connection is subscribed to now.
        count: i64,
    },
    /// The server confirmed an unsubscription.
    #[serde(rename_all = "camelCase")]
    Unsubscribed {
        /// Subscription.
        subscription_id: String,
        /// Channel, pattern or sharded channel.
        name_kind: NameKind,
        /// The name, display text.
        name: String,
        /// Names the connection is subscribed to now.
        count: i64,
    },
    /// The server refused something; the subscription goes on. When it
    /// refused a name, `nameKind` and `name` say which: it is no longer
    /// part of the subscription.
    #[serde(rename_all = "camelCase")]
    Error {
        /// Subscription.
        subscription_id: String,
        /// The server's message.
        message: String,
        /// Its code (`NOPERM`, ...).
        code: Option<String>,
        /// What the refused name is, when a name was refused.
        name_kind: Option<NameKind>,
        /// The refused name, display text.
        name: Option<String>,
    },
    /// The connection was lost; Varve opens it again (`resubscribe`).
    #[serde(rename_all = "camelCase")]
    Reconnecting {
        /// Subscription.
        subscription_id: String,
        /// 1 to [`RESUBSCRIBE_ATTEMPTS`].
        attempt: u32,
        /// Why the connection was lost.
        reason: String,
    },
    /// The subscription is back after a lost connection, and the server
    /// answered for every name (refused ones came as `error` events
    /// first): messages published between `lostAt` and `resumedAt` were
    /// missed.
    #[serde(rename_all = "camelCase")]
    Gap {
        /// Subscription.
        subscription_id: String,
        /// Why the connection was lost.
        reason: String,
        /// When it was lost, unix ms.
        lost_at: i64,
        /// When the names were subscribed again, unix ms.
        resumed_at: i64,
    },
    /// The subscription is gone.
    #[serde(rename_all = "camelCase")]
    Closed {
        /// Subscription.
        subscription_id: String,
        /// Why.
        reason: String,
    },
}

/// Hands events to the UI (a Tauri window in the app, a collector in
/// tests).
pub trait PubSubEmitter: Send + Sync {
    /// Sends one event.
    fn emit(&self, event: PubSubEvent);
}

/// What `redis_pubsub_subscribe` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PubSubSubscription {
    /// Pass to the other commands; events carry it.
    pub subscription_id: String,
    /// The Tauri event to listen to.
    pub event: &'static str,
}

/// Channels, patterns and sharded channels, display text.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PubSubNames {
    /// Channels.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Patterns.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Sharded channels (`SSUBSCRIBE`, Redis 7.0 and newer).
    #[serde(default)]
    pub shard_channels: Vec<String>,
}

/// How to subscribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscribeOptions {
    /// Payload bytes kept per message.
    pub max_payload_bytes: u64,
    /// Open a lost connection again and subscribe to the same names.
    pub resubscribe: bool,
}

impl Default for SubscribeOptions {
    fn default() -> Self {
        Self {
            max_payload_bytes: DEFAULT_PAYLOAD_BYTES,
            resubscribe: true,
        }
    }
}

/// One subscription, as `redis_pubsub_subscriptions` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PubSubSubscriptionInfo {
    /// Id.
    pub subscription_id: String,
    /// Channels the server confirmed, display text.
    pub channels: Vec<String>,
    /// Patterns the server confirmed, display text.
    pub patterns: Vec<String>,
    /// Sharded channels the server confirmed, display text.
    pub shard_channels: Vec<String>,
    /// `subscribed`, or `reconnecting` from a lost connection until the
    /// server answered for every name again.
    pub state: &'static str,
    /// Messages sent to the UI.
    pub delivered: u64,
    /// Messages dropped because the UI was behind.
    pub dropped: u64,
}

/// Names of a subscription, as bytes.
#[derive(Default, Clone, PartialEq, Eq, Debug)]
struct NameSets {
    channels: BTreeSet<Vec<u8>>,
    patterns: BTreeSet<Vec<u8>>,
    shards: BTreeSet<Vec<u8>>,
}

impl NameSets {
    fn of(&mut self, kind: NameKind) -> &mut BTreeSet<Vec<u8>> {
        match kind {
            NameKind::Channel => &mut self.channels,
            NameKind::Pattern => &mut self.patterns,
            NameKind::ShardChannel => &mut self.shards,
        }
    }

    fn len(&self) -> usize {
        self.channels.len() + self.patterns.len() + self.shards.len()
    }

    fn lists(&self) -> [(NameKind, Vec<Vec<u8>>); 3] {
        [
            (NameKind::Channel, self.channels.iter().cloned().collect()),
            (NameKind::Pattern, self.patterns.iter().cloned().collect()),
            (
                NameKind::ShardChannel,
                self.shards.iter().cloned().collect(),
            ),
        ]
    }
}

/// A message as read, before any conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Raw {
    pattern: Option<Vec<u8>>,
    channel: Vec<u8>,
    payload: Vec<u8>,
    size: u64,
    received_at: i64,
}

impl Raw {
    /// Bytes it takes in the queue: payload and names.
    fn bytes(&self) -> u64 {
        (self.payload.len() + self.channel.len() + self.pattern.as_ref().map_or(0, Vec::len)) as u64
    }

    fn message(self) -> PubSubMessage {
        let truncated = (self.payload.len() as u64) < self.size;
        PubSubMessage {
            channel: key_display(&self.channel),
            pattern: self.pattern.map(|p| key_display(&p)),
            payload: payload_value(self.payload),
            size: self.size.to_string(),
            truncated,
            received_at: self.received_at,
        }
    }
}

/// Messages waiting for the UI.
#[derive(Default)]
struct Flow {
    queue: VecDeque<Raw>,
    queued_bytes: u64,
    /// Dropped since the last batch.
    dropped: u64,
    /// Dropped in all.
    dropped_total: u64,
    /// Last batch sent.
    sent: u64,
    /// Last batch acknowledged.
    acked: u64,
}

impl Flow {
    /// Appends a message, dropping the oldest beyond the caps.
    fn push(&mut self, raw: Raw) {
        self.queued_bytes = self.queued_bytes.saturating_add(raw.bytes());
        self.queue.push_back(raw);
        while self.queue.len() > QUEUE_MESSAGES || self.queued_bytes > QUEUE_BYTES {
            let Some(old) = self.queue.pop_front() else {
                break;
            };
            self.queued_bytes = self.queued_bytes.saturating_sub(old.bytes());
            self.dropped += 1;
            self.dropped_total += 1;
        }
    }

    /// The next batch (sequence number, messages, dropped since the last
    /// one), when there is something to send and room in flight.
    fn next_batch(&mut self) -> Option<(u64, Vec<Raw>, u64)> {
        if self.sent.saturating_sub(self.acked) >= MAX_IN_FLIGHT
            || (self.queue.is_empty() && self.dropped == 0)
        {
            return None;
        }
        let mut messages = Vec::new();
        let mut bytes = 0u64;
        while let Some(front) = self.queue.front() {
            let len = front.bytes();
            if !messages.is_empty()
                && (messages.len() >= MAX_BATCH_MESSAGES || bytes + len > MAX_BATCH_BYTES)
            {
                break;
            }
            let Some(message) = self.queue.pop_front() else {
                break;
            };
            bytes += len;
            messages.push(message);
        }
        self.queued_bytes = self.queued_bytes.saturating_sub(bytes);
        self.sent += 1;
        Some((self.sent, messages, std::mem::take(&mut self.dropped)))
    }

    /// Acknowledges batches up to `seq` (never past what was sent).
    fn ack(&mut self, seq: u64) {
        self.acked = self.acked.max(seq.min(self.sent));
    }
}

/// A command sent on the subscriber connection whose answer has not come
/// yet. The server answers in order, so an error answers the oldest one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    /// One name (one command per name).
    Subscribe(NameKind, Vec<u8>),
    /// Answers still to come (one per name).
    Unsubscribe(usize),
    Ping,
}

/// A name the server refused.
#[derive(Debug, Clone)]
struct Refusal {
    error: varve_driver::DriverError,
}

/// Tells the supervisor that the connection of one generation is dead.
/// A signal for an older connection (sent while the supervisor was already
/// reconnecting) is ignored, never stored for the next one.
#[derive(Default)]
struct DeadSignal {
    notify: Notify,
    /// Generation of the last connection reported dead.
    dead: AtomicU64,
}

impl DeadSignal {
    /// Reports connection `generation` dead.
    fn mark(&self, generation: u64) {
        self.dead.fetch_max(generation, Ordering::SeqCst);
        self.notify.notify_one();
    }

    /// Waits until connection `generation` is reported dead.
    async fn wait(&self, generation: u64) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.dead.load(Ordering::SeqCst) >= generation {
                return;
            }
            notified.await;
        }
    }
}

/// Why a connection was lost.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lost {
    /// The connection failed or the server closed it.
    Closed(String),
    /// The keepalive's `PING` got no answer.
    Silent,
    /// A change left no name: the server refused every name it added and
    /// the others were removed. The text is the server's refusal.
    Emptied(String),
}

struct Subscription {
    id: String,
    session_id: String,
    opener: PubSubOpener,
    options: SubscribeOptions,
    /// `None` while reconnecting.
    writer: tokio::sync::Mutex<Option<PubSubWriter>>,
    /// The names asked for (subscribed again after a reconnect).
    desired: Mutex<NameSets>,
    /// The names the server confirmed on the current connection.
    confirmed: Mutex<NameSets>,
    /// Commands sent on the current connection awaiting their answer.
    pending: Mutex<VecDeque<Pending>>,
    /// The current connection, 1, 2, ... (0 before the first).
    generation: AtomicU64,
    flow: Mutex<Flow>,
    wake: Notify,
    /// Wakes the supervisor when the keepalive or a failed write found
    /// the connection dead.
    dead: DeadSignal,
    tasks: Mutex<Vec<AbortHandle>>,
    closed: AtomicBool,
    reconnecting: AtomicBool,
    delivered: AtomicU64,
    /// When anything was last read.
    last_seen: Mutex<Instant>,
    /// When a message was last read.
    last_message: Mutex<Option<Instant>>,
    /// The server's refusal of a name added by the current change (cleared
    /// when a change starts and when a connection is handed over).
    change_refusal: Mutex<Option<String>>,
    /// `client_output_buffer_limit_disconnections` when the connection
    /// was opened.
    baseline: Mutex<Option<u64>>,
    emitter: Arc<dyn PubSubEmitter>,
    hub: Weak<Mutex<HashMap<String, Arc<Subscription>>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Text when the payload is UTF-8, bytes otherwise (as the key browser).
fn payload_value(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(text) => Value::Text(text),
        Err(error) => Value::Bytes(error.into_bytes()),
    }
}

/// Why a subscriber connection ended, in words. `baseline` and `report`
/// are the server's count of clients dropped for their output buffer when
/// the connection was opened and after it was lost; `receiving` says
/// whether messages were arriving until the loss ([`RECEIVING_WINDOW`]).
/// The buffer limit is blamed only when the count rose while messages were
/// arriving; when it rose while this subscription was quiet, or the server
/// does not count (Redis 6.2), the cause is given as unknown (a subscriber
/// can be dropped before the first byte of a burst reaches it); when it did
/// not rise, the buffer is not mentioned.
fn loss_reason(
    lost: &Lost,
    baseline: Option<u64>,
    report: Option<&OutputBufferReport>,
    receiving: bool,
) -> String {
    let cause = match lost {
        Lost::Silent => {
            return format!(
                "The connection ended: the server did not answer a PING within {} s (the \
                 network may be down)",
                PING_TIMEOUT.as_secs()
            );
        }
        Lost::Closed(cause) | Lost::Emptied(cause) => cause,
    };
    let limit = report
        .and_then(|r| r.pubsub_limit.clone())
        .map_or_else(String::new, |l| format!(" (now \"pubsub {l}\")"));
    let advice = "To keep up with such bursts, raise the limit, for example CONFIG SET \
                  client-output-buffer-limit \"pubsub 256mb 64mb 60\"";
    let rose = match (baseline, report.and_then(|r| r.disconnections)) {
        (Some(before), Some(after)) => Some(after > before),
        _ => None,
    };
    match (rose, receiving) {
        (Some(true), true) => format!(
            "The server dropped the subscription because its pubsub output buffer limit was \
             exceeded{limit}: messages were published faster than Varve could read them. \
             {advice}"
        ),
        (Some(true), false) => format!(
            "The connection ended: {cause}. The cause is unknown: the server dropped a client \
             for exceeding its output buffer limit at about this time, and it may have been \
             this subscription, if messages were published to it faster than Varve could read \
             them (pubsub output buffer limit{limit}). {advice}"
        ),
        (None, _) => format!(
            "The connection ended: {cause}. If messages were published faster than Varve could \
             read them, the server may have dropped the subscription because its pubsub output \
             buffer limit was exceeded{limit}. {advice}"
        ),
        (Some(false), _) => format!("The connection ended: {cause}"),
    }
}

/// What a successful [`Subscription::connect`] gives.
struct Connected {
    reader: PubSubReader,
    /// Names asked for.
    asked: usize,
    /// Names the server refused (dropped from the subscription).
    refused: Vec<Refusal>,
}

impl Connected {
    /// The server refused every name asked for: the error to report.
    fn all_refused(&self) -> Option<varve_driver::DriverError> {
        all_refused(self.asked, &self.refused)
    }
}

/// The first refusal when the server refused all `asked` names (and at
/// least one was asked for).
fn all_refused(asked: usize, refused: &[Refusal]) -> Option<varve_driver::DriverError> {
    if asked == 0 || refused.len() < asked {
        return None;
    }
    refused.first().map(|r| r.error.clone())
}

impl Subscription {
    /// Closes the subscription once: stops its tasks (which drops the
    /// connection), forgets it and tells the UI.
    fn close(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(hub) = self.hub.upgrade() {
            lock(&hub).remove(&self.id);
        }
        for task in lock(&self.tasks).drain(..) {
            task.abort();
        }
        self.emitter.emit(PubSubEvent::Closed {
            subscription_id: self.id.clone(),
            reason: reason.to_string(),
        });
    }

    fn seen(&self) {
        *lock(&self.last_seen) = Instant::now();
    }

    fn next_batch(&self) -> Option<PubSubEvent> {
        let (seq, raws, dropped) = lock(&self.flow).next_batch()?;
        self.delivered
            .fetch_add(raws.len() as u64, Ordering::Relaxed);
        Some(PubSubEvent::Batch {
            subscription_id: self.id.clone(),
            seq,
            messages: raws.into_iter().map(Raw::message).collect(),
            dropped,
        })
    }

    fn info(&self) -> PubSubSubscriptionInfo {
        let names = lock(&self.confirmed);
        let shown = |set: &BTreeSet<Vec<u8>>| set.iter().map(|n| key_display(n)).collect();
        PubSubSubscriptionInfo {
            subscription_id: self.id.clone(),
            channels: shown(&names.channels),
            patterns: shown(&names.patterns),
            shard_channels: shown(&names.shards),
            state: if self.reconnecting.load(Ordering::SeqCst) {
                "reconnecting"
            } else {
                "subscribed"
            },
            delivered: self.delivered.load(Ordering::Relaxed),
            dropped: lock(&self.flow).dropped_total,
        }
    }

    /// The full name of the oldest pending subscription of `name` (the
    /// server shortens names to [`varve_driver_redis::MAX_NAME_BYTES`]).
    fn answered_subscribe(&self, kind: NameKind, name: &[u8]) -> Vec<u8> {
        let mut pending = lock(&self.pending);
        let shortened = name.len() as u64 >= varve_driver_redis::MAX_NAME_BYTES;
        let at = pending.iter().position(|p| {
            matches!(p, Pending::Subscribe(k, n)
                if *k == kind && (n.as_slice() == name || (shortened && n.starts_with(name))))
        });
        match at.and_then(|i| pending.remove(i)) {
            Some(Pending::Subscribe(_, full)) => full,
            _ => name.to_vec(),
        }
    }

    fn answered_unsubscribe(&self) {
        let mut pending = lock(&self.pending);
        let at = pending
            .iter()
            .position(|p| matches!(p, Pending::Unsubscribe(_)));
        if let Some(at) = at
            && let Some(Pending::Unsubscribe(left)) = pending.get_mut(at)
        {
            *left = left.saturating_sub(1);
            if *left == 0 {
                pending.remove(at);
            }
        }
    }

    fn answered_ping(&self) {
        let mut pending = lock(&self.pending);
        if let Some(at) = pending.iter().position(|p| *p == Pending::Ping) {
            pending.remove(at);
        }
    }

    /// True while a name sent on this connection waits for its answer.
    fn awaiting_names(&self) -> bool {
        lock(&self.pending)
            .iter()
            .any(|p| matches!(p, Pending::Subscribe(..)))
    }

    /// Handles one item read from the connection. Messages are only
    /// queued: no conversion on the socket's path. Returns the refusal
    /// when the server refused a name.
    fn handle(&self, item: PubSubItem) -> Option<Refusal> {
        self.seen();
        match item {
            PubSubItem::Message {
                pattern,
                channel,
                payload,
                size,
            } => {
                *lock(&self.last_message) = Some(Instant::now());
                lock(&self.flow).push(Raw {
                    pattern,
                    channel,
                    payload,
                    size,
                    received_at: now_ms(),
                });
                self.wake.notify_one();
            }
            PubSubItem::Subscribed { kind, name, count } => {
                let name = self.answered_subscribe(kind, &name);
                lock(&self.confirmed).of(kind).insert(name.clone());
                self.emitter.emit(PubSubEvent::Subscribed {
                    subscription_id: self.id.clone(),
                    name_kind: kind,
                    name: key_display(&name),
                    count,
                });
            }
            PubSubItem::Unsubscribed { kind, name, count } => {
                self.answered_unsubscribe();
                lock(&self.confirmed).of(kind).remove(&name);
                self.emitter.emit(PubSubEvent::Unsubscribed {
                    subscription_id: self.id.clone(),
                    name_kind: kind,
                    name: key_display(&name),
                    count,
                });
            }
            PubSubItem::Error(error) => {
                // The answer to the oldest command still waiting.
                let answered = lock(&self.pending).pop_front();
                let refused = match answered {
                    Some(Pending::Subscribe(kind, name)) => {
                        // Never asked for again (it would be refused on
                        // every reconnect).
                        lock(&self.desired).of(kind).remove(&name);
                        *lock(&self.change_refusal) = Some(error.message.clone());
                        Some((kind, name))
                    }
                    _ => None,
                };
                self.emitter.emit(PubSubEvent::Error {
                    subscription_id: self.id.clone(),
                    message: error.message.clone(),
                    code: error.code.clone(),
                    name_kind: refused.as_ref().map(|(kind, _)| *kind),
                    name: refused.as_ref().map(|(_, name)| key_display(name)),
                });
                return refused.map(|_| Refusal { error });
            }
            PubSubItem::Pong => self.answered_ping(),
        }
        None
    }

    /// Sends one subscribe command per name, noting each as pending.
    async fn send_subscribe(
        &self,
        writer: &mut PubSubWriter,
        kind: NameKind,
        names: &[Vec<u8>],
    ) -> Result<(), varve_driver::DriverError> {
        lock(&self.pending).extend(names.iter().map(|n| Pending::Subscribe(kind, n.clone())));
        writer.subscribe_each(kind, names).await
    }

    async fn send_unsubscribe(
        &self,
        writer: &mut PubSubWriter,
        kind: NameKind,
        names: &[Vec<u8>],
    ) -> Result<(), varve_driver::DriverError> {
        if names.is_empty() {
            return Ok(());
        }
        lock(&self.pending).push_back(Pending::Unsubscribe(names.len()));
        writer.unsubscribe(kind, names).await
    }

    /// Opens a connection (reading the server's output buffer drop count
    /// on it first), subscribes to the wanted names one command each, and
    /// reads until the server answered for every one; refused names are
    /// dropped. Names added or removed meanwhile are sent before the
    /// connection is handed over. Not bounded itself: callers bound it.
    async fn connect(&self) -> Result<Connected, varve_driver::DriverError> {
        let (mut reader, mut writer, report) = self.opener.open_with_report().await?;
        lock(&self.pending).clear();
        *lock(&self.confirmed) = NameSets::default();
        let asked = lock(&self.desired).clone();
        for (kind, list) in asked.lists() {
            self.send_subscribe(&mut writer, kind, &list).await?;
        }
        let mut refused = Vec::new();
        while self.awaiting_names() {
            let item = reader.next(self.options.max_payload_bytes).await?;
            refused.extend(self.handle(item));
        }
        let mut guard = self.writer.lock().await;
        // `redis_pubsub_change` while this connection was not handed over
        // only changed `desired`.
        let now = lock(&self.desired).clone();
        for ((kind, wanted), (_, before)) in now.lists().into_iter().zip(asked.lists()) {
            let added: Vec<Vec<u8>> = wanted
                .iter()
                .filter(|n| !before.contains(n))
                .cloned()
                .collect();
            self.send_subscribe(&mut writer, kind, &added).await?;
        }
        for ((kind, before), (_, wanted)) in asked.lists().into_iter().zip(now.lists()) {
            let confirmed = lock(&self.confirmed).of(kind).clone();
            let removed: Vec<Vec<u8>> = before
                .into_iter()
                .filter(|n| !wanted.contains(n) && confirmed.contains(n))
                .collect();
            self.send_unsubscribe(&mut writer, kind, &removed).await?;
        }
        *lock(&self.baseline) = report.disconnections;
        *lock(&self.change_refusal) = None;
        self.generation.fetch_add(1, Ordering::SeqCst);
        *guard = Some(writer);
        drop(guard);
        self.seen();
        Ok(Connected {
            reader,
            asked: asked.len(),
            refused,
        })
    }

    /// Reads until the connection fails or is found dead; returns why.
    async fn read_until_lost(&self, reader: &mut PubSubReader) -> Lost {
        let generation = self.generation.load(Ordering::SeqCst);
        loop {
            tokio::select! {
                item = reader.next(self.options.max_payload_bytes) => match item {
                    Ok(item) => {
                        self.handle(item);
                        if let Some(refusal) = self.emptied_by_refusal() {
                            return Lost::Emptied(refusal);
                        }
                    }
                    Err(error) => return Lost::Closed(error.message),
                },
                () = self.dead.wait(generation) => return Lost::Silent,
            }
        }
    }

    /// The refusal that left the subscription without names: a change's
    /// added names were all refused, nothing else is wanted, confirmed or
    /// waiting for an answer.
    fn emptied_by_refusal(&self) -> Option<String> {
        let refusal = lock(&self.change_refusal).clone()?;
        let empty = lock(&self.desired).len() == 0
            && lock(&self.confirmed).len() == 0
            && lock(&self.pending).iter().all(|p| *p == Pending::Ping);
        empty.then_some(refusal)
    }

    /// True when a message arrived within [`RECEIVING_WINDOW`].
    fn receiving(&self) -> bool {
        lock(&self.last_message).is_some_and(|at| at.elapsed() <= RECEIVING_WINDOW)
    }

    /// Why the connection was lost, in words. Asks the server (within the
    /// first second, on a new connection) unless the network is down.
    async fn loss_reason(&self, lost: &Lost) -> String {
        let receiving = self.receiving();
        let report = match lost {
            Lost::Silent | Lost::Emptied(_) => None,
            Lost::Closed(_) => {
                tokio::time::timeout(attempt_start(1), self.opener.output_buffer_report())
                    .await
                    .ok()
                    .and_then(Result::ok)
            }
        };
        let baseline = *lock(&self.baseline);
        loss_reason(lost, baseline, report.as_ref(), receiving)
    }
}

/// Reads the connection; on a loss, explains it and resubscribes (on the
/// schedule of [`attempt_start`]) or closes.
async fn supervise(sub: Arc<Subscription>, mut reader: PubSubReader) {
    loop {
        let lost = sub.read_until_lost(&mut reader).await;
        drop(reader);
        if let Lost::Emptied(refusal) = &lost {
            sub.close(&format!(
                "The server refused every name, so nothing is subscribed: {refusal}"
            ));
            return;
        }
        let lost_since = Instant::now();
        let lost_at = now_ms();
        sub.reconnecting.store(true, Ordering::SeqCst);
        *sub.writer.lock().await = None;
        if sub.closed.load(Ordering::SeqCst) {
            return;
        }
        let reason = sub.loss_reason(&lost).await;
        if !sub.options.resubscribe {
            sub.close(&reason);
            return;
        }
        let mut last_error = String::new();
        let mut resumed = None;
        for attempt in 1..=RESUBSCRIBE_ATTEMPTS {
            sub.emitter.emit(PubSubEvent::Reconnecting {
                subscription_id: sub.id.clone(),
                attempt,
                reason: reason.clone(),
            });
            tokio::time::sleep_until((lost_since + attempt_start(attempt)).into()).await;
            if sub.closed.load(Ordering::SeqCst) {
                return;
            }
            let window = if attempt < RESUBSCRIBE_ATTEMPTS {
                attempt_start(attempt + 1).saturating_sub(lost_since.elapsed())
            } else {
                sub.opener.connect_timeout() * 2
            };
            match tokio::time::timeout(window, sub.connect()).await {
                Ok(Ok(connected)) => {
                    resumed = Some(connected);
                    break;
                }
                Ok(Err(error)) => last_error = error.message,
                Err(_) => {
                    last_error = format!(
                        "the server did not answer within {:.1} s",
                        window.as_secs_f32()
                    );
                }
            }
        }
        let Some(connected) = resumed else {
            sub.close(&format!(
                "{reason}. Subscribing again failed {RESUBSCRIBE_ATTEMPTS} times: {last_error}"
            ));
            return;
        };
        if let Some(error) = connected.all_refused() {
            sub.close(&format!(
                "{reason}. Subscribing again was refused for every name: {}",
                error.message
            ));
            return;
        }
        reader = connected.reader;
        sub.reconnecting.store(false, Ordering::SeqCst);
        sub.emitter.emit(PubSubEvent::Gap {
            subscription_id: sub.id.clone(),
            reason,
            lost_at,
            resumed_at: now_ms(),
        });
    }
}

/// Sends batches while the UI keeps up.
async fn send_loop(sub: Arc<Subscription>) {
    loop {
        tokio::select! {
            () = sub.wake.notified() => {}
            () = tokio::time::sleep(BATCH_INTERVAL * 4) => {}
        }
        while let Some(batch) = sub.next_batch() {
            sub.emitter.emit(batch);
            tokio::time::sleep(BATCH_INTERVAL).await;
        }
    }
}

/// Pings a quiet connection and wakes the supervisor when nothing answers.
async fn keepalive(sub: Arc<Subscription>) {
    loop {
        tokio::time::sleep(KEEPALIVE_IDLE / 3).await;
        if sub.reconnecting.load(Ordering::SeqCst)
            || lock(&sub.last_seen).elapsed() < KEEPALIVE_IDLE
        {
            continue;
        }
        let pinged = Instant::now();
        let (generation, sent) = {
            let mut guard = sub.writer.lock().await;
            let Some(writer) = guard.as_mut() else {
                continue;
            };
            lock(&sub.pending).push_back(Pending::Ping);
            (
                sub.generation.load(Ordering::SeqCst),
                writer.ping().await.is_ok(),
            )
        };
        tokio::time::sleep(if sent { PING_TIMEOUT } else { Duration::ZERO }).await;
        if *lock(&sub.last_seen) < pinged {
            // A signal for a connection already replaced is ignored.
            sub.dead.mark(generation);
        }
    }
}

/// Every open subscription.
#[derive(Default)]
pub struct PubSubHub {
    subs: Arc<Mutex<HashMap<String, Arc<Subscription>>>>,
}

impl std::fmt::Debug for PubSubHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PubSubHub")
            .field("subscriptions", &lock(&self.subs).len())
            .finish()
    }
}

impl PubSubHub {
    /// Closes the subscriptions of a closed session.
    pub fn forget_session(&self, session_id: &str) {
        let subs: Vec<Arc<Subscription>> = lock(&self.subs)
            .values()
            .filter(|s| s.session_id == session_id)
            .cloned()
            .collect();
        for sub in subs {
            sub.close("the tab's session closed");
        }
    }

    /// Open subscriptions of all sessions.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.subs).len()
    }

    /// True when no subscription is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, id: &str) -> CmdResult<Arc<Subscription>> {
        lock(&self.subs)
            .get(id)
            .cloned()
            .ok_or_else(|| CmdError::NotFound {
                message: "This subscription is closed; subscribe again.".into(),
            })
    }
}

/// Names as bytes, without duplicates.
fn names_of(names: &PubSubNames) -> CmdResult<NameSets> {
    let mut sets = NameSets::default();
    for (kind, list, what) in [
        (NameKind::Channel, &names.channels, "channel"),
        (NameKind::Pattern, &names.patterns, "pattern"),
        (
            NameKind::ShardChannel,
            &names.shard_channels,
            "sharded channel",
        ),
    ] {
        for text in list {
            let bytes = name_bytes(text)?;
            if bytes.is_empty() {
                return Err(CmdError::Invalid {
                    message: format!("A {what} name is empty"),
                });
            }
            sets.of(kind).insert(bytes);
        }
    }
    Ok(sets)
}

fn too_many(message: String) -> CmdError {
    CmdError::Invalid { message }
}

fn names_cap() -> CmdError {
    too_many(format!(
        "A subscription has at most {MAX_NAMES} channels, patterns and sharded channels"
    ))
}

fn sharded_unsupported() -> CmdError {
    CmdError::Invalid {
        message: "Sharded Pub/Sub (SSUBSCRIBE) needs Redis 7.0 or newer, or Valkey".into(),
    }
}

impl Services {
    /// Subscribes to channels, patterns and sharded channels on a new
    /// dedicated connection.
    ///
    /// # Errors
    /// Unknown or non-Redis session, no or too many names, sharded channels
    /// on Redis 6.2, too many subscriptions (`invalid`), connect errors.
    pub async fn redis_pubsub_subscribe(
        &self,
        session_id: &str,
        names: &PubSubNames,
        options: SubscribeOptions,
        emitter: Arc<dyn PubSubEmitter>,
    ) -> CmdResult<PubSubSubscription> {
        if options.max_payload_bytes == 0 || options.max_payload_bytes > MAX_PAYLOAD_BYTES {
            return Err(CmdError::Invalid {
                message: format!("maxPayloadBytes must be 1 to {MAX_PAYLOAD_BYTES}"),
            });
        }
        let desired = names_of(names)?;
        if desired.len() == 0 {
            return Err(CmdError::Invalid {
                message: "Subscribe to at least one channel or pattern".into(),
            });
        }
        if desired.len() > MAX_NAMES {
            return Err(names_cap());
        }
        self.pubsub_room(session_id)?;
        let (_, session_lock) = self.redis_session_lock(session_id).await?;
        let opener = Self::redis_of(&session_lock, session_id)?
            .pubsub_opener()
            .await;
        drop(session_lock);
        if !desired.shards.is_empty() && !opener.has_sharded() {
            return Err(sharded_unsupported());
        }
        let hub = &self.m2.pubsub;
        let sub = Arc::new(Subscription {
            id: uuid::Uuid::now_v7().to_string(),
            session_id: session_id.to_string(),
            opener,
            options,
            writer: tokio::sync::Mutex::new(None),
            desired: Mutex::new(desired),
            confirmed: Mutex::new(NameSets::default()),
            pending: Mutex::new(VecDeque::new()),
            generation: AtomicU64::new(0),
            flow: Mutex::new(Flow::default()),
            wake: Notify::new(),
            dead: DeadSignal::default(),
            tasks: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            reconnecting: AtomicBool::new(false),
            delivered: AtomicU64::new(0),
            last_seen: Mutex::new(Instant::now()),
            last_message: Mutex::new(None),
            change_refusal: Mutex::new(None),
            baseline: Mutex::new(None),
            emitter,
            hub: Arc::downgrade(&hub.subs),
        });
        // Opening is bounded by the connect timeout; the server's answers
        // for the names get as long again.
        let bound = sub.opener.connect_timeout() * 2;
        let connected = tokio::time::timeout(bound, sub.connect())
            .await
            .map_err(|_| {
                driver_err(varve_driver::DriverError::new(
                    varve_driver::ErrorKind::Timeout,
                    format!(
                        "The server did not confirm the subscription within {} s",
                        bound.as_secs()
                    ),
                ))
            })?
            .map_err(driver_err)?;
        if let Some(mut error) = connected.all_refused() {
            error.detail = Some("The server refused every name, so nothing is subscribed".into());
            return Err(driver_err(error));
        }
        let reader = connected.reader;
        {
            let mut subs = lock(&hub.subs);
            let own = subs.values().filter(|s| s.session_id == session_id).count();
            if subs.len() >= MAX_TOTAL || own >= MAX_PER_SESSION {
                return Err(self.pubsub_full(subs.len()));
            }
            subs.insert(sub.id.clone(), Arc::clone(&sub));
        }
        let tasks = [
            tokio::spawn(supervise(Arc::clone(&sub), reader)).abort_handle(),
            tokio::spawn(send_loop(Arc::clone(&sub))).abort_handle(),
            tokio::spawn(keepalive(Arc::clone(&sub))).abort_handle(),
        ];
        lock(&sub.tasks).extend(tasks);
        // Closed while connecting: the hook already ran.
        if self.sessions.info(session_id).is_err() {
            sub.close("the tab's session closed");
        }
        Ok(PubSubSubscription {
            subscription_id: sub.id.clone(),
            event: EVENT,
        })
    }

    fn pubsub_room(&self, session_id: &str) -> CmdResult<()> {
        let subs = lock(&self.m2.pubsub.subs);
        let own = subs.values().filter(|s| s.session_id == session_id).count();
        if subs.len() >= MAX_TOTAL || own >= MAX_PER_SESSION {
            return Err(self.pubsub_full(subs.len()));
        }
        Ok(())
    }

    fn pubsub_full(&self, total: usize) -> CmdError {
        too_many(if total >= MAX_TOTAL {
            format!("Varve keeps at most {MAX_TOTAL} subscriptions open; close one first")
        } else {
            format!("A tab keeps at most {MAX_PER_SESSION} subscriptions open; close one first")
        })
    }

    /// Adds and removes names of a subscription. The listed names change
    /// when the server confirms them (`subscribed` / `unsubscribed`
    /// events); a name it refuses comes back as an `error` event naming it
    /// and is dropped. While the connection is being opened again, the
    /// change applies when it is back.
    ///
    /// # Errors
    /// A closed subscription (`notFound`), too many names or sharded
    /// channels on Redis 6.2 (`invalid`), a lost connection.
    pub async fn redis_pubsub_change(
        &self,
        subscription_id: &str,
        add: &PubSubNames,
        remove: &PubSubNames,
    ) -> CmdResult<()> {
        let sub = self.m2.pubsub.get(subscription_id)?;
        let add = names_of(add)?;
        let remove = names_of(remove)?;
        if !add.shards.is_empty() && !sub.opener.has_sharded() {
            return Err(sharded_unsupported());
        }
        let mut guard = sub.writer.lock().await;
        *lock(&sub.change_refusal) = None;
        {
            let mut desired = lock(&sub.desired);
            let mut next = desired.clone();
            for (kind, list) in add.lists() {
                next.of(kind).extend(list);
            }
            for (kind, list) in remove.lists() {
                for name in list {
                    next.of(kind).remove(&name);
                }
            }
            if next.len() > MAX_NAMES {
                return Err(names_cap());
            }
            *desired = next;
        }
        let Some(writer) = guard.as_mut() else {
            // Reconnecting: the desired names are subscribed when it is back.
            return Ok(());
        };
        let generation = sub.generation.load(Ordering::SeqCst);
        let sent = async {
            for (kind, list) in add.lists() {
                sub.send_subscribe(writer, kind, &list).await?;
            }
            for (kind, list) in remove.lists() {
                sub.send_unsubscribe(writer, kind, &list).await?;
            }
            Ok::<_, varve_driver::DriverError>(())
        }
        .await;
        drop(guard);
        if let Err(error) = sent {
            // The supervisor notices the lost connection and resubscribes.
            sub.dead.mark(generation);
            return Err(driver_err(error));
        }
        Ok(())
    }

    /// Acknowledges batches up to `seq`, which lets more be sent.
    ///
    /// # Errors
    /// A closed subscription (`notFound`).
    pub fn redis_pubsub_ack(&self, subscription_id: &str, seq: u64) -> CmdResult<()> {
        let sub = self.m2.pubsub.get(subscription_id)?;
        lock(&sub.flow).ack(seq);
        sub.wake.notify_one();
        Ok(())
    }

    /// Closes a subscription; unknown ids are fine (already closed).
    pub fn redis_pubsub_unsubscribe(&self, subscription_id: &str) {
        if let Ok(sub) = self.m2.pubsub.get(subscription_id) {
            sub.close("unsubscribed");
        }
    }

    /// The open subscriptions of a session.
    ///
    /// # Errors
    /// Unknown session.
    pub fn redis_pubsub_subscriptions(
        &self,
        session_id: &str,
    ) -> CmdResult<Vec<PubSubSubscriptionInfo>> {
        self.session_info(session_id)?;
        let mut list: Vec<PubSubSubscriptionInfo> = lock(&self.m2.pubsub.subs)
            .values()
            .filter(|s| s.session_id == session_id)
            .map(|s| s.info())
            .collect();
        list.sort_by(|a, b| a.subscription_id.cmp(&b.subscription_id));
        Ok(list)
    }

    /// `PUBLISH` on the tab's session; returns how many clients received
    /// the message. Refused on read-only sessions; needs confirmation when
    /// the session confirms destructive statements (production).
    ///
    /// # Errors
    /// As described, a malformed channel or message (`invalid`), driver
    /// errors (`MULTI_OPEN` while a console `MULTI` is open).
    pub async fn redis_publish(
        &self,
        session_id: &str,
        channel: &str,
        message: &Value,
        confirmed: bool,
    ) -> CmdResult<i64> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "Publishing")?;
        Self::require_writable(&info, "publishing")?;
        let channel = name_bytes(channel)?;
        if channel.is_empty() {
            return Err(CmdError::Invalid {
                message: "The channel name is empty".into(),
            });
        }
        let payload = match message {
            Value::Text(text) => text.as_bytes().to_vec(),
            Value::Bytes(bytes) => bytes.clone(),
            _ => {
                return Err(CmdError::Invalid {
                    message: "The message must be text or bytes".into(),
                });
            }
        };
        let needs = info.confirm_destructive;
        self.require_confirmation(&info, needs, confirmed, vec!["PUBLISH".into()])?;
        let (_, session_lock) = self.redis_session_lock(session_id).await?;
        let started = Instant::now();
        let result = Self::redis_of(&session_lock, session_id)?
            .publish(&channel, &payload)
            .await;
        drop(session_lock);
        let duration_ms = elapsed_ms(started);
        let text = varve_redis_syntax::join_args(&[b"PUBLISH".as_slice(), &channel, &payload]);
        let event = match &result {
            Ok(receivers) => ActivityEvent::query_executed(
                &text,
                SqlDialect::Redis,
                "PUBLISH",
                RowCounts {
                    returned: None,
                    affected: u64::try_from(*receivers).ok(),
                },
                duration_ms,
            ),
            Err(error) => ActivityEvent::query_failed(
                &text,
                SqlDialect::Redis,
                "PUBLISH",
                &ErrorDetails {
                    code: error.code.clone(),
                    message: error.message.clone(),
                },
                duration_ms,
            ),
        };
        self.record(&info, event);
        if needs && confirmed && result.is_ok() {
            self.record(
                &info,
                ActivityEvent::destructive_confirmed("PUBLISH", &environment_label(&info)),
            );
        }
        result.map_err(driver_err)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn events_serialize_with_name_kind() -> Result<(), serde_json::Error> {
        let event = PubSubEvent::Subscribed {
            subscription_id: "s".into(),
            name_kind: NameKind::ShardChannel,
            name: "orders".into(),
            count: 2,
        };
        assert_eq!(
            serde_json::to_value(&event)?,
            json!({ "kind": "subscribed", "subscriptionId": "s", "nameKind": "shardChannel", "name": "orders", "count": 2 })
        );
        let gap = PubSubEvent::Gap {
            subscription_id: "s".into(),
            reason: "r".into(),
            lost_at: 1,
            resumed_at: 2,
        };
        assert_eq!(
            serde_json::to_value(&gap)?,
            json!({ "kind": "gap", "subscriptionId": "s", "reason": "r", "lostAt": 1, "resumedAt": 2 })
        );
        let names: PubSubNames = serde_json::from_value(json!({ "channels": ["a"] }))?;
        assert!(names.patterns.is_empty() && names.shard_channels.is_empty());
        Ok(())
    }

    fn raw(n: usize, name_bytes: usize) -> Raw {
        Raw {
            pattern: None,
            channel: vec![b'c'; name_bytes],
            payload: format!("m{n}").into_bytes(),
            size: 2,
            received_at: 0,
        }
    }

    #[test]
    fn batches_respect_credits_caps_and_drops() {
        let mut flow = Flow::default();
        for n in 0..(QUEUE_MESSAGES + 5) {
            flow.push(raw(n, 1));
        }
        let mut seqs = Vec::new();
        let mut dropped = 0;
        let mut first_len = 0;
        while let Some((seq, messages, d)) = flow.next_batch() {
            if seqs.is_empty() {
                first_len = messages.len();
            }
            seqs.push(seq);
            dropped += d;
        }
        assert_eq!(seqs, [1, 2, 3, 4], "at most four in flight");
        assert_eq!(first_len, MAX_BATCH_MESSAGES);
        assert_eq!(dropped, 5, "the oldest were dropped and counted");
        flow.ack(99);
        assert_eq!(flow.acked, 4, "acks never pass what was sent");
        assert!(matches!(flow.next_batch(), Some((5, _, 0))));
    }

    #[test]
    fn names_count_against_the_queue() {
        let mut flow = Flow::default();
        // 64 KiB names: 16 MiB is reached after 256 messages, not 10000.
        for n in 0..300 {
            flow.push(raw(n, 64 * 1024));
        }
        assert!(flow.queued_bytes <= QUEUE_BYTES);
        assert_eq!(flow.queue.len(), 255);
        assert_eq!(flow.dropped_total, 45);
    }

    #[test]
    fn shortened_messages_say_so() {
        let message = Raw {
            pattern: Some(b"p.*".to_vec()),
            channel: vec![0xff, b'a'],
            payload: b"abc".to_vec(),
            size: 10,
            received_at: 5,
        }
        .message();
        assert!(message.truncated);
        assert_eq!(message.size, "10");
        assert_eq!(message.channel, "\\xffa");
        assert_eq!(message.pattern.as_deref(), Some("p.*"));
    }

    #[test]
    fn loss_reasons_name_the_output_buffer_limit_only_when_it_can_be_the_cause() {
        let report = OutputBufferReport {
            disconnections: Some(3),
            pubsub_limit: Some("33554432 8388608 60".into()),
        };
        let closed = |cause: &str| Lost::Closed(cause.into());
        // The count rose while messages were arriving: ours.
        let dropped = loss_reason(&closed("early eof"), Some(2), Some(&report), true);
        assert!(dropped.starts_with("The server dropped the subscription because its pubsub output buffer limit was exceeded"));
        assert!(dropped.contains("pubsub 33554432 8388608 60"));
        assert!(dropped.contains("CONFIG SET client-output-buffer-limit"));
        // The count rose, but this subscription was quiet: another client.
        let other = loss_reason(&closed("early eof"), Some(2), Some(&report), false);
        assert!(other.starts_with("The connection ended: early eof. The cause is unknown"));
        // Nothing claims the subscription was quiet: a burst can get it
        // dropped before its first byte arrives.
        assert!(!other.contains("not receiving"), "{other}");
        assert!(
            other.contains("it may have been this subscription"),
            "{other}"
        );
        // The count did not rise.
        let plain = loss_reason(&closed("reset by peer"), Some(3), Some(&report), true);
        assert_eq!(plain, "The connection ended: reset by peer");
        // No count (Redis 6.2): hedged.
        for receiving in [true, false] {
            let unknown = loss_reason(&closed("early eof"), None, None, receiving);
            assert!(unknown.contains("may have dropped"));
        }
    }

    #[test]
    fn a_keepalive_timeout_never_blames_the_buffer() {
        let report = OutputBufferReport {
            disconnections: Some(9),
            pubsub_limit: Some("1 1 0".into()),
        };
        for (baseline, report) in [(None, None), (Some(1), Some(&report))] {
            let reason = loss_reason(&Lost::Silent, baseline, report, true);
            assert!(reason.contains("did not answer a PING"), "{reason}");
            assert!(!reason.contains("buffer"), "{reason}");
        }
    }

    #[test]
    fn attempts_start_on_a_fixed_schedule() {
        let starts: Vec<u64> = (1..=RESUBSCRIBE_ATTEMPTS)
            .map(|n| attempt_start(n).as_secs())
            .collect();
        assert_eq!(starts, [1, 3, 7, 15, 31]);
        let gaps: Vec<u64> = starts.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(gaps, [2, 4, 8, 16]);
    }

    #[tokio::test]
    async fn a_dead_signal_for_an_old_connection_is_ignored() {
        let signal = DeadSignal::default();
        // Connection 1 was found dead while the supervisor was already
        // reconnecting: nobody waited, the signal is stored.
        signal.mark(1);
        // Connection 2 must not end on it.
        let early = tokio::time::timeout(Duration::from_millis(100), signal.wait(2)).await;
        assert!(early.is_err(), "a stale signal ended connection 2");
        // A signal for connection 2 does.
        signal.mark(2);
        let ended = tokio::time::timeout(Duration::from_millis(100), signal.wait(2)).await;
        assert!(ended.is_ok());
    }

    #[test]
    fn every_name_refused_is_an_error() {
        let refusal = || Refusal {
            error: varve_driver::DriverError::new(varve_driver::ErrorKind::Query, "NOPERM no")
                .with_code("NOPERM"),
        };
        let all = |asked: usize, refused: usize| {
            let refused: Vec<Refusal> = (0..refused).map(|_| refusal()).collect();
            all_refused(asked, &refused).is_some()
        };
        assert!(all(2, 2));
        assert!(!all(2, 1));
        assert!(!all(0, 0), "a subscription without names is not refused");
    }

    #[test]
    fn names_are_checked() -> Result<(), CmdError> {
        let names = PubSubNames {
            channels: vec!["a".into(), "a".into(), "b".into()],
            patterns: vec!["x\\x00*".into()],
            shard_channels: vec!["s".into()],
        };
        let sets = names_of(&names)?;
        assert_eq!(sets.channels.len(), 2);
        assert_eq!(sets.patterns.iter().next(), Some(&b"x\0*".to_vec()));
        assert_eq!(sets.len(), 4);
        assert!(
            names_of(&PubSubNames {
                channels: vec![String::new()],
                ..PubSubNames::default()
            })
            .is_err()
        );
        assert!(
            names_of(&PubSubNames {
                channels: vec!["bad\\q".into()],
                ..PubSubNames::default()
            })
            .is_err()
        );
        Ok(())
    }
}
