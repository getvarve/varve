//! Redis driver for Varve (design doc sections 3 and 4): the console and a
//! read-only key browser for Redis 6.2 and newer and Valkey.
//!
//! [`RedisDriver`] opens a [`RedisSession`] over TCP, optionally with TLS
//! (rustls; `prefer` uses TLS when the server speaks it, plain TCP otherwise). The session runs
//! console scripts (one command per line, parsed like `redis-cli`), refuses
//! writes on read-only connections using `COMMAND INFO` flags, and cancels
//! running commands with `CLIENT KILL ID` from a second connection.
//!
//! # Read-only sessions and command risk
//!
//! A read-only session runs a command only when it is known to be
//! read-only (an allowlist):
//!
//! - it is one of [`READ_ONLY_EXTRAS`] (vetted connection and
//!   introspection commands such as `PING`, `INFO`, `CLIENT LIST`,
//!   `CONFIG GET`), or
//! - it is a core command the server flags `readonly` (and neither
//!   `write` nor `may_replicate`) outside the `@admin` and `@dangerous` ACL
//!   categories (so `KEYS` and `SORT_RO` are refused), or
//! - it is a module command in [`READ_ONLY_MODULE_COMMANDS`] that the
//!   server flags `readonly` too, outside `@admin` and `@dangerous` (module
//!   flags alone are not trusted),
//!
//! and it is not in [`READ_ONLY_DENY`] (read-only scripts, which can run
//! non-write admin commands). Everything else is refused before it is
//! sent, unknown commands and subcommands included. Commands are classified by their real name from `COMMAND
//! INFO`, so a command renamed with `rename-command` is treated as what it
//! is. [`RedisSession::command_risk`] follows the same rules
//! ([`CommandRisk`]), and [`RedisSession::read_only_allows`] tells whether
//! a read-only session would run a command.
//!
//! Read-only is a guard against accidental writes, not access control. A
//! read-only session can still read everything the server user can read,
//! including secrets such as `CONFIG GET requirepass` or `masterauth` and
//! the password hashes in `ACL LIST` and `ACL GETUSER`. To limit what a
//! person can see or do, give them a Redis ACL user with fewer permissions.
//!
//! Init commands of a connection profile run on every connect and
//! reconnect, so they are for connection setup only: `SELECT`, `HELLO 2`
//! (also with `SETNAME`), `CLIENT SETNAME`, `CLIENT SETINFO`,
//! `CLIENT NO-EVICT`, `CLIENT NO-TOUCH`, `READONLY` and `READWRITE`.
//! `AUTH` and `HELLO ... AUTH` are refused too: a password belongs in the
//! profile's credentials (kept in the keychain), not in the init text.
//! Everything else is refused when the profile is saved
//! ([`check_init_script`], from `varve-redis-syntax`) and again at connect
//! by the command's real name ([`init_command_problem`]); at connect an
//! init command the server does not describe in `COMMAND INFO` is refused
//! as well (fail closed: its real name is unknown). These commands change
//! the connection, never data, so read-only sessions accept all of them
//! too (the console keeps its read-only rules). An init `SELECT` decides
//! the database on the first connect only; a transparent reconnect returns
//! to the database the session is on. Database numbers above 65535 cannot
//! be followed and are refused (when the profile is saved, at connect and
//! in the console).
//!
//! The console refuses `HELLO` unless its first argument is protocol
//! version 2 (or it has none): the driver speaks RESP2. The refusal never
//! repeats the arguments, which may hold a password. A call whose future
//! is dropped after it sent a command but before it read the reply leaves
//! the server state unknown (a `SELECT` or `MULTI` may have run), so its
//! connection is closed and the next call reconnects. While a console
//! `MULTI` block is open, the driver's own lookups run on a side connection
//! (never queued into the user's transaction) and the key browser and
//! `select_db` refuse with code `MULTI_OPEN`.
//!
//! When the connection is replaced (a reconnect after a timeout, cancel,
//! dropped call or lost connection) while a console `WATCH` or `MULTI`
//! block is active, the transaction state is gone with it, and the session
//! remembers each separately:
//!
//! - a lost `WATCH`: every `EXEC` is refused without being sent (code
//!   `TRANSACTION_RESET`: "the WATCH was lost. Run WATCH again (or UNWATCH)
//!   and then the transaction") until a `WATCH` or `UNWATCH` the server
//!   confirms; `MULTI` and `DISCARD` do not lift it;
//! - a lost `MULTI` block: commands meant to be queued are refused (they
//!   would run at once), `EXEC` is refused once, and `DISCARD` is answered
//!   locally (the server already dropped the block); `MULTI` starts over.
//!
//! # Threat model
//!
//! The server is trusted. Read-only checks and the risk classification rely
//! on the server's own `COMMAND INFO`; they protect users from their own
//! commands, not from a hostile server. TLS modes `prefer` and `require`
//! encrypt but do not verify the server's certificate, so they give no
//! protection against an active man in the middle (who can also answer the
//! plain probe of `prefer` like Redis and keep the session on plain TCP,
//! where the password goes unencrypted); `verify-ca` and `verify-full` do.
//!
//! # Timeouts and cancel
//!
//! With TLS mode `prefer`, the first connection of a session sends a bare
//! `PING` (no credentials) over plain TCP. A plain Redis or Valkey answers
//! it at once (`+PONG` or `-NOAUTH`) and the connect goes on, plain, on
//! that connection; a TLS server rejects the plain bytes (an alert or a
//! closed connection) and gets a TLS handshake on a new connection. So
//! `prefer` never waits for a TLS handshake that a plain server leaves
//! unanswered (a plain Redis takes a ClientHello for an inline command and
//! waits for its newline), which matters most through slow SSH tunnels. A
//! server that answers neither within half the connect timeout gets the
//! TLS attempt with the rest. Only when the probe got a plain Redis reply
//! does AUTH go over plain TCP; `require` and the verify modes never send
//! anything unencrypted and never fall back. Every later connection of the session
//! (reconnects, the key browser's side connections, cancel connections)
//! uses the transport that worked, so it neither waits for a TLS attempt
//! that cannot succeed nor falls back to plain TCP once TLS worked. That
//! outcome sticks for the life of the session: when the server is later
//! switched between plain TCP and TLS, reconnects fail until the session
//! is opened again (a new session negotiates again).
//!
//! The connect timeout covers the whole connect: TCP, TLS, AUTH,
//! `CLIENT ID`, the init commands and `INFO`, also on transparent
//! reconnects. A command that runs past the statement timeout, or is
//! cancelled, gets its connection closed, so a late reply is never read as
//! the reply to a later command; the next call reconnects. The server may
//! still finish such a command, and the error says so. When the server
//! refuses `CLIENT KILL` (an ACL user without that permission) a cancelled
//! call still returns Cancelled and its connection is closed, and the error
//! says that the command may keep running on the server. To report that, a
//! cancelled call waits for the outcome of `CLIENT KILL`: it returns as soon
//! as the kill has succeeded or failed, and after at most 2 seconds
//! otherwise (the error then says the kill did not finish in time). A
//! cancel during a transparent reconnect stops the reconnect at once.
//!
//! # Memory limits
//!
//! - One reply may take at most [`MAX_REPLY_BYTES`] (16 MiB) on the wire,
//!   in the console and the browser alike. The connection counts the bytes
//!   it reads and stops at the limit, so the protocol library never buffers
//!   a larger reply; the command fails with a Query error that suggests
//!   reading in parts, and the session reconnects. The decoded reply takes
//!   more memory than its wire size (about 100 bytes per element on top of
//!   the element data), and the console keeps at most
//!   [`varve_driver::ExecOptions::max_rows`] rows of it.
//! - One console command or browser page converts at most
//!   [`MAX_RESULT_VALUES`] values, counted at every depth (an array counts
//!   once plus its elements), so a reply of millions of tiny values fails
//!   with a Query error instead of taking hundreds of MiB once decoded.
//!   `max_rows` trims only the top level and is applied first.
//! - Browser pages hold at most `count` entries and about
//!   [`MAX_PAGE_BYTES`] of element data, fetched in batches sized from
//!   `MEMORY USAGE` and the sizes seen so far; a page can go over by at most
//!   one batch. List, sorted set and stream batches are sized to fit the
//!   remaining budget; for hashes and sets the server decides how many
//!   entries one `HSCAN`/`SSCAN` step returns, so the overshoot there is up
//!   to one step (never more than [`MAX_REPLY_BYTES`]). Strings come in
//!   [`STRING_CHUNK_BYTES`] slices.
//! - An entry larger than [`MAX_REPLY_BYTES`] cannot be read (Redis has no
//!   command that reads part of one); it is shown as a placeholder, a
//!   [`varve_driver::Value::Other`] starting with [`OVERSIZED_MARK`] and
//!   giving its size when it can be measured, and paging continues past it.
//!   When one `HSCAN`/`SSCAN` step is too large to read at once, it is
//!   read once without its values (hash field names) or with as many members
//!   as fit one reply (sets), and its entries are shown over the following
//!   pages while each page has room, so a page stays within
//!   [`MAX_PAGE_BYTES`] plus one entry. Oversized entries are measured
//!   without being held: the command is sent on a short-lived side
//!   connection whose reply is read as a stream, keeping only small parts
//!   and the lengths of the rest (no scripts, so this also works for
//!   read-only sessions and users without script permissions). Hash fields
//!   are then read by name (`HSTRLEN`, then `HGET` when the value fits). A
//!   set member cannot be read on its own, so one too large to keep while
//!   its step is read is shown as a placeholder with the size it had at
//!   the time of the scan, without a later existence check. A busy server
//!   (`BUSY`) or another server error is reported as an error.
//! - A key whose name is larger than [`MAX_REPLY_BYTES`] appears in
//!   [`RedisSession::scan_keys`] as a placeholder entry
//!   ([`KeyEntry::placeholder`] is true, the name is empty, the type is
//!   [`KeyType::Unknown`] unless the scan had a type filter), after `SCAN`
//!   was retried with smaller counts. No script is involved.
//! - All oversized measurements of one page share one side connection,
//!   opened on first use and closed when the page is done.
//! - `NOT_FOUND_MARK` (a placeholder for set members a later search did not
//!   find) was removed together with that search: set members are shown
//!   from what was measured when their step was read.
//!
//! # Hash and set paging
//!
//! A scan step is never read twice: `HSCAN`/`SSCAN` only guarantee that an
//! element present for the whole scan is returned at least once by the
//! whole scan, not that one cursor returns the same step again (rehashing,
//! deletes and growth change steps). When a page ends inside a step, the
//! rest of the step stays in the session and the page cursor is `p:<id>`;
//! when a page is full, the next non-empty step is read ahead and kept the
//! same way, so the last page reports the end of the scan (it can still be
//! empty when every entry kept for it was deleted, see below). The session
//! keeps at most [`MAX_PAGE_STATES`] such states and
//! [`MAX_PAGE_STATE_BYTES`] of their data (least recently used evicted
//! first), drops them after [`PAGE_STATE_IDLE`] unused and when it
//! reconnects after a lost connection, and each cursor works once (a
//! failed page keeps it). An unknown cursor gives a Query error with code
//! `CURSOR_EXPIRED`: start the scan again. For the UI this means: at most
//! [`MAX_PAGE_STATES`] (16) hash or set viewers per session can page at the
//! same time (the least recently used one gets `CURSOR_EXPIRED` on its next
//! page), and a cancel, a statement timeout or a lost connection drops all
//! page states of the session (the connection is closed and reopened).
//!
//! Hash fields of a kept step are read by name when shown and skipped when
//! they were deleted meanwhile, so a page (also the last one) can be empty
//! when every kept entry on it was deleted; while entries remain, a page
//! is never empty. Cursors and offsets from the UI
//! are checked: offsets and ranks above `i64::MAX` and malformed cursors
//! are Query errors.
//!
//! Every `u64` in the browser types (cursors, offsets, indexes, lengths,
//! sizes and key counts), and the `i64` times to live (`ttlMs`), are
//! serialized as decimal strings, because JavaScript numbers lose precision
//! above 2^53 and `SCAN` cursors use the whole range; deserializing accepts
//! a string or a number.
//!
//! Engine-specific features are inherent methods on [`RedisSession`],
//! reached with [`varve_driver::Session::as_any`]:
//!
//! ```no_run
//! # async fn demo(session: &dyn varve_driver::Session) -> Result<(), varve_driver::DriverError> {
//! use varve_driver_redis::RedisSession;
//! if let Some(redis) = session.as_any().downcast_ref::<RedisSession>() {
//!     let page = redis.scan_keys(0, "user:*", None, 200).await?;
//!     println!("{} keys, next cursor {}", page.keys.len(), page.cursor);
//! }
//! # Ok(())
//! # }
//! ```

mod browser;
mod bulk;
mod conn;
mod digest;
mod docs;
mod edit;
mod error;
mod measure;
mod modules;
mod oversized;
mod pagestate;
mod parse;
mod pubsub;
mod reply;
mod risk;
mod risk_table;
mod session;
mod stream_groups;
mod tls;
mod watches;
mod zrange;

use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, DriverError, ErrorKind, Session};

pub use browser::{
    DbKeyspace, FieldValue, KeyEntry, KeyInfo, KeyType, ListItem, MAX_JSON_BYTES, MAX_PAGE_BYTES,
    MAX_PAGE_COUNT, STRING_CHUNK_BYTES, ScanPage, ScoredMember, StreamEntry, ValueChunk, ValueData,
    ValuePage, parse_keyspace,
};
pub use bulk::{
    BULK_BATCH, BULK_NAME_SUFFIX, BULK_STEP_TIMEOUT, BulkCount, BulkCounts, BulkEnd, BulkFilter,
    BulkProgress, BulkTtl, BulkTtlAction,
};
pub use conn::ServerFlavor;
pub use digest::{
    DIGEST_MAX_ELEMENTS, DIGEST_TIME_BUDGET, JSON_DIGEST_BYTES, JSON_DIGEST_ELEMENTS,
    OPEN_DIGEST_BYTES, OPEN_DIGEST_ELEMENTS, OPEN_DIGEST_TIME, OPEN_JSON_ELEMENTS,
    PAGE as DIGEST_PAGE, STRING_PIECE,
};
pub use docs::CommandHelp;
pub use edit::{
    CmsIncrement, ConflictReason, DigestMissing, EDIT_NAME_SUFFIX, EditCounts, EditError,
    EditOutcome, EditProgram, FullDigest, JSON_UNAVAILABLE, KeyEdit, KeyExpect, KeyVersion,
    ListEnd, MAX_EDIT_BYTES, MAX_EDITS, MAX_TS_LABELS, RedisEditPlan, TTL_TOLERANCE_MS,
    TrimStrategy, VersionReader,
};
pub use measure::SIDE_NAME_SUFFIX;
pub use modules::{
    FeatureSupport, InfoField, LoadedModule, MAX_QUERY_INPUTS, ModuleFeature, ModuleKind,
    ModuleQuery, ModuleReport, QueryResult, QueryRow, SearchAttribute, SearchEngine, SearchIndex,
    TopKItem, TsInfo, TsLabel, TsMeta, TsRangePage, TsRangeQuery, TsRule, TsSample,
    UNRECOVERABLE_OPTIONS, WATCH_OPEN,
};
pub use oversized::OVERSIZED_MARK;
pub use pagestate::{MAX_PAGE_STATE_BYTES, MAX_PAGE_STATES, PAGE_STATE_IDLE};
pub use parse::{
    ParsedCommand, check_init_script, init_command_problem, key_display, key_from_display,
    parse_script,
};
pub use pubsub::{
    MAX_NAME_BYTES, NameKind, OutputBufferReport, PUBSUB_NAME_SUFFIX, PubSubItem, PubSubOpener,
    PubSubReader, PubSubWriter, pubsub_limit,
};
pub use reply::MAX_RESULT_VALUES;
pub use risk::{CommandRisk, READ_ONLY_DENY, READ_ONLY_EXTRAS, READ_ONLY_MODULE_COMMANDS};
pub use session::{MAX_REPLY_BYTES, RedisSession};
pub use stream_groups::{
    ConsumerPending, PendingEntry, PendingPage, PendingQuery, PendingSummary, StreamConsumer,
    StreamGroup, StreamGroupChange, StreamInfo,
};
pub use zrange::{RangeBy, ZsetBound, ZsetRange, ZsetRangePage};

/// The Redis driver.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedisDriver;

impl RedisDriver {
    /// Creates the driver.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Driver for RedisDriver {
    fn engines(&self) -> &'static [Engine] {
        &[Engine::Redis]
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
        if params.engine != Engine::Redis {
            return Err(DriverError::new(
                ErrorKind::Unsupported,
                format!("The Redis driver cannot connect to {:?}", params.engine),
            ));
        }
        Ok(Box::new(RedisSession::connect(params).await?))
    }
}

#[cfg(test)]
mod send_tests {
    //! Every public async API returns a `Send` future (the app runs them on
    //! a multi-threaded runtime). This only has to compile.

    use varve_driver::{ConnectParams, Driver, ExecOptions, Session};

    use super::*;

    fn assert_send<T: Send>(_: &T) {}

    #[allow(dead_code)] // never called: compiling it is the test
    fn futures_are_send(session: &RedisSession, params: &ConnectParams) {
        assert_send(&RedisDriver::new().connect(params));
        assert_send(&RedisSession::connect(params));
        assert_send(&session.scan_keys(0, "*", None, 10));
        assert_send(&session.scan_keys(0, "*", Some(KeyType::Hash), 10));
        assert_send(&session.key_info(b"k"));
        assert_send(&session.read_value(b"k", &ValuePage::first(10)));
        assert_send(&session.keyspace());
        assert_send(&session.database_count());
        assert_send(&session.command_risk("GET"));
        assert_send(&session.read_only_allows("GET"));
        assert_send(&session.script_risk("GET a"));
        assert_send(&session.current_db());
        assert_send(&session.select_db(1));
        assert_send(&session.execute("PING", &ExecOptions::default()));
        assert_send(&session.ping());
        assert_send(&session.cancel_handle().cancel());
    }

    #[test]
    fn public_futures_are_send() {
        let _ = futures_are_send;
    }
}
