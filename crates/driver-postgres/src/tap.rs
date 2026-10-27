//! A transparent stream wrapper that watches the backend messages
//! tokio-postgres reads, to recover what its API does not expose.
//!
//! tokio-postgres reports a finished statement as a row count only, but the
//! UI shows the server's command tag (`UPDATE 3`, `CREATE TABLE`). The tap
//! parses the message framing of everything the server sends and records the
//! tag of every `CommandComplete` (and an empty entry for every
//! `EmptyQueryResponse`) in order. Because the connection task reads the
//! bytes before it hands the decoded message to the client, a tag is always
//! queued before the client sees the matching completion.
//!
//! It also records the transaction status of every `ReadyForQuery`, which
//! the read-only guard needs and tokio-postgres does not expose, and it
//! follows the frontend messages tokio-postgres writes (`Parse`, `Close`,
//! `Sync`) to know which named prepared statements exist on the server:
//! tokio-postgres's own type lookup statements, and statements left behind
//! by a prepare that failed half way.
//!
//! The tap never changes, delays or reorders bytes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;
use tokio_postgres::tls::{ChannelBinding, TlsStream};
use varve_driver::Notice;

/// Longest command tag body kept. Real tags are a few dozen bytes.
const MAX_TAG_LEN: usize = 4096;

/// Most server notices kept per execution (a `RAISE NOTICE` in a loop must
/// not grow memory without bound).
pub(crate) const MAX_NOTICES: usize = 1000;

/// Most bytes of server notice text (severity plus message) kept per
/// execution.
pub(crate) const MAX_NOTICE_BYTES: usize = 1024 * 1024;

/// Longest statement name kept from a frontend `Parse` or `Close`.
/// tokio-postgres names its statements `s<number>`.
const MAX_STATEMENT_NAME: usize = 64;

/// The type lookup queries of tokio-postgres 0.7.18 (`src/prepare.rs`),
/// which it prepares once per connection and keeps, as patched in
/// `vendor/tokio-postgres` (every operator and cast qualified with
/// `pg_catalog`, see its `VARVE-PATCH.md`). The crate pins that version
/// exactly; a new version must be checked against these texts.
pub(crate) const TYPEINFO_QUERIES: [&str; 5] = [
    "SELECT t.typname, t.typtype, t.typelem, r.rngsubtype, t.typbasetype, n.nspname, t.typrelid
FROM pg_catalog.pg_type t
LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid OPERATOR(pg_catalog.=) t.oid
INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace OPERATOR(pg_catalog.=) n.oid
WHERE t.oid OPERATOR(pg_catalog.=) $1
",
    "SELECT t.typname, t.typtype, t.typelem, NULL::pg_catalog.oid, t.typbasetype, n.nspname, t.typrelid
FROM pg_catalog.pg_type t
INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace OPERATOR(pg_catalog.=) n.oid
WHERE t.oid OPERATOR(pg_catalog.=) $1
",
    "SELECT enumlabel
FROM pg_catalog.pg_enum
WHERE enumtypid OPERATOR(pg_catalog.=) $1
ORDER BY enumsortorder
",
    "SELECT enumlabel
FROM pg_catalog.pg_enum
WHERE enumtypid OPERATOR(pg_catalog.=) $1
ORDER BY oid
",
    "SELECT attname, atttypid
FROM pg_catalog.pg_attribute
WHERE attrelid OPERATOR(pg_catalog.=) $1
AND NOT attisdropped
AND attnum OPERATOR(pg_catalog.>) 0
ORDER BY attnum
",
];

/// A named statement sent with `Parse`: its name and whether it is one of
/// tokio-postgres's type lookups: which of them ([`LOOKUP_ROLE`]).
type Parsed = (String, Option<usize>);

/// What each of [`TYPEINFO_QUERIES`] looks up: a type, the labels of an
/// enum, the fields of a composite. tokio-postgres keeps one prepared
/// statement per role (a fallback query takes the place of the query it
/// replaces on old servers).
const LOOKUP_ROLE: [usize; TYPEINFO_QUERIES.len()] = [0, 0, 1, 1, 2];

/// The named prepared statements of the connection, as far as the frontend
/// and backend messages show.
#[derive(Debug, Default)]
struct StatementLog {
    /// `Parse`s written since the last `Sync` or `Query`.
    current: VecDeque<Parsed>,
    /// `Parse`s of each `Sync`/`Query` the server has not answered with
    /// `ReadyForQuery` yet, oldest first, not yet confirmed by
    /// `ParseComplete`.
    groups: VecDeque<VecDeque<Parsed>>,
    /// Type lookup statements that exist on the server: their role and
    /// the order in which the server confirmed them.
    typeinfo: HashMap<String, (usize, u64)>,
    /// Counts confirmed type lookup statements.
    confirmed: u64,
    /// Other named statements that exist on the server and were not closed.
    others: HashSet<String>,
}

impl StatementLog {
    fn remove(&mut self, name: &str) {
        self.typeinfo.remove(name);
        self.others.remove(name);
        self.current.retain(|(n, _)| n != name);
        for group in &mut self.groups {
            group.retain(|(n, _)| n != name);
        }
    }

    fn confirm(&mut self) {
        let parsed = match self.groups.iter_mut().find(|g| !g.is_empty()) {
            Some(group) => group.pop_front(),
            None => self.current.pop_front(),
        };
        if let Some((name, role)) = parsed {
            if let Some(role) = role {
                self.confirmed += 1;
                self.typeinfo.insert(name, (role, self.confirmed));
            } else {
                self.others.insert(name);
            }
        }
    }
}

/// Notices of one execution, capped by count and size.
#[derive(Debug, Default)]
pub(crate) struct NoticeLog {
    /// Server notices that fit the caps, in arrival order.
    server: Vec<Notice>,
    /// Bytes of text in `server`.
    bytes: usize,
    /// Server notices that did not fit.
    dropped: usize,
    /// Notices and warnings from the driver itself, in order; never capped.
    driver: Vec<Notice>,
    /// The messages in `driver`, so none is repeated.
    driver_seen: HashSet<String>,
}

/// The connections of a session, by number.
#[derive(Debug, Default)]
struct ConnectionLog {
    /// The connection in use (0 before the first one).
    current: u64,
    /// Connections at or after `current` whose task has ended.
    ended: HashSet<u64>,
}

/// Decrements the in-flight cancel count when a cancel request ends, even
/// when it is dropped half way (a timeout).
#[derive(Debug)]
pub(crate) struct CancelInFlight<'a>(&'a Shared);

impl Drop for CancelInFlight<'_> {
    fn drop(&mut self) {
        self.0.cancels_in_flight.fetch_sub(1, Ordering::AcqRel);
        self.0.cancel_done.notify_waiters();
    }
}

/// State shared between a session, its connection task and its taps.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    /// Completion tags in the order the server sent them. `None` stands for
    /// an `EmptyQueryResponse`.
    tags: Mutex<VecDeque<Option<String>>>,
    /// Notices received since the last drain.
    notices: Mutex<NoticeLog>,
    /// The latest value of every parameter the server reported with
    /// `ParameterStatus` (at startup and whenever a reported setting such as
    /// `TimeZone` or `IntervalStyle` changes).
    parameters: Mutex<HashMap<String, String>>,
    /// Transaction status byte of the last `ReadyForQuery` (`I`, `T` or
    /// `E`), or 0 before the first one.
    transaction_status: AtomicU8,
    /// Cancel requests being sent right now.
    cancels_in_flight: AtomicUsize,
    /// Signalled whenever a cancel request ends.
    cancel_done: Notify,
    /// Counts `ErrorResponse` and `ReadyForQuery` messages, so a caller can
    /// tell whether the `ReadyForQuery` after the last error was read.
    message_seq: AtomicU64,
    /// `message_seq` of the last `ErrorResponse`.
    last_error_seq: AtomicU64,
    /// `message_seq` of the last `ReadyForQuery`.
    last_ready_seq: AtomicU64,
    /// Signalled on every `ReadyForQuery`, and when a connection ends.
    ready: Notify,
    /// Hands out connection numbers: each connection opened (the first,
    /// and every reconnect) gets the next one.
    next_connection: AtomicU64,
    /// The connection in use and which connections have ended.
    connections: Mutex<ConnectionLog>,
    /// The connection in use has ended (kept in step with `connections`,
    /// for readers that must not lock).
    current_ended: AtomicBool,
    /// Named prepared statements on the server.
    statements: Mutex<StatementLog>,
    /// Counts `ParameterStatus` messages (reported setting changes).
    parameter_changes: AtomicU64,
    /// The single byte the server answered the `SSLRequest` with, or 0.
    ssl_response: AtomicU8,
    /// The server asked for the password in clear text
    /// (`AuthenticationCleartextPassword`: pg_hba methods `password`,
    /// `ldap`, `radius`, `pam`, `bsd`) on a connection the tap parses,
    /// which is one without TLS: the password went over the network as is.
    cleartext_password: AtomicBool,
    /// Counts cancel requests ever started on this session.
    cancels_started: AtomicU64,
    /// `cancels_started` when the current call began: a cancel started
    /// after that is for this call.
    call_base: AtomicU64,
    /// Where `NotificationResponse` messages go (`LISTEN` connections only;
    /// a session drops them).
    notifications: Mutex<NotificationSink>,
    /// A listener connection ([`crate::listen`]): its taps make
    /// notifications decodable and wait for the server on shutdown.
    listener: AtomicBool,
    /// For each `NotificationResponse` the tap passed on, in order: whether
    /// its channel or payload was not valid UTF-8 (and was passed on
    /// decoded lossily). tokio-postgres decodes them in the same order.
    notification_flags: Mutex<VecDeque<bool>>,
}

/// Receives the notifications of a listening connection
/// ([`crate::listen`]) and whether they had to be decoded lossily. Called
/// from the connection task: it must not block.
pub(crate) type NotificationFn = Box<dyn Fn(tokio_postgres::Notification, bool) + Send + Sync>;

/// Longest `NotificationResponse` the tap rewrites; the server sends at
/// most a 63 byte channel and a 7999 byte payload.
const MAX_NOTIFICATION_FRAME: usize = 64 * 1024;

/// The notification receiver of a connection, if any.
#[derive(Default)]
pub(crate) struct NotificationSink(Option<NotificationFn>);

impl std::fmt::Debug for NotificationSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("NotificationSink")
            .field(&self.0.is_some())
            .finish()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn push_tag(&self, tag: Option<String>) {
        lock(&self.tags).push_back(tag);
    }

    /// Forgets every queued tag.
    pub(crate) fn clear_tags(&self) {
        lock(&self.tags).clear();
    }

    /// Takes the oldest queued tag. The outer `None` means nothing is queued.
    pub(crate) fn pop_tag(&self) -> Option<Option<String>> {
        lock(&self.tags).pop_front()
    }

    /// Records a server notice, unless the caps for this execution
    /// ([`MAX_NOTICES`], [`MAX_NOTICE_BYTES`]) are reached; then it is only
    /// counted.
    pub(crate) fn push_notice(&self, notice: Notice) {
        let mut log = lock(&self.notices);
        let size = notice.severity.len() + notice.message.len();
        if log.server.len() < MAX_NOTICES && log.bytes + size <= MAX_NOTICE_BYTES {
            log.bytes += size;
            log.server.push(notice);
        } else {
            log.dropped += 1;
        }
    }

    /// Records a warning from the driver itself. The same warning is kept
    /// once per execution.
    pub(crate) fn push_driver_warning(&self, message: String) {
        self.push_driver_notice("WARNING", message);
    }

    /// Records a notice from the driver itself with `severity`. The same
    /// message is kept once per execution.
    pub(crate) fn push_driver_notice(&self, severity: &str, message: String) {
        let mut log = lock(&self.notices);
        if log.driver_seen.insert(message.clone()) {
            log.driver.push(Notice {
                severity: severity.to_owned(),
                message,
            });
        }
    }

    /// Takes the driver's own notices only, leaving the server notices.
    pub(crate) fn take_driver_notices(&self) -> Vec<Notice> {
        let mut log = lock(&self.notices);
        log.driver_seen.clear();
        std::mem::take(&mut log.driver)
    }

    /// Replaces the notice log, returning the old one (to keep the notices
    /// of an execution while a reconnect produces its own).
    pub(crate) fn swap_notices(&self, log: NoticeLog) -> NoticeLog {
        std::mem::replace(&mut *lock(&self.notices), log)
    }

    /// The last value the server reported for parameter `name`.
    pub(crate) fn parameter(&self, name: &str) -> Option<String> {
        lock(&self.parameters).get(name).cloned()
    }

    fn set_parameter(&self, name: String, value: String) {
        lock(&self.parameters).insert(name, value);
        self.parameter_changes.fetch_add(1, Ordering::AcqRel);
    }

    /// How many `ParameterStatus` messages arrived so far.
    pub(crate) fn parameter_changes(&self) -> u64 {
        self.parameter_changes.load(Ordering::Acquire)
    }

    /// Names of tokio-postgres's type lookup statements on the server.
    pub(crate) fn typeinfo_statements(&self) -> Vec<String> {
        let mut names: Vec<String> = lock(&self.statements).typeinfo.keys().cloned().collect();
        names.sort();
        names
    }

    /// The type lookup statements tokio-postgres uses: the newest of each
    /// role (see [`Self::superseded_typeinfo_statements`]). Only these must
    /// exist on the server; an older one that is gone (deallocated by
    /// Varve, whatever happened to the reply) changes nothing.
    pub(crate) fn current_typeinfo_statements(&self) -> Vec<String> {
        let superseded = self.superseded_typeinfo_statements();
        self.typeinfo_statements()
            .into_iter()
            .filter(|name| !superseded.contains(name))
            .collect()
    }

    /// Type lookup statements tokio-postgres no longer uses: it prepares
    /// one per role and keeps it, so for each role only the newest can be
    /// in use. Older ones are left behind by a call given up half way (its
    /// lookup prepared on the server, but dropped before tokio-postgres
    /// kept it). Only meaningful while no prepare is running.
    pub(crate) fn superseded_typeinfo_statements(&self) -> Vec<String> {
        let log = lock(&self.statements);
        let mut newest: HashMap<usize, u64> = HashMap::new();
        for (role, seq) in log.typeinfo.values() {
            let entry = newest.entry(*role).or_insert(*seq);
            *entry = (*entry).max(*seq);
        }
        let mut names: Vec<String> = log
            .typeinfo
            .iter()
            .filter(|(_, (role, seq))| newest.get(role) != Some(seq))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Named statements on the server that nothing owns any more: a
    /// prepare that failed after its `Parse` (in a type lookup) leaves its
    /// statement behind. Only meaningful while no prepare is running.
    pub(crate) fn leaked_statements(&self) -> Vec<String> {
        let mut names: Vec<String> = lock(&self.statements).others.iter().cloned().collect();
        names.sort();
        names
    }

    /// Forgets `name` (it was deallocated with SQL, or never existed).
    pub(crate) fn forget_statement(&self, name: &str) {
        lock(&self.statements).remove(name);
    }

    /// Forgets every statement (a new connection replaces the old one).
    pub(crate) fn reset_statements(&self) {
        *lock(&self.statements) = StatementLog::default();
    }

    fn statement_parsed(&self, name: String, role: Option<usize>) {
        lock(&self.statements).current.push_back((name, role));
    }

    fn statement_closed(&self, name: &str) {
        lock(&self.statements).remove(name);
    }

    fn sync_sent(&self) {
        let mut log = lock(&self.statements);
        let group = std::mem::take(&mut log.current);
        log.groups.push_back(group);
    }

    /// Drops every recorded notice and resets the caps.
    pub(crate) fn clear_notices(&self) {
        *lock(&self.notices) = NoticeLog::default();
    }

    /// Takes every recorded notice: the server's, then one saying how many
    /// were dropped (if any), then the driver's warnings.
    pub(crate) fn take_notices(&self) -> Vec<Notice> {
        let mut out = self.take_server_notices();
        out.extend(std::mem::take(&mut *lock(&self.notices)).driver);
        out
    }

    /// Takes the server's notices (and the one saying how many were
    /// dropped), leaving the driver's own warnings.
    pub(crate) fn take_server_notices(&self) -> Vec<Notice> {
        let mut log = lock(&self.notices);
        let mut out = std::mem::take(&mut log.server);
        let dropped = std::mem::take(&mut log.dropped);
        log.bytes = 0;
        if dropped > 0 {
            out.push(Notice {
                severity: "WARNING".into(),
                message: format!(
                    "{dropped} more notices were dropped: Varve keeps at most {MAX_NOTICES} \
                     notices or {} KiB of notice text per run",
                    MAX_NOTICE_BYTES / 1024
                ),
            });
        }
        out
    }

    /// The transaction status of the last `ReadyForQuery`: `b'I'` (idle),
    /// `b'T'` (in a transaction) or `b'E'` (in a failed transaction).
    pub(crate) fn transaction_status(&self) -> Option<u8> {
        match self.transaction_status.load(Ordering::Acquire) {
            0 => None,
            byte => Some(byte),
        }
    }

    /// Forgets the transaction status (a new connection replaces the old).
    pub(crate) fn reset_transaction_status(&self) {
        self.transaction_status.store(0, Ordering::Release);
    }

    /// Waits until the `ReadyForQuery` that follows the last error has been
    /// read, so [`Self::transaction_status`] is current. tokio-postgres hands
    /// a statement's error over as soon as it arrives, before the server's
    /// `ReadyForQuery`. Returns `false` when it did not come within `limit`
    /// (a connection that is going away).
    ///
    /// A connection that has ended sends nothing more: this returns `false`
    /// at once (the status is not known), also while waiting.
    pub(crate) async fn wait_until_ready(&self, limit: std::time::Duration) -> bool {
        let wait = async {
            loop {
                let notified = self.ready.notified();
                let mut notified = std::pin::pin!(notified);
                notified.as_mut().enable();
                if self.connection_ended() {
                    return false;
                }
                // Sequence numbers are never shared, so equal means no
                // message yet (both 0).
                if self.last_ready_seq.load(Ordering::Acquire)
                    >= self.last_error_seq.load(Ordering::Acquire)
                {
                    return true;
                }
                notified.await;
            }
        };
        tokio::time::timeout(limit, wait).await.unwrap_or(false)
    }

    /// Starts a new connection (the first one, or a reconnect): returns its
    /// number, for [`Self::end_connection`]. From now on only its end counts.
    pub(crate) fn begin_connection(&self) -> u64 {
        self.next_connection.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Makes connection `number` (from [`Self::begin_connection`]) the one
    /// in use, once it has replaced the previous one. A connection that was
    /// begun but never put in use (a connect or reconnect given up half
    /// way) never counts, so the end of the one in use is always seen.
    pub(crate) fn use_connection(&self, number: u64) {
        let mut log = lock(&self.connections);
        log.current = number;
        log.ended.retain(|ended| *ended >= number);
        self.current_ended
            .store(log.ended.contains(&number), Ordering::Release);
        drop(log);
        self.ready.notify_waiters();
    }

    /// Records that the task of connection `number` has ended (the server
    /// closed it, the socket broke, or the client went away), and wakes
    /// whoever waits for a `ReadyForQuery` that will never come.
    pub(crate) fn end_connection(&self, number: u64) {
        let mut log = lock(&self.connections);
        // An older connection ending is of no interest any more; a newer one
        // may still be put in use.
        if number >= log.current {
            log.ended.insert(number);
        }
        if number == log.current {
            self.current_ended.store(true, Ordering::Release);
        }
        drop(log);
        self.ready.notify_waiters();
    }

    /// True when the connection in use has ended: nothing more can be sent
    /// or read on it.
    pub(crate) fn connection_ended(&self) -> bool {
        self.current_ended.load(Ordering::Acquire)
    }

    /// Waits until the connection in use has ended.
    pub(crate) async fn wait_connection_end(&self) {
        loop {
            let notified = self.ready.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            if self.connection_ended() {
                return;
            }
            notified.await;
        }
    }

    /// Sends the connection's notifications to `sink` from now on.
    /// Also makes this a listener connection: set it before connecting.
    pub(crate) fn set_notification_sink(&self, sink: NotificationFn) {
        lock(&self.notifications).0 = Some(sink);
        self.listener.store(true, Ordering::Release);
    }

    /// True for a listener connection.
    pub(crate) fn is_listener(&self) -> bool {
        self.listener.load(Ordering::Acquire)
    }

    fn push_notification_flag(&self, lossy: bool) {
        lock(&self.notification_flags).push_back(lossy);
    }

    /// Hands a notification to the sink; without one it is dropped.
    pub(crate) fn push_notification(&self, notification: tokio_postgres::Notification) {
        let lossy = lock(&self.notification_flags).pop_front().unwrap_or(false);
        if let Some(sink) = &lock(&self.notifications).0 {
            sink(notification, lossy);
        }
    }

    /// Marks a cancel request as being sent until the guard is dropped.
    /// From then on [`Self::cancel_requested`] is true for the call running
    /// now. A call that begins while it is in flight first waits until it
    /// has been sent, and only cancels started after that count for it.
    pub(crate) fn cancel_started(&self) -> CancelInFlight<'_> {
        self.cancels_in_flight.fetch_add(1, Ordering::AcqRel);
        self.cancels_started.fetch_add(1, Ordering::AcqRel);
        CancelInFlight(self)
    }

    /// Waits until no cancel request is being sent, so a late request cannot
    /// interrupt the next execution instead of the one it was meant for.
    pub(crate) async fn wait_for_cancels(&self) {
        loop {
            let notified = self.cancel_done.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            if self.cancels_in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// True when a connection of this session sent the password in clear
    /// text over a connection without TLS (see `cleartext_password`).
    pub(crate) fn password_in_clear(&self) -> bool {
        self.cleartext_password.load(Ordering::Acquire)
    }

    /// The server's answer to the `SSLRequest`, when one was read.
    pub(crate) fn ssl_response(&self) -> Option<u8> {
        match self.ssl_response.load(Ordering::Acquire) {
            0 => None,
            byte => Some(byte),
        }
    }

    /// Begins a call: cancel requests started before this belong to
    /// earlier calls. Taken before waiting for those requests to be sent,
    /// so a cancel started at any moment after it (also while waiting) is
    /// this call's, and none is lost between two calls.
    pub(crate) fn begin_call(&self) {
        self.call_base.store(
            self.cancels_started.load(Ordering::Acquire),
            Ordering::Release,
        );
    }

    /// True when Varve's cancel handle was asked to cancel the current call
    /// (since [`Self::begin_call`]).
    pub(crate) fn cancel_requested(&self) -> bool {
        self.cancels_started.load(Ordering::Acquire) > self.call_base.load(Ordering::Acquire)
    }
}

/// `AuthenticationCleartextPassword`: the `R` message's request type 3.
const AUTH_CLEARTEXT_PASSWORD: i32 = 3;

/// Incremental parser of the backend message framing: one type byte, a
/// big-endian `i32` length that counts itself, then the body.
#[derive(Debug, Default)]
struct FrameScanner {
    header: [u8; 5],
    header_len: usize,
    remaining: usize,
    body: Vec<u8>,
}

impl FrameScanner {
    fn feed(&mut self, mut data: &[u8], shared: &Shared) {
        while !data.is_empty() {
            if self.header_len < self.header.len() {
                let take = (self.header.len() - self.header_len).min(data.len());
                self.header[self.header_len..self.header_len + take].copy_from_slice(&data[..take]);
                self.header_len += take;
                data = &data[take..];
                if self.header_len == self.header.len() {
                    let length = u32::from_be_bytes([
                        self.header[1],
                        self.header[2],
                        self.header[3],
                        self.header[4],
                    ]);
                    self.remaining = usize::try_from(length.saturating_sub(4)).unwrap_or(0);
                    self.body.clear();
                    if self.remaining == 0 {
                        self.finish(shared);
                    }
                }
                continue;
            }
            let take = self.remaining.min(data.len());
            if matches!(self.header[0], b'C' | b'Z' | b'S' | b'R') && self.body.len() < MAX_TAG_LEN
            {
                let keep = take.min(MAX_TAG_LEN - self.body.len());
                self.body.extend_from_slice(&data[..keep]);
            }
            self.remaining -= take;
            data = &data[take..];
            if self.remaining == 0 {
                self.finish(shared);
            }
        }
    }

    fn finish(&mut self, shared: &Shared) {
        match self.header[0] {
            b'C' => {
                let end = self
                    .body
                    .iter()
                    .position(|b| *b == 0)
                    .unwrap_or(self.body.len());
                shared.push_tag(Some(
                    String::from_utf8_lossy(&self.body[..end]).into_owned(),
                ));
            }
            b'I' => shared.push_tag(None),
            b'1' => lock(&shared.statements).confirm(),
            b'Z' => {
                lock(&shared.statements).groups.pop_front();
                if let Some(&status) = self.body.first() {
                    shared.transaction_status.store(status, Ordering::Release);
                }
                let seq = shared.message_seq.fetch_add(1, Ordering::AcqRel) + 1;
                shared.last_ready_seq.store(seq, Ordering::Release);
                shared.ready.notify_waiters();
            }
            b'E' => {
                let seq = shared.message_seq.fetch_add(1, Ordering::AcqRel) + 1;
                shared.last_error_seq.store(seq, Ordering::Release);
            }
            // An authentication request: type 3 asks for the password in
            // clear text. The tap only parses connections without TLS.
            b'R' => {
                if self.body.get(..4) == Some(&AUTH_CLEARTEXT_PASSWORD.to_be_bytes()) {
                    shared.cleartext_password.store(true, Ordering::Release);
                }
            }
            // ParameterStatus: name NUL value NUL. A body cut short by
            // MAX_TAG_LEN has no second NUL and is ignored.
            b'S' => {
                let mut parts = self.body.split(|b| *b == 0);
                if let (Some(name), Some(value), Some(_)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    shared.set_parameter(
                        String::from_utf8_lossy(name).into_owned(),
                        String::from_utf8_lossy(value).into_owned(),
                    );
                }
            }
            _ => {}
        }
        self.header_len = 0;
        self.body.clear();
    }
}

/// Where a `Parse` body is: name, query, or the parameter types after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ParsePart {
    #[default]
    Name,
    Query,
    Rest,
}

/// A `Parse` body read as it streams by. The query (user SQL, which can hold
/// a password) is never stored: it is only compared byte by byte with the
/// known type lookup queries.
#[derive(Debug, Default)]
struct ParseBody {
    part: ParsePart,
    name: Vec<u8>,
    /// Which of [`TYPEINFO_QUERIES`] the query still matches.
    matching: [bool; TYPEINFO_QUERIES.len()],
    /// Bytes of the query seen so far.
    query_len: usize,
}

impl ParseBody {
    fn new() -> Self {
        Self {
            matching: [true; TYPEINFO_QUERIES.len()],
            ..Self::default()
        }
    }

    fn feed(&mut self, data: &[u8]) {
        for &byte in data {
            match self.part {
                ParsePart::Name if byte == 0 => self.part = ParsePart::Query,
                ParsePart::Name => {
                    if self.name.len() < MAX_STATEMENT_NAME {
                        self.name.push(byte);
                    }
                }
                ParsePart::Query if byte == 0 => {
                    for (i, query) in TYPEINFO_QUERIES.iter().enumerate() {
                        self.matching[i] &= query.len() == self.query_len;
                    }
                    self.part = ParsePart::Rest;
                }
                ParsePart::Query => {
                    for (i, query) in TYPEINFO_QUERIES.iter().enumerate() {
                        self.matching[i] &= query.as_bytes().get(self.query_len) == Some(&byte);
                    }
                    self.query_len += 1;
                }
                ParsePart::Rest => {}
            }
        }
    }

    /// The statement name and, for a type lookup, its role
    /// ([`LOOKUP_ROLE`]), once complete.
    fn finish(&self) -> Option<(String, Option<usize>)> {
        if self.part != ParsePart::Rest || self.name.is_empty() {
            return None;
        }
        Some((
            String::from_utf8_lossy(&self.name).into_owned(),
            self.matching
                .iter()
                .position(|m| *m)
                .and_then(|i| LOOKUP_ROLE.get(i).copied()),
        ))
    }
}

/// Incremental parser of the frontend messages the client writes. The
/// first `untyped` messages (`SSLRequest`, `StartupMessage`) have no type
/// byte. Only `Parse`, `Close`, `Sync`, `Query` and `FunctionCall` are
/// looked at; other bodies (passwords among them) are skipped unread, and
/// of a `Parse` only the statement name is kept.
///
/// Unnamed statements (an empty name) are not tracked: the server replaces
/// the unnamed statement with the next one, so it cannot accumulate, and
/// tokio-postgres's `prepare` always names its statements.
#[derive(Debug)]
struct FrontScanner {
    untyped: u8,
    header: [u8; 5],
    header_len: usize,
    remaining: usize,
    /// Body of a `Close`: kind byte and statement name (at most
    /// [`MAX_STATEMENT_NAME`] bytes).
    body: Vec<u8>,
    parse: ParseBody,
}

impl FrontScanner {
    fn new(untyped: u8) -> Self {
        Self {
            untyped,
            header: [0; 5],
            header_len: 0,
            remaining: 0,
            body: Vec::new(),
            parse: ParseBody::new(),
        }
    }

    fn header_size(&self) -> usize {
        if self.untyped > 0 { 4 } else { 5 }
    }

    fn feed(&mut self, mut data: &[u8], shared: &Shared) {
        while !data.is_empty() {
            let size = self.header_size();
            if self.header_len < size {
                let take = (size - self.header_len).min(data.len());
                self.header[self.header_len..self.header_len + take].copy_from_slice(&data[..take]);
                self.header_len += take;
                data = &data[take..];
                if self.header_len == size {
                    let l = &self.header[size - 4..size];
                    let length = u32::from_be_bytes([l[0], l[1], l[2], l[3]]);
                    self.remaining = usize::try_from(length.saturating_sub(4)).unwrap_or(0);
                    self.body.clear();
                    self.parse = ParseBody::new();
                    if self.remaining == 0 {
                        self.finish(shared);
                    }
                }
                continue;
            }
            let take = self.remaining.min(data.len());
            if self.untyped == 0 {
                match self.header[0] {
                    b'P' => self.parse.feed(&data[..take]),
                    b'C' if self.body.len() <= MAX_STATEMENT_NAME => {
                        let keep = take.min(MAX_STATEMENT_NAME + 1 - self.body.len());
                        self.body.extend_from_slice(&data[..keep]);
                    }
                    _ => {}
                }
            }
            self.remaining -= take;
            data = &data[take..];
            if self.remaining == 0 {
                self.finish(shared);
            }
        }
    }

    fn finish(&mut self, shared: &Shared) {
        if self.untyped > 0 {
            self.untyped -= 1;
        } else {
            match self.header[0] {
                // Parse: name NUL query NUL parameter types.
                b'P' => {
                    if let Some((name, role)) = self.parse.finish() {
                        shared.statement_parsed(name, role);
                    }
                }
                // Close: 'S' name NUL (a statement) or 'P' (a portal).
                b'C' => {
                    if let Some((b'S', rest)) = self.body.split_first() {
                        let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
                        shared.statement_closed(&String::from_utf8_lossy(&rest[..end]));
                    }
                }
                b'S' | b'Q' | b'F' => shared.sync_sent(),
                _ => {}
            }
        }
        self.header_len = 0;
        self.body.clear();
        self.parse = ParseBody::new();
    }
}

/// Makes every `NotificationResponse` of a listener connection decodable.
///
/// On a `SQL_ASCII` database any client can send a channel or payload that
/// is not valid UTF-8, and tokio-postgres ends the connection when it
/// cannot decode one. The rewriter passes every other message on as it
/// arrives and replaces such a notification by one with the same fields
/// decoded lossily (invalid bytes become U+FFFD), recording that it did
/// ([`Shared::push_notification`] reports it).
#[derive(Debug, Default)]
struct NotificationRewriter {
    /// Bytes read and not yet passed on.
    raw: Vec<u8>,
    /// Bytes of the current message that pass on unchanged.
    passthrough: usize,
    /// Bytes ready to be handed to the reader, from `out_pos`.
    out: Vec<u8>,
    out_pos: usize,
}

impl NotificationRewriter {
    /// Moves what can be passed on from `raw` to `out`.
    fn process(&mut self, shared: &Shared) {
        loop {
            if self.passthrough > 0 {
                let take = self.passthrough.min(self.raw.len());
                self.out.extend(self.raw.drain(..take));
                self.passthrough -= take;
                if self.passthrough > 0 {
                    return;
                }
                continue;
            }
            if self.raw.len() < 5 {
                return;
            }
            let length = u32::from_be_bytes([self.raw[1], self.raw[2], self.raw[3], self.raw[4]]);
            let total = usize::try_from(length)
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            if self.raw[0] != b'A' || !(5..=MAX_NOTIFICATION_FRAME).contains(&total) {
                self.passthrough = total.max(5);
                continue;
            }
            if self.raw.len() < total {
                return;
            }
            let frame: Vec<u8> = self.raw.drain(..total).collect();
            let (frame, lossy) = decodable_notification(frame);
            shared.push_notification_flag(lossy);
            self.out.extend(frame);
        }
    }

    /// Passes on what is left (the connection ended).
    fn flush(&mut self) {
        self.out.append(&mut self.raw);
        self.passthrough = 0;
    }

    fn pending(&self) -> &[u8] {
        &self.out[self.out_pos..]
    }

    fn consume(&mut self, n: usize) {
        self.out_pos += n;
        if self.out_pos >= self.out.len() {
            self.out.clear();
            self.out_pos = 0;
        }
    }
}

/// A `NotificationResponse` frame (`A`, length, pid, channel NUL, payload
/// NUL) whose channel and payload are valid UTF-8: the frame itself, or one
/// with both decoded lossily (then `true`). A malformed frame is returned
/// unchanged.
fn decodable_notification(frame: Vec<u8>) -> (Vec<u8>, bool) {
    let Some(body) = frame.get(9..) else {
        return (frame, false);
    };
    let Some(channel_end) = body.iter().position(|b| *b == 0) else {
        return (frame, false);
    };
    let (channel, rest) = (&body[..channel_end], &body[channel_end + 1..]);
    let Some(payload_end) = rest.iter().position(|b| *b == 0) else {
        return (frame, false);
    };
    let payload = &rest[..payload_end];
    if std::str::from_utf8(channel).is_ok() && std::str::from_utf8(payload).is_ok() {
        return (frame, false);
    }
    let channel = String::from_utf8_lossy(channel);
    let payload = String::from_utf8_lossy(payload);
    let length = 4 + 4 + channel.len() + 1 + payload.len() + 1;
    let mut out = Vec::with_capacity(length + 1);
    out.push(b'A');
    out.extend_from_slice(&u32::try_from(length).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(&frame[5..9]);
    out.extend_from_slice(channel.as_bytes());
    out.push(0);
    out.extend_from_slice(payload.as_bytes());
    out.push(0);
    (out, true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TapMode {
    /// The next byte is the one-byte answer to an `SSLRequest`.
    AwaitSslResponse,
    /// Bytes are protocol messages.
    Parsing,
    /// Bytes are not looked at (TLS records, or a cancel connection).
    Passthrough,
}

/// A stream that forwards everything to `inner` and parses what it reads.
#[derive(Debug)]
pub(crate) struct TapStream<S> {
    inner: S,
    mode: TapMode,
    scanner: FrameScanner,
    /// Parses what is written, when the written bytes are protocol messages.
    writes: Option<FrontScanner>,
    shared: Arc<Shared>,
    /// On shutdown, also wait until the server closes its side.
    drain_on_shutdown: bool,
    /// The write side is already shut down.
    write_shut: bool,
    /// Rewrites notifications of a listener connection while parsing.
    rewriter: NotificationRewriter,
}

impl<S> TapStream<S> {
    /// A tap for a raw socket on which an `SSLRequest` is sent first.
    pub(crate) fn expecting_ssl_response(inner: S, shared: Arc<Shared>) -> Self {
        Self {
            // SSLRequest, then (when the server refuses TLS) the startup.
            writes: Some(FrontScanner::new(2)),
            ..Self::with_mode(inner, TapMode::AwaitSslResponse, shared)
        }
    }

    /// A tap whose first byte is already a protocol message.
    pub(crate) fn parsing(inner: S, shared: Arc<Shared>) -> Self {
        Self {
            writes: Some(FrontScanner::new(1)),
            ..Self::with_mode(inner, TapMode::Parsing, shared)
        }
    }

    /// A tap that only forwards, for a cancel request: its shutdown also
    /// waits until the server closes the connection. PostgreSQL closes it
    /// only after it has signalled the backend, so once the shutdown is
    /// done the signal is pending on the backend and reaches it before any
    /// later statement (the server discards a cancel signal that arrives
    /// while the backend waits for a command).
    pub(crate) fn cancel_request(inner: S, shared: Arc<Shared>) -> Self {
        Self {
            drain_on_shutdown: true,
            ..Self::with_mode(inner, TapMode::Passthrough, shared)
        }
    }

    fn with_mode(inner: S, mode: TapMode, shared: Arc<Shared>) -> Self {
        Self {
            inner,
            mode,
            scanner: FrameScanner::default(),
            writes: None,
            shared,
            drain_on_shutdown: false,
            write_shut: false,
            rewriter: NotificationRewriter::default(),
        }
    }

    /// Stops parsing: the bytes from here on are TLS records.
    pub(crate) fn stop_parsing(&mut self) {
        self.mode = TapMode::Passthrough;
        self.writes = None;
    }

    fn observe_write(&mut self, data: &[u8]) {
        if let Some(scanner) = &mut self.writes {
            scanner.feed(data, &self.shared);
        }
    }

    fn observe(&mut self, mut data: &[u8]) {
        if self.mode == TapMode::AwaitSslResponse {
            let Some((&first, rest)) = data.split_first() else {
                return;
            };
            self.shared.ssl_response.store(first, Ordering::Release);
            self.mode = if first == b'S' {
                self.writes = None;
                TapMode::Passthrough
            } else {
                TapMode::Parsing
            };
            data = rest;
        }
        if self.mode == TapMode::Parsing {
            self.scanner.feed(data, &self.shared);
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for TapStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.mode == TapMode::Parsing && this.shared.is_listener() {
            return this.poll_read_rewriting(cx, buf);
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                this.observe(&buf.filled()[before..]);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncRead + Unpin> TapStream<S> {
    /// Reads through the [`NotificationRewriter`] (listener connections).
    fn poll_read_rewriting(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let pending = self.rewriter.pending();
            if !pending.is_empty() {
                let n = pending.len().min(buf.remaining());
                buf.put_slice(&pending[..n]);
                self.scanner.feed(&pending[..n], &self.shared);
                self.rewriter.consume(n);
                return Poll::Ready(Ok(()));
            }
            let mut chunk = [0u8; 8192];
            let mut read = ReadBuf::new(&mut chunk);
            match Pin::new(&mut self.inner).poll_read(cx, &mut read) {
                Poll::Ready(Ok(())) if read.filled().is_empty() => {
                    if self.rewriter.raw.is_empty() {
                        return Poll::Ready(Ok(()));
                    }
                    self.rewriter.flush();
                }
                Poll::Ready(Ok(())) => {
                    self.rewriter.raw.extend_from_slice(read.filled());
                    self.rewriter.process(&self.shared);
                }
                other => return other,
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for TapStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &result {
            this.observe_write(&buf[..*written]);
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.write_shut {
            match Pin::new(&mut this.inner).poll_shutdown(cx) {
                Poll::Ready(Ok(())) => this.write_shut = true,
                other => return other,
            }
        }
        // A listener's close is done once the server has ended the backend
        // (it closes the connection then).
        let drain =
            this.drain_on_shutdown || (this.mode == TapMode::Parsing && this.shared.is_listener());
        if drain {
            let mut scratch = [0u8; 256];
            loop {
                let mut buf = ReadBuf::new(&mut scratch);
                match Pin::new(&mut this.inner).poll_read(cx, &mut buf) {
                    Poll::Ready(Ok(())) if buf.filled().is_empty() => break,
                    Poll::Ready(Ok(())) => {}
                    // A reset also means the server is done with it.
                    Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::ConnectionReset => break,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            }
        }
        Poll::Ready(Ok(()))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(written)) = &result {
            let mut left = *written;
            for slice in bufs {
                if left == 0 {
                    break;
                }
                let take = left.min(slice.len());
                this.observe_write(&slice[..take]);
                left -= take;
            }
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

impl<S: TlsStream + Unpin> TlsStream for TapStream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        self.inner.channel_binding()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cleartext_password_request_is_noticed() {
        let shared = Shared::default();
        let mut scanner = FrameScanner::default();
        // AuthenticationSASL (10) and AuthenticationMD5Password (5) do not
        // send the password as is.
        scanner.feed(&message(b'R', &10i32.to_be_bytes()), &shared);
        let mut md5 = 5i32.to_be_bytes().to_vec();
        md5.extend_from_slice(b"salt");
        scanner.feed(&message(b'R', &md5), &shared);
        assert!(!shared.password_in_clear());
        // AuthenticationCleartextPassword (3), split across reads.
        let cleartext = message(b'R', &3i32.to_be_bytes());
        let (a, b) = cleartext.split_at(6);
        scanner.feed(a, &shared);
        assert!(!shared.password_in_clear());
        scanner.feed(b, &shared);
        assert!(shared.password_in_clear());
    }

    fn notification(pid: i32, channel: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut body = pid.to_be_bytes().to_vec();
        body.extend_from_slice(channel);
        body.push(0);
        body.extend_from_slice(payload);
        body.push(0);
        message(b'A', &body)
    }

    #[test]
    fn notifications_that_are_not_utf8_are_decoded_lossily() {
        let valid = notification(7, b"orders", "caf\u{e9}".as_bytes());
        assert_eq!(decodable_notification(valid.clone()), (valid, false));
        let (fixed, lossy) = decodable_notification(notification(7, b"c\xff", b"caf\xe9"));
        assert!(lossy);
        assert_eq!(
            fixed,
            notification(7, "c\u{fffd}".as_bytes(), "caf\u{fffd}".as_bytes())
        );
        // Malformed frames go on unchanged (tokio-postgres reports them).
        let short = message(b'A', &[0, 0, 0, 7, b'x']);
        assert_eq!(decodable_notification(short.clone()), (short, false));
    }

    #[test]
    fn the_rewriter_keeps_framing_across_any_split() {
        let shared = Shared::default();
        let mut input = message(b'Z', b"I");
        input.extend(notification(1, b"a", b"\xe9"));
        input.extend(message(b'C', b"SELECT 1\0"));
        input.extend(notification(2, b"b", b"ok"));
        let mut expected = message(b'Z', b"I");
        expected.extend(notification(1, b"a", "\u{fffd}".as_bytes()));
        expected.extend(message(b'C', b"SELECT 1\0"));
        expected.extend(notification(2, b"b", b"ok"));
        for chunk in [1, 3, 7, input.len()] {
            let mut rewriter = NotificationRewriter::default();
            let mut out = Vec::new();
            for piece in input.chunks(chunk) {
                rewriter.raw.extend_from_slice(piece);
                rewriter.process(&shared);
                out.extend_from_slice(rewriter.pending());
                let n = rewriter.pending().len();
                rewriter.consume(n);
            }
            assert_eq!(out, expected, "chunks of {chunk}");
        }
        // One flag per notification, in order, for every run above.
        let flags: Vec<bool> = lock(&shared.notification_flags).iter().copied().collect();
        assert_eq!(flags, [true, false].repeat(4));
    }

    fn message(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let length = u32::try_from(body.len() + 4).unwrap_or(u32::MAX);
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn drain(shared: &Shared) -> Vec<Option<String>> {
        std::iter::from_fn(|| shared.pop_tag()).collect()
    }

    #[test]
    fn records_command_tags_and_empty_queries() {
        let shared = Shared::default();
        let mut scanner = FrameScanner::default();
        let mut bytes = message(b'T', b"\0\0");
        bytes.extend(message(b'D', b"\0\x01\0\0\0\x011"));
        bytes.extend(message(b'C', b"SELECT 1\0"));
        bytes.extend(message(b'I', b""));
        bytes.extend(message(b'C', b"UPDATE 3\0"));
        bytes.extend(message(b'Z', b"I"));
        assert_eq!(shared.transaction_status(), None);
        scanner.feed(&bytes, &shared);
        assert_eq!(
            drain(&shared),
            vec![Some("SELECT 1".into()), None, Some("UPDATE 3".into())]
        );
        assert_eq!(shared.transaction_status(), Some(b'I'));
        scanner.feed(&message(b'Z', b"T"), &shared);
        assert_eq!(shared.transaction_status(), Some(b'T'));
        scanner.feed(&message(b'Z', b"E"), &shared);
        assert_eq!(shared.transaction_status(), Some(b'E'));
    }

    #[tokio::test]
    async fn waits_for_ready_after_an_error() {
        let shared = Arc::new(Shared::default());
        let limit = std::time::Duration::from_millis(50);
        // Nothing failed: ready at once.
        assert!(shared.wait_until_ready(limit).await);
        let mut scanner = FrameScanner::default();
        scanner.feed(&message(b'Z', b"T"), &shared);
        scanner.feed(&message(b'E', b"SERROR\0C22P02\0\0"), &shared);
        // The error is read, its ReadyForQuery not yet.
        assert!(!shared.wait_until_ready(limit).await);
        assert_eq!(shared.transaction_status(), Some(b'T'));
        let waiter = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                shared
                    .wait_until_ready(std::time::Duration::from_secs(2))
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        scanner.feed(&message(b'Z', b"E"), &shared);
        assert!(matches!(waiter.await, Ok(true)));
        assert_eq!(shared.transaction_status(), Some(b'E'));
    }

    #[test]
    fn cancels_belong_to_the_call_they_start_in() {
        let shared = Shared::default();
        assert!(!shared.cancel_requested());
        // A cancel of a call that has ended does not carry over.
        drop(shared.cancel_started());
        assert!(shared.cancel_requested());
        shared.begin_call();
        assert!(!shared.cancel_requested());
        // A cancel started as soon as the next call began is that call's.
        let in_flight = shared.cancel_started();
        assert!(shared.cancel_requested());
        drop(in_flight);
        assert!(shared.cancel_requested());
        shared.begin_call();
        assert!(!shared.cancel_requested());
    }

    #[tokio::test]
    async fn an_ended_connection_is_never_ready() -> Result<(), tokio::task::JoinError> {
        let shared = Arc::new(Shared::default());
        let first = shared.begin_connection();
        shared.use_connection(first);
        assert!(!shared.connection_ended());
        let mut scanner = FrameScanner::default();
        scanner.feed(&message(b'Z', b"I"), &shared);
        // A FATAL error, then the server closes the connection.
        scanner.feed(&message(b'E', b"SFATAL\0C57P01\0\0"), &shared);
        let waiter = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let ready = shared
                    .wait_until_ready(std::time::Duration::from_secs(5))
                    .await;
                (ready, started.elapsed())
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        shared.end_connection(first);
        let (ready, waited) = waiter.await?;
        assert!(!ready);
        assert!(waited < std::time::Duration::from_secs(1), "{waited:?}");
        assert!(shared.connection_ended());
        // Even with no error pending, an ended connection is not ready.
        assert!(
            !shared
                .wait_until_ready(std::time::Duration::from_secs(5))
                .await
        );
        // A reconnect: the old connection ending later does not count.
        let second = shared.begin_connection();
        shared.use_connection(second);
        assert!(!shared.connection_ended());
        shared.end_connection(first);
        assert!(!shared.connection_ended());
        shared.end_connection(second);
        assert!(shared.connection_ended());
        Ok(())
    }

    #[test]
    fn a_connection_never_put_in_use_does_not_hide_the_end() {
        let shared = Shared::default();
        let first = shared.begin_connection();
        shared.use_connection(first);
        // A reconnect given up half way: its number is taken, never used.
        let abandoned = shared.begin_connection();
        assert!(!shared.connection_ended());
        // The connection still in use ends: that is seen.
        shared.end_connection(first);
        assert!(shared.connection_ended());
        // The abandoned one ending changes nothing for the one in use.
        shared.end_connection(abandoned);
        assert!(shared.connection_ended());

        // A new connection that ended before it was put in use counts as
        // ended as soon as it is.
        let shared = Shared::default();
        let first = shared.begin_connection();
        shared.use_connection(first);
        let second = shared.begin_connection();
        shared.end_connection(second);
        assert!(!shared.connection_ended());
        shared.use_connection(second);
        assert!(shared.connection_ended());
    }

    fn front(tag: u8, body: &[u8]) -> Vec<u8> {
        message(tag, body)
    }

    fn parse(name: &str, query: &str) -> Vec<u8> {
        let mut body = name.as_bytes().to_vec();
        body.push(0);
        body.extend_from_slice(query.as_bytes());
        body.extend_from_slice(b"\0\0\0");
        front(b'P', &body)
    }

    #[test]
    fn finds_type_lookups_a_dropped_call_left_behind() {
        let shared = Shared::default();
        let mut writes = FrontScanner::new(0);
        let mut reads = FrameScanner::default();
        // Each lookup in its own Sync group, confirmed by ParseComplete.
        let mut prepare = |name: &str, query: &str| {
            let mut bytes = parse(name, query);
            bytes.extend(front(b'S', b""));
            writes.feed(&bytes, &shared);
            reads.feed(&message(b'1', b""), &shared);
            reads.feed(&message(b'Z', b"I"), &shared);
        };
        // A dropped call left s1 (a type lookup) behind; s3 replaced it.
        prepare("s1", TYPEINFO_QUERIES[0]);
        prepare("s2", TYPEINFO_QUERIES[2]);
        prepare("s3", TYPEINFO_QUERIES[0]);
        prepare("s4", TYPEINFO_QUERIES[4]);
        prepare("s5", "SELECT 1");
        assert_eq!(shared.typeinfo_statements(), vec!["s1", "s2", "s3", "s4"]);
        assert_eq!(shared.superseded_typeinfo_statements(), vec!["s1"]);
        // A fallback query takes the role of the query it replaces.
        prepare("s6", TYPEINFO_QUERIES[3]);
        assert_eq!(shared.superseded_typeinfo_statements(), vec!["s1", "s2"]);
        // Only the newest of each role must exist.
        assert_eq!(shared.current_typeinfo_statements(), vec!["s3", "s4", "s6"]);
        shared.forget_statement("s1");
        shared.forget_statement("s2");
        assert!(shared.superseded_typeinfo_statements().is_empty());
        assert_eq!(shared.typeinfo_statements(), vec!["s3", "s4", "s6"]);
    }

    #[test]
    fn follows_named_statements() {
        let shared = Shared::default();
        let mut writes = FrontScanner::new(1);
        let mut reads = FrameScanner::default();
        // Startup (untyped), then a type lookup and a user statement.
        let mut startup = 8u32.to_be_bytes().to_vec();
        startup.extend_from_slice(&196_608u32.to_be_bytes());
        writes.feed(&startup, &shared);
        let mut bytes = parse("s1", TYPEINFO_QUERIES[0]);
        bytes.extend(front(b'D', b"Ss1\0"));
        bytes.extend(front(b'S', b""));
        bytes.extend(parse("s2", "SELECT 'x'::mood"));
        bytes.extend(front(b'S', b""));
        bytes.extend(parse("s3", "SELEC broken"));
        bytes.extend(front(b'S', b""));
        // Fed one byte at a time.
        for byte in &bytes {
            writes.feed(std::slice::from_ref(byte), &shared);
        }
        let mut answers = message(b'1', b"");
        answers.extend(message(b'Z', b"I"));
        answers.extend(message(b'1', b""));
        answers.extend(message(b'Z', b"I"));
        answers.extend(message(b'E', b"SERROR\0C42601\0\0"));
        answers.extend(message(b'Z', b"I"));
        reads.feed(&answers, &shared);
        assert_eq!(shared.typeinfo_statements(), vec!["s1".to_owned()]);
        // s2 failed in its type lookup and was never closed; s3 never existed.
        assert_eq!(shared.leaked_statements(), vec!["s2".to_owned()]);
        writes.feed(&front(b'C', b"Ss2\0"), &shared);
        assert!(shared.leaked_statements().is_empty());
        // A password message is skipped without being kept, and of a Parse
        // with a password in its SQL only the name stays.
        writes.feed(&front(b'p', b"hunter2\0"), &shared);
        let secret = parse("s9", "ALTER ROLE r PASSWORD 'hunter2'");
        writes.feed(&secret[..secret.len() - 3], &shared);
        assert!(!format!("{writes:?}").contains("hunter2"), "{writes:?}");
        writes.feed(&secret[secret.len() - 3..], &shared);
        assert!(writes.body.is_empty() && writes.parse.name.is_empty());
        assert!(!format!("{writes:?}").contains("hunter2"));
        assert!(!format!("{shared:?}").contains("hunter2"));
        // A query that only starts like a type lookup, or is longer, is not one.
        let mut longer = TYPEINFO_QUERIES[2].to_owned();
        longer.push_str("LIMIT 1");
        writes.feed(&parse("s10", &longer), &shared);
        writes.feed(&parse("s11", &TYPEINFO_QUERIES[2][..20]), &shared);
        writes.feed(&front(b'S', b""), &shared);
        let mut answers = message(b'1', b"");
        answers.extend(message(b'1', b""));
        answers.extend(message(b'1', b""));
        answers.extend(message(b'Z', b"I"));
        reads.feed(&answers, &shared);
        assert_eq!(shared.typeinfo_statements(), vec!["s1".to_owned()]);
        assert_eq!(
            shared.leaked_statements(),
            vec!["s10".to_owned(), "s11".to_owned(), "s9".to_owned()]
        );
        shared.forget_statement("s1");
        assert!(shared.typeinfo_statements().is_empty());
    }

    #[test]
    fn records_parameter_status() {
        let shared = Shared::default();
        let mut scanner = FrameScanner::default();
        scanner.feed(&message(b'S', b"TimeZone\0UTC\0"), &shared);
        scanner.feed(&message(b'S', b"IntervalStyle\0postgres\0"), &shared);
        scanner.feed(&message(b'S', b"TimeZone\0Europe/Amsterdam\0"), &shared);
        assert_eq!(
            shared.parameter("TimeZone").as_deref(),
            Some("Europe/Amsterdam")
        );
        assert_eq!(
            shared.parameter("IntervalStyle").as_deref(),
            Some("postgres")
        );
        assert_eq!(shared.parameter("DateStyle"), None);
    }

    #[tokio::test]
    async fn cancel_shutdown_waits_for_the_server_to_close() -> io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut server) = tokio::io::duplex(64);
        let mut tap = TapStream::cancel_request(client, Arc::new(Shared::default()));
        tap.write_all(b"cancel").await?;
        let shutdown = tokio::spawn(async move { tap.shutdown().await });
        let mut got = Vec::new();
        server.read_to_end(&mut got).await?;
        assert_eq!(got, b"cancel");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        // The server has not closed its side yet.
        assert!(!shutdown.is_finished());
        server.write_all(b"ignored").await?;
        drop(server);
        shutdown.await.map_err(io::Error::other)??;
        Ok(())
    }

    fn notice(message: &str) -> Notice {
        Notice {
            severity: "NOTICE".into(),
            message: message.into(),
        }
    }

    #[test]
    fn notices_are_capped_by_count() {
        let shared = Shared::default();
        for i in 0..MAX_NOTICES + 25 {
            shared.push_notice(notice(&i.to_string()));
        }
        shared.push_driver_warning("restored".into());
        shared.push_driver_warning("restored".into());
        let notices = shared.take_notices();
        assert_eq!(notices.len(), MAX_NOTICES + 2);
        assert_eq!(
            notices[MAX_NOTICES - 1],
            notice(&(MAX_NOTICES - 1).to_string())
        );
        assert_eq!(notices[MAX_NOTICES].severity, "WARNING");
        assert!(
            notices[MAX_NOTICES]
                .message
                .starts_with("25 more notices were dropped"),
            "{notices:?}"
        );
        assert_eq!(notices[MAX_NOTICES + 1].message, "restored");
        // Draining resets the caps.
        shared.push_notice(notice("again"));
        assert_eq!(shared.take_notices(), vec![notice("again")]);
    }

    #[test]
    fn notices_are_capped_by_size() {
        let shared = Shared::default();
        let big = "x".repeat(MAX_NOTICE_BYTES / 4);
        for _ in 0..5 {
            shared.push_notice(notice(&big));
        }
        // A small notice still fits after a big one was dropped.
        shared.push_notice(notice("small"));
        let notices = shared.take_notices();
        assert_eq!(notices.len(), 5);
        assert_eq!(notices[3], notice("small"));
        assert!(
            notices[4]
                .message
                .starts_with("2 more notices were dropped")
        );
        shared.clear_notices();
        assert!(shared.take_notices().is_empty());
    }

    #[tokio::test]
    async fn waits_for_cancel_requests_in_flight() -> Result<(), tokio::time::error::Elapsed> {
        let shared = Arc::new(Shared::default());
        shared.wait_for_cancels().await;
        let guard = shared.cancel_started();
        let waiter = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move { shared.wait_for_cancels().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(guard);
        let joined = tokio::time::timeout(std::time::Duration::from_secs(1), waiter).await?;
        assert!(joined.is_ok());
        Ok(())
    }

    #[test]
    fn handles_messages_split_across_reads() {
        let shared = Shared::default();
        let mut scanner = FrameScanner::default();
        let mut bytes = message(b'N', b"SNOTICE\0Mhi\0\0");
        bytes.extend(message(b'C', b"CREATE TABLE\0"));
        for byte in &bytes {
            scanner.feed(std::slice::from_ref(byte), &shared);
        }
        assert_eq!(drain(&shared), vec![Some("CREATE TABLE".into())]);
    }

    #[test]
    fn ssl_answer_switches_mode() {
        let shared = Arc::new(Shared::default());
        let mut refused = TapStream::expecting_ssl_response((), Arc::clone(&shared));
        let mut bytes = vec![b'N'];
        bytes.extend(message(b'C', b"BEGIN\0"));
        refused.observe(&bytes);
        assert_eq!(shared.ssl_response(), Some(b'N'));
        assert_eq!(drain(&shared), vec![Some("BEGIN".into())]);

        let accepted_shared = Arc::new(Shared::default());
        let mut accepted = TapStream::expecting_ssl_response((), Arc::clone(&accepted_shared));
        accepted.observe(b"S\x16\x03\x03");
        assert_eq!(accepted_shared.ssl_response(), Some(b'S'));
        assert_eq!(accepted.mode, TapMode::Passthrough);
        assert!(drain(&accepted_shared).is_empty());
    }
}
