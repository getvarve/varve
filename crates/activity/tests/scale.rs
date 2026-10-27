//! Every redaction pass is linear: each shape from the round 7 and round 8
//! audits, and every trigger word repeated, grows at most about linearly
//! in time from 64 KiB to 256 KiB to 1 MiB, at text levels `normalized`
//! and `full`, and is redacted rather than hidden whole (a pass that ran
//! out of its work budget would hide the text and look fast).
//!
//! Release builds run the three sizes and also bound the 1 MiB time; a
//! shape whose timing misses a bound is timed again (three tries) before
//! it fails, so other work on the machine does not fail the test; debug
//! builds (ten times slower or more) run 8 KiB and 32 KiB and check the
//! growth only. Run the full check with
//! `cargo test --release -p varve-activity --test scale`.

use std::time::{Duration, Instant};

use varve_activity::{REDACTED, SqlDialect, TextLevel, prepare_statement_text};

const PG: SqlDialect = SqlDialect::Postgres;
const MY: SqlDialect = SqlDialect::Mysql;
const RD: SqlDialect = SqlDialect::Redis;

/// A repeated construct: `prefix + unit * n + middle + close * n + suffix`.
struct Shape {
    prefix: &'static str,
    unit: &'static str,
    middle: &'static str,
    close: &'static str,
    suffix: &'static str,
}

impl Shape {
    fn fill(
        prefix: &'static str,
        unit: &'static str,
        middle: &'static str,
        close: &'static str,
        suffix: &'static str,
    ) -> Shape {
        Shape {
            prefix,
            unit,
            middle,
            close,
            suffix,
        }
    }

    fn rep(prefix: &'static str, unit: &'static str, suffix: &'static str) -> Shape {
        Shape::fill(prefix, unit, "", "", suffix)
    }

    /// The text at about `bytes` long.
    fn build(&self, bytes: usize) -> String {
        let fixed = self.prefix.len() + self.middle.len() + self.suffix.len();
        let n = bytes.saturating_sub(fixed) / (self.unit.len() + self.close.len()).max(1);
        let mut text = String::with_capacity(bytes);
        text.push_str(self.prefix);
        for _ in 0..n {
            text.push_str(self.unit);
        }
        text.push_str(self.middle);
        for _ in 0..n {
            text.push_str(self.close);
        }
        text.push_str(self.suffix);
        text
    }
}

/// The audit's shapes (round 7, `perf.rs`), without the nesting cases that
/// `adversarial_nesting.rs` covers.
fn audit_shapes() -> Vec<(&'static str, SqlDialect, Shape)> {
    vec![
        (
            "nested AES_ENCRYPT( closed (my)",
            MY,
            Shape::fill("SELECT ", "AES_ENCRYPT(", "'x'", ", 'k')", ""),
        ),
        (
            "nested AES_ENCRYPT( closed (pg)",
            PG,
            Shape::fill("SELECT ", "AES_ENCRYPT(", "'x'", ", 'k')", ""),
        ),
        (
            "nested HMAC( unclosed (pg)",
            PG,
            Shape::rep("SELECT ", "HMAC(", "'x'"),
        ),
        (
            "nested pgp_sym_encrypt( unclosed (pg)",
            PG,
            Shape::rep("SELECT ", "pgp_sym_encrypt(", "'x'"),
        ),
        (
            "sibling HMAC('a', 'b') (pg)",
            PG,
            Shape::rep("SELECT ", "HMAC('a', 'b'), ", "1"),
        ),
        (
            "block comment HMAC( unclosed (pg)",
            PG,
            Shape::rep("SELECT 1 /* ", "HMAC(", " */"),
        ),
        (
            "line comments HMAC( block (pg)",
            PG,
            Shape::rep("SELECT 1\n", "-- HMAC(\n", ""),
        ),
        (
            "line comments crypt( block (my)",
            MY,
            Shape::rep("SELECT 1\n", "-- crypt(\n", ""),
        ),
        (
            "block comment password = ' unterminated (pg)",
            PG,
            Shape::rep("SELECT 1 /* ", "password '", " */"),
        ),
        (
            "line comments PASSWORD x1 (pg)",
            PG,
            Shape::rep("SELECT 1\n", "-- PASSWORD x1\n", ""),
        ),
        (
            "block comment IDENTIFIED (my)",
            MY,
            Shape::rep("SELECT 1 /* ", "IDENTIFIED ", " */"),
        ),
        (
            "SET a = SET a = ... (my)",
            MY,
            Shape::rep("", "SET a = ", "1"),
        ),
        (
            "SET a = SET a = ... (pg)",
            PG,
            Shape::rep("", "SET a = ", "1"),
        ),
        (
            "SET init_connect = 'x' || (my)",
            MY,
            Shape::rep("SET GLOBAL init_connect = ", "'a' ", ""),
        ),
        (
            "SET init_connect = CONCAT( nested (my)",
            MY,
            Shape::rep("SET GLOBAL init_connect = ", "CONCAT(", "'x'"),
        ),
        // Round 8: an init_connect value whose nested ranges overlap (each
        // word of a glued chunk redacts the whole chunk) was mapped back
        // byte by byte per range.
        (
            "init_connect 'CREATE USER u x.x.x...' (my)",
            MY,
            Shape::rep("SET GLOBAL init_connect = 'CREATE USER u ", "x.", "'"),
        ),
        (
            "init_connect 'ALTER USER u IDENTIFIED BY a.a.a...' (my)",
            MY,
            Shape::rep(
                "SET GLOBAL init_connect = 'ALTER USER u IDENTIFIED BY ",
                "a.",
                "'",
            ),
        ),
        (
            "init_connect 'SELECT PASSWORD=PASSWORD=...' (my)",
            MY,
            Shape::rep("SET GLOBAL init_connect = 'SELECT ", "PASSWORD=", "'"),
        ),
        // Round 8: a long list of secret columns with more values than
        // columns was searched once per value.
        (
            "INSERT many secret columns, more values (pg)",
            PG,
            Shape::fill("INSERT INTO t (", "pw, ", "pw) VALUES (", "1, 1, ", "1)"),
        ),
        (
            "INSERT many secret columns, more values (my)",
            MY,
            Shape::fill("INSERT INTO t (", "pw, ", "pw) VALUES (", "1, 1, ", "1)"),
        ),
        // Round 12: every COPY data block of a dump is found in one pass.
        (
            "pg_dump COPY blocks (pg)",
            PG,
            Shape::rep(
                "SET client_encoding = 'UTF8';\n",
                "COPY t (id, note, password) FROM stdin;\n1\tit's\tZq9wSecret\n\\.\n\n",
                "SELECT 1;\n",
            ),
        ),
        (
            "COPY ... FROM stdin; without terminator (pg)",
            PG,
            Shape::rep("", "COPY t (a) FROM stdin;\n1\n", ""),
        ),
        // Round 14: script bodies whose strings run SQL, and JS regexes.
        (
            "plpython body of plpy.execute strings (pg)",
            PG,
            Shape::rep(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\n",
                "plpy.execute(\"UPDATE t SET a = 'it''s' WHERE b = 1\")  # note\n",
                "$$",
            ),
        ),
        (
            "plv8 body of regex literals (pg)",
            PG,
            Shape::rep(
                "CREATE FUNCTION f(s text) RETURNS text LANGUAGE plv8 AS $$\n",
                "s = s.replace(/'/g, ''); // c\n",
                "return s;\n$$",
            ),
        ),
        (
            "CONCAT( nested unclosed (my)",
            MY,
            Shape::rep("SELECT ", "CONCAT(", "'x'"),
        ),
        (
            "CONCAT( nested closed (pg)",
            PG,
            Shape::fill("SELECT ", "CONCAT(", "'x'", ")", ""),
        ),
        (
            "OPTIONS ( unclosed repeated (pg)",
            PG,
            Shape::rep("CREATE SERVER s FOREIGN DATA WRAPPER w ", "OPTIONS (", ""),
        ),
        (
            "set_config( repeated (pg)",
            PG,
            Shape::rep("SELECT ", "set_config(", ""),
        ),
        (
            "pw = pw = pw ... (pg)",
            PG,
            Shape::rep("SELECT * FROM t WHERE ", "pw = ", "1"),
        ),
        (
            "password = password = ... (my)",
            MY,
            Shape::rep("UPDATE t SET ", "password = ", "1"),
        ),
        (
            "BEGIN INSERT INTO t (pw) VALUES ('a'), (my)",
            MY,
            Shape::rep(
                "CREATE PROCEDURE p() ",
                "BEGIN INSERT INTO t (pw) VALUES ('a'), ",
                "",
            ),
        ),
        (
            "THEN ALTER USER u, (my)",
            MY,
            Shape::rep("CREATE PROCEDURE p() ", "THEN ALTER USER u, ", ""),
        ),
        (
            "ELSE CREATE ROLE r WITH (pg)",
            PG,
            Shape::rep("", "ELSE CREATE ROLE r WITH ", ""),
        ),
        (
            "INSERT INTO t (pw) SELECT ... THEN (pg)",
            PG,
            Shape::rep("", "THEN INSERT INTO t (pw) SELECT ", "1"),
        ),
        (
            "LOOP SET PASSWORD FOR (my)",
            MY,
            Shape::rep("", "LOOP SET PASSWORD FOR ", ""),
        ),
        (
            "password=\\\\\\\" repeated (pg)",
            PG,
            Shape::rep("SELECT '", "password=\\\\\\\" ", "'"),
        ),
        (
            "backslash runs (my)",
            MY,
            Shape::rep("SELECT '", "\\\\\\\\\\\\\\\\x", "'"),
        ),
        (
            "odd backslash + quote runs (my)",
            MY,
            Shape::rep("SELECT '", "\\'", ""),
        ),
        (
            "password=\" unterminated repeated (my)",
            MY,
            Shape::rep("SELECT '", "password=\\\"", "'"),
        ),
        (
            "password: x9 yaml repeated (pg)",
            PG,
            Shape::rep("SELECT '", "password: x9\\n", "'"),
        ),
        (
            "Authorization: repeated (pg)",
            PG,
            Shape::rep("SELECT '", "Authorization: ", "'"),
        ),
        (
            "Authorization: Bearer repeated (pg)",
            PG,
            Shape::rep("SELECT '", "Authorization: Bearer ", "'"),
        ),
        (
            "Cookie: a= repeated (pg)",
            PG,
            Shape::rep("SELECT '", "Cookie: a=", "'"),
        ),
        (
            "postgres:// no @ repeated (pg)",
            PG,
            Shape::rep("SELECT '", "postgres://a:", "'"),
        ),
        (
            "postgres://u:p repeated no host end (pg)",
            PG,
            Shape::rep("SELECT '", "postgres://u:p", "'"),
        ),
        (
            "url ?password= repeated (pg)",
            PG,
            Shape::rep("SELECT 'http://h/?", "password=x&", "'"),
        ),
        (
            "conninfo host= repeated (pg)",
            PG,
            Shape::rep("SELECT dblink_connect('", "host=h password ", "')"),
        ),
        (
            "key%3D repeated (pg)",
            PG,
            Shape::rep("SELECT '", "password%3D", "'"),
        ),
        (
            "quoted key \"password\": repeated (pg)",
            PG,
            Shape::rep("SELECT '", "\"password\": ", "'"),
        ),
        (
            "''password'' = '' repeated (pg)",
            PG,
            Shape::rep("SELECT '", "password = ''", "'"),
        ),
        (
            "jsonb_build_object('password', repeated (pg)",
            PG,
            Shape::rep("SELECT jsonb_build_object(", "'password', ", "1)"),
        ),
        (
            "->>'password' = repeated (pg)",
            PG,
            Shape::rep("SELECT * FROM t WHERE ", "c->>'password' = ", "'x'"),
        ),
        (
            "routine body mentions password, many literals (pg)",
            PG,
            Shape::rep(
                "CREATE FUNCTION f() RETURNS void AS $$ BEGIN PERFORM 1; ALTER ROLE r PASSWORD 'x'; ",
                "PERFORM 'a'; ",
                "END $$ LANGUAGE plpgsql",
            ),
        ),
        (
            "many PREPARE FROM 'ALTER USER' (my)",
            MY,
            Shape::rep(
                "",
                "PREPARE s FROM 'ALTER USER u IDENTIFIED BY ''x'''; ",
                "",
            ),
        ),
        (
            "many CREATE USER statements (my)",
            MY,
            Shape::rep("", "CREATE USER u IDENTIFIED BY 'x'; ", ""),
        ),
        (
            "CREATE USER a, b, c ... (pg)",
            PG,
            Shape::rep("CREATE ROLE ", "a, ", "b"),
        ),
        (
            "ALTER USER u IDENTIFIED BY IDENTIFIED BY ... (my)",
            MY,
            Shape::rep("ALTER USER u ", "IDENTIFIED BY ", "'x'"),
        ),
        (
            "multibyte filler (pg)",
            PG,
            Shape::rep("SELECT '", "\u{1F600}password=\u{00E9}", "'"),
        ),
        ("redis AUTH AUTH ... ", RD, Shape::rep("", "AUTH ", "")),
        (
            "redis ACL SETUSER >p ...",
            RD,
            Shape::rep("ACL SETUSER u ", ">p1 ", ""),
        ),
        (
            "redis CONFIG SET masterauth x ...",
            RD,
            Shape::rep("CONFIG SET ", "masterauth x ", ""),
        ),
        (
            "redis MIGRATE AUTH ...",
            RD,
            Shape::rep("MIGRATE h 1 \"\" 0 5 ", "AUTH2 u p ", ""),
        ),
        ("redis many lines AUTH", RD, Shape::rep("", "AUTH x\n", "")),
        (
            "redis many quotes",
            RD,
            Shape::rep("SET k ", "\"a\\\"b\" ", ""),
        ),
        // Round 18: one statement split by many psql meta-command lines,
        // read again as one statement at full (with a break at its end),
        // and an INSERT with a meta-command line between every row.
        (
            "select list split by meta lines, break at the end",
            PG,
            Shape::rep("SELECT a,\n", "\\echo x\nb,", "\nc FROM t WHERE x = 'oops"),
        ),
        (
            "insert rows split by meta lines",
            PG,
            Shape::rep(
                "INSERT INTO users (name, password) VALUES ('a', 'pw0')",
                ",\n\\echo n\n('b', 'pw1')",
                ";\n",
            ),
        ),
        (
            "redis unterminated quotes",
            RD,
            Shape::rep("SET k ", "\"", ""),
        ),
        (
            "redis URL arguments",
            RD,
            Shape::rep("SET k ", "redis://:p@h ", ""),
        ),
        (
            "redis RESP many args",
            RD,
            Shape::rep("*999999\r\n$3\r\nSET\r\n", "$1\r\nk\r\n", ""),
        ),
    ]
}

/// Words and constructs that start a pass, each repeated on its own.
const TRIGGERS: &[&str] = &[
    "PASSWORD ",
    "PASSWORD FOR ",
    "IDENTIFIED BY ",
    "BY ",
    "SET ",
    "SET a = ",
    "SET (",
    "BEGIN ",
    "THEN ",
    "ELSE ",
    "LOOP ",
    "REPEAT ",
    "CONCAT(",
    "format(",
    "OPTIONS (",
    "AES_ENCRYPT(",
    "HMAC(",
    "pgp_pub_decrypt(",
    "crypt(",
    "current_setting(",
    "INSERT INTO t (pw) VALUES (",
    "INSERT INTO t (pw) SELECT ",
    "COPY ",
    "PROGRAM ",
    "init_connect = ",
    "'ALTER ROLE r PASSWORD ' || ",
    "pw = ",
    "BINARY ",
    "(",
    "[",
    "ALTER ROLE r ",
    "CREATE USER u ",
    "SET PASSWORD ",
    "DO $$ ",
    "set_config(",
    "jsonb_build_object('password', ",
    "'api_key' = ",
    "-- PASSWORD\n",
    "/* HMAC( ",
    "/* SET PASSWORD FOR ",
    "-- mysql -p",
    "Authorization: ",
    "Bearer ",
    "password: ",
];

/// The best of two runs, against noise from other processes, and the
/// text stored.
fn timed(text: &str, dialect: SqlDialect, level: TextLevel) -> (Duration, String) {
    let mut best = Duration::MAX;
    let mut stored = String::new();
    for _ in 0..2 {
        let start = Instant::now();
        stored = prepare_statement_text(text, dialect, level).unwrap_or_default();
        best = best.min(start.elapsed());
    }
    (best, stored)
}

/// Shapes whose text is not valid SQL by construction (an unterminated
/// literal): the redactor fails closed and hides them whole at `full`.
const BROKEN_QUOTING: &[&str] = &["odd backslash + quote runs (my)"];

/// Checks that `shape` grows about linearly at both levels and that it is
/// redacted rather than hidden: a pass that ran out of budget would look
/// linear while it hides the whole text. A shape with broken quoting must
/// be hidden instead. (Input built to exhaust the budget or the nesting
/// limit is in `adversarial_nesting.rs`.)
fn check(name: &str, dialect: SqlDialect, shape: &Shape, failures: &mut Vec<String>) {
    let broken = BROKEN_QUOTING.contains(&name);
    let release = !cfg!(debug_assertions);
    let sizes: &[usize] = if release {
        &[64 << 10, 256 << 10, 1 << 20]
    } else {
        &[8 << 10, 32 << 10]
    };
    for level in [TextLevel::Normalized, TextLevel::Full] {
        // Wall time is noisy when other work shares the machine: a timing
        // that misses its bound is measured again, up to ATTEMPTS times,
        // and only a shape that misses it every time fails. Quadratic work
        // misses it every time.
        let mut last_failure = None;
        for _ in 0..ATTEMPTS {
            let mut times = Vec::new();
            for &size in sizes {
                let (time, stored) = timed(&shape.build(size), dialect, level);
                times.push(time);
                if level == TextLevel::Full && (stored == REDACTED) != broken {
                    failures.push(format!(
                        "{name}: {size} B at full, hidden whole: {}",
                        stored == REDACTED
                    ));
                }
            }
            last_failure = timing_failure(name, level, release, &times);
            if last_failure.is_none() {
                break;
            }
        }
        failures.extend(last_failure);
    }
}

/// How often a shape is timed before a missed bound counts.
const ATTEMPTS: usize = 3;

/// The bound a run of `times` (one per size, each four times the last)
/// misses, if any: four times the input may take at most eight times as
/// long (quadratic work would take sixteen; times under 2 ms are noise),
/// and in release 1 MiB at most 3 s.
fn timing_failure(
    name: &str,
    level: TextLevel,
    release: bool,
    times: &[Duration],
) -> Option<String> {
    let floor = Duration::from_millis(2);
    if times
        .windows(2)
        .any(|pair| pair[1] > pair[0].max(floor) * 8)
    {
        return Some(format!("{name} at {level}: {times:?}"));
    }
    if release && times.last().is_some_and(|t| *t > Duration::from_secs(3)) {
        return Some(format!("{name} at {level}: 1 MiB took {times:?}"));
    }
    None
}

#[test]
fn every_pass_is_linear() {
    let mut failures = Vec::new();
    for (name, dialect, shape) in audit_shapes() {
        check(name, dialect, &shape, &mut failures);
    }
    for trigger in TRIGGERS {
        for dialect in [PG, MY] {
            let shape = Shape::rep("SELECT 1; ", trigger, "");
            check(trigger, dialect, &shape, &mut failures);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
