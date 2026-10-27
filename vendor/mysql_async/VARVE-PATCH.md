# Varve patch to mysql_async 0.37.1

This directory is `mysql_async` 0.37.1 exactly as published on crates.io
(MIT or Apache-2.0, see `LICENSE-MIT` and `LICENSE-APACHE`), with the small
change described here. The root `Cargo.toml` points cargo at it:

```toml
[patch.crates-io]
mysql_async = { path = "vendor/mysql_async" }
```

The only user is `crates/driver-mysql`, which pins `mysql_async = "=0.37.1"`.

## Why

A read-only Varve session (`SET SESSION TRANSACTION READ ONLY`) must notice
when the server leaves read-only mode, even when that happens inside a stored
function or procedure the driver cannot see into, for example
`SELECT f(); INSERT ...` where `f()` runs `SET @@session.transaction_read_only = 0`.
The driver must also know the session time zone to label `TIMESTAMP` values,
and it must not run statements of its own between the user's statements,
because those change `FOUND_ROWS()`, `ROW_COUNT()` and the warning list.

MySQL (5.7 and later) and MariaDB (10.2 and later) report such changes in
every OK packet when the client asks for session state tracking
(`CLIENT_SESSION_TRACK`) and the session enables the trackers
(`session_track_system_variables`, `session_track_transaction_info`).
`mysql_common` already parses that data (`OkPacket::session_state_info`),
but `mysql_async` 0.37.1:

1. never sets `CLIENT_SESSION_TRACK` in the handshake, and offers no option to
   add capability flags, so the server never sends the data; and
2. keeps only the last OK packet (`Conn::last_ok_packet`), so the changes of
   the earlier statements of a multi-statement query, or of the OK packets
   inside a `CALL`, are lost before the caller can look at them.

The driver also reports which statements of a failed multi-statement script
completed and whether a transaction was open after each (the partial output
of `DriverError`). The status flags of every OK packet, including result set
terminators, carry `SERVER_STATUS_IN_TRANS`, but `mysql_async` keeps only
the flags of the last one.

## Changes against upstream 0.37.1

Every change is marked with a `VARVE-PATCH` comment.

1. `src/opts/mod.rs`, `Opts::get_capabilities`: adds
   `CapabilityFlags::CLIENT_SESSION_TRACK`. The server then appends the info
   string and, when `SERVER_SESSION_STATE_CHANGED` is set, the session state
   data to OK packets. `mysql_common` handles both layouts already. Servers
   send state data only for trackers the session enabled, so a session that
   does not enable any sees no difference beyond the info string encoding.
2. `src/conn/mod.rs`:
   - `ConnInner` gets `session_state_changes: Vec<SessionStateChange<'static>>`.
   - `Conn::handle_ok` (the one place every OK packet passes, including
     result set terminators) decodes the packet's session state entries and
     appends them. Entries that fail to decode are skipped, as the C client
     does with tracker types it does not know.
   - `Conn::write_command_raw` (every command goes through it) clears the
     list, so it always holds the changes of the current command.
   - New public `Conn::session_state_changes(&self) -> &[SessionStateChange<'static>]`.
3. `src/queryable/query_result/mod.rs`: new public
   `QueryResult::session_state_changes(&self)`, the same list while a query
   result is being read, so a caller can see what each statement changed
   between result sets.
4. `src/conn/mod.rs` and `src/queryable/query_result/mod.rs`: `ConnInner`
   gets `ok_status_flags: Vec<StatusFlags>`, filled by `Conn::handle_ok` with
   the status flags of every OK packet and cleared with the session state
   changes in `Conn::write_command_raw`; new public
   `Conn::ok_status_flags(&self) -> &[StatusFlags]` and
   `QueryResult::ok_status_flags(&self)`.

Nothing else differs from the published crate. (The crates.io extraction
marker `.cargo-ok` was removed; `.cargo_vcs_info.json` names the upstream
commit.)

The full diff against the published sources:

```diff
diff -ru --label upstream/src --label vendor/src upstream/src/conn/mod.rs vendor/mysql_async/src/conn/mod.rs
--- upstream/src
+++ vendor/src
@@ -17,7 +17,7 @@
     packets::{
         AuthPlugin, AuthSwitchRequest, CommonOkPacket, ErrPacket, HandshakePacket,
         HandshakeResponse, OkPacket, OkPacketDeserializer, OldAuthSwitchRequest, OldEofPacket,
-        ResultSetTerminator, SslRequest,
+        session_state_change::SessionStateChange, ResultSetTerminator, SslRequest,
     },
     proto::MySerialize,
     row::Row,
@@ -104,6 +104,12 @@
     mariadb_capabilities: MariadbCapabilities,
     status: StatusFlags,
     last_ok_packet: Option<OkPacket<'static>>,
+    /// VARVE-PATCH: session state changes reported by the OK packets of
+    /// the current command (cleared when the next command is sent).
+    session_state_changes: Vec<SessionStateChange<'static>>,
+    /// VARVE-PATCH: status flags of every OK packet of the current command
+    /// (cleared when the next command is sent).
+    ok_status_flags: Vec<StatusFlags>,
     last_err_packet: Option<mysql_common::packets::ServerError<'static>>,
     handshake_complete: bool,
     pool: Option<Pool>,
@@ -154,6 +160,8 @@
             mariadb_capabilities: MariadbCapabilities::empty(),
             status: StatusFlags::empty(),
             last_ok_packet: None,
+            session_state_changes: Vec::new(),
+            ok_status_flags: Vec::new(),
             last_err_packet: None,
             handshake_complete: false,
             stream: None,
@@ -256,6 +264,25 @@
         self.inner.last_ok_packet.as_ref()
     }
 
+    /// VARVE-PATCH: session state changes the server reported in the OK
+    /// packets of the current command, in the order they arrived.
+    ///
+    /// Needs session tracking on the server (`session_track_*` variables).
+    /// The list covers every OK packet of the command, including the ones
+    /// between the result sets of a multi-statement query or a `CALL`, and
+    /// is cleared when the next command is sent.
+    pub fn session_state_changes(&self) -> &[SessionStateChange<'static>] {
+        &self.inner.session_state_changes
+    }
+
+    /// VARVE-PATCH: status flags of every OK packet of the current command,
+    /// in the order they arrived: one per statement result and result set
+    /// terminator of a multi-statement query or a `CALL`. Cleared when the
+    /// next command is sent.
+    pub fn ok_status_flags(&self) -> &[StatusFlags] {
+        &self.inner.ok_status_flags
+    }
+
     /// Turns on/off automatic connection reset (see [`crate::PoolOpts::with_reset_connection`]).
     ///
     /// Only makes sense for pooled connections.
@@ -305,7 +332,19 @@
     /// Handles OK packet.
     pub(crate) fn handle_ok(&mut self, ok_packet: OkPacket<'static>) {
         self.inner.status = ok_packet.status_flags();
+        // VARVE-PATCH: keep the status flags of every OK packet.
+        self.inner.ok_status_flags.push(ok_packet.status_flags());
         self.inner.last_err_packet = None;
+        // VARVE-PATCH: keep the session state changes of every OK packet.
+        // An entry that does not decode is skipped, as the C client does
+        // with tracker types it does not know.
+        if let Ok(infos) = ok_packet.session_state_info() {
+            for info in infos {
+                if let Ok(change) = info.decode() {
+                    self.inner.session_state_changes.push(change.into_owned());
+                }
+            }
+        }
         self.inner.last_ok_packet = Some(ok_packet);
     }
 
@@ -975,6 +1014,9 @@
         debug_assert!(!body.is_empty());
         self.clean_dirty().await?;
         self.reset_seq_id();
+        // VARVE-PATCH: a new command starts a new list of changes.
+        self.inner.session_state_changes.clear();
+        self.inner.ok_status_flags.clear();
         self.write_packet(body).await
     }
 
diff -ru --label upstream/src --label vendor/src upstream/src/opts/mod.rs vendor/mysql_async/src/opts/mod.rs
--- upstream/src
+++ vendor/src
@@ -1159,7 +1159,10 @@
             | CapabilityFlags::CLIENT_MULTI_RESULTS
             | CapabilityFlags::CLIENT_PS_MULTI_RESULTS
             | CapabilityFlags::CLIENT_DEPRECATE_EOF
-            | CapabilityFlags::CLIENT_PLUGIN_AUTH;
+            | CapabilityFlags::CLIENT_PLUGIN_AUTH
+            // VARVE-PATCH: ask the server for session state tracking, so OK
+            // packets carry the session state changes (see VARVE-PATCH.md).
+            | CapabilityFlags::CLIENT_SESSION_TRACK;
 
         if self.inner.mysql_opts.db_name.is_some() {
             out |= CapabilityFlags::CLIENT_CONNECT_WITH_DB;
diff -ru --label upstream/src --label vendor/src upstream/src/queryable/query_result/mod.rs vendor/mysql_async/src/queryable/query_result/mod.rs
--- upstream/src
+++ vendor/src
@@ -221,6 +221,20 @@
         self.conn.get_warnings()
     }
 
+    /// VARVE-PATCH: session state changes reported so far by this query's
+    /// OK packets, in order (see [`crate::Conn::session_state_changes`]).
+    /// Read it between result sets to see what each statement changed.
+    pub fn session_state_changes(&self) -> &[crate::SessionStateChange<'static>] {
+        self.conn.session_state_changes()
+    }
+
+    /// VARVE-PATCH: status flags of this query's OK packets so far, in
+    /// order (see [`crate::Conn::ok_status_flags`]). A result set's
+    /// terminator is added once its last row has been read.
+    pub fn ok_status_flags(&self) -> &[crate::consts::StatusFlags] {
+        self.conn.ok_status_flags()
+    }
+
     /// Collects the current result set of this query result.
     ///
     /// It is parametrized by `R` and internally calls `R::from_row(Row)` on each row.
```

## Updating or dropping the patch

- To check the vendored copy:
  `diff -r ~/.cargo/registry/src/*/mysql_async-0.37.1/src vendor/mysql_async/src`
  must show only the diff above.
- To move to a newer `mysql_async`: vendor the new release, re-apply the four
  changes (or drop them once upstream has an equivalent), update the pin in
  `crates/driver-mysql/Cargo.toml`, and run the driver's tests, which fail
  without tracking: `cargo test -p varve-driver-mysql -- --include-ignored`
  (`read_only_mode_cannot_be_switched_off`,
  `timestamps_follow_the_session_time_zone`, and the transaction states in
  `tests/partial.rs`).
- Once upstream supports this, delete `vendor/mysql_async` and the
  `[patch.crates-io]` section, and use the upstream API in
  `crates/driver-mysql/src/session.rs` and `src/connect.rs`.

## Draft upstream issue and pull request

**Title:** Support session state tracking (`CLIENT_SESSION_TRACK`) and expose
the session state changes of each OK packet

**Body:**

MySQL 5.7+ and MariaDB 10.2+ can report session state changes in OK packets:
changed system variables (`session_track_system_variables`), the current
schema, transaction characteristics and state
(`session_track_transaction_info`), GTIDs. `mysql_common` already parses
this (`OkPacket::session_state_info`, `SessionStateChange`), but
`mysql_async` cannot use it:

1. `Opts::get_capabilities` never includes `CLIENT_SESSION_TRACK`, and there
   is no option to add it, so servers never send the data.
2. Only the last OK packet is kept (`Conn::last_ok_packet`), so in a
   multi-statement query or a `CALL` the changes reported by earlier OK
   packets are gone by the time the caller can look.

Use cases: noticing that a statement (possibly inside a stored routine)
changed `time_zone`, `autocommit` or the transaction access mode without
running extra queries (which would disturb `FOUND_ROWS()`, `ROW_COUNT()` and
the warning list); keeping a client-side view of the current schema or
character set in sync; GTID-based read-your-writes.

Proposal (backwards compatible):

- Add `OptsBuilder::session_track(bool)` (default `false`, so the handshake
  stays as it is today) that adds `CLIENT_SESSION_TRACK` to the capabilities.
- Collect the decoded `SessionStateChange`s of every OK packet of the current
  command in `Conn` (cleared when the next command is sent), and expose them
  as `Conn::session_state_changes(&self) -> &[SessionStateChange<'static>]`
  and `QueryResult::session_state_changes(&self)`.

A working implementation is about 40 lines (the diff above, minus making
the capability opt-in through `Opts`). We have run it against MySQL 8.0 and
9.7 and MariaDB 10.6 and 12.3: functions and procedures that change
`transaction_read_only`/`tx_read_only` or `time_zone`, `START TRANSACTION READ
WRITE` from a procedure, and multi-statement queries all report as expected.
Happy to open a PR with tests if this direction is welcome.
