//! Regression tests from adversarial audits of the redactor (75, 94,
//! 196, 320, 248, 213, from the eighth 57 leak and 38 ordinary, from the
//! ninth 147 leak and 20 ordinary inputs), each recorded at every text level as an executed or failed query, with
//! the server error message the engine really returns. No listed secret may
//! appear in the stored statement or error message.

use std::sync::Arc;

use varve_activity::{
    ActivityEvent, ErrorDetails, Recorder, RecorderSettings, SqlDialect, TextLevel,
    prepare_statement_text, redact_secrets,
};
use varve_store::{Store, core_migrations};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Case {
    tag: &'static str,
    d: SqlDialect,
    stmt: String,
    code: Option<&'static str>,
    msg: &'static str,
    secrets: &'static [&'static str],
    /// The "secret" is an ordinary literal with nothing marking it as a
    /// secret. Text level `full` stores statement text as written (design
    /// doc 9.3), so it is checked at `none` and `normalized` only.
    plain_data: bool,
    /// The secret sits where the statement names something (a select list
    /// column, an alias, a key name): the user wrote it as a name, and
    /// names are kept at every level.
    name_position: bool,
}

const PG: SqlDialect = SqlDialect::Postgres;
const MY: SqlDialect = SqlDialect::Mysql;
const RD: SqlDialect = SqlDialect::Redis;

fn c(
    tag: &'static str,
    d: SqlDialect,
    stmt: &str,
    code: Option<&'static str>,
    msg: &'static str,
    secrets: &'static [&'static str],
) -> Case {
    Case {
        tag,
        d,
        stmt: stmt.to_string(),
        code,
        msg,
        secrets,
        plain_data: false,
        name_position: false,
    }
}

fn plain_at_full(
    tag: &'static str,
    d: SqlDialect,
    stmt: &str,
    code: Option<&'static str>,
    msg: &'static str,
    secrets: &'static [&'static str],
) -> Case {
    Case {
        plain_data: true,
        ..c(tag, d, stmt, code, msg, secrets)
    }
}

/// The audit's 75 inputs.
fn audit_cases() -> Vec<Case> {
    vec![
        // ---------------- PostgreSQL ----------------
        c(
            "PG01 quoted",
            PG,
            "ALTER ROLE alice WITH PASSWORD 'hunter2xyz'",
            Some("42704"),
            "role \"alice\" does not exist",
            &["hunter2xyz"],
        ),
        c(
            "PG02 bare",
            PG,
            "ALTER ROLE alice PASSWORD hunter2xyz",
            Some("42601"),
            "syntax error at or near \"hunter2xyz\"",
            &["hunter2xyz"],
        ),
        c(
            "PG03 bare @",
            PG,
            "ALTER ROLE alice PASSWORD p@ssw0rdXY",
            Some("42601"),
            "syntax error at or near \"p\"",
            &["ssw0rdXY"],
        ),
        c(
            "PG04 bare dash",
            PG,
            "CREATE ROLE alice LOGIN PASSWORD Sup3r-Secr3tXY",
            Some("42601"),
            "syntax error at or near \"Sup3r\"",
            &["Secr3tXY"],
        ),
        c(
            "PG05 bare digit-led",
            PG,
            "ALTER ROLE alice PASSWORD 123hunterXY",
            Some("42601"),
            "trailing junk after numeric literal at or near \"123h\"",
            &["hunterXY"],
        ),
        c(
            "PG06 typo PASSWROD quoted",
            PG,
            "ALTER ROLE alice PASSWROD 'hunter2xyz'",
            Some("42601"),
            "syntax error at or near \"PASSWROD\"",
            &["hunter2xyz"],
        ),
        c(
            "PG07 typo PASSWROD bare",
            PG,
            "ALTER ROLE alice PASSWROD hunter2xyz",
            Some("42601"),
            "syntax error at or near \"PASSWROD\"",
            &["hunter2xyz"],
        ),
        c(
            "PG08 CREATE USER WITH ENCRYPTED",
            PG,
            "CREATE USER bob WITH ENCRYPTED PASSWORD 'hunter2xyz' VALID UNTIL 'infinity'",
            Some("42710"),
            "role \"bob\" already exists",
            &["hunter2xyz"],
        ),
        c(
            "PG09 DO format %L",
            PG,
            "DO $$ BEGIN EXECUTE format('ALTER ROLE %I PASSWORD %L', 'a', 'hunter2xyz'); END $$",
            Some("42704"),
            "role \"a\" does not exist",
            &["hunter2xyz"],
        ),
        c(
            "PG10 dblink conninfo",
            PG,
            "SELECT dblink_connect('host=db user=u password=hunter2xyz dbname=x')",
            Some("08001"),
            "could not establish connection",
            &["hunter2xyz"],
        ),
        c(
            "PG11 conninfo missing =",
            PG,
            "CREATE SUBSCRIPTION s CONNECTION 'host=db password hunter2xyz' PUBLICATION p",
            Some("42601"),
            "invalid connection string syntax: missing \"=\" after \"hunter2xyz\" in connection info string",
            &["hunter2xyz"],
        ),
        c(
            "PG12 user mapping",
            PG,
            "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'bob', password 'hunter2xyz')",
            Some("42704"),
            "server \"s\" does not exist",
            &["hunter2xyz"],
        ),
        c(
            "PG13 url in COPY",
            PG,
            "COPY t FROM PROGRAM 'psql postgres://u:hunter2xyz@h/db -c x'",
            Some("38000"),
            "program \"psql postgres://u:hunter2xyz@h/db -c x\" failed",
            &["hunter2xyz"],
        ),
        c(
            "PG14 primary_conninfo",
            PG,
            "ALTER SYSTEM SET primary_conninfo = 'host=h user=rep password=hunter2xyz'",
            Some("XX000"),
            "could not open",
            &["hunter2xyz"],
        ),
        plain_at_full(
            "PG15 type cast echo",
            PG,
            "SELECT 'hunter2xyz'::int",
            Some("22P02"),
            "invalid input syntax for type integer: \"hunter2xyz\"",
            &["hunter2xyz"],
        ),
        c(
            "PG16 multi stmt",
            PG,
            "CREATE USER alice; ALTER USER alice PASSWORD hunter2xyz",
            Some("42601"),
            "syntax error at or near \"hunter2xyz\"",
            &["hunter2xyz"],
        ),
        c(
            "PG17 IF EXISTS",
            PG,
            "ALTER USER IF EXISTS alice WITH LOGIN PASSWORD hunter2xyz",
            Some("42601"),
            "syntax error at or near \"IF\"",
            &["hunter2xyz"],
        ),
        c(
            "PG18 role named valid kw pw",
            PG,
            "ALTER ROLE alice PASSWORD hunter2xyz VALID UNTIL 'infinity'",
            Some("42601"),
            "syntax error at or near \"hunter2xyz\"",
            &["hunter2xyz"],
        ),
        c(
            "PG19 pw with unicode",
            PG,
            "ALTER ROLE alice PASSWORD wachtwoördXY",
            Some("42601"),
            "syntax error at or near \"wachtwoördXY\"",
            &["wachtwoördXY"],
        ),
        c(
            "PG20 code missing",
            PG,
            "ALTER ROLE alice PASSWORD hunter2xyz",
            None,
            "syntax error at or near \"hunter2xyz\"",
            &["hunter2xyz"],
        ),
        c(
            "PG21 conninfo pw quoted spaces",
            PG,
            "SELECT dblink_connect('host=db password=''hun ter2xyz'' dbname=x')",
            Some("08001"),
            "could not establish connection",
            &["ter2xyz"],
        ),
        // ---------------- MySQL ----------------
        c(
            "MY01 IDENTIFIED BY",
            MY,
            "CREATE USER 'u'@'%' IDENTIFIED BY 'hunter2xyz'",
            Some("1396 (HY000)"),
            "Operation CREATE USER failed for 'u'@'%'",
            &["hunter2xyz"],
        ),
        c(
            "MY02 IDENTIFIED WITH .. BY",
            MY,
            "ALTER USER u IDENTIFIED WITH caching_sha2_password BY 'hunter2xyz' RETAIN CURRENT PASSWORD",
            Some("1396 (HY000)"),
            "Operation ALTER USER failed for 'u'@'%'",
            &["hunter2xyz"],
        ),
        c(
            "MY03 SET PASSWORD FOR",
            MY,
            "SET PASSWORD FOR 'u'@'%' = 'hunter2xyz'",
            Some("1133 (28000)"),
            "Can't find any matching row in the user table",
            &["hunter2xyz"],
        ),
        c(
            "MY04 IDENTIFED typo",
            MY,
            "CREATE USER u IDENTIFED BY 'hunter2xyz'",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'IDENTIFED BY 'hunter2xyz'' at line 1",
            &["hunter2xyz"],
        ),
        c(
            "MY05 bare special chars",
            MY,
            "CREATE USER u IDENTIFIED BY p@ss!w0rdXY",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near '@ss!w0rdXY' at line 1",
            &["ss!w0rdXY", "w0rdXY"],
        ),
        c(
            "MY06 BYE typo",
            MY,
            "ALTER USER u IDENTIFIED BYE 'hunter2xyz'",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'BYE 'hunter2xyz'' at line 1",
            &["hunter2xyz"],
        ),
        c(
            "MY07 PASWORD typo",
            MY,
            "SET PASWORD = 'hunter2xyz'",
            Some("1193 (HY000)"),
            "Unknown system variable 'PASWORD'",
            &["hunter2xyz"],
        ),
        c(
            "MY08 replication",
            MY,
            "CHANGE REPLICATION SOURCE TO SOURCE_HOST='h', SOURCE_USER='rep', SOURCE_PASSWORD='hunter2xyz'",
            Some("3021 (HY000)"),
            "This operation cannot be performed with a running replica io thread",
            &["hunter2xyz"],
        ),
        c(
            "MY09 START REPLICA",
            MY,
            "START REPLICA USER='rep' PASSWORD='hunter2xyz'",
            Some("1200 (HY000)"),
            "x",
            &["hunter2xyz"],
        ),
        c(
            "MY10 federated server",
            MY,
            "CREATE SERVER s FOREIGN DATA WRAPPER mysql OPTIONS (HOST 'h', USER 'u', PASSWORD 'hunter2xyz')",
            Some("1476 (HY000)"),
            "x",
            &["hunter2xyz"],
        ),
        c(
            "MY11 bare unquoted 1064 echo",
            MY,
            "ALTER USER u IDENTIFIED BY hunter2xyz",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'hunter2xyz' at line 1",
            &["hunter2xyz"],
        ),
        c(
            "MY12 USR typo echo",
            MY,
            "ALTER USR u IDENTIFIED BY 'hunter2xyz'",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'USR u IDENTIFIED BY 'hunter2xyz'' at line 1",
            &["hunter2xyz"],
        ),
        c(
            "MY13 url in value",
            MY,
            "INSERT INTO cfg VALUES ('mysql://root:hunter2xyz@db/x')",
            Some("1146 (42S02)"),
            "Table 'app.cfg' doesn't exist",
            &["hunter2xyz"],
        ),
        c(
            "MY14 hash comment pw",
            MY,
            "ALTER USER u IDENTIFIED BY abc#hunter2xyz",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near '' at line 1",
            &["hunter2xyz"],
        ),
        c(
            "MY15 dup entry echo",
            MY,
            "INSERT INTO users(email, pw) VALUES ('a@x', 'hunter2xyz')",
            Some("1062 (23000)"),
            "Duplicate entry 'hunter2xyz' for key 'users.pw'",
            &["hunter2xyz"],
        ),
        // ---------------- MariaDB ----------------
        c(
            "MA01 ed25519 USING PASSWORD()",
            MY,
            "CREATE USER u IDENTIFIED VIA ed25519 USING PASSWORD('hunter2xyz')",
            Some("1396 (HY000)"),
            "Operation CREATE USER failed for 'u'@'%'",
            &["hunter2xyz"],
        ),
        c(
            "MA02 VAI typo",
            MY,
            "CREATE USER u IDENTIFIED VAI ed25519 USING PASSWORD('hunter2xyz')",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MariaDB server version for the right syntax to use near 'VAI ed25519 USING PASSWORD('hunter2xyz')' at line 1",
            &["hunter2xyz"],
        ),
        c(
            "MA03 SET PASSWORD = PASSWORD()",
            MY,
            "SET PASSWORD FOR u@localhost = PASSWORD('hunter2xyz')",
            Some("1133 (28000)"),
            "Can't find any matching row in the user table",
            &["hunter2xyz"],
        ),
        c(
            "MA04 GRANT IDENTIFIED BY",
            MY,
            "GRANT ALL ON db.* TO u@'%' IDENTIFIED BY 'hunter2xyz'",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MariaDB server version for the right syntax to use near 'IDENTIFIED BY 'hunter2xyz'' at line 1",
            &["hunter2xyz"],
        ),
        c(
            "MA05 CHANGE MASTER",
            MY,
            "CHANGE MASTER TO MASTER_HOST='h', MASTER_PASSWORD='hunter2xyz'",
            Some("1198 (HY000)"),
            "x",
            &["hunter2xyz"],
        ),
        c(
            "MA06 bare pw with dash",
            MY,
            "ALTER USER u IDENTIFIED BY Sup3r-Secr3tXY",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MariaDB server version for the right syntax to use near '-Secr3tXY' at line 1",
            &["Secr3tXY"],
        ),
        // ---------------- Redis ----------------
        c(
            "RD01 AUTH",
            RD,
            "AUTH hunter2xyz",
            Some("WRONGPASS"),
            "WRONGPASS invalid username-password pair or user is disabled.",
            &["hunter2xyz"],
        ),
        c(
            "RD02 AUHT typo",
            RD,
            "AUHT hunter2xyz",
            Some("ERR"),
            "ERR unknown command 'AUHT', with args beginning with: 'hunter2xyz' ",
            &["hunter2xyz"],
        ),
        c(
            "RD03 HELLO AUTH",
            RD,
            "HELLO 3 AUTH default hunter2xyz SETNAME c",
            Some("WRONGPASS"),
            "WRONGPASS invalid username-password pair",
            &["hunter2xyz"],
        ),
        c(
            "RD04 HELLO SETNAME auth",
            RD,
            "HELLO 3 SETNAME auth AUTH default hunter2xyz",
            Some("WRONGPASS"),
            "WRONGPASS x",
            &["hunter2xyz"],
        ),
        c(
            "RD05 MIGRATE AUTH",
            RD,
            "MIGRATE 10.0.0.2 6379 k 0 5000 AUTH hunter2xyz",
            Some("IOERR"),
            "IOERR error or timeout",
            &["hunter2xyz"],
        ),
        c(
            "RD06 MIGRATE missing timeout",
            RD,
            "MIGRATE 10.0.0.2 6379 k 0 AUTH hunter2xyz",
            Some("ERR"),
            "ERR value is not an integer or out of range",
            &["hunter2xyz"],
        ),
        c(
            "RD07 MIGRATE AUTH2",
            RD,
            "MIGRATE h 6379 \"\" 0 5000 AUTH2 rep hunter2xyz KEYS a b",
            Some("IOERR"),
            "IOERR x",
            &["hunter2xyz"],
        ),
        c(
            "RD08 CONFIG SET requirepass",
            RD,
            "CONFIG SET requirepass hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD09 CONFIG SET masterauth",
            RD,
            "config set masterauth hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD10 CONFIG SETT typo",
            RD,
            "CONFIG SETT requirepass hunter2xyz",
            Some("ERR"),
            "ERR unknown subcommand 'SETT'. Try CONFIG HELP.",
            &["hunter2xyz"],
        ),
        c(
            "RD11 ACL >pw",
            RD,
            "ACL SETUSER alice on >hunter2xyz ~* +@all",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD12 ACL #hash",
            RD,
            "ACL SETUSER alice on #5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8",
            None,
            "",
            &["5e884898da28"],
        ),
        c(
            "RD13 ACL no username",
            RD,
            "ACL SETUSER >hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD14 ACL missing >",
            RD,
            "ACL SETUSER alice on hunter2xyz",
            Some("ERR"),
            "ERR Error in ACL SETUSER modifier 'hunter2xyz': Syntax error",
            &["hunter2xyz"],
        ),
        c(
            "RD15 ACL > space",
            RD,
            "ACL SETUSER alice on > hunter2xyz",
            Some("ERR"),
            "ERR Error in ACL SETUSER modifier 'hunter2xyz': Syntax error",
            &["hunter2xyz"],
        ),
        c(
            "RD16 CLIENT SETINFO url",
            RD,
            "CLIENT SETINFO LIB-NAME redis://:hunter2xyz@cache:6379",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD17 conf paste",
            RD,
            "requirepass hunter2xyz",
            Some("ERR"),
            "ERR unknown command 'requirepass', with args beginning with: 'hunter2xyz' ",
            &["hunter2xyz"],
        ),
        c(
            "RD18 comment conf",
            RD,
            "# requirepass hunter2xyz\nPING",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD19 AUTH NUL",
            RD,
            "AUTH\0default\0hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD20 AUTH2 top",
            RD,
            "AUTH2 default hunter2xyz",
            Some("ERR"),
            "ERR unknown command 'AUTH2', with args beginning with: 'default' 'hunter2xyz' ",
            &["hunter2xyz"],
        ),
        c(
            "RD21 SENTINEL auth-pass",
            RD,
            "SENTINEL SET mymaster auth-pass hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD22 url in SET",
            RD,
            "SET cfg \"redis://default:hunter2xyz@cache:6379\"",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD23 EVAL AUTH",
            RD,
            "EVAL \"return redis.call('AUTH','hunter2xyz')\" 0",
            Some("ERR"),
            "ERR This Redis command is not allowed from script script: 1, on @user_script:1.",
            &["hunter2xyz"],
        ),
        c(
            "RD24 old-style unknown cmd",
            RD,
            "AUHT hunter2xyz",
            Some("ERR"),
            "ERR unknown command `AUHT`, with args beginning with: `hunter2xyz`, ",
            &["hunter2xyz"],
        ),
        c(
            "RD25 CONFIG SET wrong arity",
            RD,
            "CONFIG SET requirepass",
            Some("ERR"),
            "ERR wrong number of arguments for 'config|set' command",
            &[],
        ),
        c(
            "RD26 HELLO AUTH no proto",
            RD,
            "HELLO AUTH default hunter2xyz",
            Some("NOPROTO"),
            "NOPROTO unsupported protocol version",
            &["hunter2xyz"],
        ),
        c(
            "RD27 auth multi-line",
            RD,
            "MULTI\nAUTH hunter2xyz\nEXEC",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD28 ACL SETUSER via CLIENT? reset pw",
            RD,
            "ACL SETUSER alice resetpass >hunter2xyz <old0pw1x",
            None,
            "",
            &["hunter2xyz", "old0pw1x"],
        ),
        c(
            "RD29 CONFIG SET tls pass",
            RD,
            "CONFIG SET tls-key-file-pass hunter2xyz tls-client-key-file-pass other9pw",
            None,
            "",
            &["hunter2xyz", "other9pw"],
        ),
        c(
            "RD30 CONFIG SET mixed",
            RD,
            "CONFIG SET maxmemory 1gb masterauth hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD31 unknown subcmd MODULE",
            RD,
            "MODULE LOADEX /x.so ARGS password hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "RD32 ACL quoted >",
            RD,
            "ACL SETUSER alice on \">hunter 2xyz\"",
            None,
            "",
            &["2xyz", "hunter"],
        ),
        c(
            "RD33 inline # not comment",
            RD,
            "AUTH default hunter2xyz # login",
            Some("ERR"),
            "ERR syntax error",
            &["hunter2xyz"],
        ),
        // ---------------- Further patterns from the audit report --------
        c(
            "X01 MySQL bare after BY with #",
            MY,
            "ALTER USER u IDENTIFIED BY abc#hunter2xyz\nSELECT 1",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'SELECT 1' at line 2",
            &["hunter2xyz", "abc#"],
        ),
        c(
            "X02 SET PASWORD FOR",
            MY,
            "SET PASWORD FOR 'u'@'%' = 'hunter2xyz'",
            Some("1193 (HY000)"),
            "Unknown system variable 'PASWORD'",
            &["hunter2xyz"],
        ),
        c(
            "X03 ALTR typo verb",
            MY,
            "ALTR USER u IDENTIFIED BY 'hunter2xyz'",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'ALTR USER u IDENTIFIED BY 'hunter2xyz'' at line 1",
            &["hunter2xyz"],
        ),
        c(
            "X04 PG function body quoted",
            PG,
            "CREATE FUNCTION f() RETURNS void LANGUAGE plpgsql AS 'BEGIN EXECUTE ''ALTER ROLE a PASSWORD hunter2xyz''; END'",
            Some("42P13"),
            "x",
            &["hunter2xyz"],
        ),
        c(
            "X05 PG function body dollar",
            PG,
            "CREATE OR REPLACE FUNCTION f() RETURNS void AS $f$ BEGIN PERFORM set_pw('hunter2xyz'); -- password\nEND $f$ LANGUAGE plpgsql",
            Some("42883"),
            "function set_pw(unknown) does not exist",
            &["hunter2xyz"],
        ),
        c(
            "X06 UPDATE pw column",
            MY,
            "UPDATE users SET pw = 'hunter2xyz' WHERE id = 1",
            Some("1054 (42S22)"),
            "Unknown column 'pw' in 'field list'",
            &["hunter2xyz"],
        ),
        c(
            "X07 ACL SETUSR typo",
            RD,
            "ACL SETUSR alice on >hunter2xyz",
            Some("ERR"),
            "ERR unknown subcommand 'SETUSR'. Try ACL HELP.",
            &["hunter2xyz"],
        ),
        c(
            "X08 MIGRTE typo",
            RD,
            "MIGRTE h 6379 k 0 5000 AUTH hunter2xyz",
            Some("ERR"),
            "ERR unknown command 'MIGRTE', with args beginning with: 'h' '6379' 'k' '0' '5000' 'AUTH' 'hunter2xyz' ",
            &["hunter2xyz"],
        ),
        c(
            "X09 MODULE LOAD args",
            RD,
            "MODULE LOAD /x.so password hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "X10 MODULE LOADEX CONFIG",
            RD,
            "MODULE LOADEX /x.so CONFIG auth-secret hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "X11 FUNCTION LOAD",
            RD,
            "FUNCTION LOAD REPLACE \"#!lua name=l\\nredis.register_function('f', function() return redis.call('AUTH', 'hunter2xyz') end)\"",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "X12 EVAL ARGV",
            RD,
            "EVAL \"return redis.call('AUTH', ARGV[1])\" 0 hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "X13 masterauth comment",
            RD,
            "# masterauth hunter2xyz\nPING",
            None,
            "",
            &["hunter2xyz"],
        ),
        c(
            "X14 HELLO AUTH pw SETNAME",
            RD,
            "HELLO 3 AUTH hunter2xyz SETNAME c",
            Some("WRONGPASS"),
            "WRONGPASS x",
            &["hunter2xyz"],
        ),
        c(
            "X15 ACL selector + unknown",
            RD,
            "ACL SETUSER alice on \"(~k* +get)\" hunter2xyz",
            Some("ERR"),
            "ERR Error in ACL SETUSER modifier 'hunter2xyz': Syntax error",
            &["hunter2xyz"],
        ),
        c(
            "X16 HSET password field",
            RD,
            "HSET user:1 password hunter2xyz",
            None,
            "",
            &["hunter2xyz"],
        ),
    ]
}

/// The second audit's 94 inputs. None of them is plain data.
fn second_audit_cases() -> Vec<Case> {
    vec![
        // ---------------- PostgreSQL ----------------
        c(
            "P01 unicode bare",
            PG,
            "ALTER ROLE alice PASSWORD ünïcødeSecr3t",
            Some("42601"),
            "syntax error at or near \"ünïcødeSecr3t\"",
            &["ünïcødeSecr3t", "Secr3t"],
        ),
        c(
            "P02 emoji quoted",
            PG,
            "ALTER ROLE alice PASSWORD 'p🔑ss-Zq9w'",
            Some("42704"),
            "role \"alice\" does not exist",
            &["Zq9w", "🔑"],
        ),
        c(
            "P03 pw contains LOGIN kw",
            PG,
            "ALTER ROLE alice PASSWORD login_Zq9wXy",
            Some("42601"),
            "syntax error at or near \"login_Zq9wXy\"",
            &["Zq9wXy"],
        ),
        c(
            "P04 pw equals VALID kw-ish",
            PG,
            "CREATE ROLE alice LOGIN PASSWORD validuntilZq9",
            Some("42601"),
            "syntax error at or near \"validuntilZq9\"",
            &["validuntilZq9"],
        ),
        c(
            "P05 quote inside bare run",
            PG,
            "ALTER ROLE alice PASSWORD ab'Zq9w'cd LOGIN",
            Some("42601"),
            "syntax error at or near \"ab\"",
            &["Zq9w", "cd LOGIN"],
        ),
        c(
            "P06 multi stmt mixed",
            PG,
            "SELECT 1; CREATE ROLE bob; ALTER ROLE bob PASSWORD Zq9wSecret; SELECT 2",
            Some("42601"),
            "syntax error at or near \"Zq9wSecret\"",
            &["Zq9wSecret"],
        ),
        c(
            "P07 comment between",
            PG,
            "ALTER ROLE alice PASSWORD /* new one */ Zq9wSecret",
            Some("42601"),
            "syntax error at or near \"Zq9wSecret\"",
            &["Zq9wSecret"],
        ),
        c(
            "P08 line comment between",
            PG,
            "ALTER ROLE alice PASSWORD -- new\n 'Zq9wSecret'",
            Some("42704"),
            "role \"alice\" does not exist",
            &["Zq9wSecret"],
        ),
        c(
            "P09 newline bare",
            PG,
            "ALTER ROLE alice\nPASSWORD\nZq9wSecret\nLOGIN",
            Some("42601"),
            "syntax error at or near \"Zq9wSecret\"",
            &["Zq9wSecret"],
        ),
        c(
            "P10 dollar quoted tag",
            PG,
            "ALTER ROLE alice PASSWORD $pw$Zq9w'Secret$pw$",
            Some("42704"),
            "role \"alice\" does not exist",
            &["Zq9w", "Secret"],
        ),
        c(
            "P11 dollar unicode tag",
            PG,
            "ALTER ROLE alice PASSWORD $ü$Zq9wSecret$ü$",
            Some("42704"),
            "x",
            &["Zq9wSecret"],
        ),
        c(
            "P12 dollar unterminated",
            PG,
            "ALTER ROLE alice PASSWORD $$Zq9wSecret",
            Some("42601"),
            "unterminated dollar-quoted string at or near \"$$Zq9wSecret\"",
            &["Zq9wSecret"],
        ),
        c(
            "P13 typo verb ALTR",
            PG,
            "ALTR ROLE alice PASSWORD Zq9wSecret",
            Some("42601"),
            "syntax error at or near \"ALTR\"",
            &["Zq9wSecret"],
        ),
        c(
            "P14 typo object ROEL",
            PG,
            "CREATE ROEL alice PASSWORD 'Zq9wSecret'",
            Some("42601"),
            "syntax error at or near \"ROEL\"",
            &["Zq9wSecret"],
        ),
        c(
            "P15 GROUP with pw",
            PG,
            "CREATE GROUP g WITH PASSWORD Zq9wSecret",
            Some("42601"),
            "syntax error at or near \"Zq9wSecret\"",
            &["Zq9wSecret"],
        ),
        c(
            "P16 role SET custom pw guc",
            PG,
            "ALTER ROLE alice SET app.password = 'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P17 SET guc TO",
            PG,
            "SET app.password TO 'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P18 ALTER DATABASE SET guc TO",
            PG,
            "ALTER DATABASE d SET app.password TO 'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P19 set_config",
            PG,
            "SELECT set_config('app.password', 'Zq9wSecret', false)",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P20 SET api_token TO",
            PG,
            "SET myapp.api_token TO 'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P21 DO quote_literal",
            PG,
            "DO $$ BEGIN EXECUTE 'ALTER ROLE a PASSWORD ' || quote_literal('Zq9wSecret'); END $$",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P22 DO E-string body",
            PG,
            "DO E'BEGIN EXECUTE ''ALTER ROLE a PASSWORD ''''Zq9wSecret''''''; END'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P23 function body dblink",
            PG,
            "CREATE FUNCTION f() RETURNS void LANGUAGE sql AS $f$ SELECT dblink_connect('host=h pass=Zq9wSecret') $f$",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P24 conninfo password no =",
            PG,
            "SELECT dblink_connect('dbname=x password  Zq9wSecret')",
            Some("08001"),
            "invalid connection string syntax: missing \"=\" after \"Zq9wSecret\" in connection info string",
            &["Zq9wSecret"],
        ),
        c(
            "P25 conninfo quoted value",
            PG,
            "SELECT dblink_connect('host=h password=''Zq9w Secret''')",
            Some("08001"),
            "x",
            &["Zq9w", "Secret"],
        ),
        c(
            "P26 PGPASSWORD in COPY",
            PG,
            "COPY t FROM PROGRAM 'PGPASSWORD=Zq9wSecret psql -h h -c x'",
            Some("38000"),
            "program \"PGPASSWORD=Zq9wSecret psql -h h -c x\" failed",
            &["Zq9wSecret"],
        ),
        c(
            "P27 INSERT secret col",
            PG,
            "INSERT INTO users (email, \"Password\") VALUES ('a@x', 'Zq9wSecret'), ('b@x', 'Zq9wOther')",
            Some("23505"),
            "duplicate key value violates unique constraint \"users_password_key\"",
            &["Zq9wSecret", "Zq9wOther"],
        ),
        c(
            "P28 INSERT unquoted value",
            PG,
            "INSERT INTO users (email, password) VALUES ('a@x', Zq9wSecret)",
            Some("42703"),
            "column \"zq9wsecret\" does not exist",
            &["Zq9wSecret", "zq9wsecret"],
        ),
        c(
            "P29 UPDATE row syntax",
            PG,
            "UPDATE users SET (email, password) = ('a@x', 'Zq9wSecret') WHERE id = 1",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P30 UPDATE unquoted lowercased echo",
            PG,
            "UPDATE users SET password = Zq9wSecret WHERE id = 1",
            Some("42703"),
            "column \"zq9wsecret\" does not exist",
            &["Zq9wSecret", "zq9wsecret"],
        ),
        c(
            "P31 INSERT SELECT secret col",
            PG,
            "INSERT INTO users (email, password) SELECT 'a@x', 'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P32 user mapping typo option",
            PG,
            "ALTER USER MAPPING FOR bob SERVER s OPTIONS (SET pasword 'Zq9wSecret')",
            Some("HV00D"),
            "invalid option \"pasword\"",
            &["Zq9wSecret"],
        ),
        c(
            "P33 two roles comma",
            PG,
            "CREATE ROLE a, b PASSWORD Zq9wSecret",
            Some("42601"),
            "syntax error at or near \",\"",
            &["Zq9wSecret"],
        ),
        c(
            "P34 RENAME then pw",
            PG,
            "ALTER ROLE alice RENAME TO bob; ALTER ROLE bob PASSWORD 'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "P35 trailing comment echo",
            PG,
            "ALTER ROLE alice PASSWORD 'x1y2z3w4' -- old was Zq9wSecret",
            None,
            "",
            &["x1y2z3w4"],
        ),
        c(
            "P36 U& escape pw",
            PG,
            "ALTER ROLE alice PASSWORD U&'Zq9w\\0053ecret'",
            None,
            "",
            &["Zq9w"],
        ),
        c(
            "P37 LINE context echo",
            PG,
            "ALTER ROLE alice PASSWORD Zq9wSecret",
            Some("42601"),
            "syntax error at or near \"Zq9wSecret\"\nLINE 1: ALTER ROLE alice PASSWORD Zq9wSecret\n                                  ^",
            &["Zq9wSecret"],
        ),
        // ---------------- MySQL / MariaDB ----------------
        c(
            "M01 hex literal pw",
            MY,
            "ALTER USER u IDENTIFIED BY 0x5a713977536563726574",
            Some("1064 (42000)"),
            "x",
            &["5a713977536563726574"],
        ),
        c(
            "M02 X'' literal pw",
            MY,
            "ALTER USER u IDENTIFIED BY X'5a713977536563726574'",
            None,
            "",
            &["5a713977536563726574"],
        ),
        c(
            "M03 b'' literal pw",
            MY,
            "SET PASSWORD = b'0101101001110001'",
            None,
            "",
            &["0101101001110001"],
        ),
        c(
            "M04 charset introducer spaced",
            MY,
            "GRANT ALL ON db.* TO u IDENTIFIED BY _utf8mb4 'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "M05 IDENTIFIED WITH plugin AS hash",
            MY,
            "CREATE USER u IDENTIFIED WITH caching_sha2_password AS '$A$005$Zq9wSecretHashXYZ'",
            None,
            "",
            &["Zq9wSecretHash"],
        ),
        c(
            "M06 RETAIN CURRENT",
            MY,
            "ALTER USER u IDENTIFIED BY 'Zq9wSecret' REPLACE 'Old0Secret' RETAIN CURRENT PASSWORD",
            None,
            "",
            &["Zq9wSecret", "Old0Secret"],
        ),
        c(
            "M07 MariaDB SET PASSWORD()",
            MY,
            "SET PASSWORD = PASSWORD('Zq9wSecret')",
            Some("1133 (28000)"),
            "Can't find any matching row in the user table",
            &["Zq9wSecret"],
        ),
        c(
            "M08 OR REPLACE typo",
            MY,
            "CREATE OR REPLACE USER u IDENTIFED BY 'Zq9wSecret'",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MariaDB server version for the right syntax to use near 'IDENTIFED BY 'Zq9wSecret'' at line 1",
            &["Zq9wSecret"],
        ),
        c(
            "M09 OR REPLACE bare",
            MY,
            "CREATE OR REPLACE USER u IDENTIFIED BY Zq9wSecret",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MariaDB server version for the right syntax to use near 'Zq9wSecret' at line 1",
            &["Zq9wSecret"],
        ),
        c(
            "M10 PREPARE FROM string",
            MY,
            "PREPARE s FROM 'ALTER USER u IDENTIFIED BY ''Zq9wSecret'''",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "M11 SET @sql CONCAT",
            MY,
            "SET @sql = CONCAT('ALTER USER u IDENTIFIED BY ''', 'Zq9wSecret', '''')",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "M12 SET @pw var",
            MY,
            "SET @pw = 'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "M13 proc body typo",
            MY,
            "CREATE PROCEDURE p() BEGIN ALTER USER u IDENTIFED BY 'Zq9wSecret'; END",
            Some("1064 (42000)"),
            "x",
            &["Zq9wSecret"],
        ),
        c(
            "M14 multi stmt mixed",
            MY,
            "SELECT 1; ALTER USER u IDENTIFIED BY Zq9wSecret; SELECT 2",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'Zq9wSecret; SELECT 2' at line 1",
            &["Zq9wSecret"],
        ),
        c(
            "M15 comment between BY and value",
            MY,
            "ALTER USER u IDENTIFIED BY /* pw */ 'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "M16 hash comment then newline value",
            MY,
            "ALTER USER u IDENTIFIED BY # pw follows\n'Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "M17 echo truncated 80",
            MY,
            "ALTER USER someone_with_a_long_name@'host.example.com' IDENTIFIED BYY 'Zq9wSecretLongValue'",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'BYY 'Zq9wSecretLo' at line 1",
            &["Zq9wSecretLo", "Zq9wSecret"],
        ),
        c(
            "M18 unicode pw bare",
            MY,
            "ALTER USER u IDENTIFIED BY wáchtwóørd9",
            Some("1064 (42000)"),
            "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'wáchtwóørd9' at line 1",
            &["wáchtwóørd9", "chtw"],
        ),
        c(
            "M19 pw contains keyword BY",
            MY,
            "ALTER USER u IDENTIFIED BY BY_Zq9wSecret",
            Some("1064 (42000)"),
            "x",
            &["Zq9wSecret"],
        ),
        c(
            "M20 INSERT unquoted secret col",
            MY,
            "INSERT INTO users (email, password) VALUES ('a@x', Zq9wSecret)",
            Some("1054 (42S22)"),
            "Unknown column 'Zq9wSecret' in 'field list'",
            &["Zq9wSecret"],
        ),
        c(
            "M21 INSERT SET form",
            MY,
            "INSERT INTO users SET email='a@x', api_token='Zq9wSecret'",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "M22 SET GLOBAL pw-like var",
            MY,
            "SET GLOBAL validate_password.policy = 'Zq9wSecret'",
            None,
            "",
            &[],
        ),
        c(
            "M23 CHANGE MASTER typo",
            MY,
            "CHANGE MASTER TO MASTER_PASWORD='Zq9wSecret'",
            Some("1064 (42000)"),
            "x",
            &["Zq9wSecret"],
        ),
        c(
            "M24 dup entry echo stmt secret",
            MY,
            "UPDATE users SET pw = 'Zq9wSecret' WHERE id = 2",
            Some("1062 (23000)"),
            "Duplicate entry 'Zq9wSecret' for key 'users.pw'",
            &["Zq9wSecret"],
        ),
        c(
            "M25 exec comment typo",
            MY,
            "/*!80000 ALTER USER u IDENTIFED BY 'Zq9wSecret' */",
            Some("1064 (42000)"),
            "x",
            &["Zq9wSecret"],
        ),
        c(
            "M26 two users comma",
            MY,
            "CREATE USER a IDENTIFIED BY 'Zq9wOne', b IDENTIFIED BY Zq9wTwo",
            None,
            "",
            &["Zq9wOne", "Zq9wTwo"],
        ),
        c(
            "M27 VIA plugin USING quoted",
            MY,
            "ALTER USER u IDENTIFIED VIA ed25519 USING 'Zq9wSecretHash' OR unix_socket",
            None,
            "",
            &["Zq9wSecretHash"],
        ),
        c(
            "M28 SET PASSWORD FOR typo",
            MY,
            "SET PASWORD FOR u = 'Zq9wSecret'",
            Some("1064 (42000)"),
            "x",
            &["Zq9wSecret"],
        ),
        // ---------------- Redis ----------------
        c(
            "R01 AUTH quoted spaces",
            RD,
            "AUTH \"Zq9w secret phrase\"",
            None,
            "",
            &["Zq9w", "secret phrase"],
        ),
        c(
            "R02 AUTH user quoted spaces",
            RD,
            "AUTH default 'Zq9w secret'",
            None,
            "",
            &["Zq9w", "secret"],
        ),
        c(
            "R03 AUTH escaped quote",
            RD,
            "AUTH \"Zq9w\\\"inner\"",
            None,
            "",
            &["Zq9w", "inner"],
        ),
        c(
            "R04 ACL multiple >pw",
            RD,
            "ACL SETUSER alice on >Zq9wOne >Zq9wTwo <Zq9wOld ~* +@all",
            None,
            "",
            &["Zq9wOne", "Zq9wTwo", "Zq9wOld"],
        ),
        c(
            "R05 ACL selector with pw",
            RD,
            "ACL SETUSER alice on (>Zq9wSel ~k*)",
            None,
            "",
            &["Zq9wSel"],
        ),
        c(
            "R06 ACL SET USER typo",
            RD,
            "ACL SET USER alice on >Zq9wSecret",
            Some("ERR"),
            "ERR unknown subcommand 'SET'. Try ACL HELP.",
            &["Zq9wSecret"],
        ),
        c(
            "R07 CONFIG SET 3 pairs",
            RD,
            "CONFIG SET requirepass Zq9wOne maxmemory 1gb masterauth Zq9wTwo",
            None,
            "",
            &["Zq9wOne", "Zq9wTwo"],
        ),
        c(
            "R08 CONFIG SET quoted pair",
            RD,
            "CONFIG SET \"requirepass\" \"Zq9w with space\"",
            None,
            "",
            &["Zq9w", "with space"],
        ),
        c(
            "R09 CONFIG SET misaligned",
            RD,
            "CONFIG SET maxmemory requirepass Zq9wSecret",
            Some("ERR"),
            "ERR Unknown option or number of arguments for CONFIG SET - 'Zq9wSecret'",
            &["Zq9wSecret"],
        ),
        c(
            "R10 pipelined lines",
            RD,
            "SET a 1\nAUTH Zq9wSecret\nGET a\nCONFIG SET masterauth Zq9wTwo",
            None,
            "",
            &["Zq9wSecret", "Zq9wTwo"],
        ),
        c(
            "R11 CRLF lines",
            RD,
            "PING\r\nAUTH default Zq9wSecret\r\nPING",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R12 RESP paste",
            RD,
            "*2\n$4\nAUTH\n$10\nZq9wSecret",
            Some("ERR"),
            "ERR unknown command 'Zq9wSecret', with args beginning with: ",
            &["Zq9wSecret"],
        ),
        c(
            "R13 SCRIPT LOAD auth",
            RD,
            "SCRIPT LOAD \"return redis.call('AUTH','Zq9wSecret')\"",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R14 EVAL ACL no keyword",
            RD,
            "EVAL \"return redis.call('ACL','SETUSER','u','>' .. ARGV[1])\" 0 Zq9wSecret",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R15 EVAL pw as KEY",
            RD,
            "EVAL \"return redis.call('AUTH', KEYS[1])\" 1 Zq9wSecret",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R16 EVAL long string",
            RD,
            "EVAL \"redis.call('CONFIG','SET','requirepass',[[Zq9wSecret]])\" 0",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R17 HELLO lowercase",
            RD,
            "hello 3 auth default Zq9wSecret setname app",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R18 HELLO quoted pw",
            RD,
            "HELLO 3 AUTH default \"Zq9w secret\"",
            None,
            "",
            &["Zq9w", "secret"],
        ),
        c(
            "R19 MIGRATE KEYS before AUTH",
            RD,
            "MIGRATE h 6379 \"\" 0 5000 KEYS a AUTH Zq9wSecret",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R20 MODULE LOAD args",
            RD,
            "MODULE LOAD /m.so apikey Zq9wSecret",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R21 MODULE LOADEX CONFIG",
            RD,
            "MODULE LOADEX /m.so CONFIG m.password Zq9wSecret",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R22 AUTH2 fuzzy cmd",
            RD,
            "AUTH3 default Zq9wSecret",
            Some("ERR"),
            "ERR unknown command 'AUTH3', with args beginning with: 'default' 'Zq9wSecret' ",
            &["Zq9wSecret"],
        ),
        c(
            "R23 HSET pw field",
            RD,
            "HSET user:1 password Zq9wSecret email a@x",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R24 CONFIG SET truncated echo",
            RD,
            "CONFIG SET requirepass",
            Some("ERR"),
            "ERR wrong number of arguments",
            &[],
        ),
        c(
            "R25 SENTINEL CONFIG SET",
            RD,
            "SENTINEL CONFIG SET sentinel-pass Zq9wSecret",
            None,
            "",
            &["Zq9wSecret"],
        ),
        c(
            "R26 unicode AUTH",
            RD,
            "AUTH wáchtwóørdZq",
            Some("WRONGPASS"),
            "WRONGPASS invalid username-password pair",
            &["wáchtwóørdZq"],
        ),
        c(
            "R27 ACL user then bare pw typo",
            RD,
            "ACL SETUSER alice on Zq9wSecret ~*",
            Some("ERR"),
            "ERR Error in ACL SETUSER modifier 'Zq9wSecret': Syntax error",
            &["Zq9wSecret"],
        ),
        c(
            "R28 AUTH comment trailing",
            RD,
            "AUTH Zq9wSecret\n# AUTH Zq9wOther",
            None,
            "",
            &["Zq9wSecret", "Zq9wOther"],
        ),
        c(
            "R29 CLIENT NO-EVICT pw typo",
            RD,
            "CONFG SET requirepass Zq9wSecret",
            Some("ERR"),
            "ERR unknown command 'CONFG', with args beginning with: 'SET' 'requirepass' 'Zq9wSecret' ",
            &["Zq9wSecret"],
        ),
    ]
}

/// Third audit: a secret in a value position, recorded as executed.
fn v(tag: &'static str, d: SqlDialect, stmt: &str, secrets: &'static [&'static str]) -> Case {
    c(tag, d, stmt, None, "", secrets)
}

/// Third audit: a secret in a name position (accepted when kept).
fn a(tag: &'static str, d: SqlDialect, stmt: &str, secrets: &'static [&'static str]) -> Case {
    Case {
        name_position: true,
        ..v(tag, d, stmt, secrets)
    }
}

/// Third audit: a failed statement with its server message.
fn e(
    tag: &'static str,
    d: SqlDialect,
    stmt: &str,
    code: &'static str,
    msg: &'static str,
    secrets: &'static [&'static str],
) -> Case {
    c(tag, d, stmt, Some(code), msg, secrets)
}

const H: &[&str] = &["hunter2xyz"];
const PA: &[&str] = &["ssw0rdXy"];

/// Third-audit cases whose secret is plain data at text level `full`: an
/// ordinary value (`WHERE pw = P@ssw0rdXy`, `SET k hunter2xyz`) that
/// nothing marks as a secret. `full` stores statement text as written, so
/// they are checked at `none` and `normalized` only.
const THIRD_AUDIT_PLAIN_AT_FULL: [&str; 77] = [
    "V02 rhs amp",
    "V03 rhs concat",
    "V04 rhs IS NOT NULL",
    "V05 rhs dotted",
    "V06 rhs hyphens",
    "V07 rhs plus",
    "V08 IN list",
    "V09 THEN value",
    "V10 THEN concat",
    "V11 SET type =",
    "V12 WHERE user =",
    "V13 SET role =",
    "V15 ODKU 2nd value",
    "V16 mysql $word set",
    "V17 mysql $word where",
    "V18 mysql $word values",
    "V19 COMMENT IS qualified",
    "V20 COLLATE then rhs",
    "V21 SET NAMES then var",
    "V22 EXECUTE USING",
    "V23 substring FROM",
    "V24 trim FROM",
    "V25 unknown stmt dotted",
    "V26 unknown stmt at",
    "V31 row assign",
    "V36 mysql hash comment",
    "V37 mysql --no-space",
    "V41 unbalanced parens",
    "V43 nested subselect",
    "V47 3000 open parens",
    "V50 OPTIONS SET user",
    "V51 OPTIONS SET sslkey",
    "V55 glued introducer",
    "V60 BETWEEN",
    "V61 ANY ARRAY",
    "V68 bare VALUES",
    "V70 DEFAULT then concat",
    "V71 CALL arg",
    "V75 column DEFAULT",
    "V76 CHECK",
    "V77 SET DEFAULT",
    "V82 pg label",
    "V84 caret",
    "V87 ALTER SYSTEM at",
    "V89 WHERE type =",
    "V90 WHERE schema =",
    "V91 WHERE owner =",
    "V92 HAVING value op",
    "V94 USING expr",
    "V95 FROM in EXTRACT",
    "V96 PG secret alias of value",
    "X08 mysql $ in ODKU",
    "X09 note concat quoted",
    "X10 CALL quoted",
    "X11 WHERE user = quoted",
    "X13 SET ROLE bare",
    "X14 SET SESSION AUTHORIZATION",
    "R09 CLIENT SETNAME",
    "R25 EVAL keys",
    "R26 FCALL",
    "R27 MSET value",
    "R30 RESP value looks like GET",
    "R33 RESP value DEL",
    "R34 ZUNIONSTORE big count",
    "R35 SINTERCARD big count",
    "R37 PUBLISH msg",
    "R38 SET EX",
    "R40 CLIENT SETINFO",
    "R48 MEMORY USAGE SAMPLES",
    "R49 SORT BY",
    "R50 GEOADD member",
    "R52 RESP value CONFIG GET",
    "R54 LMPOP big count",
    "R55 XREAD id",
    "R57 MSET odd count",
    "R59 unterminated quote",
    "R60 quote mid arg",
];

/// The third audit's 196 inputs.
fn third_audit_cases() -> Vec<Case> {
    let long = format!(
        "SELECT * FROM t WHERE {}pw = P@ssw0rdXy",
        "a = 1 AND ".repeat(5000)
    );
    let long_paren = format!("SELECT * FROM t WHERE x IN {}hunter2xyz", "(".repeat(3000));
    let cases = vec![
        // ---------- real name positions (keeping is acceptable) ----------
        a("A01 select list", PG, "SELECT hunter2xyz FROM t", H),
        a("A02 AS alias", PG, "SELECT a AS hunter2xyz FROM t", H),
        a("A03 ORDER BY", PG, "SELECT a FROM t ORDER BY hunter2xyz", H),
        a(
            "A04 CREATE TABLE",
            PG,
            "CREATE TABLE hunter2xyz (id int)",
            H,
        ),
        a("A05 quoted ident", PG, "SELECT \"hunter2xyz\" FROM t", H),
        a("A06 backtick", MY, "SELECT `hunter2xyz` FROM t", H),
        a("A07 qualified", PG, "SELECT t.hunter2xyz FROM t", H),
        a("A08 function name", PG, "SELECT hunter2xyz(1)", H),
        a("A09 cast type", PG, "SELECT 1::hunter2xyz", H),
        a("A10 user var", MY, "SELECT @hunter2xyz", H),
        a(
            "A11 label",
            MY,
            "CREATE PROCEDURE p() hunter2xyz: LOOP LEAVE hunter2xyz; END LOOP hunter2xyz",
            H,
        ),
        a("A12 GROUP BY", PG, "SELECT a FROM t GROUP BY hunter2xyz", H),
        a(
            "A13 insert column",
            PG,
            "INSERT INTO t (hunter2xyz) VALUES (1)",
            H,
        ),
        a(
            "A14 where lhs",
            PG,
            "SELECT * FROM t WHERE hunter2xyz = 1",
            H,
        ),
        a("A15 set lhs", PG, "UPDATE t SET hunter2xyz = 1", H),
        a("A16 index name", PG, "CREATE INDEX hunter2xyz ON t (a)", H),
        a("A17 table alias", PG, "SELECT * FROM t hunter2xyz", H),
        a(
            "A18 join alias",
            PG,
            "SELECT * FROM t JOIN u hunter2xyz ON hunter2xyz.id = t.id",
            H,
        ),
        a(
            "A19 grant to role",
            PG,
            "GRANT SELECT ON t TO hunter2xyz",
            H,
        ),
        a(
            "A20 rhs qualified col",
            PG,
            "SELECT * FROM t t2 WHERE t2.pw = t2.hunter2xyz",
            H,
        ),
        a(
            "A21 rhs function",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz(1)",
            H,
        ),
        a(
            "A22 rhs cast type",
            PG,
            "SELECT * FROM t WHERE pw = 'a'::hunter2xyz",
            H,
        ),
        a(
            "A23 rhs user var",
            MY,
            "SELECT * FROM t WHERE pw = @hunter2xyz",
            H,
        ),
        a(
            "A24 subselect list",
            PG,
            "SELECT * FROM t WHERE pw IN (SELECT hunter2xyz FROM u)",
            H,
        ),
        a(
            "A25 rhs quoted qualified",
            PG,
            "SELECT * FROM t WHERE pw = \"hunter2xyz\".\"x\"",
            H,
        ),
        // ---------- value positions (must become ?) ----------
        v(
            "V01 rhs P@ss",
            PG,
            "SELECT * FROM t WHERE pw = P@ssw0rdXy",
            PA,
        ),
        v(
            "V02 rhs amp",
            PG,
            "SELECT * FROM t WHERE note = Tr0ub4dor&3xq",
            &["Tr0ub4dor"],
        ),
        v(
            "V03 rhs concat",
            PG,
            "SELECT * FROM t WHERE note = hunter2xyz || 'a'",
            H,
        ),
        v(
            "V04 rhs IS NOT NULL",
            PG,
            "SELECT * FROM t WHERE note = hunter2xyz IS NOT NULL",
            H,
        ),
        v(
            "V05 rhs dotted",
            PG,
            "SELECT * FROM t WHERE note = hunter2xyz.part",
            H,
        ),
        v(
            "V06 rhs hyphens",
            PG,
            "SELECT * FROM t WHERE note = correct-horse-battery",
            &["correct-horse"],
        ),
        v(
            "V07 rhs plus",
            PG,
            "SELECT * FROM t WHERE note = hunter2xyz + 1",
            H,
        ),
        v(
            "V08 IN list",
            PG,
            "SELECT * FROM t WHERE pw IN (hunter2xyz)",
            H,
        ),
        v(
            "V09 THEN value",
            PG,
            "SELECT CASE WHEN x THEN hunter2xyz ELSE y END FROM t",
            H,
        ),
        v(
            "V10 THEN concat",
            PG,
            "SELECT CASE WHEN x = 1 THEN hunter2xyz || 'x' END FROM t",
            H,
        ),
        v(
            "V11 SET type =",
            MY,
            "UPDATE users SET type = hunter2xyz WHERE id = 1",
            H,
        ),
        v(
            "V12 WHERE user =",
            MY,
            "SELECT * FROM mysql.user WHERE user = hunter2xyz",
            H,
        ),
        v("V13 SET role =", PG, "UPDATE t SET role = hunter2xyz", H),
        v(
            "V14 ODKU value",
            MY,
            "INSERT INTO t (a, pw) VALUES (1, 'x') ON DUPLICATE KEY UPDATE pw = hunter2xyz",
            H,
        ),
        v(
            "V15 ODKU 2nd value",
            MY,
            "INSERT INTO t (a) VALUES (1) ON DUPLICATE KEY UPDATE a = 1, note = hunter2xyz",
            H,
        ),
        v(
            "V16 mysql $word set",
            MY,
            "UPDATE t SET note = $ecretXyz9 WHERE id = 1",
            &["ecretXyz9"],
        ),
        v(
            "V17 mysql $word where",
            MY,
            "SELECT * FROM t WHERE note = $ecretXyz9",
            &["ecretXyz9"],
        ),
        v(
            "V18 mysql $word values",
            MY,
            "INSERT INTO t VALUES ($ecretXyz9)",
            &["ecretXyz9"],
        ),
        v(
            "V19 COMMENT IS qualified",
            PG,
            "COMMENT ON COLUMN t.c IS hunter2xyz",
            H,
        ),
        v(
            "V20 COLLATE then rhs",
            PG,
            "SELECT * FROM t WHERE name COLLATE \"C\" = hunter2xyz",
            H,
        ),
        v(
            "V21 SET NAMES then var",
            MY,
            "SET NAMES utf8mb4, @note = hunter2xyz",
            H,
        ),
        v("V22 EXECUTE USING", MY, "EXECUTE s USING hunter2xyz", H),
        v(
            "V23 substring FROM",
            PG,
            "SELECT substring(note FROM hunter2xyz) FROM t",
            H,
        ),
        v("V24 trim FROM", PG, "SELECT trim(both FROM hunter2xyz)", H),
        v("V25 unknown stmt dotted", PG, "XYZZY hunter2xyz.more", H),
        v("V26 unknown stmt at", PG, "FOO P@ssw0rdXy", PA),
        v("V27 SET password bare", PG, "SET password hunter2xyz", H),
        v(
            "V28 mysql SET PASSWORD bare",
            MY,
            "SET PASSWORD hunter2xyz",
            H,
        ),
        v("V29 SET guc =", PG, "SET app.token = hunter2xyz", H),
        v(
            "V30 SET guc TO dotted",
            PG,
            "SET app.token TO hunter2xyz.more",
            H,
        ),
        v(
            "V31 row assign",
            PG,
            "UPDATE t SET (a, b) = (1, hunter2xyz)",
            H,
        ),
        v(
            "V32 INSERT SELECT bare",
            PG,
            "INSERT INTO users (email, password) SELECT 'a', hunter2xyz",
            H,
        ),
        v(
            "V33 multi stmt",
            PG,
            "SELECT 1 WHERE x = 1; SELECT * FROM t WHERE pw = P@ssw0rdXy",
            PA,
        ),
        v(
            "V34 comment before value",
            PG,
            "SELECT * FROM t WHERE pw = /* c */ P@ssw0rdXy",
            PA,
        ),
        v(
            "V35 comment splits token",
            PG,
            "SELECT * FROM t WHERE pw = hun/**/ter2xyz",
            &["ter2xyz"],
        ),
        v(
            "V36 mysql hash comment",
            MY,
            "SELECT * FROM t WHERE pw = 'x' # P@ssw0rdXy",
            PA,
        ),
        v(
            "V37 mysql --no-space",
            MY,
            "SELECT * FROM t WHERE pw = 1 --P@ssw0rdXy",
            PA,
        ),
        v(
            "V38 E string",
            PG,
            "SELECT * FROM t WHERE pw = E'hunter2xyz\\''",
            H,
        ),
        v(
            "V39 dollar quote",
            PG,
            "SELECT * FROM t WHERE pw = $q$hunter2xyz$q$",
            H,
        ),
        v(
            "V40 unterminated quote",
            PG,
            "SELECT * FROM t WHERE pw = 'hunter2xyz",
            H,
        ),
        v(
            "V41 unbalanced parens",
            PG,
            "SELECT * FROM t WHERE pw IN ((hunter2xyz)",
            H,
        ),
        v(
            "V42 extra close paren",
            PG,
            "SELECT * FROM t WHERE pw IN (1)) OR pw = P@ssw0rdXy",
            PA,
        ),
        v(
            "V43 nested subselect",
            PG,
            "SELECT * FROM t WHERE a = (SELECT b FROM c WHERE d = P@ssw0rdXy)",
            PA,
        ),
        v(
            "V44 unicode bare",
            PG,
            "SELECT * FROM t WHERE pw = ünïcødeSecr3t",
            &["ünïcødeSecr3t"],
        ),
        v(
            "V45 unicode at",
            PG,
            "SELECT * FROM t WHERE pw = ünï@cødeSecr3t",
            &["cødeSecr3t"],
        ),
        Case {
            tag: "V46 very long",
            ..v("", PG, &long, PA)
        },
        Case {
            tag: "V47 3000 open parens",
            ..v("", PG, &long_paren, H)
        },
        v(
            "V48 COPY PASSWORD",
            PG,
            "COPY t FROM STDIN WITH PASSWORD hunter2xyz",
            H,
        ),
        v(
            "V49 server OPTIONS bare",
            PG,
            "CREATE SERVER s FOREIGN DATA WRAPPER w OPTIONS (password hunter2xyz)",
            H,
        ),
        v(
            "V50 OPTIONS SET user",
            PG,
            "ALTER USER MAPPING FOR bob SERVER s OPTIONS (SET user hunter2xyz)",
            H,
        ),
        v(
            "V51 OPTIONS SET sslkey",
            PG,
            "ALTER SERVER s OPTIONS (SET sslkey hunter2xyz)",
            H,
        ),
        v(
            "V52 role SET guc",
            PG,
            "ALTER ROLE alice SET app.pw = hunter2xyz",
            H,
        ),
        v(
            "V53 set_config bare",
            PG,
            "SELECT set_config('app.pw', hunter2xyz, false)",
            H,
        ),
        v(
            "V54 BINARY rhs",
            MY,
            "SELECT * FROM t WHERE pw = BINARY hunter2xyz",
            H,
        ),
        v(
            "V55 glued introducer",
            MY,
            "SELECT * FROM t WHERE pw = _hunter2xyz'x'",
            H,
        ),
        v(
            "V56 hex x literal",
            MY,
            "SELECT * FROM t WHERE pw = x'68756e746572'",
            &["68756e746572"],
        ),
        v(
            "V57 U& literal",
            PG,
            "SELECT * FROM t WHERE pw = U&'hunter2xyz'",
            H,
        ),
        v(
            "V58 quoted ident rhs",
            PG,
            "SELECT * FROM t WHERE pw = \"hunter2xyz\"",
            H,
        ),
        v(
            "V59 LIMIT after",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz LIMIT 1",
            H,
        ),
        v(
            "V60 BETWEEN",
            PG,
            "SELECT * FROM t WHERE pw BETWEEN hunter2xyz AND x",
            H,
        ),
        v(
            "V61 ANY ARRAY",
            PG,
            "SELECT * FROM t WHERE pw = ANY(ARRAY[hunter2xyz])",
            H,
        ),
        v(
            "V62 RETURNING after",
            PG,
            "DELETE FROM t WHERE pw = hunter2xyz RETURNING *",
            H,
        ),
        v(
            "V63 paren condition",
            PG,
            "SELECT * FROM t WHERE (pw = hunter2xyz)",
            H,
        ),
        v(
            "V64 minus number",
            PG,
            "SELECT * FROM t WHERE x = 1 AND pw = hunter2xyz-1",
            H,
        ),
        v(
            "V65 regex rhs",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz ~ 'a'",
            H,
        ),
        v("V66 select list eq", PG, "SELECT pw = hunter2xyz FROM t", H),
        v(
            "V67 ORDER BY eq",
            PG,
            "SELECT * FROM t ORDER BY pw = hunter2xyz",
            H,
        ),
        v("V68 bare VALUES", PG, "VALUES (hunter2xyz)", H),
        v(
            "V69 CTE update",
            PG,
            "WITH x AS (SELECT 1) UPDATE t SET pw = hunter2xyz",
            H,
        ),
        v(
            "V70 DEFAULT then concat",
            PG,
            "UPDATE t SET pw = DEFAULT, note = hunter2xyz || 'x'",
            H,
        ),
        v("V71 CALL arg", PG, "CALL p(hunter2xyz)", H),
        v(
            "V72 PREPARE AS",
            PG,
            "PREPARE q AS SELECT * FROM t WHERE pw = P@ssw0rdXy",
            PA,
        ),
        v(
            "V73 VIEW body",
            PG,
            "CREATE VIEW v AS SELECT * FROM t WHERE pw = hunter2xyz || ''",
            H,
        ),
        v(
            "V74 LOAD DATA SET",
            MY,
            "LOAD DATA INFILE 'f' INTO TABLE t SET pw = hunter2xyz",
            H,
        ),
        v(
            "V75 column DEFAULT",
            PG,
            "CREATE TABLE t (pw text DEFAULT hunter2xyz)",
            H,
        ),
        v(
            "V76 CHECK",
            PG,
            "CREATE TABLE t (pw text CHECK (pw <> hunter2xyz))",
            H,
        ),
        v(
            "V77 SET DEFAULT",
            PG,
            "ALTER TABLE t ALTER COLUMN pw SET DEFAULT hunter2xyz",
            H,
        ),
        v(
            "V78 NOT LIKE after",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz NOT LIKE x",
            H,
        ),
        v(
            "V79 short P@ss",
            PG,
            "SELECT * FROM t WHERE pw = P@ssZq AND x = 1",
            &["ssZq"],
        ),
        v(
            "V80 XOR",
            MY,
            "SELECT * FROM t WHERE pw = hunter2xyz XOR 1",
            H,
        ),
        v(
            "V81 percent",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz%2",
            H,
        ),
        v("V82 pg label", PG, "DO $$ <<hunter2xyz>> BEGIN END $$", H),
        v(
            "V83 exec comment",
            MY,
            "/*!50000 SELECT * FROM t WHERE pw = P@ssw0rdXy */",
            PA,
        ),
        v(
            "V84 caret",
            PG,
            "SELECT * FROM t WHERE pw = 'a' OR note = Tr0ub4dor^3",
            &["Tr0ub4dor"],
        ),
        v(
            "V85 MASTER_PASSWORD bare",
            MY,
            "CHANGE MASTER TO MASTER_HOST='h', MASTER_PASSWORD=hunter2xyz",
            H,
        ),
        v(
            "V86 SOURCE_PASSWORD at",
            MY,
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = P@ssw0rdXy",
            PA,
        ),
        v(
            "V87 ALTER SYSTEM at",
            PG,
            "ALTER SYSTEM SET app.x = P@ssw0rdXy",
            PA,
        ),
        v(
            "V88 keyword-like secret",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz",
            H,
        ),
        v(
            "V89 WHERE type =",
            PG,
            "SELECT * FROM t WHERE type = hunter2xyz",
            H,
        ),
        v(
            "V90 WHERE schema =",
            MY,
            "SELECT * FROM t WHERE schema = hunter2xyz",
            H,
        ),
        v(
            "V91 WHERE owner =",
            PG,
            "SELECT * FROM t WHERE owner = hunter2xyz",
            H,
        ),
        v(
            "V92 HAVING value op",
            PG,
            "SELECT a FROM t GROUP BY a HAVING max(pw) = hunter2xyz || ''",
            H,
        ),
        v(
            "V93 JOIN ON value op",
            PG,
            "SELECT * FROM t JOIN u ON u.pw = P@ssw0rdXy",
            PA,
        ),
        v(
            "V94 USING expr",
            PG,
            "ALTER TABLE t ALTER COLUMN c TYPE int USING hunter2xyz",
            H,
        ),
        v(
            "V95 FROM in EXTRACT",
            PG,
            "SELECT extract(year FROM hunter2xyz)",
            H,
        ),
        v(
            "V96 PG secret alias of value",
            PG,
            "SELECT 'x' AS a, hunter2xyz AS b",
            H,
        ),
        e(
            "V97 msg unquoted echo",
            MY,
            "SELECT * FROM t WHERE pw = P@ssw0rdXy",
            "1054 (42S22)",
            "Unknown column 'P@ssw0rdXy' in 'where clause'",
            PA,
        ),
        v(
            "X01 BINARY quoted pw col",
            MY,
            "SELECT * FROM users WHERE password = BINARY 'Zq9wSecret'",
            &["Zq9wSecret"],
        ),
        v(
            "X02 typo ALTR at",
            PG,
            "ALTR ROLE alice PASSWORD P@ssw0rdXy",
            PA,
        ),
        v(
            "X03 typo IDENTIFED dotted",
            MY,
            "CREAT USER u IDENTIFED BY hunter2xyz.more",
            H,
        ),
        v(
            "X04 GRANT typo at",
            MY,
            "GRANT ALL ON *.* TO u IDENTIFED BY P@ssw0rdXy",
            PA,
        ),
        v(
            "X05 mysql $ SET PASSWORD",
            MY,
            "SET PASSWORD = $ecretXyz9",
            &["ecretXyz9"],
        ),
        v(
            "X06 mysql $ IDENTIFIED",
            MY,
            "ALTER USER u IDENTIFIED BY $ecretXyz9",
            &["ecretXyz9"],
        ),
        v(
            "X07 IDENTIFIED dotted",
            MY,
            "CREATE USER u IDENTIFIED BY P@ss.word99",
            &["word99"],
        ),
        v(
            "X08 mysql $ in ODKU",
            MY,
            "INSERT INTO t (a) VALUES (1) ON DUPLICATE KEY UPDATE a = $ecretXyz9",
            &["ecretXyz9"],
        ),
        v(
            "X09 note concat quoted",
            PG,
            "UPDATE users SET note = 'Zq9wSecret' || x",
            &["Zq9wSecret"],
        ),
        v(
            "X10 CALL quoted",
            PG,
            "CALL p('Zq9wSecret')",
            &["Zq9wSecret"],
        ),
        v(
            "X11 WHERE user = quoted",
            MY,
            "SELECT * FROM mysql.user WHERE user = 'Zq9wSecret'",
            &["Zq9wSecret"],
        ),
        v(
            "X12 PASSWORD typo dotted pg",
            PG,
            "ALTER ROLE alice PASSWROD hunter2xyz.more",
            H,
        ),
        v("X13 SET ROLE bare", PG, "SET ROLE hunter2xyz", H),
        v(
            "X14 SET SESSION AUTHORIZATION",
            PG,
            "SET SESSION AUTHORIZATION hunter2xyz",
            H,
        ),
        // ---------- Redis ----------
        a("R01 GET key", RD, "GET hunter2xyz", H),
        a("R02 SUBSCRIBE", RD, "SUBSCRIBE hunter2xyz", H),
        a("R03 ACL DELUSER", RD, "ACL DELUSER hunter2xyz", H),
        a("R04 OBJECT ENCODING", RD, "OBJECT ENCODING hunter2xyz", H),
        a("R05 RENAME", RD, "RENAME a hunter2xyz", H),
        a("R06 BLPOP", RD, "BLPOP hunter2xyz 0", H),
        a("R07 MIGRATE key", RD, "MIGRATE h 6379 hunter2xyz 0 5000", H),
        v("R08 SET password", RD, "SET password hunter2xyz", H),
        v("R09 CLIENT SETNAME", RD, "CLIENT SETNAME hunter2xyz", H),
        v("R10 CONFIG SET", RD, "CONFIG SET requirepass hunter2xyz", H),
        v(
            "R11 CONFIG SET two",
            RD,
            "CONFIG SET requirepass hunter2xyz masterauth Other9Secret",
            &["hunter2xyz", "Other9Secret"],
        ),
        v(
            "R12 CONFIG SET one arg",
            RD,
            "CONFIG SET \"requirepass hunter2xyz\"",
            H,
        ),
        v(
            "R13 CONFIG GET typo",
            RD,
            "CONFIG GET requirepass hunter2xyz",
            H,
        ),
        v(
            "R14 config set lower",
            RD,
            "config set REQUIREPASS hunter2xyz",
            H,
        ),
        v(
            "R15 CONFIG SET spaced pw",
            RD,
            "CONFIG SET requirepass hunter2 xyz9Secret",
            &["xyz9Secret"],
        ),
        v(
            "R16 ACL SETUSER",
            RD,
            "ACL SETUSER alice on >hunter2xyz ~* +@all",
            H,
        ),
        v("R17 ACL SETUSER no name", RD, "ACL SETUSER >hunter2xyz", H),
        v(
            "R18 ACL selector",
            RD,
            "ACL SETUSER alice on (>hunter2xyz ~k*) resetpass",
            H,
        ),
        v(
            "R19 ACL DRYRUN AUTH",
            RD,
            "ACL DRYRUN alice AUTH hunter2xyz",
            H,
        ),
        v("R20 AUTH", RD, "AUTH alice hunter2xyz", H),
        v(
            "R21 HELLO",
            RD,
            "HELLO 3 AUTH alice hunter2xyz SETNAME x",
            H,
        ),
        v(
            "R22 MIGRATE AUTH",
            RD,
            "MIGRATE h 6379 \"\" 0 5000 AUTH hunter2xyz KEYS a b",
            H,
        ),
        v(
            "R23 MIGRATE AUTH2 after",
            RD,
            "MIGRATE h 6379 \"\" 0 5000 KEYS a b AUTH2 alice hunter2xyz",
            H,
        ),
        v(
            "R24 MIGRATE auth lower",
            RD,
            "MIGRATE h 6379 \"\" 0 5000 KEYS a auth hunter2xyz",
            H,
        ),
        v("R25 EVAL keys", RD, "EVAL \"return 1\" 1 hunter2xyz arg", H),
        v("R26 FCALL", RD, "FCALL f 1 hunter2xyz", H),
        v("R27 MSET value", RD, "MSET a hunter2xyz b 2", H),
        v("R28 pipeline", RD, "SET a 1\nAUTH hunter2xyz\nGET b", H),
        v(
            "R29 RESP AUTH",
            RD,
            "*2\r\n$4\r\nAUTH\r\n$10\r\nhunter2xyz\r\n",
            H,
        ),
        v(
            "R30 RESP value looks like GET",
            RD,
            "*3\r\n$3\r\nSET\r\n$4\r\nnote\r\n$14\r\nGET hunter2xyz\r\n",
            H,
        ),
        v(
            "R31 RESP wrong lengths",
            RD,
            "*2\n$99\nAUTH\n$1\nhunter2xyz",
            H,
        ),
        v(
            "R32 RESP then inline",
            RD,
            "*2\r\n$4\r\nAUTH\r\n$10\r\nhunter2xyz\r\nGET k",
            H,
        ),
        v(
            "R33 RESP value DEL",
            RD,
            "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$14\r\nDEL hunter2xyz\r\n",
            H,
        ),
        v(
            "R34 ZUNIONSTORE big count",
            RD,
            "ZUNIONSTORE dest 9 a WEIGHTS hunter2xyz",
            H,
        ),
        v(
            "R35 SINTERCARD big count",
            RD,
            "SINTERCARD 5 a b LIMIT hunter2xyz",
            H,
        ),
        v("R36 XADD field", RD, "XADD s * pw hunter2xyz", H),
        v("R37 PUBLISH msg", RD, "PUBLISH ch hunter2xyz", H),
        v("R38 SET EX", RD, "SET k hunter2xyz EX 10", H),
        v("R39 HSET", RD, "HSET u password hunter2xyz", H),
        v(
            "R40 CLIENT SETINFO",
            RD,
            "CLIENT SETINFO lib-name hunter2xyz",
            H,
        ),
        v(
            "R41 SENTINEL SET",
            RD,
            "SENTINEL SET m auth-pass hunter2xyz",
            H,
        ),
        v(
            "R42 SENTINEL CONFIG SET",
            RD,
            "SENTINEL CONFIG SET sentinel-pass hunter2xyz",
            H,
        ),
        v(
            "R43 SCRIPT LOAD",
            RD,
            "SCRIPT LOAD \"redis.call('AUTH','hunter2xyz')\"",
            H,
        ),
        v("R44 comment line", RD, "# AUTH hunter2xyz\nGET k", H),
        v("R45 quoted cmd name", RD, "\"AUTH\" \"hunter2xyz\"", H),
        v("R46 MULTI block", RD, "MULTI\nAUTH hunter2xyz\nEXEC", H),
        v("R47 CRLF inline", RD, "AUTH hunter2xyz\r\nGET k", H),
        v(
            "R48 MEMORY USAGE SAMPLES",
            RD,
            "MEMORY USAGE k SAMPLES hunter2xyz",
            H,
        ),
        v("R49 SORT BY", RD, "SORT k BY hunter2xyz", H),
        v("R50 GEOADD member", RD, "GEOADD k 1 2 hunter2xyz", H),
        v(
            "R51 RESP CONFIG SET",
            RD,
            "*4\r\n$6\r\nCONFIG\r\n$3\r\nSET\r\n$11\r\nrequirepass\r\n$10\r\nhunter2xyz\r\n",
            H,
        ),
        v(
            "R52 RESP value CONFIG GET",
            RD,
            "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$22\r\nCONFIG GET hunter2xyz\r\n",
            H,
        ),
        v("R53 typo AUHT", RD, "AUHT hunter2xyz", H),
        v(
            "R54 LMPOP big count",
            RD,
            "LMPOP 9 a LEFT COUNT hunter2xyz",
            H,
        ),
        v("R55 XREAD id", RD, "XREAD STREAMS s hunter2xyz", H),
        v(
            "R56 ACL SETUSER lower",
            RD,
            "acl setuser alice ON >hunter2xyz",
            H,
        ),
        v("R57 MSET odd count", RD, "MSET a 1 hunter2xyz", H),
        v("R58 nul separator", RD, "AUTH\0hunter2xyz", H),
        v("R59 unterminated quote", RD, "SET k \"hunter2xyz", H),
        v("R60 quote mid arg", RD, "SET k ab\"hunter2xyz\"", H),
    ];
    cases
        .into_iter()
        .map(|case| Case {
            plain_data: THIRD_AUDIT_PLAIN_AT_FULL.contains(&case.tag),
            ..case
        })
        .collect()
}

/// Ordinary statements with no secret, and their normalized text: nothing
/// is redacted, `full` keeps them as written, and `normalized` keeps table
/// and column names.
const ORDINARY: [(SqlDialect, &str, &str); 50] = [
    (
        PG,
        "SELECT id, email FROM users WHERE id = 42",
        "SELECT id, email FROM users WHERE id = ?",
    ),
    (
        PG,
        "SELECT u.name, count(*) FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.name ORDER BY 2 DESC LIMIT 10",
        "SELECT u.name, count(*) FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.name ORDER BY ? DESC LIMIT ?",
    ),
    (
        PG,
        "INSERT INTO users (email, name) VALUES ('a@example.com', 'Alice')",
        "INSERT INTO users (email, name) VALUES (?, ?)",
    ),
    (
        PG,
        "UPDATE users SET name = 'Bob', updated_at = now() WHERE id = 7",
        "UPDATE users SET name = ?, updated_at = now() WHERE id = ?",
    ),
    (
        PG,
        "DELETE FROM sessions WHERE expires_at < now() - interval '1 day'",
        "DELETE FROM sessions WHERE expires_at < now() - INTERVAL ?",
    ),
    (
        PG,
        "SELECT * FROM accounts WHERE login = 'alice' AND role = 'admin'",
        "SELECT * FROM accounts WHERE login = ? AND role = ?",
    ),
    (
        PG,
        "WITH t AS (SELECT 1 AS x) SELECT x FROM t",
        "WITH t AS (SELECT ? AS x) SELECT x FROM t",
    ),
    (
        PG,
        "SELECT password_changed_at FROM users WHERE id = 1",
        "SELECT password_changed_at FROM users WHERE id = ?",
    ),
    (
        PG,
        "ALTER TABLE users ADD COLUMN password_hash text",
        "ALTER TABLE users ADD COLUMN password_hash TEXT",
    ),
    (
        PG,
        "CREATE INDEX ON users (login)",
        "CREATE INDEX ON users(login)",
    ),
    (
        MY,
        "SELECT `id`, `name` FROM `users` WHERE `id` IN (1, 2, 3)",
        "SELECT `id`, `name` FROM `users` WHERE `id` IN (?, ?, ?)",
    ),
    (
        MY,
        "INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')",
        "INSERT INTO t (a, b) VALUES (?, ?), (?, ?)",
    ),
    (
        MY,
        "UPDATE t SET a = a + 1, b = \"z\" WHERE c LIKE 'foo%'",
        "UPDATE t SET a = ? + ?, b = ? WHERE c LIKE ?",
    ),
    (
        MY,
        "SELECT user, host FROM mysql.user",
        "SELECT user, host FROM mysql.user",
    ),
    (
        MY,
        "REPLACE INTO kv (k, v) VALUES ('role', 'admin')",
        "REPLACE INTO kv (k, v) VALUES (?, ?)",
    ),
    (
        MY,
        "SELECT * FROM users WHERE login = 'by' ORDER BY id",
        "SELECT * FROM users WHERE login = ? ORDER BY id",
    ),
    (RD, "SET session:1 hello EX 60", "SET session:1 ? EX ?"),
    (
        RD,
        "HSET user:1 name Alice email a@x",
        "HSET user:1 ? ? ? ?",
    ),
    (RD, "GET user:1:name", "GET user:1:name"),
    (RD, "ZADD board 10 alice 20 bob", "ZADD board ? ? ? ?"),
    (
        PG,
        "SELECT id, name, email FROM users WHERE id = 42",
        "SELECT id, name, email FROM users WHERE id = ?",
    ),
    (
        PG,
        "SELECT u.id, o.total FROM users u LEFT JOIN orders o ON o.user_id = u.id WHERE o.total > 100 ORDER BY o.total DESC LIMIT 20",
        "SELECT u.id, o.total FROM users u LEFT JOIN orders o ON o.user_id = u.id WHERE o.total > ? ORDER BY o.total DESC LIMIT ?",
    ),
    (
        PG,
        "INSERT INTO users (email, name) VALUES ('a@example.com', 'Alice') RETURNING id",
        "INSERT INTO users (email, name) VALUES (?, ?) RETURNING id",
    ),
    (
        PG,
        "SELECT count(*) FROM events WHERE created_at >= date_trunc('day', now())",
        "SELECT count(*) FROM events WHERE created_at >= date_trunc(?, now())",
    ),
    (
        PG,
        "SELECT status, count(*) FROM orders GROUP BY status HAVING count(*) > 5",
        "SELECT status, count(*) FROM orders GROUP BY status HAVING count(*) > ?",
    ),
    (
        PG,
        "WITH recent AS (SELECT * FROM orders WHERE created_at > now() - interval '7 days') SELECT user_id, sum(total) FROM recent GROUP BY user_id",
        "WITH recent AS (SELECT * FROM orders WHERE created_at > now() - INTERVAL ?) SELECT user_id, sum(?) FROM recent GROUP BY user_id",
    ),
    (
        PG,
        "SELECT * FROM products WHERE category_id IN (SELECT id FROM categories WHERE parent_id = 3)",
        "SELECT * FROM products WHERE category_id IN (SELECT id FROM categories WHERE parent_id = ?)",
    ),
    (
        PG,
        "CREATE TABLE audit (id bigserial PRIMARY KEY, actor text NOT NULL, at timestamptz DEFAULT now())",
        "CREATE TABLE audit (id bigserial PRIMARY KEY, actor TEXT NOT NULL, at TIMESTAMPTZ DEFAULT now())",
    ),
    (
        PG,
        "ALTER TABLE users ADD COLUMN last_login timestamptz",
        "ALTER TABLE users ADD COLUMN last_login TIMESTAMPTZ",
    ),
    (
        PG,
        "CREATE INDEX CONCURRENTLY idx_orders_user ON orders (user_id, created_at)",
        "CREATE INDEX CONCURRENTLY idx_orders_user ON orders(user_id, created_at)",
    ),
    (
        PG,
        "SELECT data->>'name' AS name FROM docs WHERE data @> '{\"a\":1}'",
        "SELECT ? ->> ? AS name FROM docs WHERE ? @> ?",
    ),
    (
        PG,
        "SELECT CASE WHEN total > 100 THEN 'big' ELSE 'small' END AS size FROM orders",
        "SELECT CASE WHEN ? > ? THEN ? ELSE ? END AS size FROM orders",
    ),
    (
        PG,
        "INSERT INTO kv (k, v) VALUES ('a', 1) ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
        "INSERT INTO kv (k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = ?",
    ),
    (
        PG,
        "SELECT id::text, created_at::date FROM users",
        "SELECT id::TEXT, created_at::DATE FROM users",
    ),
    (
        PG,
        "EXPLAIN ANALYZE SELECT * FROM users WHERE email = 'x'",
        "EXPLAIN ANALYZE SELECT * FROM users WHERE email = ?",
    ),
    (
        PG,
        "SELECT * FROM users WHERE name ILIKE '%bob%' AND deleted_at IS NULL",
        "SELECT * FROM users WHERE name ILIKE ? AND deleted_at IS NULL",
    ),
    (
        PG,
        "GRANT SELECT, INSERT ON orders TO reporting",
        "GRANT SELECT, INSERT ON orders TO reporting",
    ),
    (
        PG,
        "SELECT u.name FROM public.users u WHERE u.active",
        "SELECT u.name FROM public.users u WHERE ?",
    ),
    (
        MY,
        "INSERT INTO counters (id, n) VALUES (1, 1) ON DUPLICATE KEY UPDATE n = n + 1",
        "INSERT INTO counters (id, n) VALUES (?, ?) ON DUPLICATE KEY UPDATE n = ? + ?",
    ),
    (
        MY,
        "SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = 'paid'",
        "SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = ?",
    ),
    (
        MY,
        "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(255) NOT NULL) ENGINE=InnoDB",
        "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(?) NOT NULL) ENGINE = InnoDB",
    ),
    (MY, "SHOW TABLES", "SHOW TABLES"),
    (
        MY,
        "SELECT DATE(created_at) AS d, COUNT(*) FROM events GROUP BY d ORDER BY d",
        "SELECT DATE(?) AS d, COUNT(*) FROM events GROUP BY d ORDER BY d",
    ),
    (
        MY,
        "DELETE FROM sessions WHERE last_seen < NOW() - INTERVAL 1 DAY",
        "DELETE FROM sessions WHERE last_seen < NOW() - INTERVAL ? DAY",
    ),
    (RD, "LPUSH queue job1 job2", "LPUSH queue ? ?"),
    (RD, "EXPIRE session:1 300", "EXPIRE session:1 ?"),
    (
        RD,
        "SCAN 0 MATCH user:* COUNT 100",
        "SCAN ? MATCH ? COUNT ?",
    ),
    (
        RD,
        "XADD events * type login user 7",
        "XADD events ? ? ? ? ?",
    ),
    (RD, "DEL a b c", "DEL a b c"),
    (RD, "CONFIG GET maxmemory", "CONFIG GET maxmemory"),
];

fn record(
    recorder: &Recorder,
    case: &Case,
    connection: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let builder = if case.code.is_some() || !case.msg.is_empty() {
        ActivityEvent::query_failed(
            &case.stmt,
            case.d,
            "x",
            &ErrorDetails {
                code: case.code.map(str::to_string),
                message: case.msg.to_string(),
            },
            1,
        )
    } else {
        ActivityEvent::query_executed(&case.stmt, case.d, "x", Default::default(), 1)
    };
    recorder.record(builder.connection(connection).build()?)?;
    let event = recorder.query_history(connection, 1)?.remove(0);
    let text = |v: Option<&serde_json::Value>| {
        v.and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok((
        text(event.details.get("statement")),
        text(event.details.get("error").and_then(|e| e.get("message"))),
    ))
}

/// Records every case at every level; returns the leaks found.
fn leaks(cases: &[Case]) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("audit"));
    let mut leaks = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        for level in [TextLevel::None, TextLevel::Normalized, TextLevel::Full] {
            recorder.set_text_level(level);
            let connection = format!("c{i}-{level}");
            let (statement, error) = record(&recorder, case, &connection)?;
            // `[redacted]` never shows in normalized text.
            if level == TextLevel::Normalized && statement.contains("[redacted]") {
                leaks.push(format!("{} at {level}: marker in {statement:?}", case.tag));
            }
            let stored = format!("{statement} {error}");
            for secret in case.secrets {
                let accepted = case.name_position || (case.plain_data && level == TextLevel::Full);
                if stored.contains(secret) && !accepted {
                    leaks.push(format!("{} at {level}: `{secret}` in {stored:?}", case.tag));
                }
            }
            // A redaction never lands inside a word of the message.
            if error.contains("corres[redacted]") {
                leaks.push(format!("{} at {level}: mangled {error:?}", case.tag));
            }
        }
    }
    Ok(leaks)
}

#[test]
fn no_audited_secret_reaches_the_log() -> TestResult {
    let cases = audit_cases();
    assert_eq!(cases.len(), 91);
    // The only plain-data exception: `SELECT 'hunter2xyz'::int` has nothing
    // marking the literal as a secret, and `full` keeps literals.
    let plain: Vec<&str> = cases
        .iter()
        .filter(|c| c.plain_data)
        .map(|c| c.tag)
        .collect();
    assert_eq!(plain, ["PG15 type cast echo"]);
    let found = leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:\n{}",
        found.len(),
        found.join("\n")
    );
    Ok(())
}

#[test]
fn no_secret_of_the_second_audit_reaches_the_log() -> TestResult {
    let cases = second_audit_cases();
    assert_eq!(cases.len(), 94);
    assert!(cases.iter().all(|c| !c.plain_data));
    let found = leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:\n{}",
        found.len(),
        found.join("\n")
    );
    Ok(())
}

#[test]
fn ordinary_statements_keep_their_shape() -> TestResult {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("audit"));
    for (i, (dialect, statement, normalized)) in ORDINARY.iter().enumerate() {
        for (level, expected) in [
            (TextLevel::Normalized, *normalized),
            (TextLevel::Full, *statement),
        ] {
            recorder.set_text_level(level);
            let connection = format!("o{i}-{level}");
            recorder.record(
                ActivityEvent::query_executed(statement, *dialect, "x", Default::default(), 1)
                    .connection(connection.as_str())
                    .build()?,
            )?;
            let event = recorder.query_history(&connection, 1)?.remove(0);
            assert_eq!(
                event.details["statement"], expected,
                "{statement} at {level}"
            );
        }
    }
    Ok(())
}

#[test]
fn no_secret_of_the_third_audit_reaches_the_log() -> TestResult {
    let cases = third_audit_cases();
    assert_eq!(cases.len(), 196);
    let plain = cases.iter().filter(|c| c.plain_data).count();
    assert_eq!(
        plain,
        THIRD_AUDIT_PLAIN_AT_FULL.len(),
        "every listed tag exists"
    );
    let found = leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:\n{}",
        found.len(),
        found.join("\n")
    );
    Ok(())
}

/// A fourth-audit input. `k`: `'v'` value position (none and normalized
/// must be clean), `'a'` real name position (keeping it at normalized is
/// accepted), `'p'` password context (full must be clean too).
struct Case4 {
    tag: String,
    d: SqlDialect,
    stmt: String,
    secrets: &'static [&'static str],
    k: char,
}

const Z: &[&str] = &["Zq9wSecret"];

fn c4(k: char, tag: &str, d: SqlDialect, stmt: &str, secrets: &'static [&'static str]) -> Case4 {
    Case4 {
        tag: tag.to_string(),
        d,
        stmt: stmt.to_string(),
        secrets,
        k,
    }
}

/// The fourth audit's 320 inputs.
fn fourth_audit_cases() -> Vec<Case4> {
    let mut v = vec![
        // ---------- tree printer re-emits text verbatim ----------
        c4(
            'v',
            "T01 pg typmod string in cast",
            PG,
            "SELECT * FROM t WHERE pw = 'x'::foo('Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "T02 pg typmod string in column",
            PG,
            "CREATE TABLE t (c foo('Zq9wSecret'))",
            Z,
        ),
        c4(
            'v',
            "T03 pg typmod word in CAST",
            PG,
            "SELECT CAST(pw AS varchar(hunter2xyz)) FROM t",
            H,
        ),
        c4(
            'v',
            "T04 pg typmod qualified type",
            PG,
            "SELECT x::my.type('Zq9wSecret', 3) FROM t",
            Z,
        ),
        c4(
            'v',
            "T05 pg typmod with spaces",
            PG,
            "SELECT 'a'::foo('my Zq9wSecret pw')",
            Z,
        ),
        c4(
            'v',
            "T06 pg optimizer hint select",
            PG,
            "SELECT /*+ SeqScan(t) Zq9wSecret */ * FROM t",
            Z,
        ),
        c4(
            'v',
            "T07 my optimizer hint update",
            MY,
            "UPDATE /*+ NO_INDEX(t) 'Zq9wSecret' */ t SET a = 1",
            Z,
        ),
        c4(
            'v',
            "T08 my optimizer hint insert",
            MY,
            "INSERT /*+ Zq9wSecret */ INTO t VALUES (1)",
            Z,
        ),
        c4(
            'v',
            "T09 pg optimizer hint delete",
            PG,
            "DELETE /*+ Zq9wSecret */ FROM t WHERE id = 1",
            Z,
        ),
        c4(
            'v',
            "T10 hint with prefix",
            MY,
            "SELECT /*abc+ Zq9wSecret */ 1",
            Z,
        ),
        c4(
            'v',
            "T11 single line hint",
            PG,
            "SELECT --+ Zq9wSecret\n * FROM t",
            Z,
        ),
        c4(
            'v',
            "T12 my hint SET_VAR string",
            MY,
            "SELECT /*+ SET_VAR(init_connect='Zq9wSecret') */ * FROM t",
            Z,
        ),
        c4(
            'v',
            "T13 hint after multi stmt",
            PG,
            "SELECT 1; UPDATE /*+ Zq9wSecret */ t SET a = 1",
            Z,
        ),
        c4(
            'v',
            "T14 nested comment in hint",
            PG,
            "SELECT /*+ /* Zq9wSecret */ */ 1",
            Z,
        ),
        c4(
            'v',
            "T15 my hint not after SELECT",
            MY,
            "SELECT 1 /*+ Zq9wSecret */",
            Z,
        ),
        c4(
            'v',
            "T16 my table COMMENT ''",
            MY,
            "CREATE TABLE t (id INT) COMMENT 'it''s P@ssw0rdXy'",
            PA,
        ),
        c4(
            'v',
            "T17 my table COMMENT= \\'",
            MY,
            r"CREATE TABLE t (id INT) COMMENT='it\'s P@ssw0rdXy'",
            PA,
        ),
        c4(
            'v',
            "T18 my index COMMENT ''",
            MY,
            "CREATE INDEX i ON t (a) COMMENT 'it''s P@ssw0rdXy'",
            PA,
        ),
        c4(
            'v',
            "T19 my col comments backslash",
            MY,
            r"CREATE TABLE t (a INT COMMENT 'C:\\', b INT COMMENT 'pw P@ssw0rdXy x')",
            PA,
        ),
        c4(
            'v',
            "T20 my inline index COMMENT",
            MY,
            "CREATE TABLE t (a INT, INDEX i (a) COMMENT 'x''y P@ssw0rdXy')",
            PA,
        ),
        c4(
            'v',
            "T21 my ALTER TABLE COMMENT",
            MY,
            "ALTER TABLE t COMMENT = 'it''s P@ssw0rdXy'",
            PA,
        ),
        c4(
            'v',
            "T22 my table CONNECTION",
            MY,
            "CREATE TABLE t (a INT) ENGINE=FEDERATED CONNECTION='mysql://u:P@ssw0rdXy@h/db/t'",
            PA,
        ),
        c4(
            'v',
            "T23 my table PASSWORD opt",
            MY,
            "CREATE TABLE t (a INT) PASSWORD='P@ssw0rdXy'",
            PA,
        ),
        c4(
            'v',
            "T24 my ENUM member ''",
            MY,
            "CREATE TABLE t (a ENUM('x', 'it''s P@ssw0rdXy'))",
            PA,
        ),
        c4(
            'v',
            "T25 my SET member ''",
            MY,
            "CREATE TABLE t (a SET('a', 'it''s P@ssw0rdXy'))",
            PA,
        ),
        c4(
            'v',
            "T26 my col comment + idx comment bs",
            MY,
            r"CREATE TABLE t (a INT COMMENT 'C:\\', INDEX i (a) COMMENT ' P@ssw0rdXy ')",
            PA,
        ),
        c4(
            'v',
            "T27 my two separators bs",
            MY,
            r"SELECT GROUP_CONCAT(a SEPARATOR '\\'), GROUP_CONCAT(b SEPARATOR ' P@ssw0rdXy ') FROM t",
            PA,
        ),
        c4(
            'v',
            "T28 my SHOW TABLES LIKE ''",
            MY,
            "SHOW TABLES LIKE 'it''s Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T29 my SHOW COLUMNS LIKE ''",
            MY,
            "SHOW COLUMNS FROM t LIKE 'it''s Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T30 my SHOW TABLES WHERE",
            MY,
            "SHOW TABLES WHERE Tables_in_db = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T31 my SHOW DATABASES LIKE bs",
            MY,
            r"SHOW DATABASES LIKE 'x\\'",
            &[],
        ),
        c4(
            'v',
            "T32 my ALTER ADD col comment ''",
            MY,
            "ALTER TABLE t ADD COLUMN c INT COMMENT 'it''s P@ssw0rdXy'",
            PA,
        ),
        c4(
            'v',
            "T33 my MODIFY default+comment",
            MY,
            "ALTER TABLE t MODIFY pw VARCHAR(20) DEFAULT 'Zq9wSecret' COMMENT 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T34 my JSON_TABLE path",
            MY,
            "SELECT * FROM JSON_TABLE(doc, '$[*]' COLUMNS (a VARCHAR(10) PATH '$.Zq9wSecret' DEFAULT '\"x\"' ON EMPTY)) AS jt",
            Z,
        ),
        c4(
            'v',
            "T35 my MATCH AGAINST",
            MY,
            "SELECT * FROM t WHERE MATCH(body) AGAINST ('Zq9wSecret' IN BOOLEAN MODE)",
            Z,
        ),
        c4(
            'v',
            "T36 pg TABLESAMPLE REPEATABLE",
            PG,
            "SELECT * FROM t TABLESAMPLE BERNOULLI (10) REPEATABLE (hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "T37 pg typed DATE",
            PG,
            "SELECT * FROM t WHERE d = DATE 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T38 pg typed TIMESTAMPTZ",
            PG,
            "SELECT * FROM t WHERE ts = TIMESTAMP WITH TIME ZONE 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T39 pg INTERVAL string",
            PG,
            "SELECT now() - INTERVAL 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T40 my INTERVAL word",
            MY,
            "SELECT NOW() - INTERVAL hunter2xyz DAY",
            H,
        ),
        c4(
            'v',
            "T41 pg INTERVAL odd unit",
            PG,
            "SELECT INTERVAL '1' hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T42 pg EXTRACT custom field",
            PG,
            "SELECT extract(hunter2xyz FROM now())",
            H,
        ),
        c4(
            'v',
            "T43 pg EXTRACT quoted field",
            PG,
            "SELECT extract('Zq9wSecret' FROM now())",
            Z,
        ),
        c4(
            'v',
            "T44 pg date_part",
            PG,
            "SELECT date_part('Zq9wSecret', now())",
            Z,
        ),
        c4(
            'v',
            "T45 my typed literal word",
            MY,
            "SELECT * FROM t WHERE pw = hunter2xyz 'x'",
            H,
        ),
        c4(
            'a',
            "T46 pg generic typed literal",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz 'x'",
            H,
        ),
        c4(
            'v',
            "T47 pg psql :var",
            PG,
            "SELECT * FROM t WHERE pw = :hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T48 pg psql :'var'",
            PG,
            "SELECT * FROM t WHERE pw = :'hunter2xyz'",
            H,
        ),
        c4(
            'v',
            "T49 pg @ prefix",
            PG,
            "SELECT * FROM t WHERE pw = @hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T50 pg $2abc",
            PG,
            "SELECT * FROM t WHERE pw = $1 AND note = $2hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T51 my ?word",
            MY,
            "SELECT * FROM t WHERE pw = ?hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T52 pg $1$ not dollar quote",
            PG,
            "SELECT $1$hunter2xyz$1$",
            H,
        ),
        c4(
            'v',
            "T01b pg typmod string (non-pw col)",
            PG,
            "SELECT * FROM t WHERE note = 'x'::foo('Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "T03b pg typmod word (non-pw col)",
            PG,
            "SELECT CAST(note AS varchar(hunter2xyz)) FROM t",
            H,
        ),
        c4(
            'v',
            "T53 pg typmod in INSERT value",
            PG,
            "INSERT INTO t (note) VALUES ('x'::citext('Zq9wSecret'))",
            Z,
        ),
        c4(
            'v',
            "T54 pg typmod in UPDATE",
            PG,
            "UPDATE t SET note = CAST('x' AS foo('Zq9wSecret')) WHERE id = 1",
            Z,
        ),
        c4(
            'v',
            "T55 pg hint realistic",
            PG,
            "SELECT /*+ IndexScan(users) */ * FROM users WHERE email = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T56 my hint with comment note",
            MY,
            "SELECT /*+ BKA(t) token=Zq9wSecret */ id FROM t",
            Z,
        ),
        c4(
            'v',
            "T57 my col comment win path",
            MY,
            r"CREATE TABLE t (a INT COMMENT 'dir C:\\temp\\', b INT COMMENT 'api key Zq9wSecret')",
            Z,
        ),
        // ---------- MySQL identifier rules wider in sqlparser ----------
        c4(
            'v',
            "M01 my P@ss in projection",
            MY,
            "SELECT id, P@ssw0rdXy FROM users",
            PA,
        ),
        c4(
            'v',
            "M02 my P@ss IS NULL",
            MY,
            "SELECT * FROM t WHERE P@ssw0rdXy IS NULL",
            PA,
        ),
        c4(
            'v',
            "M03 my INSERT SELECT P@ss",
            MY,
            "INSERT INTO t (a, pw) SELECT 1, P@ssw0rdXy FROM dual",
            PA,
        ),
        c4(
            'v',
            "M04 my ORDER BY P@ss",
            MY,
            "SELECT * FROM t ORDER BY P@ssw0rdXy",
            PA,
        ),
        c4(
            'v',
            "M05 my GROUP BY P@ss",
            MY,
            "SELECT a FROM t GROUP BY P@ssw0rdXy",
            PA,
        ),
        c4(
            'v',
            "M06 my P@ss = 1",
            MY,
            "SELECT * FROM t WHERE P@ssw0rdXy = 1",
            PA,
        ),
        c4(
            'v',
            "M07 pg P@ss in projection",
            PG,
            "SELECT id, P@ssw0rdXy FROM users",
            PA,
        ),
        c4(
            'v',
            "M08 pg P@ss IS NULL",
            PG,
            "SELECT * FROM t WHERE P@ssw0rdXy IS NULL",
            PA,
        ),
        c4('a', "M09 my a$word", MY, "SELECT a$hunter2xyz FROM t", H),
        c4('a', "M10 pg pw$word", PG, "SELECT pw$hunter2xyz FROM t", H),
        c4(
            'v',
            "M11 my 9word value",
            MY,
            "SELECT * FROM t WHERE pw = 9hunter2xyz",
            H,
        ),
        c4(
            'v',
            "M12 pg 9word value",
            PG,
            "SELECT * FROM t WHERE pw = 9hunter2xyz",
            H,
        ),
        c4(
            'v',
            "M13 pg 9word projection",
            PG,
            "SELECT 9hunter2xyz FROM t",
            H,
        ),
        c4(
            'v',
            "M14 my 9word projection",
            MY,
            "SELECT 9hunter2xyz FROM t",
            H,
        ),
        c4(
            'v',
            "M15 my RETURNING-ish P@ss = ",
            MY,
            "UPDATE t SET a = 1 WHERE P@ssw0rdXy = 'x'",
            PA,
        ),
        c4(
            'v',
            "M16 my t.pw = 'str'.x",
            MY,
            "SELECT * FROM t WHERE t.pw = 'Zq9wSecret'.x",
            Z,
        ),
        c4(
            'a',
            "M17 pg both qualified",
            PG,
            "SELECT * FROM t WHERE t.pw = \"hunter2xyz\".x",
            H,
        ),
        c4(
            'v',
            "M18 my \"str\" projection",
            MY,
            "SELECT \"Zq9wSecret\" FROM t",
            Z,
        ),
        c4(
            'v',
            "M19 my \"str\" AS alias",
            MY,
            "SELECT id, \"Zq9wSecret\" AS pw FROM t",
            Z,
        ),
        c4(
            'v',
            "M20 my \"str\" = col",
            MY,
            "SELECT * FROM t WHERE \"Zq9wSecret\" = pw",
            Z,
        ),
        c4(
            'v',
            "M21 my \"str\" IS NULL",
            MY,
            "SELECT * FROM t WHERE \"Zq9wSecret\" IS NULL",
            Z,
        ),
        c4(
            'v',
            "M22 my \"str\" ORDER BY",
            MY,
            "SELECT * FROM t ORDER BY \"Zq9wSecret\"",
            Z,
        ),
        c4(
            'v',
            "M23 my backtick value",
            MY,
            "UPDATE users SET pw = `Zq9wSecret` WHERE id = 1",
            Z,
        ),
        c4(
            'a',
            "M24 pg quoted projection",
            PG,
            "SELECT \"Zq9wSecret\" FROM t",
            Z,
        ),
        c4(
            'v',
            "M25 pg quoted value",
            PG,
            "UPDATE users SET pw = \"Zq9wSecret\" WHERE id = 1",
            Z,
        ),
        c4(
            'v',
            "M26 pg \"q\" = col",
            PG,
            "SELECT * FROM t WHERE \"hunter2xyz\" = pw",
            H,
        ),
        c4(
            'v',
            "M27 pg U&\"ident\" value",
            PG,
            "SELECT * FROM t WHERE pw = U&\"hunter2xyz\"",
            H,
        ),
        c4(
            'v',
            "M28 keyword secret SECRET",
            PG,
            "UPDATE users SET pw = Secret WHERE id = 1",
            &["Secret"],
        ),
        c4(
            'v',
            "M29 keyword secret Tabular",
            MY,
            "UPDATE users SET pw = Tabular WHERE id = 1",
            &["Tabular"],
        ),
        c4(
            'v',
            "M30 my INSERT SELECT \"str\"",
            MY,
            "INSERT INTO t (pw) SELECT \"Zq9wSecret\" FROM dual",
            Z,
        ),
        // ---------- string syntaxes ----------
        c4(
            'v',
            "S01 pg E'' hides ;",
            PG,
            r"SELECT E'a\'; SELECT hunter2xyz FROM t; '",
            H,
        ),
        c4(
            'v',
            "S02 pg e'' lowercase hides ;",
            PG,
            r"SELECT e'a\'; SELECT hunter2xyz FROM t; '",
            H,
        ),
        c4(
            'v',
            "S03 pg U&'' literal",
            PG,
            r"SELECT * FROM t WHERE pw = U&'d\0061t Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S04 pg U&'' UESCAPE",
            PG,
            "SELECT * FROM t WHERE pw = U&'Zq9wSecret' UESCAPE '!'",
            Z,
        ),
        c4('v', "S05 pg $$ body", PG, "SELECT $$Zq9wSecret$$", Z),
        c4(
            'v',
            "S06 pg $tag$ hides ;",
            PG,
            "SELECT $a$ x; SELECT hunter2xyz FROM t; $a$",
            H,
        ),
        c4(
            'v',
            "S07 pg nested comment hides ;",
            PG,
            "SELECT 1 /* a /* b */ ; SELECT hunter2xyz FROM t; */",
            H,
        ),
        c4(
            'v',
            "S08 my comment no nesting",
            MY,
            "SELECT 1 /* a /* b */ , 'Zq9wSecret' FROM t",
            Z,
        ),
        c4(
            'v',
            "S09 pg --x comment",
            PG,
            "SELECT 1 --hunter2xyz\nFROM t",
            H,
        ),
        c4(
            'v',
            "S10 my --x not comment",
            MY,
            "SELECT 1 --hunter2xyz FROM t",
            H,
        ),
        c4('v', "S11 pg # operator", PG, "SELECT 1 # hunter2xyz", H),
        c4(
            'v',
            "S12 my -- tab comment",
            MY,
            "SELECT 1 --\thunter2xyz\nFROM t",
            H,
        ),
        c4(
            'v',
            "S13 my backslash hides ;",
            MY,
            r"SELECT 'a\'; SELECT hunter2xyz FROM t; '",
            H,
        ),
        c4(
            'v',
            "S14 my dq backslash hides ;",
            MY,
            r#"SELECT "a\"; SELECT hunter2xyz FROM t; ""#,
            H,
        ),
        c4(
            'a',
            "S15 pg std string then stmt",
            PG,
            r"SELECT 'a\'; SELECT hunter2xyz FROM t; '",
            H,
        ),
        c4(
            'v',
            "S16 pg ident with ;",
            PG,
            "SELECT \"a;b\", 'Zq9wSecret' FROM t",
            Z,
        ),
        c4(
            'v',
            "S17 my backtick with ;",
            MY,
            "SELECT `a;b`, 'Zq9wSecret' FROM t",
            Z,
        ),
        c4(
            'v',
            "S18 my backtick with '",
            MY,
            "SELECT `it's`, a FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S19 pg ident with '",
            PG,
            "SELECT \"it's\" FROM t WHERE pw = 'Zq9wSecret' AND x = 'y'",
            Z,
        ),
        c4(
            'v',
            "S20 my backtick with backslash",
            MY,
            r"SELECT `a\` FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S21 pg ident with /*",
            PG,
            "SELECT \"/*\" FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S22 pg ident with --",
            PG,
            "SELECT \"--\" FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S23 my _utf8mb4 + COLLATE",
            MY,
            "SELECT * FROM t WHERE pw = _utf8mb4 'Zq9wSecret' COLLATE utf8mb4_bin",
            Z,
        ),
        c4(
            'v',
            "S24 my _binary",
            MY,
            "SELECT * FROM t WHERE pw = _binary'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S25 my N''",
            MY,
            "SELECT * FROM t WHERE pw = N'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S26 my X''",
            MY,
            "SELECT * FROM t WHERE pw = X'68756e7465723278797a'",
            &["68756e7465723278797a"],
        ),
        c4(
            'v',
            "S27 my b''",
            MY,
            "SELECT * FROM t WHERE pw = b'0110100001110101'",
            &["0110100001110101"],
        ),
        c4(
            'v',
            "S28 my 0x hex",
            MY,
            "SELECT * FROM t WHERE pw = 0x5A713977AB",
            &["5A713977AB"],
        ),
        c4(
            'v',
            "S29 pg B''",
            PG,
            "SELECT * FROM t WHERE pw = B'0110100001110101'",
            &["0110100001110101"],
        ),
        c4(
            'v',
            "S30 pg X''",
            PG,
            "SELECT * FROM t WHERE pw = X'5A713977AB'",
            &["5A713977AB"],
        ),
        c4(
            'v',
            "S31 pg string continuation",
            PG,
            "SELECT 'a'\n'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S32 my adjacent strings",
            MY,
            "SELECT * FROM t WHERE pw = 'a' 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S33 pg [redacted] literal",
            PG,
            "SELECT '[redacted]', hunter2xyz FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S34 pg MARK char ident",
            PG,
            "SELECT \"\u{1}0\u{1}\", a FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "S35 pg MARK char literal",
            PG,
            "SELECT a FROM t WHERE pw = '\u{1}0\u{1}Zq9wSecret'",
            Z,
        ),
        // ---------- operators, JSON, arrays, rows ----------
        c4(
            'v',
            "O01 pg ->> key",
            PG,
            "SELECT * FROM t WHERE data->>'Zq9wSecret' = 'x'",
            Z,
        ),
        c4(
            'v',
            "O02 pg @> json",
            PG,
            "SELECT * FROM t WHERE data @> '{\"pw\":\"Zq9wSecret\"}'",
            Z,
        ),
        c4(
            'v',
            "O03 pg #>> path",
            PG,
            "SELECT data #>> '{a,Zq9wSecret}' FROM t",
            Z,
        ),
        c4(
            'v',
            "O04 pg ? key",
            PG,
            "SELECT * FROM t WHERE data ? 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "O05 pg ?| array",
            PG,
            "SELECT * FROM t WHERE data ?| array['Zq9wSecret']",
            Z,
        ),
        c4(
            'v',
            "O06 pg jsonpath",
            PG,
            "SELECT jsonb_path_query(data, '$.pw ? (@ == \"Zq9wSecret\")') FROM t",
            Z,
        ),
        c4(
            'v',
            "O07 my ->> path",
            MY,
            "SELECT doc->>'$.Zq9wSecret' FROM t",
            Z,
        ),
        c4(
            'v',
            "O08 my -> \"path\"",
            MY,
            "SELECT doc->\"$.Zq9wSecret\" FROM t",
            Z,
        ),
        c4(
            'v',
            "O09 my JSON_EXTRACT",
            MY,
            "SELECT JSON_EXTRACT(doc, '$.Zq9wSecret') FROM t",
            Z,
        ),
        c4(
            'v',
            "O10 my <=>",
            MY,
            "SELECT * FROM t WHERE pw <=> 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "O11 my || as OR",
            MY,
            "SELECT * FROM t WHERE pw = 'a' || note = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "O12 pg || concat",
            PG,
            "SELECT * FROM t WHERE pw = 'a' || hunter2xyz",
            H,
        ),
        c4(
            'v',
            "O13 pg ::text",
            PG,
            "SELECT * FROM t WHERE pw = 'Zq9wSecret'::text",
            Z,
        ),
        c4(
            'v',
            "O14 pg ident::text",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz::text",
            H,
        ),
        c4(
            'v',
            "O15 pg ANY array literal",
            PG,
            "SELECT * FROM t WHERE pw = ANY('{Zq9wSecret}'::text[])",
            Z,
        ),
        c4(
            'v',
            "O16 pg && ARRAY",
            PG,
            "SELECT * FROM t WHERE tags && ARRAY['Zq9wSecret']",
            Z,
        ),
        c4(
            'v',
            "O17 pg subscript ident",
            PG,
            "SELECT * FROM t WHERE arr[hunter2xyz] = 1",
            H,
        ),
        c4(
            'v',
            "O18 pg row compare",
            PG,
            "SELECT * FROM t WHERE (a, pw) = (1, hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "O19 pg ROW()",
            PG,
            "SELECT ROW(1, hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "O20 pg row IN",
            PG,
            "SELECT * FROM t WHERE (a, b) IN ((1, 'Zq9wSecret'))",
            Z,
        ),
        c4(
            'v',
            "O21 pg IS DISTINCT FROM",
            PG,
            "SELECT * FROM t WHERE pw IS DISTINCT FROM 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "O22 pg SIMILAR TO",
            PG,
            "SELECT * FROM t WHERE pw SIMILAR TO 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "O23 pg ~*",
            PG,
            "SELECT * FROM t WHERE pw ~* 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "O24 pg LIKE ESCAPE",
            PG,
            "SELECT * FROM t WHERE pw LIKE 'x' ESCAPE 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "O25 pg position IN",
            PG,
            "SELECT position('Zq9wSecret' IN pw) FROM t",
            Z,
        ),
        c4(
            'v',
            "O26 pg overlay placing",
            PG,
            "SELECT overlay(pw placing 'Zq9wSecret' from 1) FROM t",
            Z,
        ),
        c4(
            'v',
            "O27 pg trim leading",
            PG,
            "SELECT trim(leading 'Zq9wSecret' from pw) FROM t",
            Z,
        ),
        c4(
            'v',
            "O28 pg AT TIME ZONE",
            PG,
            "SELECT * FROM t WHERE ts = 'a' AT TIME ZONE 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "O29 pg IS NORMALIZED",
            PG,
            "SELECT 'Zq9wSecret' IS NORMALIZED",
            Z,
        ),
        c4(
            'v',
            "O30 pg = ALL subquery",
            PG,
            "SELECT * FROM t WHERE pw = ALL (SELECT 'Zq9wSecret')",
            Z,
        ),
        c4(
            'a',
            "O31 pg COLLATE name",
            PG,
            "SELECT * FROM t WHERE pw = 'x' COLLATE \"hunter2xyz\"",
            H,
        ),
        c4(
            'v',
            "O32 my CONVERT USING",
            MY,
            "SELECT CONVERT('Zq9wSecret' USING utf8mb4)",
            Z,
        ),
        c4(
            'a',
            "O33 my CAST CHARACTER SET",
            MY,
            "SELECT CAST('x' AS CHAR CHARACTER SET hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "O34 pg ::regclass",
            PG,
            "SELECT 'Zq9wSecret'::regclass",
            Z,
        ),
        c4(
            'v',
            "O35 pg ROW()::type",
            PG,
            "SELECT ROW(1,'Zq9wSecret')::mytype",
            Z,
        ),
        c4(
            'v',
            "O36 pg string_agg sep",
            PG,
            "SELECT string_agg(pw, 'Zq9wSecret' ORDER BY id) FROM t",
            Z,
        ),
        c4(
            'v',
            "O37 pg FILTER",
            PG,
            "SELECT count(*) FILTER (WHERE pw = 'Zq9wSecret') FROM t",
            Z,
        ),
        c4(
            'v',
            "O38 pg WITHIN GROUP ident",
            PG,
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "O39 my SEPARATOR",
            MY,
            "SELECT GROUP_CONCAT(a SEPARATOR 'Zq9wSecret') FROM t",
            Z,
        ),
        c4(
            'v',
            "O40 pg json_build_object",
            PG,
            "SELECT json_build_object('pw', 'Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "O41 pg jsonb_set",
            PG,
            "SELECT jsonb_set(data, '{pw}', '\"Zq9wSecret\"') FROM t",
            Z,
        ),
        c4(
            'v',
            "O42 pg xmlelement",
            PG,
            "SELECT xmlelement(name foo, 'Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "O43 pg DISTINCT ON",
            PG,
            "SELECT DISTINCT ON (pw = 'Zq9wSecret') a FROM t",
            Z,
        ),
        c4(
            'v',
            "O44 pg LIMIT/OFFSET idents",
            PG,
            "SELECT * FROM t LIMIT hunter2xyz OFFSET 'Zq9wSecret'",
            &["hunter2xyz", "Zq9wSecret"],
        ),
        c4(
            'v',
            "O45 pg FETCH FIRST ident",
            PG,
            "SELECT * FROM t FETCH FIRST hunter2xyz ROWS ONLY",
            H,
        ),
        // ---------- windows, CTEs, LATERAL, DML ----------
        c4(
            'v',
            "D01 pg window PARTITION BY",
            PG,
            "SELECT sum(a) OVER (PARTITION BY hunter2xyz ORDER BY b) FROM t",
            H,
        ),
        c4(
            'v',
            "D02 pg lag default",
            PG,
            "SELECT lag(pw, 1, 'Zq9wSecret') OVER w FROM t WINDOW w AS (ORDER BY id)",
            Z,
        ),
        c4(
            'v',
            "D03 pg CTE literal",
            PG,
            "WITH s AS (SELECT 'Zq9wSecret' AS pw) SELECT * FROM s",
            Z,
        ),
        c4(
            'v',
            "D04 pg data-modifying CTE upd",
            PG,
            "WITH u AS (UPDATE users SET pw = 'Zq9wSecret' RETURNING id) SELECT * FROM u",
            Z,
        ),
        c4(
            'v',
            "D05 pg data-modifying CTE ins",
            PG,
            "WITH u AS (INSERT INTO t (pw) VALUES (hunter2xyz) RETURNING *) SELECT * FROM u",
            H,
        ),
        c4(
            'v',
            "D06 pg LATERAL subselect",
            PG,
            "SELECT * FROM t, LATERAL (SELECT hunter2xyz) x",
            H,
        ),
        c4(
            'v',
            "D07 pg LATERAL function",
            PG,
            "SELECT * FROM t CROSS JOIN LATERAL crypt('Zq9wSecret', t.salt) AS c",
            Z,
        ),
        c4(
            'v',
            "D08 pg FROM function",
            PG,
            "SELECT * FROM generate_series(1, 3), md5('Zq9wSecret')",
            Z,
        ),
        c4(
            'a',
            "D09 pg INSERT SELECT col",
            PG,
            "INSERT INTO users (email, pw) SELECT email, hunter2xyz FROM staging",
            H,
        ),
        c4(
            'v',
            "D10 pg ON CONFLICT WHERE",
            PG,
            "INSERT INTO users (id, pw) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET pw = EXCLUDED.pw WHERE users.pw <> 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "D11 pg ON CONFLICT bare",
            PG,
            "INSERT INTO users (id, pw) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "D12 pg ON CONSTRAINT",
            PG,
            "INSERT INTO t VALUES (1) ON CONFLICT ON CONSTRAINT c DO UPDATE SET pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "D13 pg conflict target WHERE",
            PG,
            "INSERT INTO t VALUES (1) ON CONFLICT (k) WHERE pw = 'Zq9wSecret' DO NOTHING",
            Z,
        ),
        c4(
            'v',
            "D14 my REPLACE INTO",
            MY,
            "REPLACE INTO t VALUES ('Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "D15 my INSERT SET",
            MY,
            "INSERT INTO t SET pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "D16 my ODKU row alias",
            MY,
            "INSERT INTO t (a) VALUES (1) AS new ON DUPLICATE KEY UPDATE pw = new.pw, note = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "D17 pg MERGE",
            PG,
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET pw = 'Zq9wSecret' WHEN NOT MATCHED THEN INSERT VALUES (s.id, hunter2xyz)",
            &["Zq9wSecret", "hunter2xyz"],
        ),
        c4(
            'a',
            "D18 pg RETURNING ident",
            PG,
            "UPDATE t SET a = 1 RETURNING hunter2xyz",
            H,
        ),
        c4(
            'v',
            "D19 pg RETURNING expr",
            PG,
            "DELETE FROM t RETURNING pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "D20 pg UPDATE FROM subselect",
            PG,
            "UPDATE t SET pw = s.pw FROM (SELECT 'Zq9wSecret' AS pw) s WHERE t.id = 1",
            Z,
        ),
        c4(
            'v',
            "D21 pg DELETE USING",
            PG,
            "DELETE FROM t USING u WHERE t.id = u.id AND u.pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "D22 my multi-table UPDATE",
            MY,
            "UPDATE t JOIN u ON u.id = t.id SET t.pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "D23 my DELETE JOIN ON ident",
            MY,
            "DELETE t FROM t JOIN u ON u.k = hunter2xyz",
            H,
        ),
        c4(
            'a',
            "D24 pg UNION projections",
            PG,
            "SELECT 'Zq9wSecret' FROM t UNION SELECT hunter2xyz FROM u",
            H,
        ),
        c4(
            'v',
            "D25 pg UNION literal",
            PG,
            "SELECT 'Zq9wSecret' FROM t UNION SELECT hunter2xyz FROM u",
            Z,
        ),
        c4(
            'v',
            "D26 pg VALUES",
            PG,
            "VALUES ('Zq9wSecret'), (hunter2xyz)",
            &["Zq9wSecret", "hunter2xyz"],
        ),
        c4(
            'v',
            "D27 pg FROM VALUES",
            PG,
            "SELECT * FROM (VALUES ('Zq9wSecret')) AS v(pw)",
            Z,
        ),
        c4(
            'v',
            "D28 pg INSERT DEFAULT",
            PG,
            "INSERT INTO t VALUES (DEFAULT, 'Zq9wSecret') ON CONFLICT DO NOTHING",
            Z,
        ),
        c4(
            'v',
            "D29 my SELECT INTO OUTFILE",
            MY,
            "SELECT 'Zq9wSecret' INTO OUTFILE '/tmp/Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "D30 my LOCK IN SHARE MODE",
            MY,
            "SELECT * FROM t WHERE pw = 'Zq9wSecret' LOCK IN SHARE MODE",
            Z,
        ),
        c4(
            'v',
            "D31 my UPDATE hint + value",
            MY,
            "UPDATE /*+ SET_VAR(foreign_key_checks=OFF) */ users SET pw = 'Zq9wSecret'",
            Z,
        ),
        // ---------- functions with secret string args ----------
        c4(
            'v',
            "F01 pg CALL named =>",
            PG,
            "CALL set_pw(p_user => 'bob', p_pw => 'Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "F02 pg CALL named bare",
            PG,
            "CALL set_pw(p_pw => hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "F03 pg named :=",
            PG,
            "SELECT f(pw := 'Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "F04 pg DO block",
            PG,
            "DO $$ BEGIN PERFORM set_pw('Zq9wSecret'); END $$",
            Z,
        ),
        c4(
            'v',
            "F05 pg DO LANGUAGE",
            PG,
            "DO LANGUAGE plpgsql $body$ BEGIN RAISE NOTICE 'Zq9wSecret'; END $body$",
            Z,
        ),
        c4(
            'v',
            "F06 pg pgp_sym_encrypt",
            PG,
            "SELECT pgp_sym_encrypt('data', 'Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "F07 pg crypt",
            PG,
            "SELECT crypt('Zq9wSecret', gen_salt('bf'))",
            Z,
        ),
        c4(
            'v',
            "F08 my AES_ENCRYPT",
            MY,
            "SELECT AES_ENCRYPT('data', 'Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "F09 my AES nested",
            MY,
            "SELECT AES_ENCRYPT('data', UNHEX(SHA2('Zq9wSecret', 512)))",
            Z,
        ),
        c4(
            'v',
            "F10 pg dblink_connect",
            PG,
            "SELECT dblink_connect('host=h user=u password=Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "F11 pg dblink table fn",
            PG,
            "SELECT * FROM dblink('dbname=x password=Zq9wSecret', 'SELECT 1') AS t(a int)",
            Z,
        ),
        c4(
            'v',
            "F12 pg current_setting",
            PG,
            "SELECT current_setting('app.Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "F13 pg EXECUTE params",
            PG,
            "EXECUTE q('Zq9wSecret', hunter2xyz)",
            &["Zq9wSecret", "hunter2xyz"],
        ),
        c4(
            'v',
            "F14 pg PREPARE body",
            PG,
            "PREPARE q(text) AS UPDATE users SET pw = $1 WHERE note = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "F15 my PREPARE FROM",
            MY,
            "PREPARE s FROM 'UPDATE users SET pw = ''Zq9wSecret'''",
            Z,
        ),
        c4(
            'v',
            "F16 my SELECT @v :=",
            MY,
            "SELECT @pw := 'Zq9wSecret'",
            Z,
        ),
        // ---------- SET / DDL ----------
        c4('v', "E01 my SET @v", MY, "SET @pw = 'Zq9wSecret'", Z),
        c4('v', "E02 my SET @v :=", MY, "SET @pw := 'Zq9wSecret'", Z),
        c4(
            'v',
            "E03 my SET @@session",
            MY,
            "SET @@session.sql_mode = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "E04 my SET GLOBAL init_connect",
            MY,
            "SET GLOBAL init_connect = 'SET @x = ''Zq9wSecret'''",
            Z,
        ),
        c4(
            'v',
            "E05 my SET NAMES COLLATE",
            MY,
            "SET NAMES utf8mb4 COLLATE 'Zq9wSecret'",
            Z,
        ),
        c4(
            'a',
            "E06 my SET CHARACTER SET",
            MY,
            "SET CHARACTER SET hunter2xyz",
            H,
        ),
        c4(
            'v',
            "E07 pg SET search_path",
            PG,
            "SET search_path TO 'Zq9wSecret', public",
            Z,
        ),
        c4(
            'v',
            "E08 pg SET LOCAL",
            PG,
            "SET LOCAL app.pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "E09 pg SET TIME ZONE",
            PG,
            "SET TIME ZONE 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "E10 pg SET TRANSACTION SNAPSHOT",
            PG,
            "SET TRANSACTION SNAPSHOT 'Zq9wSecret'",
            Z,
        ),
        c4('a', "E11 pg RESET name", PG, "RESET app.hunter2xyz", H),
        c4('a', "E12 pg SHOW name", PG, "SHOW app.hunter2xyz", H),
        c4(
            'v',
            "E13 pg CREATE VIEW literal",
            PG,
            "CREATE VIEW v AS SELECT 'Zq9wSecret' AS pw",
            Z,
        ),
        c4(
            'v',
            "E14 pg MATERIALIZED VIEW",
            PG,
            "CREATE MATERIALIZED VIEW mv AS SELECT * FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "E15 pg CTAS",
            PG,
            "CREATE TABLE t AS SELECT 'Zq9wSecret' AS pw",
            Z,
        ),
        c4(
            'v',
            "E16 pg GENERATED AS",
            PG,
            "CREATE TABLE t (pw text GENERATED ALWAYS AS ('Zq9wSecret') STORED)",
            Z,
        ),
        c4(
            'v',
            "E17 pg WITH options",
            PG,
            "CREATE TABLE t (pw text) WITH (fillfactor = 70, foo = 'Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "E18 pg PARTITION OF",
            PG,
            "CREATE TABLE t PARTITION OF p FOR VALUES IN ('Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "E19 pg partial index",
            PG,
            "CREATE INDEX i ON t (pw) WHERE pw <> 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "E20 pg expression index",
            PG,
            "CREATE INDEX i ON t ((pw || 'Zq9wSecret'))",
            Z,
        ),
        c4(
            'a',
            "E21 pg INCLUDE",
            PG,
            "CREATE INDEX i ON t (lower(pw)) INCLUDE (hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "E22 pg ADD CHECK",
            PG,
            "ALTER TABLE t ADD CONSTRAINT c CHECK (pw <> 'Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "E23 pg SET DEFAULT",
            PG,
            "ALTER TABLE t ALTER COLUMN pw SET DEFAULT 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "E24 pg TYPE USING",
            PG,
            "ALTER TABLE t ALTER COLUMN pw TYPE varchar(20) USING 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "E25 my ALTER SET DEFAULT ident",
            MY,
            "ALTER TABLE t ALTER COLUMN c SET DEFAULT hunter2xyz",
            H,
        ),
        c4(
            'a',
            "E26 pg OWNER TO",
            PG,
            "ALTER TABLE t OWNER TO hunter2xyz",
            H,
        ),
        c4('a', "E27 my USE", MY, "USE hunter2xyz", H),
        c4(
            'v',
            "E28 my DEFAULT (ident)",
            MY,
            "CREATE TABLE t (a INT DEFAULT (hunter2xyz))",
            H,
        ),
        c4(
            'a',
            "E29 my CHARACTER SET col",
            MY,
            "CREATE TABLE t (pw VARCHAR(10) CHARACTER SET hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "E30 my ENGINE=word",
            MY,
            "CREATE TABLE t (a INT) ENGINE=hunter2xyz",
            H,
        ),
        c4(
            'a',
            "E31 pg COLLATE col",
            PG,
            "CREATE TABLE t (pw text COLLATE \"hunter2xyz\")",
            H,
        ),
        c4(
            'v',
            "E32 pg EXPLAIN",
            PG,
            "EXPLAIN SELECT * FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "E33 pg EXPLAIN options UPDATE",
            PG,
            "EXPLAIN (ANALYZE, FORMAT JSON) UPDATE t SET pw = hunter2xyz",
            H,
        ),
        c4('a', "E34 pg SAVEPOINT", PG, "SAVEPOINT hunter2xyz", H),
        c4(
            'a',
            "E35 pg FOR UPDATE OF",
            PG,
            "SELECT * FROM t FOR UPDATE OF hunter2xyz NOWAIT",
            H,
        ),
        c4(
            'a',
            "E36 my PARTITION()",
            MY,
            "SELECT * FROM t PARTITION (hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "E37 my USE INDEX",
            MY,
            "SELECT * FROM t USE INDEX (hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "E38 my GRANT host",
            MY,
            "GRANT SELECT ON db.* TO 'bob'@'Zq9wSecret'",
            Z,
        ),
        c4(
            'a',
            "E39 pg GRANTED BY",
            PG,
            "GRANT SELECT ON t TO bob WITH GRANT OPTION GRANTED BY hunter2xyz",
            H,
        ),
        c4(
            'v',
            "E40 pg SELECT 1 AS \"secret\"",
            PG,
            "SELECT 'Zq9wSecret' AS \"pw\"",
            Z,
        ),
        c4(
            'v',
            "E41 pg comment before",
            PG,
            "/* Zq9wSecret */ SELECT 1",
            Z,
        ),
        c4(
            'v',
            "E42 pg comment inside",
            PG,
            "SELECT /* Zq9wSecret */ 1",
            Z,
        ),
        c4(
            'v',
            "E43 pg trailing -- comment",
            PG,
            "SELECT 1 FROM t WHERE pw = 'a' -- Zq9wSecret",
            Z,
        ),
        c4(
            'v',
            "E44 pg long literal",
            PG,
            &format!(
                "SELECT * FROM t WHERE pw = '{}Zq9wSecret'",
                "x".repeat(4000)
            ),
            Z,
        ),
        // ---------- password contexts (Full must also be clean) ----------
        c4(
            'p',
            "P01 pg PASSWORD E''",
            PG,
            r"ALTER ROLE alice WITH PASSWORD E'hunter2xyz\'tail9Q'",
            &["hunter2xyz", "tail9Q"],
        ),
        c4(
            'p',
            "P02 pg PASSWORD $$",
            PG,
            "ALTER ROLE alice PASSWORD $$hunter2xyz$$",
            H,
        ),
        c4(
            'p',
            "P03 pg PASSWORD $p$",
            PG,
            "ALTER ROLE alice PASSWORD $p$hunter2xyz$p$",
            H,
        ),
        c4(
            'p',
            "P04 pg PASSWORD U&''",
            PG,
            "ALTER ROLE alice PASSWORD U&'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P05 pg PASSWORD ''",
            PG,
            "CREATE ROLE a LOGIN PASSWORD 'x''hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P06 my IDENTIFIED BY \\'",
            MY,
            r"ALTER USER u IDENTIFIED BY 'a\'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P07 my IDENTIFIED BY \"\"",
            MY,
            "ALTER USER u IDENTIFIED BY \"hunter2xyz\"",
            H,
        ),
        c4(
            'p',
            "P08 my IDENTIFIED BY _utf8mb4",
            MY,
            "CREATE USER u IDENTIFIED BY _utf8mb4'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P09 my IDENTIFIED BY X''",
            MY,
            "CREATE USER u IDENTIFIED BY X'68756e746572'",
            &["68756e746572"],
        ),
        c4(
            'p',
            "P10 my IDENTIFIED WITH BY",
            MY,
            "ALTER USER u IDENTIFIED WITH caching_sha2_password BY 'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P11 my IDENTIFIED WITH AS",
            MY,
            "ALTER USER u IDENTIFIED WITH mysql_native_password AS '*ABCDEF0123hash'",
            &["ABCDEF0123hash"],
        ),
        c4(
            'p',
            "P12 my SET PASSWORD FOR",
            MY,
            "SET PASSWORD FOR u = 'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P13 my comments in IDENTIFIED",
            MY,
            "ALTER USER /* c */ u IDENTIFIED /* c */ BY 'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P14 my REPLACE current pw",
            MY,
            "ALTER USER u IDENTIFIED BY 'hunter2xyz' REPLACE 'Other9Secret'",
            &["hunter2xyz", "Other9Secret"],
        ),
        c4(
            'p',
            "P15 my SET PASSWORD = PASSWORD()",
            MY,
            "SET PASSWORD = PASSWORD('hunter2xyz')",
            H,
        ),
        c4(
            'p',
            "P16 pg USER MAPPING",
            PG,
            "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'u', password 'hunter2xyz')",
            H,
        ),
        c4(
            'p',
            "P17 pg dblink conninfo",
            PG,
            "SELECT dblink_connect('myconn', 'host=h password=hunter2xyz')",
            H,
        ),
        c4(
            'p',
            "P18 pg COPY PROGRAM curl",
            PG,
            "COPY t FROM PROGRAM 'curl -u u:hunter2xyz http://x'",
            H,
        ),
        c4(
            'p',
            "P19 my SOURCE_PASSWORD \"\"",
            MY,
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = \"hunter2xyz\"",
            H,
        ),
        c4(
            'p',
            "P20 my START REPLICA PASSWORD",
            MY,
            "START REPLICA USER = 'u' PASSWORD = 'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P21 my CREATE SERVER OPTIONS",
            MY,
            "CREATE SERVER s FOREIGN DATA WRAPPER mysql OPTIONS (USER 'u', PASSWORD 'hunter2xyz')",
            H,
        ),
        c4(
            'p',
            "P22 pg VALID UNTIL",
            PG,
            "ALTER ROLE alice PASSWORD 'hunter2xyz' VALID UNTIL 'infinity'",
            H,
        ),
        c4(
            'p',
            "P23 pg SCRAM verifier",
            PG,
            "ALTER ROLE \"alice\" WITH ENCRYPTED PASSWORD 'SCRAM-SHA-256$4096:salt$hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P24 my GRANT IDENTIFIED BY",
            MY,
            "GRANT ALL ON *.* TO 'u'@'%' IDENTIFIED BY 'hunter2xyz' WITH GRANT OPTION",
            H,
        ),
        c4(
            'p',
            "P25 pg ssl_passphrase_command",
            PG,
            "ALTER SYSTEM SET ssl_passphrase_command = 'echo hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P26 my PASSWORD EXPIRE",
            MY,
            "CREATE USER u IDENTIFIED BY 'hunter2xyz' PASSWORD EXPIRE",
            H,
        ),
        c4(
            'p',
            "P27 my multi SET PASSWORD",
            MY,
            "ALTER USER u IDENTIFIED BY 'a'; SET PASSWORD = 'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P28 pg PASSWORD hint",
            PG,
            "ALTER ROLE alice /*+ x */ PASSWORD 'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P29 pg CREATE USER quoted name",
            PG,
            "CREATE USER \"a;b\" PASSWORD 'hunter2xyz'",
            H,
        ),
        c4(
            'p',
            "P30 my IDENTIFIED BY N''",
            MY,
            "CREATE USER u IDENTIFIED BY N'hunter2xyz'",
            H,
        ),
        // ---------- Redis normalized ----------
        c4(
            'v',
            "R01 CONFIG SET known param",
            RD,
            "CONFIG SET maxmemory-policy hunter2xyz",
            H,
        ),
        c4(
            'v',
            "R02 CONFIG SET unknown param",
            RD,
            "CONFIG SET hunter2xyz 1",
            H,
        ),
        c4(
            'p',
            "R03 RESP count mismatch",
            RD,
            "*3\r\n$4\r\nAUTH\r\n$10\r\nhunter2xyz\r\n",
            H,
        ),
        c4(
            'p',
            "R04 RESP null bulk",
            RD,
            "*3\r\n$4\r\nAUTH\r\n$-1\r\n$10\r\nhunter2xyz\r\n",
            H,
        ),
        c4(
            'p',
            "R05 config set MasterAuth",
            RD,
            "config set MasterAuth hunter2xyz",
            H,
        ),
        c4(
            'p',
            "R06 CONFIG SET multi pairs",
            RD,
            "CONFIG SET save \"900 1\" requirepass hunter2xyz",
            H,
        ),
        c4(
            'p',
            "R07 tls-key-file-pass",
            RD,
            "CONFIG SET tls-key-file-pass hunter2xyz",
            H,
        ),
        c4(
            'p',
            "R08 masteruser+masterauth",
            RD,
            "CONFIG SET masteruser bob masterauth hunter2xyz",
            H,
        ),
        c4(
            'p',
            "R09 RESP negative array",
            RD,
            "*-1\r\nAUTH hunter2xyz",
            H,
        ),
        c4(
            'p',
            "R10 RESP $ len too short",
            RD,
            "*2\r\n$4\r\nAUTH\r\n$3\r\nhunter2xyz\r\n",
            H,
        ),
        c4(
            'v',
            "R11 CONFIG SET param case",
            RD,
            "CONFIG SET MAXMEMORY hunter2xyz",
            H,
        ),
        c4(
            'v',
            "R12 CONFIG SET glob param",
            RD,
            "CONFIG SET maxmemory* hunter2xyz",
            H,
        ),
    ];

    // ---------- fallback boundary ----------
    for (n, tag) in [
        (497, "B01 1996 tokens"),
        (498, "B02 2000 tokens"),
        (499, "B03 2004 tokens"),
    ] {
        let s = format!(
            "SELECT * FROM t WHERE {}note = hunter2xyz",
            "a = 1 AND ".repeat(n)
        );
        v.push(Case4 {
            tag: tag.to_string(),
            d: PG,
            stmt: s,
            secrets: H,
            k: 'v',
        });
        let s = format!(
            "SELECT * FROM t WHERE {}note = 'x'::foo('Zq9wSecret')",
            "a = 1 AND ".repeat(n)
        );
        v.push(Case4 {
            tag: format!("{tag} + typmod"),
            d: PG,
            stmt: s,
            secrets: Z,
            k: 'v',
        });
    }
    for k in [30, 60, 63, 64, 65, 200] {
        let s = format!(
            "SELECT * FROM t WHERE note = {}hunter2xyz{}",
            "(".repeat(k),
            ")".repeat(k)
        );
        v.push(Case4 {
            tag: format!("B04 depth {k}"),
            d: PG,
            stmt: s,
            secrets: H,
            k: 'v',
        });
        let s = format!(
            "SELECT {}'x'::foo('Zq9wSecret'){} FROM t",
            "(".repeat(k),
            ")".repeat(k)
        );
        v.push(Case4 {
            tag: format!("B05 depth {k} typmod"),
            d: PG,
            stmt: s,
            secrets: Z,
            k: 'v',
        });
    }
    v
}

/// The fourth audit's 60 ordinary statements.
fn fourth_ordinary() -> Vec<(SqlDialect, &'static str)> {
    vec![
        (
            PG,
            "WITH RECURSIVE tree AS (SELECT id, parent_id FROM nodes WHERE parent_id IS NULL UNION ALL SELECT n.id, n.parent_id FROM nodes n JOIN tree t ON n.parent_id = t.id) SELECT * FROM tree",
        ),
        (
            PG,
            "SELECT user_id, row_number() OVER (PARTITION BY user_id ORDER BY created_at DESC) AS rn FROM orders",
        ),
        (
            PG,
            "SELECT id, sum(total) OVER (ORDER BY created_at ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) FROM orders",
        ),
        (
            PG,
            "INSERT INTO users (id, email) VALUES (1, 'a@x') ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email RETURNING id",
        ),
        (
            PG,
            "INSERT INTO archive (id, payload) SELECT id, payload FROM events WHERE created_at < now() - interval '30 days'",
        ),
        (
            PG,
            "WITH moved AS (DELETE FROM events WHERE created_at < now() - interval '30 days' RETURNING *) INSERT INTO archive SELECT * FROM moved",
        ),
        (
            PG,
            "SELECT u.id, l.last_order FROM users u CROSS JOIN LATERAL (SELECT max(created_at) AS last_order FROM orders o WHERE o.user_id = u.id) l",
        ),
        (
            PG,
            "SELECT data->'address'->>'city' AS city, count(*) FROM customers GROUP BY 1 ORDER BY 2 DESC",
        ),
        (
            PG,
            "SELECT * FROM docs WHERE tags @> ARRAY['urgent'] AND data ? 'owner'",
        ),
        (
            PG,
            "SELECT id FROM t WHERE created_at BETWEEN '2024-01-01' AND '2024-12-31'",
        ),
        (
            PG,
            "SELECT date_trunc('month', created_at) AS month, count(*) FROM signups GROUP BY month ORDER BY month",
        ),
        (
            PG,
            "UPDATE accounts SET balance = balance - 100 WHERE id = 1 RETURNING balance",
        ),
        (
            PG,
            "DELETE FROM sessions s USING users u WHERE s.user_id = u.id AND u.disabled",
        ),
        (
            PG,
            "SELECT coalesce(nickname, name) AS display FROM users WHERE id = ANY($1)",
        ),
        (
            PG,
            "SELECT * FROM orders WHERE status = $1 AND created_at > $2 LIMIT $3",
        ),
        (
            PG,
            "CREATE TABLE IF NOT EXISTS invoices (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, customer_id bigint REFERENCES customers (id), amount numeric(12, 2) NOT NULL)",
        ),
        (
            PG,
            "ALTER TABLE invoices ADD CONSTRAINT amount_positive CHECK (amount > 0)",
        ),
        (
            PG,
            "CREATE UNIQUE INDEX users_email_idx ON users (lower(email))",
        ),
        (
            PG,
            "CREATE VIEW active_users AS SELECT id, email FROM users WHERE deleted_at IS NULL",
        ),
        (PG, "SELECT EXISTS (SELECT 1 FROM users WHERE email = 'x')"),
        (
            PG,
            "SELECT id, CASE status WHEN 'a' THEN 1 WHEN 'b' THEN 2 ELSE 0 END FROM t",
        ),
        (
            PG,
            "SELECT t.id, array_agg(tg.name ORDER BY tg.name) FROM t JOIN tags tg ON tg.t_id = t.id GROUP BY t.id",
        ),
        (
            PG,
            "SELECT count(*) FILTER (WHERE status = 'paid') AS paid FROM orders",
        ),
        (PG, "SELECT * FROM generate_series(1, 10) AS g(n)"),
        (PG, "SELECT extract(epoch FROM now())"),
        (PG, "BEGIN"),
        (PG, "COMMIT"),
        (PG, "SET statement_timeout = '5s'"),
        (PG, "SHOW search_path"),
        (PG, "TRUNCATE TABLE logs RESTART IDENTITY"),
        (PG, "DROP TABLE IF EXISTS tmp_import"),
        (
            PG,
            "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM orders WHERE user_id = 7",
        ),
        (
            PG,
            "SELECT o.id FROM orders o WHERE NOT EXISTS (SELECT 1 FROM refunds r WHERE r.order_id = o.id)",
        ),
        (
            PG,
            "SELECT name FROM users ORDER BY name NULLS LAST OFFSET 20 FETCH FIRST 10 ROWS ONLY",
        ),
        (
            PG,
            "MERGE INTO stock s USING deliveries d ON s.item = d.item WHEN MATCHED THEN UPDATE SET qty = s.qty + d.qty",
        ),
        (
            MY,
            "SELECT `o`.`id`, `c`.`name` FROM `orders` `o` INNER JOIN `customers` `c` ON `c`.`id` = `o`.`customer_id`",
        ),
        (MY, "INSERT INTO events (type, at) VALUES ('login', NOW())"),
        (
            MY,
            "INSERT INTO t (a, b) VALUES (1, 2) AS new ON DUPLICATE KEY UPDATE b = new.b",
        ),
        (MY, "REPLACE INTO kv (k, v) VALUES ('a', 'b')"),
        (
            MY,
            "UPDATE orders o JOIN customers c ON c.id = o.customer_id SET o.region = c.region WHERE o.region IS NULL",
        ),
        (
            MY,
            "DELETE FROM logs WHERE created_at < DATE_SUB(NOW(), INTERVAL 30 DAY) LIMIT 1000",
        ),
        (
            MY,
            "SELECT JSON_EXTRACT(payload, '$.user.id') AS uid FROM events",
        ),
        (MY, "SELECT payload->>'$.type' FROM events WHERE id = 3"),
        (
            MY,
            "SELECT GROUP_CONCAT(name ORDER BY name SEPARATOR ', ') FROM tags",
        ),
        (MY, "SELECT * FROM t WHERE a <=> NULL"),
        (MY, "SELECT SQL_CALC_FOUND_ROWS * FROM t LIMIT 10"),
        (
            MY,
            "SELECT id FROM users WHERE MATCH(bio) AGAINST ('rust' IN NATURAL LANGUAGE MODE)",
        ),
        (
            MY,
            "CREATE TABLE logs (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, msg TEXT, created_at DATETIME DEFAULT CURRENT_TIMESTAMP) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
        ),
        (MY, "ALTER TABLE logs ADD INDEX idx_created (created_at)"),
        (MY, "SET @rank = 0"),
        (MY, "SET SESSION sql_mode = 'STRICT_ALL_TABLES'"),
        (MY, "SHOW COLUMNS FROM users"),
        (MY, "SHOW DATABASES"),
        (MY, "USE shop"),
        (MY, "SELECT a, COUNT(*) FROM t GROUP BY a WITH ROLLUP"),
        (MY, "START TRANSACTION"),
        (
            MY,
            "SELECT id FROM t WHERE created_at >= CURDATE() - INTERVAL 7 DAY ORDER BY id DESC",
        ),
        (MY, "SELECT CAST(amount AS DECIMAL(10, 2)) FROM payments"),
        (
            MY,
            "SELECT * FROM t1 LEFT JOIN t2 USING (id) WHERE t2.id IS NULL",
        ),
        (
            MY,
            "SELECT IF(score > 50, 'pass', 'fail') AS result FROM exams",
        ),
    ]
}

/// Records audit cases of the fourth and fifth rounds at every level and
/// returns the leaks: a secret at `none` or `normalized` (unless the case is
/// an accepted name position), at `full` in a password context, or
/// `[redacted]` in normalized text.
fn audit4_leaks(cases: &[Case4]) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("audit"));
    let mut leaks = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        for level in [TextLevel::None, TextLevel::Normalized, TextLevel::Full] {
            recorder.set_text_level(level);
            let connection = format!("d{i}-{level}");
            recorder.record(
                ActivityEvent::query_executed(&case.stmt, case.d, "x", Default::default(), 1)
                    .connection(connection.as_str())
                    .build()?,
            )?;
            let event = recorder.query_history(&connection, 1)?.remove(0);
            let stored = event.details.to_string();
            let hit = case.secrets.iter().any(|s| stored.contains(s));
            let marker = level == TextLevel::Normalized && stored.contains("[redacted]");
            let accepted = level == TextLevel::Normalized && case.k == 'a';
            let checked = match level {
                TextLevel::None | TextLevel::Normalized => true,
                TextLevel::Full => case.k == 'p',
            };
            if checked && ((hit && !accepted) || marker) {
                leaks.push(format!("{} at {level}: {stored}", case.tag));
            }
        }
    }
    Ok(leaks)
}

#[test]
fn no_secret_of_the_fourth_audit_reaches_the_log() -> TestResult {
    let cases = fourth_audit_cases();
    assert_eq!(cases.len(), 320);
    let found = audit4_leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:\n{}",
        found.len(),
        found.join("\n")
    );
    Ok(())
}

#[test]
fn fourth_audit_ordinary_statements_are_not_redacted() {
    for (dialect, statement) in fourth_ordinary() {
        let normalized =
            prepare_statement_text(statement, dialect, TextLevel::Normalized).unwrap_or_default();
        let full = prepare_statement_text(statement, dialect, TextLevel::Full);
        assert!(
            !normalized.contains("[redacted]"),
            "{statement} => {normalized}"
        );
        assert_eq!(full.as_deref(), Some(statement));
    }
}

/// p99 of normalizing a mix of 1,000 statements (ordinary and audit
/// inputs). The target is 1 ms in release builds; debug builds get a
/// generous bound. The timing is printed either way.
#[test]
fn normalization_p99_stays_low() {
    let ordinary = fourth_ordinary();
    let cases = fourth_audit_cases();
    let pool: Vec<(SqlDialect, String)> = ordinary
        .iter()
        .map(|(d, s)| (*d, (*s).to_string()))
        .chain(
            cases
                .iter()
                .filter(|c| c.stmt.len() < 5000)
                .map(|c| (c.d, c.stmt.clone())),
        )
        .collect();
    let mix: Vec<&(SqlDialect, String)> = (0..1000).map(|i| &pool[(i * 7) % pool.len()]).collect();
    for (d, s) in &mix {
        let _ = prepare_statement_text(s, *d, TextLevel::Normalized);
    }
    let mut times: Vec<std::time::Duration> = mix
        .iter()
        .map(|(d, s)| {
            let start = std::time::Instant::now();
            let _ = prepare_statement_text(s, *d, TextLevel::Normalized);
            start.elapsed()
        })
        .collect();
    times.sort_unstable();
    let p99 = times[989];
    println!(
        "normalized 1,000-statement mix: p50={:?} p99={p99:?}",
        times[499]
    );
    let bound = if cfg!(debug_assertions) {
        std::time::Duration::from_millis(20)
    } else {
        std::time::Duration::from_millis(1)
    };
    assert!(p99 < bound, "p99 {p99:?} over {bound:?}");
}

const HZ: &[&str] = &["hunter2xyz", "Zq9wSecret"];
const ZS: &[&str] = &["Zq9w Secret"];

/// The fifth audit's 248 inputs.
fn fifth_audit_cases() -> Vec<Case4> {
    vec![
        // ---------- date/time fields held as identifiers ----------
        c4(
            'v',
            "D01 pg CEIL TO word",
            PG,
            "SELECT CEIL(ts TO hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "D02 pg FLOOR TO word",
            PG,
            "SELECT FLOOR(ts TO hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "D03 my CEIL TO word",
            MY,
            "SELECT CEIL(ts TO hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "D04 pg CEIL TO string",
            PG,
            "SELECT CEIL(ts TO 'Zq9wSecret') FROM t",
            Z,
        ),
        c4(
            'v',
            "D05 pg CEIL TO in WHERE",
            PG,
            "SELECT * FROM t WHERE pw = FLOOR(ts TO hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "D06 pg INTERVAL unit in WHERE",
            PG,
            "SELECT * FROM t WHERE d > now() - INTERVAL '1' hunter2xyz",
            H,
        ),
        c4(
            'v',
            "D07 pg EXTRACT in WHERE",
            PG,
            "SELECT * FROM t WHERE pw = extract(hunter2xyz FROM ts)",
            H,
        ),
        c4(
            'v',
            "D08 pg CEIL TO keyword-shaped",
            PG,
            "SELECT CEIL(ts TO Login) FROM t",
            &["Login"],
        ),
        c4(
            'v',
            "D09 pg CEIL TO quoted",
            PG,
            "SELECT CEIL(ts TO \"Zq9wSecret\") FROM t",
            Z,
        ),
        c4(
            'v',
            "D10 pg EXTRACT quoted ident",
            PG,
            "SELECT extract(\"Zq9wSecret\" FROM ts) FROM t",
            Z,
        ),
        // ---------- function arguments ----------
        c4(
            'a',
            "F01 pg named arg name =>",
            PG,
            "SELECT f(hunter2xyz => 1)",
            H,
        ),
        c4(
            'v',
            "F02 pg named arg value =>",
            PG,
            "SELECT f(a => hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "F03 pg named arg value :=",
            PG,
            "SELECT f(a := hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "F04 pg aliased fn arg no AS",
            PG,
            "SELECT f(1 hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "F05 pg aliased fn arg AS",
            PG,
            "SELECT f(1 AS hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "F06 pg json_build_object",
            PG,
            "SELECT json_build_object('k', hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "F07 my JSON_OBJECT bare",
            MY,
            "SELECT JSON_OBJECT('pw', hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "F08 pg string_agg sep word",
            PG,
            "SELECT string_agg(a, hunter2xyz ORDER BY b) FROM t",
            H,
        ),
        c4(
            'v',
            "F09 my GROUP_CONCAT sep word",
            MY,
            "SELECT GROUP_CONCAT(a SEPARATOR hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "F10 pg FILTER WHERE",
            PG,
            "SELECT count(*) FILTER (WHERE pw = hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "F11 pg WITHIN GROUP",
            PG,
            "SELECT percentile_cont(hunter2xyz) WITHIN GROUP (ORDER BY a) FROM t",
            H,
        ),
        c4(
            'v',
            "F12 pg position/trim/substring",
            PG,
            "SELECT position(hunter2xyz IN pw), trim(BOTH hunter2xyz FROM pw), substring(pw FROM hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "F13 pg overlay",
            PG,
            "SELECT overlay(pw PLACING hunter2xyz FROM 1) FROM t",
            H,
        ),
        c4(
            'v',
            "F14 pg AT TIME ZONE word",
            PG,
            "SELECT * FROM t WHERE ts = 'a' AT TIME ZONE hunter2xyz",
            H,
        ),
        c4(
            'v',
            "F15 pg xmlelement name",
            PG,
            "SELECT xmlelement(name hunter2xyz, 'Zq9wSecret')",
            HZ,
        ),
        c4(
            'a',
            "F16 my MATCH columns",
            MY,
            "SELECT * FROM t WHERE MATCH (hunter2xyz) AGAINST ('x')",
            H,
        ),
        c4(
            'v',
            "F17 pg CALL named arg",
            PG,
            "CALL p(pw => hunter2xyz)",
            H,
        ),
        c4('v', "F18 my CALL bare arg", MY, "CALL p(hunter2xyz)", H),
        c4(
            'v',
            "F19 pg table fn named arg",
            PG,
            "SELECT * FROM f(pw => hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "F20 pg generate_series word",
            PG,
            "SELECT * FROM generate_series(1, hunter2xyz)",
            H,
        ),
        // ---------- JSON, arrays, rows, VALUES ----------
        c4(
            'v',
            "J01 pg -> word",
            PG,
            "SELECT data->hunter2xyz FROM t",
            H,
        ),
        c4(
            'v',
            "J02 pg ->> word",
            PG,
            "SELECT * FROM t WHERE data->>hunter2xyz = 'x'",
            H,
        ),
        c4(
            'v',
            "J03 pg subscript string",
            PG,
            "SELECT data['Zq9wSecret'] FROM t",
            Z,
        ),
        c4(
            'v',
            "J04 pg subscript word",
            PG,
            "SELECT data[hunter2xyz] FROM t",
            H,
        ),
        c4(
            'a',
            "J05 pg composite field",
            PG,
            "SELECT (data).hunter2xyz FROM t",
            H,
        ),
        c4(
            'v',
            "J06 pg ARRAY[...] mixed",
            PG,
            "SELECT ARRAY[hunter2xyz, 'Zq9wSecret']",
            HZ,
        ),
        c4(
            'v',
            "J07 pg ANY(ARRAY[word])",
            PG,
            "SELECT * FROM t WHERE pw = ANY(ARRAY[hunter2xyz])",
            H,
        ),
        c4('v', "J08 pg ROW(word)", PG, "SELECT ROW(hunter2xyz, 1)", H),
        c4(
            'v',
            "J09 pg row compare",
            PG,
            "SELECT * FROM t WHERE (a, pw) = (1, hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "J10 my ROW compare",
            MY,
            "SELECT * FROM t WHERE (a, pw) = ROW(1, hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "J11 pg VALUES bare words",
            PG,
            "VALUES (hunter2xyz), ('Zq9wSecret')",
            HZ,
        ),
        c4(
            'v',
            "J12 my VALUES ROW(word)",
            MY,
            "INSERT INTO t (a) VALUES ROW(hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "J13 my INSERT SET word",
            MY,
            "INSERT INTO t SET pw = hunter2xyz",
            H,
        ),
        c4(
            'a',
            "J14 my INSERT row alias",
            MY,
            "INSERT INTO t (pw) VALUES (1) AS hunter2xyz",
            H,
        ),
        c4(
            'v',
            "J15 my ODKU word",
            MY,
            "INSERT INTO t (pw) VALUES (1) ON DUPLICATE KEY UPDATE pw = hunter2xyz",
            H,
        ),
        c4(
            'a',
            "J16 pg INSERT table alias",
            PG,
            "INSERT INTO t AS hunter2xyz (pw) VALUES (1)",
            H,
        ),
        c4(
            'v',
            "J17 pg unnest ARRAY word",
            PG,
            "SELECT * FROM unnest(ARRAY[hunter2xyz]) AS u(x)",
            H,
        ),
        c4(
            'v',
            "J18 my VALUES list words",
            MY,
            "INSERT INTO t (a, b) VALUES (hunter2xyz, `Zq9wSecret`)",
            HZ,
        ),
        // ---------- ON CONFLICT ----------
        c4(
            'a',
            "C01 pg conflict target col",
            PG,
            "INSERT INTO t (pw) VALUES (1) ON CONFLICT (hunter2xyz) DO NOTHING",
            H,
        ),
        c4(
            'a',
            "C02 pg ON CONSTRAINT",
            PG,
            "INSERT INTO t (pw) VALUES (1) ON CONFLICT ON CONSTRAINT hunter2xyz DO NOTHING",
            H,
        ),
        c4(
            'v',
            "C03 pg DO UPDATE SET word",
            PG,
            "INSERT INTO t (pw) VALUES (1) ON CONFLICT (id) DO UPDATE SET pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "C04 pg DO UPDATE WHERE word",
            PG,
            "INSERT INTO t (pw) VALUES (1) ON CONFLICT (id) DO UPDATE SET pw = 'x' WHERE t.pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "C05 pg conflict target WHERE",
            PG,
            "INSERT INTO t (pw) VALUES (1) ON CONFLICT (id) WHERE pw = hunter2xyz DO NOTHING",
            H,
        ),
        c4(
            'v',
            "C06 pg DO UPDATE excluded",
            PG,
            "INSERT INTO t (pw) VALUES (hunter2xyz) ON CONFLICT (id) DO UPDATE SET pw = EXCLUDED.pw",
            H,
        ),
        // ---------- LIMIT / OFFSET / FETCH / locks ----------
        c4(
            'v',
            "L01 pg LIMIT word",
            PG,
            "SELECT * FROM t LIMIT hunter2xyz",
            H,
        ),
        c4(
            'v',
            "L02 pg OFFSET word",
            PG,
            "SELECT * FROM t OFFSET hunter2xyz",
            H,
        ),
        c4(
            'v',
            "L03 my LIMIT a, word",
            MY,
            "SELECT * FROM t LIMIT 1, hunter2xyz",
            H,
        ),
        c4(
            'v',
            "L04 pg FETCH FIRST word",
            PG,
            "SELECT * FROM t FETCH FIRST hunter2xyz ROWS ONLY",
            H,
        ),
        c4(
            'v',
            "L05 my UPDATE LIMIT word",
            MY,
            "UPDATE t SET a = 1 LIMIT hunter2xyz",
            H,
        ),
        c4(
            'v',
            "L06 my DELETE ORDER BY LIMIT",
            MY,
            "DELETE FROM t ORDER BY a LIMIT hunter2xyz",
            H,
        ),
        c4(
            'a',
            "L07 pg FOR UPDATE OF",
            PG,
            "SELECT * FROM t FOR UPDATE OF hunter2xyz",
            H,
        ),
        c4(
            'v',
            "L08 my OFFSET word",
            MY,
            "SELECT * FROM t LIMIT 1 OFFSET hunter2xyz",
            H,
        ),
        // ---------- GROUPING SETS / window ----------
        c4(
            'v',
            "G01 pg GROUPING SETS",
            PG,
            "SELECT a FROM t GROUP BY GROUPING SETS ((a), (hunter2xyz))",
            H,
        ),
        c4(
            'v',
            "G02 pg ROLLUP",
            PG,
            "SELECT a FROM t GROUP BY ROLLUP (a, hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "G03 pg CUBE",
            PG,
            "SELECT a FROM t GROUP BY CUBE (hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "G04 my WITH ROLLUP HAVING",
            MY,
            "SELECT a FROM t GROUP BY a WITH ROLLUP HAVING a = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "W01 pg frame offset word",
            PG,
            "SELECT sum(a) OVER (ORDER BY b ROWS BETWEEN hunter2xyz PRECEDING AND CURRENT ROW) FROM t",
            H,
        ),
        c4(
            'a',
            "W02 pg named window",
            PG,
            "SELECT sum(a) OVER hunter2xyz FROM t WINDOW hunter2xyz AS (ORDER BY b)",
            H,
        ),
        c4(
            'v',
            "W03 pg PARTITION BY word",
            PG,
            "SELECT sum(a) OVER (PARTITION BY hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "W04 pg RANGE INTERVAL unit",
            PG,
            "SELECT sum(a) OVER (ORDER BY b RANGE BETWEEN INTERVAL '1' hunter2xyz PRECEDING AND CURRENT ROW) FROM t",
            H,
        ),
        c4(
            'v',
            "W05 pg GROUPS EXCLUDE",
            PG,
            "SELECT sum(a) OVER (ORDER BY b GROUPS hunter2xyz PRECEDING EXCLUDE TIES) FROM t",
            H,
        ),
        c4(
            'v',
            "W06 pg window frame string",
            PG,
            "SELECT sum(a) OVER (ORDER BY b RANGE BETWEEN 'Zq9wSecret' PRECEDING AND CURRENT ROW) FROM t",
            Z,
        ),
        // ---------- TABLESAMPLE ----------
        c4(
            'v',
            "S01 pg TABLESAMPLE method word",
            PG,
            "SELECT * FROM t TABLESAMPLE hunter2xyz (10)",
            H,
        ),
        c4(
            'v',
            "S02 pg TABLESAMPLE qty word",
            PG,
            "SELECT * FROM t TABLESAMPLE SYSTEM (hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "S03 pg TABLESAMPLE seed string",
            PG,
            "SELECT * FROM t TABLESAMPLE SYSTEM (10) REPEATABLE ('Zq9wSecret')",
            Z,
        ),
        // ---------- COLLATE / charset / types ----------
        c4(
            'a',
            "K01 pg COLLATE word after value",
            PG,
            "SELECT * FROM t WHERE pw = 'x' COLLATE hunter2xyz",
            H,
        ),
        c4(
            'v',
            "K02 pg value before COLLATE",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz COLLATE \"C\"",
            H,
        ),
        c4(
            'a',
            "K03 my COLLATE word",
            MY,
            "SELECT * FROM t WHERE pw = 'x' COLLATE hunter2xyz",
            H,
        ),
        c4(
            'a',
            "K04 my CONVERT USING word",
            MY,
            "SELECT CONVERT(pw USING hunter2xyz) FROM t",
            H,
        ),
        c4(
            'a',
            "K05 my CAST CHARACTER SET word",
            MY,
            "SELECT CAST(pw AS CHAR CHARACTER SET hunter2xyz) FROM t",
            H,
        ),
        c4(
            'a',
            "K06 pg ::word type",
            PG,
            "SELECT x::hunter2xyz FROM t",
            H,
        ),
        c4(
            'v',
            "K07 pg word::text",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz::text",
            H,
        ),
        c4(
            'a',
            "K08 pg ::\"quoted\" type",
            PG,
            "SELECT * FROM t WHERE pw = 'x'::\"Zq9wSecret\"",
            Z,
        ),
        c4(
            'v',
            "K09 pg OPERATOR(schema.op)",
            PG,
            "SELECT * FROM t WHERE pw OPERATOR(pg_catalog.=) hunter2xyz",
            H,
        ),
        c4(
            'a',
            "K10 pg OPERATOR(word.op)",
            PG,
            "SELECT * FROM t WHERE pw OPERATOR(hunter2xyz.=) 'x'",
            H,
        ),
        c4(
            'v',
            "K11 pg IS DISTINCT FROM word",
            PG,
            "SELECT * FROM t WHERE pw IS DISTINCT FROM hunter2xyz",
            H,
        ),
        c4(
            'v',
            "K12 pg custom operator chars",
            PG,
            "SELECT * FROM t WHERE pw = 'x' AND a <!@#%^&> b",
            &["<!@#%^&>"],
        ),
        // ---------- quoted identifiers ----------
        c4(
            'a',
            "Q01 pg quoted name w/ spaces in projection",
            PG,
            "SELECT \"my pass Zq9wSecret\" FROM t",
            Z,
        ),
        c4(
            'v',
            "Q02 pg quoted = quoted",
            PG,
            "SELECT * FROM t WHERE \"my Zq9wSecret\" = \"other col\"",
            Z,
        ),
        c4(
            'v',
            "Q03 pg quoted value w/ space",
            PG,
            "SELECT * FROM t WHERE pw = \"Zq9w Secret!\"",
            ZS,
        ),
        c4(
            'v',
            "Q04 my backtick value w/ space",
            MY,
            "SELECT * FROM t WHERE pw = `Zq9w Secret!`",
            ZS,
        ),
        c4(
            'v',
            "Q05 pg quoted value, same text kept as name",
            PG,
            "SELECT \"pw\" FROM t WHERE pw = \"Zq9wSecret\"",
            Z,
        ),
        c4(
            'v',
            "Q06 pg kept quoted name vs bare modifier",
            PG,
            "ALTER TABLE t ADD COLUMN \"Zq9wSecret\" foo(Zq9wSecret)",
            Z,
        ),
        c4(
            'v',
            "Q07 pg kept lower name vs upper modifier",
            PG,
            "ALTER TABLE t ADD COLUMN hunter2 foo(HUNTER2XYZ)",
            &["HUNTER2XYZ"],
        ),
        c4(
            'v',
            "Q08 pg modifier with kept-name prefix",
            PG,
            "ALTER TABLE t ADD COLUMN hunter foo(hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "Q09 my kept backtick vs bare tablespace",
            MY,
            "CREATE TABLE `Zq9wSecret` (a INT) TABLESPACE Zq9wSecret",
            Z,
        ),
        c4(
            'v',
            "Q10 pg doubled quote ident then string",
            PG,
            "SELECT \"a\"\"b\" FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "Q11 my doubled backtick then string",
            MY,
            "SELECT `a``b` FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "Q12 my backtick w/ hash then string",
            MY,
            "SELECT `a#b`, `c--d` FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "Q13 my backtick w/ bs-quote",
            MY,
            "SELECT `a\\'` FROM t WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "Q14 pg non-ASCII name then value",
            PG,
            "SELECT naïve FROM t WHERE pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "Q15 my alias \"str\" no AS",
            MY,
            "SELECT a \"Zq9wSecret\" FROM t",
            Z,
        ),
        c4(
            'v',
            "Q16 my alias 'str' AS",
            MY,
            "SELECT a AS 'Zq9wSecret' FROM t",
            Z,
        ),
        c4(
            'v',
            "Q17 pg alias without AS after value",
            PG,
            "SELECT * FROM t WHERE pw = 'x' hunter2xyz",
            H,
        ),
        c4(
            'a',
            "Q18 pg table alias without AS",
            PG,
            "SELECT * FROM t hunter2xyz",
            H,
        ),
        c4(
            'v',
            "Q19 pg NBSP before word",
            PG,
            "SELECT a FROM t WHERE pw = \u{a0}hunter2xyz",
            H,
        ),
        // ---------- SET variants ----------
        c4(
            'v',
            "V01 pg SET search_path TO word",
            PG,
            "SET search_path TO hunter2xyz",
            H,
        ),
        c4(
            'v',
            "V02 pg SET custom = word",
            PG,
            "SET app.pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "V03 pg SET LOCAL TO string",
            PG,
            "SET LOCAL app.pw TO 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "V04 pg SET SESSION word",
            PG,
            "SET SESSION app.token = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "V05 pg SET TIME ZONE word",
            PG,
            "SET TIME ZONE hunter2xyz",
            H,
        ),
        c4('v', "V06 pg SET ROLE word", PG, "SET ROLE hunter2xyz", H),
        c4(
            'v',
            "V07 pg SET SESSION AUTHORIZATION",
            PG,
            "SET SESSION AUTHORIZATION hunter2xyz",
            H,
        ),
        c4('v', "V08 my SET @v = word", MY, "SET @pw = hunter2xyz", H),
        c4(
            'v',
            "V09 my SET @@ and @",
            MY,
            "SET @@session.pw = hunter2xyz, @x = 'Zq9wSecret'",
            HZ,
        ),
        c4('v', "V10 my SET NAMES word", MY, "SET NAMES hunter2xyz", H),
        c4(
            'v',
            "V11 my SET NAMES COLLATE word",
            MY,
            "SET NAMES utf8mb4 COLLATE hunter2xyz",
            H,
        ),
        c4(
            'v',
            "V12 my SET CHARACTER SET word",
            MY,
            "SET CHARACTER SET hunter2xyz",
            H,
        ),
        c4(
            'v',
            "V13 pg SET TRANSACTION SNAPSHOT",
            PG,
            "SET TRANSACTION SNAPSHOT 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "V14 pg SET (a, b) = (..)",
            PG,
            "SET (a, b) = (1, hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "V15 my SET GLOBAL init_connect",
            MY,
            "SET GLOBAL init_connect = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "V16 pg SET list of values",
            PG,
            "SET app.list = hunter2xyz, 'Zq9wSecret'",
            HZ,
        ),
        c4(
            'v',
            "V17 pg SET var TO quoted",
            PG,
            "SET app.pw TO \"Zq9wSecret\"",
            Z,
        ),
        c4(
            'v',
            "V18 my SET var = backtick",
            MY,
            "SET @pw = `Zq9wSecret`",
            Z,
        ),
        c4(
            'v',
            "V19 my SET PERSIST word",
            MY,
            "SET PERSIST init_connect = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "V20 my SET SESSION var word",
            MY,
            "SET SESSION sql_mode = hunter2xyz",
            H,
        ),
        c4(
            'p',
            "V21 pg SET password-ish GUC",
            PG,
            "SET app.password = hunter2xyz",
            H,
        ),
        c4(
            'p',
            "V22 my SET PASSWORD word",
            MY,
            "SET PASSWORD = hunter2xyz",
            H,
        ),
        c4(
            'p',
            "V23 my SET PASSWORD FOR",
            MY,
            "SET PASSWORD FOR bob = hunter2xyz",
            H,
        ),
        // ---------- GRANT ----------
        c4(
            'a',
            "R01 pg GRANT TO role",
            PG,
            "GRANT SELECT ON t TO hunter2xyz",
            H,
        ),
        c4(
            'v',
            "R02 my GRANT TO 'u'@'host'",
            MY,
            "GRANT SELECT ON t TO 'bob'@'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "R03 my GRANT TO u@host bare",
            MY,
            "GRANT SELECT ON t TO bob@hunter2xyz",
            H,
        ),
        c4(
            'p',
            "R04 my GRANT IDENTIFIED BY",
            MY,
            "GRANT SELECT ON *.* TO 'u'@'%' IDENTIFIED BY 'Zq9wSecret'",
            Z,
        ),
        c4(
            'a',
            "R05 pg GRANT role TO",
            PG,
            "GRANT hunter2xyz TO bob",
            H,
        ),
        c4(
            'a',
            "R06 pg GRANT column list",
            PG,
            "GRANT SELECT (hunter2xyz) ON t TO bob",
            H,
        ),
        c4(
            'p',
            "R07 my GRANT IDENTIFIED BY bare",
            MY,
            "GRANT ALL ON *.* TO bob IDENTIFIED BY hunter2xyz",
            H,
        ),
        // ---------- EXPLAIN ----------
        c4(
            'v',
            "E01 pg EXPLAIN FORMAT word",
            PG,
            "EXPLAIN (FORMAT hunter2xyz) SELECT 1",
            H,
        ),
        c4(
            'a',
            "E02 pg EXPLAIN option name",
            PG,
            "EXPLAIN (hunter2xyz) SELECT 1",
            H,
        ),
        c4(
            'v',
            "E03 pg EXPLAIN SETTINGS word",
            PG,
            "EXPLAIN (ANALYZE, SETTINGS hunter2xyz) SELECT * FROM t",
            H,
        ),
        c4(
            'v',
            "E04 my EXPLAIN FORMAT=word",
            MY,
            "EXPLAIN FORMAT=hunter2xyz SELECT 1",
            H,
        ),
        // ---------- CREATE TABLE / INDEX / VIEW ----------
        c4(
            'v',
            "T01 pg WITH (opt = word)",
            PG,
            "CREATE TABLE t (a int) WITH (fillfactor = hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "T02 pg WITH (word)",
            PG,
            "CREATE TABLE t (a int) WITH (hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "T03 my ROW_FORMAT word",
            MY,
            "CREATE TABLE t (a INT) ROW_FORMAT = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T04 my DEFAULT CHARSET word",
            MY,
            "CREATE TABLE t (a INT) DEFAULT CHARSET = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T05 my TABLESPACE word",
            MY,
            "CREATE TABLE t (a INT) TABLESPACE hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T06 my DATA DIRECTORY",
            MY,
            "CREATE TABLE t (a INT) DATA DIRECTORY = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T07 my INSERT_METHOD word",
            MY,
            "CREATE TABLE t (a INT) INSERT_METHOD = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T08 my KEY_BLOCK_SIZE word",
            MY,
            "CREATE TABLE t (a INT) KEY_BLOCK_SIZE = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T09 my ENCRYPTION string",
            MY,
            "CREATE TABLE t (a INT) ENCRYPTION = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "T10 my unknown option = word",
            MY,
            "CREATE TABLE t (a INT) hunter2xyz = 1",
            H,
        ),
        c4(
            'v',
            "T11 my COLLATE = word",
            MY,
            "CREATE TABLE t (a INT) COLLATE = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T12 pg DEFAULT word",
            PG,
            "CREATE TABLE t (a int DEFAULT hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "T13 pg column COLLATE word",
            PG,
            "CREATE TABLE t (a text COLLATE hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "T14 my column CHARACTER SET",
            MY,
            "CREATE TABLE t (a VARCHAR(10) CHARACTER SET hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "T15 pg IDENTITY START WITH word",
            PG,
            "CREATE TABLE t (a int GENERATED ALWAYS AS IDENTITY (START WITH hunter2xyz))",
            H,
        ),
        c4(
            'v',
            "T16 pg CHECK word",
            PG,
            "CREATE TABLE t (a int CHECK (a <> hunter2xyz))",
            H,
        ),
        c4(
            'v',
            "T17 pg PARTITION OF FOR VALUES",
            PG,
            "CREATE TABLE t PARTITION OF p FOR VALUES IN (hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "T18 pg PARTITION BY col",
            PG,
            "CREATE TABLE t (a int) PARTITION BY RANGE (hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "T19 pg INDEX USING method",
            PG,
            "CREATE INDEX i ON t USING hunter2xyz (a)",
            H,
        ),
        c4(
            'v',
            "T20 pg INDEX WITH word",
            PG,
            "CREATE INDEX i ON t (a) WITH (fillfactor = hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "T21 pg INDEX WHERE word",
            PG,
            "CREATE INDEX i ON t (a) WHERE pw = hunter2xyz",
            H,
        ),
        c4(
            'a',
            "T22 pg INDEX COLLATE",
            PG,
            "CREATE INDEX i ON t (a COLLATE hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "T23 pg INDEX opclass",
            PG,
            "CREATE INDEX i ON t (a hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "T24 pg DEFAULT typed custom",
            PG,
            "CREATE TABLE t (a int DEFAULT 'Zq9wSecret'::hunter2xyz)",
            Z,
        ),
        c4(
            'v',
            "T25 pg VIEW WITH option word",
            PG,
            "CREATE VIEW v WITH (security_barrier = hunter2xyz) AS SELECT 1",
            H,
        ),
        c4(
            'v',
            "T26 pg VIEW WHERE word",
            PG,
            "CREATE VIEW v AS SELECT * FROM t WHERE pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T27 my ENGINE then word",
            MY,
            "CREATE TABLE t (a INT) ENGINE = InnoDB hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T28 my UNION = (words)",
            MY,
            "CREATE TABLE t (a INT) ENGINE = MERGE UNION = (hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "T29 pg CREATE TABLE AS word",
            PG,
            "CREATE TABLE t AS SELECT hunter2xyz",
            H,
        ),
        c4(
            'v',
            "T30 my column DEFAULT word",
            MY,
            "CREATE TABLE t (a VARCHAR(10) DEFAULT hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "T31 my col ON UPDATE word",
            MY,
            "CREATE TABLE t (a TIMESTAMP ON UPDATE hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "T32 my AUTO_INCREMENT = word",
            MY,
            "CREATE TABLE t (a INT) AUTO_INCREMENT = hunter2xyz",
            H,
        ),
        // ---------- ALTER TABLE ----------
        c4(
            'v',
            "A01 pg ADD COLUMN typmod word",
            PG,
            "ALTER TABLE t ADD COLUMN c foo(hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "A02 pg ADD COLUMN typmod string",
            PG,
            "ALTER TABLE t ADD COLUMN c foo('Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "A03 pg ALTER TYPE typmod word",
            PG,
            "ALTER TABLE t ALTER COLUMN c TYPE foo(hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "A04 pg SET DEFAULT word",
            PG,
            "ALTER TABLE t ALTER COLUMN c SET DEFAULT hunter2xyz",
            H,
        ),
        c4(
            'a',
            "A05 pg OWNER TO word",
            PG,
            "ALTER TABLE t OWNER TO hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A06 pg SET (opt = word)",
            PG,
            "ALTER TABLE t SET (fillfactor = hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "A07 my ALGORITHM word",
            MY,
            "ALTER TABLE t ALGORITHM = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A08 my AUTO_INCREMENT word",
            MY,
            "ALTER TABLE t AUTO_INCREMENT = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A09 pg ALTER TYPE USING word",
            PG,
            "ALTER TABLE t ALTER COLUMN c TYPE text USING hunter2xyz",
            H,
        ),
        c4(
            'a',
            "A10 my MODIFY CHARACTER SET",
            MY,
            "ALTER TABLE t MODIFY c VARCHAR(10) CHARACTER SET hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A11 pg ADD CONSTRAINT CHECK",
            PG,
            "ALTER TABLE t ADD CONSTRAINT c CHECK (pw <> hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "A12 my ENGINE word",
            MY,
            "ALTER TABLE t ENGINE = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A13 my COMMENT word",
            MY,
            "ALTER TABLE t COMMENT hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A14 my ADD COLUMN typmod string",
            MY,
            "ALTER TABLE t ADD COLUMN c foo('Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "A15 my ADD COLUMN DEFAULT word",
            MY,
            "ALTER TABLE t ADD COLUMN c INT DEFAULT hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A16 my ADD COLUMN COMMENT",
            MY,
            "ALTER TABLE t ADD COLUMN c INT COMMENT 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "A17 my ADD ENUM value",
            MY,
            "ALTER TABLE t ADD COLUMN c ENUM('Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "A18 pg ADD COLUMN typmod qualified",
            PG,
            "ALTER TABLE t ADD COLUMN c s.foo(hunter2xyz, 3)",
            H,
        ),
        c4(
            'v',
            "A19 my ALTER COLUMN SET DEFAULT",
            MY,
            "ALTER TABLE t ALTER COLUMN c SET DEFAULT hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A20 my CHANGE COLUMN typmod",
            MY,
            "ALTER TABLE t CHANGE COLUMN a b foo('Zq9wSecret')",
            Z,
        ),
        // ---------- PREPARE / EXECUTE / misc statements ----------
        c4(
            'v',
            "P01 pg PREPARE typmod word",
            PG,
            "PREPARE s (foo(hunter2xyz)) AS SELECT 1",
            H,
        ),
        c4(
            'v',
            "P02 pg PREPARE WHERE word",
            PG,
            "PREPARE s AS SELECT * FROM t WHERE pw = hunter2xyz",
            H,
        ),
        c4('v', "P03 pg EXECUTE word", PG, "EXECUTE s (hunter2xyz)", H),
        c4(
            'v',
            "P04 my PREPARE FROM string",
            MY,
            "PREPARE s FROM 'SELECT Zq9wSecret'",
            Z,
        ),
        c4(
            'a',
            "P05 pg DEALLOCATE word",
            PG,
            "DEALLOCATE hunter2xyz",
            H,
        ),
        c4(
            'v',
            "P06 pg EXECUTE USING AS",
            PG,
            "EXECUTE s USING 1 AS hunter2xyz",
            H,
        ),
        c4(
            'v',
            "P07 pg DISTINCT ON word",
            PG,
            "SELECT DISTINCT ON (hunter2xyz) a FROM t",
            H,
        ),
        c4(
            'a',
            "P08 pg SELECT INTO",
            PG,
            "SELECT a INTO hunter2xyz FROM t",
            H,
        ),
        c4(
            'v',
            "P09 my SELECT INTO @var",
            MY,
            "SELECT a INTO @hunter2xyz FROM t",
            H,
        ),
        c4(
            'a',
            "P10 pg CTE name",
            PG,
            "WITH hunter2xyz AS (SELECT 1) SELECT * FROM hunter2xyz",
            H,
        ),
        c4(
            'a',
            "P11 pg JOIN USING",
            PG,
            "SELECT * FROM t JOIN u USING (hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "P12 my USE INDEX",
            MY,
            "SELECT * FROM t USE INDEX (hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "P13 my PARTITION (p)",
            MY,
            "SELECT * FROM t PARTITION (hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "P14 my JSON_TABLE column name",
            MY,
            "SELECT * FROM JSON_TABLE(doc, '$' COLUMNS (hunter2xyz INT PATH '$.a')) AS jt",
            H,
        ),
        c4(
            'v',
            "P15 pg nested value",
            PG,
            "SELECT * FROM t WHERE pw = (hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "P16 pg keyword-shaped value",
            PG,
            "UPDATE t SET pw = Password WHERE id = 1",
            &["Password"],
        ),
        c4(
            'a',
            "P17 my SHOW TABLES FROM",
            MY,
            "SHOW TABLES FROM hunter2xyz",
            H,
        ),
        c4('a', "P18 my USE db", MY, "USE hunter2xyz", H),
        c4('a', "P19 pg SAVEPOINT", PG, "SAVEPOINT hunter2xyz", H),
        c4(
            'v',
            "P20 pg START TRANSACTION bad level",
            PG,
            "START TRANSACTION ISOLATION LEVEL hunter2xyz",
            H,
        ),
        c4(
            'v',
            "P21 pg UPDATE SET (a,b) = (..)",
            PG,
            "UPDATE t SET (a, pw) = (1, hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "P22 pg UPDATE FROM WHERE",
            PG,
            "UPDATE t SET pw = u.x FROM u WHERE u.k = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "P23 my UPDATE ORDER BY LIMIT",
            MY,
            "UPDATE t SET pw = hunter2xyz ORDER BY a LIMIT 1",
            H,
        ),
        c4(
            'v',
            "P24 pg DELETE USING",
            PG,
            "DELETE FROM t USING u WHERE u.pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "P25 pg DELETE RETURNING",
            PG,
            "DELETE FROM t WHERE pw = hunter2xyz RETURNING *",
            H,
        ),
        c4(
            'v',
            "P26 pg RETURNING expr",
            PG,
            "INSERT INTO t (a) VALUES (1) RETURNING a || hunter2xyz",
            H,
        ),
        c4(
            'v',
            "P27 my DELETE ORDER BY word",
            MY,
            "DELETE FROM t WHERE a = 1 ORDER BY hunter2xyz",
            H,
        ),
        c4(
            'v',
            "P28 pg LISTEN/NOTIFY payload",
            PG,
            "NOTIFY ch, 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "P29 pg COMMENT ON IS string",
            PG,
            "COMMENT ON TABLE t IS 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "P30 pg CREATE INDEX INCLUDE word",
            PG,
            "CREATE INDEX i ON t (a) INCLUDE (hunter2xyz) WHERE b = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "P31 my HANDLER/LOAD fallback",
            MY,
            "LOAD DATA INFILE 'Zq9wSecret' INTO TABLE t",
            Z,
        ),
        c4(
            'v',
            "P32 pg EXPLAIN UPDATE word",
            PG,
            "EXPLAIN UPDATE t SET pw = hunter2xyz",
            H,
        ),
        // ---------- round-5 follow-ups: construct word licensed by a same-spelled name ----------
        c4(
            'v',
            "X01 pg CEIL TO word + table CEIL",
            PG,
            "SELECT CEIL(ts TO hunter2xyz) FROM CEIL",
            H,
        ),
        c4(
            'v',
            "X02 pg CEIL TO word + alias AS CEIL",
            PG,
            "SELECT CEIL(price TO hunter2xyz) AS CEIL FROM t",
            H,
        ),
        c4(
            'v',
            "X03 pg FLOOR TO in WHERE + col FLOOR",
            PG,
            "SELECT FLOOR FROM t WHERE ts = FLOOR(ts TO hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "X04 my CEIL TO + alias AS CEIL",
            MY,
            "SELECT CEIL(ts TO hunter2xyz) AS CEIL FROM t",
            H,
        ),
        c4(
            'v',
            "X05 pg CEIL TO string + table CEIL",
            PG,
            "SELECT CEIL(ts TO 'Zq9wSecret') FROM CEIL",
            Z,
        ),
        c4(
            'v',
            "X06 my GROUP_CONCAT SEPARATOR word licensed",
            MY,
            "SELECT GROUP_CONCAT(a SEPARATOR hunter2xyz) AS SEPARATOR FROM GROUP_CONCAT",
            H,
        ),
        c4(
            'v',
            "X07 pg typmod word licensed by col name",
            PG,
            "ALTER TABLE foo ADD COLUMN c foo(hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "X08 pg typmod word = column name (control)",
            PG,
            "ALTER TABLE t ADD COLUMN hunter2xyz foo(hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "X09 pg TABLESAMPLE licensed",
            PG,
            "SELECT * FROM TABLESAMPLE TABLESAMPLE hunter2xyz (10)",
            H,
        ),
        c4(
            'v',
            "X10 pg overlay licensed",
            PG,
            "SELECT overlay(pw PLACING hunter2xyz FROM 1) AS overlay, PLACING FROM t",
            H,
        ),
        // ---------- keyword-list words that are not SQL keywords ----------
        c4(
            'v',
            "Y01 pg typmod Login (raw)",
            PG,
            "ALTER TABLE t ADD COLUMN c foo(Login)",
            &["Login"],
        ),
        c4(
            'v',
            "Y02 pg typmod Superuser (raw)",
            PG,
            "ALTER TABLE t ADD COLUMN c foo(Superuser)",
            &["Superuser"],
        ),
        c4(
            'v',
            "Y03 pg fallback Public",
            PG,
            "UPDATE users SET pw = Public WHERE",
            &["Public"],
        ),
        c4(
            'v',
            "Y04 pg fallback Plpgsql",
            PG,
            "UPDATE users SET pw = Plpgsql WHERE",
            &["Plpgsql"],
        ),
        c4(
            'v',
            "Y05 my fallback Bytea",
            MY,
            "UPDATE users SET pw = Bytea WHERE",
            &["Bytea"],
        ),
        c4(
            'v',
            "Y06 my tablespace Createdb (raw)",
            MY,
            "CREATE TABLE t (a INT) TABLESPACE Createdb",
            &["Createdb"],
        ),
        c4(
            'v',
            "Y07 pg fallback Perform",
            PG,
            "SELECT * FROM t WHERE pw = Perform AND",
            &["Perform"],
        ),
        c4(
            'v',
            "Y08 pg typmod Password (real kw)",
            PG,
            "ALTER TABLE t ADD COLUMN c foo(Password)",
            &["Password"],
        ),
        // ---------- more EXECUTE / alias forms ----------
        c4(
            'v',
            "Z01 pg EXECUTE INTO word",
            PG,
            "EXECUTE s(1) INTO hunter2xyz",
            H,
        ),
        c4(
            'v',
            "Z02 pg EXECUTE IMMEDIATE",
            PG,
            "EXECUTE IMMEDIATE 'SELECT Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "Z03 my EXECUTE USING AS",
            MY,
            "EXECUTE s USING @a AS hunter2xyz",
            H,
        ),
        c4(
            'v',
            "Z04 pg EXECUTE USING string AS",
            PG,
            "EXECUTE s USING 'Zq9wSecret' AS pw",
            Z,
        ),
        c4(
            'a',
            "Z05 pg table alias col type",
            PG,
            "SELECT * FROM unnest(ARRAY[1]) AS u(x hunter2xyz)",
            H,
        ),
        c4(
            'a',
            "Z06 pg named window ref in spec",
            PG,
            "SELECT sum(a) OVER (hunter2xyz ORDER BY b) FROM t WINDOW hunter2xyz AS (PARTITION BY c)",
            H,
        ),
    ]
}

/// Fifth-audit cases whose flagged text is accepted at text level
/// `normalized`: K12's "secret" is a custom operator made of punctuation
/// only, and in Q06 the secret is also the quoted column name (a real name
/// position); the bare type modifier the case is about is masked.
const FIFTH_AUDIT_ACCEPTED: [&str; 2] = [
    "K12 pg custom operator chars",
    "Q06 pg kept quoted name vs bare modifier",
];

#[test]
fn no_secret_of_the_fifth_audit_reaches_the_log() -> TestResult {
    let cases: Vec<Case4> = fifth_audit_cases()
        .into_iter()
        .map(|case| Case4 {
            k: if FIFTH_AUDIT_ACCEPTED.contains(&case.tag.as_str()) {
                'a'
            } else {
                case.k
            },
            ..case
        })
        .collect();
    assert_eq!(cases.len(), 248);
    let found = audit4_leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:\n{}",
        found.len(),
        found.join("\n")
    );
    Ok(())
}

const JWT: &[&str] = &["SflKxwRJSMeKKF2QT4fwpM"];
const PIECES: &[&str] = &["Zq9w", "Secret"];

/// The sixth audit's 213 inputs.
fn sixth_audit_cases() -> Vec<Case4> {
    vec![
        // ---------- (a) value-like tokens in every Masker::name / license call site ----------
        // keep_condition: qualified = qualified
        c4(
            'v',
            "A01 pg qualified = dotted JWT",
            PG,
            "SELECT * FROM users u WHERE u.token = eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.SflKxwRJSMeKKF2QT4fwpM",
            JWT,
        ),
        c4(
            'v',
            "A02 pg UPDATE qualified = dotted",
            PG,
            "UPDATE users u SET a = 1 WHERE u.pw = hunter2xyz.Zq9wSecret",
            HZ,
        ),
        c4(
            'v',
            "A03 pg JOIN ON qualified = dotted",
            PG,
            "SELECT * FROM a JOIN b ON a.pw = hunter2xyz.part",
            H,
        ),
        c4(
            'v',
            "A04 my qualified = dotted",
            MY,
            "SELECT * FROM users u WHERE u.pw = hunter2xyz.Zq9wSecret",
            HZ,
        ),
        c4(
            'v',
            "A05 pg qualified IN dotted list",
            PG,
            "SELECT * FROM users u WHERE u.pw IN (hunter2xyz.a, 'x')",
            H,
        ),
        c4(
            'v',
            "A06 pg qualified BETWEEN dotted",
            PG,
            "SELECT * FROM users u WHERE u.pw BETWEEN hunter2xyz.a AND 'z'",
            H,
        ),
        // name_type via TypedString
        c4(
            'v',
            "A07 pg word 'str' typed string",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz 'x'",
            H,
        ),
        c4(
            'v',
            "A08 pg word'str' glued",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz'x'",
            H,
        ),
        c4(
            'v',
            "A09 my word 'str'",
            MY,
            "SELECT * FROM t WHERE pw = hunter2xyz 'x'",
            H,
        ),
        c4(
            'v',
            "A10 pg typed string in VALUES",
            PG,
            "INSERT INTO t (pw) VALUES (hunter2xyz 'Zq9wSecret')",
            HZ,
        ),
        c4(
            'a',
            "A11 pg CAST AS custom type",
            PG,
            "SELECT * FROM t WHERE pw = CAST('x' AS hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "A12 pg cast type modifier",
            PG,
            "UPDATE t SET pw = 'x'::foo(hunter2xyz, 'Zq9wSecret')",
            HZ,
        ),
        c4(
            'v',
            "A13 pg TypedString modifier",
            PG,
            "SELECT * FROM t WHERE pw = foo(hunter2xyz) 'x'",
            H,
        ),
        c4(
            'v',
            "A14 pg PREPARE type modifier",
            PG,
            "PREPARE s(foo('Zq9wSecret')) AS SELECT 1",
            Z,
        ),
        c4(
            'v',
            "A15 pg alias column type modifier",
            PG,
            "SELECT * FROM f() AS x(a foo('Zq9wSecret'))",
            Z,
        ),
        c4(
            'v',
            "A16 pg CREATE VIEW column modifier",
            PG,
            "CREATE VIEW v AS SELECT 'Zq9wSecret'::foo(hunter2xyz) AS a",
            HZ,
        ),
        // Collate / charset
        c4(
            'a',
            "A17 pg COLLATE word",
            PG,
            "SELECT * FROM t WHERE pw = 'x' COLLATE hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A18 my known introducer + word",
            MY,
            "SELECT * FROM t WHERE pw = _utf8mb4 hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A19 my unknown introducer projection FROM",
            MY,
            "SELECT _hunter2xyz 'x' FROM t",
            H,
        ),
        c4(
            'a',
            "A20 my column CHARACTER SET word",
            MY,
            "CREATE TABLE t (a TEXT CHARACTER SET hunter2xyz)",
            H,
        ),
        // ENGINE / table options
        c4(
            'v',
            "A21 my unknown ENGINE",
            MY,
            "CREATE TABLE t (a INT) ENGINE = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A22 my ENGINE + COMMENT",
            MY,
            "CREATE TABLE t (a INT) ENGINE = InnoDB COMMENT 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "A23 pg WITH (opt = word)",
            PG,
            "CREATE TABLE t (a int) WITH (fillfactor = hunter2xyz)",
            H,
        ),
        c4(
            'p',
            "A24 my table PASSWORD option",
            MY,
            "CREATE TABLE t (a INT) PASSWORD = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'p',
            "A25 my CONNECTION url",
            MY,
            "CREATE TABLE t (a INT) ENGINE = FEDERATED CONNECTION = 'mysql://u:Zq9wSecret@h/db/t'",
            Z,
        ),
        // aliases
        c4(
            'a',
            "A26 pg table function alias no AS",
            PG,
            "SELECT * FROM generate_series(1, 2) hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A27 pg derived alias, value inside",
            PG,
            "SELECT * FROM (SELECT 'Zq9wSecret') hunter2xyz",
            Z,
        ),
        c4(
            'v',
            "A28 pg alias after '--as' comment",
            PG,
            "SELECT INTERVAL '1' --as\n hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A29 pg 9 + '--as' comment + word",
            PG,
            "SELECT 9 --as\nhunter2xyz",
            H,
        ),
        c4(
            'v',
            "A30 pg dotted .as then word",
            PG,
            "SELECT x.as hunter2xyz FROM t",
            H,
        ),
        c4(
            'v',
            "A31 pg alias after /*as*/ ",
            PG,
            "SELECT 9 /*as*/ hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A32 pg CTE column alias + value",
            PG,
            "WITH c(hunter2xyz) AS (SELECT 'Zq9wSecret') SELECT 1",
            Z,
        ),
        c4(
            'v',
            "A33 pg INSERT AS alias + value",
            PG,
            "INSERT INTO t AS hunter2xyz VALUES ('Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "A34 my INSERT row alias + value",
            MY,
            "INSERT INTO t VALUES ('Zq9wSecret') AS hunter2xyz",
            Z,
        ),
        // INSERT / UPDATE / DELETE positions
        c4(
            'v',
            "A35 pg INSERT SELECT word no FROM",
            PG,
            "INSERT INTO t (pw) SELECT hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A36 pg ON CONFLICT DO UPDATE value",
            PG,
            "INSERT INTO t VALUES (1) ON CONFLICT (pw) DO UPDATE SET pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A37 my ON DUPLICATE VALUES(word)",
            MY,
            "INSERT INTO t (pw) VALUES ('a') ON DUPLICATE KEY UPDATE pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A38 pg UPDATE tuple assignment",
            PG,
            "UPDATE t SET (a, b) = (hunter2xyz, 'Zq9wSecret')",
            HZ,
        ),
        c4(
            'v',
            "A39 pg RETURNING literal alias",
            PG,
            "DELETE FROM t RETURNING 'Zq9wSecret' AS x",
            Z,
        ),
        c4(
            'v',
            "A40 my INSERT SET assignment",
            MY,
            "INSERT INTO t SET pw = hunter2xyz",
            H,
        ),
        // SET
        c4(
            'v',
            "A41 my SET @var = value",
            MY,
            "SET @hunter2xyz = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "A42 pg SET var = word",
            PG,
            "SET hunter2xyz = Zq9wSecret",
            Z,
        ),
        c4(
            'v',
            "A43 my SET multiple",
            MY,
            "SET @a = 1, @b = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A44 pg SET search_path TO",
            PG,
            "SET search_path TO hunter2xyz, public",
            H,
        ),
        c4(
            'v',
            "A45 pg SET TIME ZONE word",
            PG,
            "SET TIME ZONE hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A46 pg SET TRANSACTION SNAPSHOT",
            PG,
            "SET TRANSACTION SNAPSHOT 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "A47 pg SET LOCAL x = word",
            PG,
            "SET LOCAL statement_timeout = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A48 my SET (var) = word parenthesized",
            MY,
            "SET (a, b) = (hunter2xyz, 1)",
            H,
        ),
        // GRANT / REVOKE
        c4(
            'a',
            "A49 my GRANT user@barehost",
            MY,
            "GRANT SELECT ON t TO u@hunter2xyz",
            H,
        ),
        c4(
            'p',
            "A50 my GRANT IDENTIFIED BY",
            MY,
            "GRANT SELECT ON t TO u IDENTIFIED BY 'Zq9wSecret'",
            Z,
        ),
        c4(
            'a',
            "A51 pg GRANT role TO",
            PG,
            "GRANT hunter2xyz TO alice",
            H,
        ),
        c4(
            'v',
            "A52 pg GRANT column priv",
            PG,
            "GRANT SELECT (hunter2xyz) ON t TO alice",
            H,
        ),
        // EXECUTE / PREPARE / CALL / USE / savepoints
        c4(
            'v',
            "A53 pg EXECUTE params",
            PG,
            "EXECUTE s(hunter2xyz, 'Zq9wSecret')",
            HZ,
        ),
        c4(
            'a',
            "A54 my EXECUTE USING @var",
            MY,
            "EXECUTE s USING @hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A55 pg PREPARE body value",
            PG,
            "PREPARE s(text) AS SELECT * FROM t WHERE pw = hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A56 my PREPARE FROM string",
            MY,
            "PREPARE s FROM 'SELECT AES_DECRYPT(c, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'v',
            "A57 pg CALL args",
            PG,
            "CALL p('Zq9wSecret', hunter2xyz)",
            HZ,
        ),
        c4(
            'a',
            "A58 pg ROLLBACK TO SAVEPOINT",
            PG,
            "ROLLBACK TO SAVEPOINT hunter2xyz",
            H,
        ),
        c4('a', "A59 my USE word", MY, "USE hunter2xyz", H),
        // CREATE INDEX / constraints / columns
        c4(
            'v',
            "A60 pg partial index predicate",
            PG,
            "CREATE INDEX i ON t (a) WHERE pw = hunter2xyz",
            H,
        ),
        c4(
            'a',
            "A61 pg index opclass",
            PG,
            "CREATE INDEX i ON t (a hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "A62 pg expression index",
            PG,
            "CREATE INDEX i ON t ((pw = 'Zq9wSecret'))",
            Z,
        ),
        c4(
            'v',
            "A63 my index COMMENT",
            MY,
            "CREATE INDEX i ON t (a) COMMENT 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "A64 pg CHECK constraint value",
            PG,
            "CREATE TABLE t (a int REFERENCES u (b), CONSTRAINT c CHECK (a <> hunter2xyz))",
            H,
        ),
        c4(
            'v',
            "A65 pg column DEFAULT word",
            PG,
            "CREATE TABLE t (pw text DEFAULT hunter2xyz)",
            H,
        ),
        c4(
            'v',
            "A66 pg GENERATED AS literal",
            PG,
            "CREATE TABLE t (a text GENERATED ALWAYS AS ('Zq9wSecret') STORED)",
            Z,
        ),
        c4(
            'v',
            "A67 pg column type modifier",
            PG,
            "CREATE TABLE t (a foo('Zq9wSecret'))",
            Z,
        ),
        c4(
            'v',
            "A68 pg ALTER TYPE USING word",
            PG,
            "ALTER TABLE t ALTER COLUMN a TYPE text USING hunter2xyz",
            H,
        ),
        c4(
            'v',
            "A69 pg ALTER SET DEFAULT",
            PG,
            "ALTER TABLE t ALTER COLUMN pw SET DEFAULT 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "A70 my CHANGE COLUMN DEFAULT",
            MY,
            "ALTER TABLE t CHANGE a b VARCHAR(10) DEFAULT 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "A71 my MODIFY ENUM member",
            MY,
            "ALTER TABLE t MODIFY a ENUM('Zq9wSecret', 'b')",
            Z,
        ),
        c4(
            'v',
            "A72 pg ADD CONSTRAINT CHECK",
            PG,
            "ALTER TABLE t ADD CONSTRAINT c CHECK (pw <> 'Zq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "A73 pg CREATE VIEW no-FROM word",
            PG,
            "CREATE VIEW v (a) AS SELECT hunter2xyz",
            H,
        ),
        // projection / grouping / functions / EXTRACT
        c4(
            'a',
            "A74 pg qualified wildcard",
            PG,
            "SELECT hunter2xyz.* FROM t",
            H,
        ),
        c4(
            'a',
            "A75 pg function name",
            PG,
            "SELECT * FROM t WHERE pw = hunter2xyz()",
            H,
        ),
        c4(
            'v',
            "A76 pg EXTRACT string field",
            PG,
            "SELECT extract('Zq9wSecret' FROM ts) FROM t",
            Z,
        ),
        c4(
            'v',
            "A77 pg tuple of qualified",
            PG,
            "SELECT * FROM users u WHERE (u.pw, u.x) = (s.hunter2xyz, s.y)",
            H,
        ),
        c4(
            'v',
            "A78 pg window PARTITION BY word",
            PG,
            "SELECT sum(a) OVER (PARTITION BY hunter2xyz) FROM t",
            H,
        ),
        c4(
            'v',
            "A79 pg JOIN USING + value",
            PG,
            "SELECT * FROM a JOIN b USING (id) WHERE pw = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'v',
            "A80 pg ORDER BY word, no FROM",
            PG,
            "SELECT 1 ORDER BY hunter2xyz",
            H,
        ),
        // ---------- (b) new Full patterns: encryption keys ----------
        c4(
            'p',
            "B01 my AES key nested UNHEX(SHA2())",
            MY,
            "SELECT AES_ENCRYPT('data', UNHEX(SHA2('Zq9wSecret', 512)))",
            Z,
        ),
        c4(
            'p',
            "B02 my AES key X'hex'",
            MY,
            "SELECT AES_ENCRYPT('d', X'5a71397753656372657431')",
            &["5a71397753656372657431"],
        ),
        c4(
            'p',
            "B03 my AES key 0xhex",
            MY,
            "SELECT AES_ENCRYPT('d', 0x5a7139775365637265743132)",
            &["5a7139775365637265743132"],
        ),
        c4(
            'p',
            "B04 my AES key b'bits'",
            MY,
            "SELECT AES_ENCRYPT('d', b'0101101001110001')",
            &["0101101001110001"],
        ),
        c4(
            'p',
            "B05 my AES key _binary'..'",
            MY,
            "SELECT AES_ENCRYPT('d', _binary'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "B06 my AES key _binary 0x",
            MY,
            "SELECT AES_ENCRYPT('d', _binary 0x5a7139775365637265743132)",
            &["5a7139775365637265743132"],
        ),
        c4(
            'p',
            "B07 my AES key CONCAT pieces",
            MY,
            "SELECT AES_ENCRYPT(CONCAT('a', 'b'), CONCAT('hunter2xyz', 'Zq9wSecret'))",
            HZ,
        ),
        c4(
            'p',
            "B08 pg pgp_pub_decrypt 3rd arg psw",
            PG,
            "SELECT pgp_pub_decrypt(msg, dearmor('KEYBLOCK'), 'Zq9wSecret') FROM t",
            Z,
        ),
        c4(
            'p',
            "B09 pg hmac key",
            PG,
            "SELECT hmac('data', 'Zq9wSecret', 'sha256')",
            Z,
        ),
        c4(
            'p',
            "B10 pg crypt(password, salt)",
            PG,
            "INSERT INTO users (login, pw_hash) VALUES ('a', crypt('hunter2xyz', gen_salt('bf')))",
            H,
        ),
        c4(
            'p',
            "B11 pg ARRAY[...] commas before key",
            PG,
            "SELECT encrypt(ARRAY['a','b']::text::bytea, 'Zq9wSecret', 'aes')",
            Z,
        ),
        c4(
            'p',
            "B12 pg quoted function name",
            PG,
            "SELECT \"pgp_sym_encrypt\"('data', 'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "B13 pg schema-qualified",
            PG,
            "SELECT public.pgp_sym_encrypt('data', 'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "B14 pg dollar-quoted key",
            PG,
            "SELECT pgp_sym_encrypt('data', $k$Zq9wSecret$k$)",
            Z,
        ),
        c4(
            'p',
            "B15 pg E-string key escaped quote",
            PG,
            r"SELECT pgp_sym_encrypt('d', E'hunter2xyz\'Zq9wSecret')",
            HZ,
        ),
        c4(
            'p',
            "B16 pg std string with backslash",
            PG,
            r"SELECT pgp_sym_encrypt('d', 'hunter2xyz\'Zq9wSecret')",
            HZ,
        ),
        c4(
            'p',
            "B17 my key backslash quote",
            MY,
            r"SELECT AES_ENCRYPT('d', 'hunter2xyz\'Zq9wSecret')",
            HZ,
        ),
        c4(
            'p',
            "B18 my key after comment",
            MY,
            "SELECT AES_ENCRYPT('d', /* k */ 'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "B19 my space before paren",
            MY,
            "SELECT AES_ENCRYPT ('d', 'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "B20 my lowercase aes_decrypt",
            MY,
            "SELECT aes_decrypt(col, 'Zq9wSecret') FROM t",
            Z,
        ),
        c4(
            'p',
            "B21 my ENCODE / DECODE",
            MY,
            "SELECT ENCODE('d', 'Zq9wSecret'), DECODE(c, 'hunter2xyz') FROM t",
            HZ,
        ),
        c4(
            'p',
            "B22 pg string continuation key",
            PG,
            "SELECT pgp_sym_encrypt('d', 'Zq9w'\n'Secret')",
            PIECES,
        ),
        c4(
            'p',
            "B23 my key in subquery",
            MY,
            "SELECT AES_ENCRYPT('d', (SELECT 'Zq9wSecret'))",
            Z,
        ),
        c4(
            'p',
            "B24 my key in WHERE comparison",
            MY,
            "SELECT * FROM t WHERE AES_DECRYPT(c, 'Zq9wSecret') = 'x'",
            Z,
        ),
        c4(
            'p',
            "B25 pg pgp_sym_decrypt_bytea",
            PG,
            "SELECT pgp_sym_decrypt_bytea(c, 'Zq9wSecret') FROM t",
            Z,
        ),
        c4(
            'p',
            "B26 my key user variable",
            MY,
            "SET @aes_key = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'p',
            "B27 pg key via set_config",
            PG,
            "SELECT set_config('app.encryption_key', 'Zq9wSecret', false)",
            Z,
        ),
        c4(
            'p',
            "B28 my key U&/N'' prefix",
            MY,
            "SELECT AES_ENCRYPT('d', N'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "B29 pg key cast",
            PG,
            "SELECT pgp_sym_encrypt('d', 'Zq9wSecret'::text)",
            Z,
        ),
        c4(
            'p',
            "B30 pg encrypt_iv key",
            PG,
            "SELECT encrypt_iv('d', 'Zq9wSecret', 'iv', 'aes')",
            Z,
        ),
        // ---------- (b) init_connect / init_replica / init_slave ----------
        c4(
            'p',
            "I01 my init_connect doubled quotes",
            MY,
            "SET GLOBAL init_connect = 'SET @k = AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I02 my init_connect backslash quotes",
            MY,
            r"SET GLOBAL init_connect = 'SELECT AES_DECRYPT(x, \'Zq9wSecret\')'",
            Z,
        ),
        c4(
            'p',
            "I03 my init_connect double-quoted",
            MY,
            "SET GLOBAL init_connect = \"SELECT AES_DECRYPT(x, 'Zq9wSecret')\"",
            Z,
        ),
        c4(
            'p',
            "I04 my init_connect :=",
            MY,
            "SET GLOBAL init_connect := 'SELECT AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I05 my init_connect 2nd assignment",
            MY,
            "SET GLOBAL max_connections = 10, GLOBAL init_connect = 'SELECT AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I06 my @@GLOBAL.init_connect",
            MY,
            "SET @@GLOBAL.init_connect = 'SELECT AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I07 my PERSIST init_replica",
            MY,
            "SET PERSIST init_replica = 'SELECT AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I08 my init_slave",
            MY,
            "SET GLOBAL init_slave = 'SELECT AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I09 my backtick variable",
            MY,
            "SET GLOBAL `init_connect` = 'SELECT AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I10 my comment before variable",
            MY,
            "SET GLOBAL /* x */ init_connect = 'SELECT AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I11 my init_connect SET PASSWORD",
            MY,
            "SET GLOBAL init_connect = 'SET PASSWORD = ''Zq9wSecret'''",
            Z,
        ),
        c4(
            'p',
            "I12 my init_connect CONCAT value",
            MY,
            "SET GLOBAL init_connect = CONCAT('SELECT AES_DECRYPT(x, ''', 'Zq9wSecret', ''')')",
            Z,
        ),
        c4(
            'p',
            "I13 my init_connect introducer",
            MY,
            "SET GLOBAL init_connect = _utf8mb4'SELECT AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I14 my init_connect dsn key=value",
            MY,
            "SET GLOBAL init_connect = 'SET @dsn = ''host=h password=Zq9wSecret'''",
            Z,
        ),
        c4(
            'p',
            "I15 my init_connect inner comment",
            MY,
            "SET GLOBAL init_connect = 'SELECT 1 -- PASSWORD ''Zq9wSecret''\n'",
            Z,
        ),
        c4(
            'p',
            "I16 my init_connect \\\\ + doubled quote",
            MY,
            r"SET GLOBAL init_connect = 'SELECT AES_DECRYPT(x, ''Zq9w\\''Secret'')'",
            PIECES,
        ),
        c4(
            'p',
            "I17 my init_connect inner backslash quote",
            MY,
            r"SET GLOBAL init_connect = 'SELECT AES_DECRYPT(x, ''hunter2xyz\''''Zq9wSecret'')'",
            HZ,
        ),
        c4(
            'p',
            "I18 my init_connect comment hides quote",
            MY,
            r"SET GLOBAL init_connect = 'SELECT 1 /* \' */, AES_DECRYPT(x, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "I19 my init_connect IDENTIFIED BY",
            MY,
            "SET GLOBAL init_connect = 'ALTER USER u IDENTIFIED BY ''Zq9wSecret'''",
            Z,
        ),
        c4(
            'p',
            "I20 my init_connect key column",
            MY,
            "SET GLOBAL init_connect = 'SET @x = (SELECT 1 FROM t WHERE api_key = ''Zq9wSecret'')'",
            Z,
        ),
        // ---------- (b) comments ----------
        c4(
            'p',
            "C01 pg -- PASSWORD 'x'",
            PG,
            "-- PASSWORD 'Zq9wSecret'\nSELECT 1",
            Z,
        ),
        c4(
            'p',
            "C02 pg -- apostrophe then PASSWORD",
            PG,
            "SELECT 1 -- don't use PASSWORD 'Zq9wSecret'",
            Z,
        ),
        c4(
            'p',
            "C03 my /* apostrophe */ IDENTIFIED BY",
            MY,
            "/* it's: ALTER USER u IDENTIFIED BY 'Zq9wSecret' */ SELECT 1",
            Z,
        ),
        c4(
            'p',
            "C04 pg nested comment",
            PG,
            "/* a /* ALTER ROLE r PASSWORD 'Zq9wSecret' */ b */ SELECT 1",
            Z,
        ),
        c4(
            'p',
            "C05 pg nested + outer apostrophe",
            PG,
            "/* don't /* ALTER ROLE r PASSWORD 'Zq9wSecret' */ */ SELECT 1",
            Z,
        ),
        c4(
            'p',
            "C06 my executable comment key",
            MY,
            "SELECT 1 /*!50700 , AES_ENCRYPT('d', 'Zq9wSecret') */",
            Z,
        ),
        c4(
            'p',
            "C07 my MariaDB /*M! */",
            MY,
            "/*M!100100 ALTER USER u IDENTIFIED BY 'Zq9wSecret' */",
            Z,
        ),
        c4(
            'p',
            "C08 my hint + comment",
            MY,
            "SELECT /*+ SET_VAR(x = 1) */ 1 /* IDENTIFIED BY 'Zq9wSecret' */",
            Z,
        ),
        c4(
            'p',
            "C09 my # SET PASSWORD",
            MY,
            "SELECT 1 # SET PASSWORD = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'p',
            "C10 pg -- password: word",
            PG,
            "SELECT 1 -- password: hunter2xyz",
            H,
        ),
        c4(
            'p',
            "C11 pg -- password word",
            PG,
            "SELECT 1 -- password hunter2xyz",
            H,
        ),
        c4('p', "C12 pg -- pw=word", PG, "SELECT 1 -- pw=hunter2xyz", H),
        c4(
            'p',
            "C13 pg -- URL",
            PG,
            "SELECT 1 -- postgres://u:Zq9wSecret@h/db",
            Z,
        ),
        c4(
            'p',
            "C14 pg /* api_key= */",
            PG,
            "SELECT 1 /* api_key=Zq9wSecret */",
            Z,
        ),
        c4(
            'p',
            "C15 pg -- Authorization header",
            PG,
            "SELECT 1 -- Authorization: Bearer Zq9wSecret",
            Z,
        ),
        c4(
            'p',
            "C16 pg unterminated /* PASSWORD",
            PG,
            "SELECT 1 /* PASSWORD 'Zq9wSecret",
            Z,
        ),
        c4(
            'p',
            "C17 my --PASSWORD= (code, not comment)",
            MY,
            "SELECT 1 --PASSWORD='Zq9wSecret'",
            Z,
        ),
        c4(
            'p',
            "C18 pg statement split over -- lines",
            PG,
            "-- ALTER USER u IDENTIFIED BY\n-- 'Zq9wSecret'\nSELECT 1",
            Z,
        ),
        c4(
            'p',
            "C19 pg commented UPDATE password",
            PG,
            "-- UPDATE users SET password = 'Zq9wSecret'\nSELECT 1",
            Z,
        ),
        c4(
            'p',
            "C20 pg comment E-string",
            PG,
            "/* SELECT pgp_sym_encrypt('d', E'Zq9w\\'Secret') */ SELECT 1",
            PIECES,
        ),
        c4(
            'p',
            "C21 my # with apostrophe",
            MY,
            "SELECT 1 # Bob's PASSWORD 'Zq9wSecret'",
            Z,
        ),
        c4(
            'p',
            "C22 pg -- apostrophe then URL",
            PG,
            "SELECT 1 -- don't share postgres://u:Zq9wSecret@h/db",
            Z,
        ),
        c4(
            'p',
            "C23 pg -- apostrophe then key=",
            PG,
            "SELECT 1 -- it's api_key=Zq9wSecret",
            Z,
        ),
        c4(
            'p',
            "C24 my hint with password",
            MY,
            "SELECT /*+ PASSWORD('Zq9wSecret') */ 1",
            Z,
        ),
        // ---------- (b) JSON / key=value / headers / URL query ----------
        c4(
            'p',
            "J01 pg JSON api_key",
            PG,
            "INSERT INTO cfg VALUES ('{\"api_key\": \"Zq9wSecret\"}')",
            Z,
        ),
        c4(
            'p',
            "J02 my JSON backslash quotes",
            MY,
            r#"INSERT INTO cfg VALUES ('{\"api_key\": \"Zq9wSecret\"}')"#,
            Z,
        ),
        c4(
            'p',
            "J03 pg JSON value escaped quote",
            PG,
            r#"INSERT INTO cfg VALUES ('{"api_key": "Zq9w\"Secret"}')"#,
            PIECES,
        ),
        c4(
            'p',
            "J04 pg JSON in JSON",
            PG,
            r#"INSERT INTO cfg VALUES ('{"cfg": "{\"api_key\": \"Zq9wSecret\"}"}')"#,
            Z,
        ),
        c4(
            'p',
            "J05 my JSON in JSON (\\\\\")",
            MY,
            r#"INSERT INTO cfg VALUES ('{"cfg": "{\\"api_key\\": \\"Zq9wSecret\\"}"}')"#,
            Z,
        ),
        c4(
            'p',
            "J06 pg JSON Authorization header",
            PG,
            "INSERT INTO hooks VALUES ('{\"headers\": {\"Authorization\": \"Bearer Zq9wSecret\"}}')",
            Z,
        ),
        c4(
            'p',
            "J07 pg JSON Proxy-Authorization",
            PG,
            "INSERT INTO hooks VALUES ('{\"Proxy-Authorization\": \"Basic Zq9wSecret\"}')",
            Z,
        ),
        c4(
            'p',
            "J08 pg JSON X-Api-Key",
            PG,
            "INSERT INTO hooks VALUES ('{\"X-Api-Key\": \"Zq9wSecret\"}')",
            Z,
        ),
        c4(
            'p',
            "J09 pg JSON accessToken",
            PG,
            "INSERT INTO t VALUES ('{\"accessToken\": \"Zq9wSecret\"}')",
            Z,
        ),
        c4(
            'p',
            "J10 pg JSON client_secret no space",
            PG,
            "INSERT INTO t VALUES ('{\"client_secret\":\"Zq9wSecret\"}')",
            Z,
        ),
        c4(
            'p',
            "J11 pg JSON passphrase",
            PG,
            "INSERT INTO t VALUES ('{\"passphrase\": \"Zq9wSecret\"}')",
            Z,
        ),
        c4(
            'p',
            "J12 pg JSON encryption_key",
            PG,
            "INSERT INTO t VALUES ('{\"encryption_key\": \"Zq9wSecret\"}')",
            Z,
        ),
        c4(
            'p',
            "J13 pg JSON newline before colon",
            PG,
            "INSERT INTO t VALUES ('{\"api_key\"\n: \"Zq9wSecret\"}')",
            Z,
        ),
        c4(
            'p',
            "J14 pg JSON value on next line",
            PG,
            "INSERT INTO t VALUES ('{\"api_key\":\n  \"Zq9wSecret\"}')",
            Z,
        ),
        c4(
            'p',
            "J15 pg JSON unicode-escaped key",
            PG,
            r#"INSERT INTO t VALUES ('{"api_key": "Zq9wSecret"}')"#,
            Z,
        ),
        c4(
            'p',
            "J16 pg YAML api_key: value",
            PG,
            "INSERT INTO t VALUES ('api_key: Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "J17 pg YAML password: value",
            PG,
            "INSERT INTO t VALUES ('password: Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "J18 pg jsonb_build_object key, value",
            PG,
            "SELECT jsonb_build_object('api_key', 'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "J19 my JSON_OBJECT key, value",
            MY,
            "SELECT JSON_OBJECT('password', 'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "J20 my JSON_SET path, value",
            MY,
            "UPDATE t SET cfg = JSON_SET(cfg, '$.api_key', 'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "J21 pg ->> api_key compare",
            PG,
            "SELECT * FROM t WHERE cfg->>'api_key' = 'Zq9wSecret'",
            Z,
        ),
        c4(
            'p',
            "J22 pg JSON private_key PEM",
            PG,
            "INSERT INTO t VALUES ('{\"private_key\": \"-----BEGIN PRIVATE KEY-----\\nZq9wSecret\\n-----END PRIVATE KEY-----\"}')",
            Z,
        ),
        c4(
            'p',
            "J23 pg JSON single-quoted python dict",
            PG,
            "INSERT INTO t VALUES ('{''api_key'': ''Zq9wSecret''}')",
            Z,
        ),
        c4(
            'p',
            "J24 pg key => value",
            PG,
            "INSERT INTO t VALUES ('token => Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H01 pg Authorization header",
            PG,
            "INSERT INTO t VALUES ('Authorization: Bearer Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H02 pg Authorization no space",
            PG,
            "INSERT INTO t VALUES ('Authorization:Bearer Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H03 pg E-string multi-line headers",
            PG,
            r"INSERT INTO t VALUES (E'GET / HTTP/1.1\r\nAuthorization: Basic Zq9wSecret\r\nX-Api-Key: hunter2xyz\r\n')",
            HZ,
        ),
        c4(
            'p',
            "H04 pg real newlines headers",
            PG,
            "INSERT INTO t VALUES ('Host: h\nX-Auth-Token: Zq9wSecret\nAccept: */*')",
            Z,
        ),
        c4(
            'p',
            "H05 pg PRIVATE-TOKEN header",
            PG,
            "INSERT INTO t VALUES ('PRIVATE-TOKEN: Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H06 pg Ocp-Apim-Subscription-Key",
            PG,
            "INSERT INTO t VALUES ('Ocp-Apim-Subscription-Key: Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H07 pg Cookie session=",
            PG,
            "INSERT INTO t VALUES ('Cookie: session=Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H08 pg header value with doubled quote",
            PG,
            "INSERT INTO t VALUES ('Authorization: Bearer Zq9w''Secret')",
            PIECES,
        ),
        c4(
            'p',
            "H09 pg authorization=Bearer kv",
            PG,
            "INSERT INTO t VALUES ('authorization=Bearer Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H10 pg column authorization =",
            PG,
            "UPDATE webhooks SET authorization = 'Bearer Zq9wSecret'",
            Z,
        ),
        c4(
            'p',
            "H11 pg lowercase header",
            PG,
            "INSERT INTO t VALUES ('authorization: bearer Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H12 pg header tab separator",
            PG,
            "INSERT INTO t VALUES ('X-Api-Key:\tZq9wSecret')",
            Z,
        ),
        c4(
            'v',
            "H13 pg Digest response",
            PG,
            "INSERT INTO t VALUES ('Authorization: Digest username=\"u\", response=\"Zq9wSecret\"')",
            Z,
        ),
        c4(
            'p',
            "H14 my header in backslash-escaped string",
            MY,
            r"INSERT INTO t VALUES ('Authorization: Bearer Zq9w\'Secret')",
            PIECES,
        ),
        c4(
            'p',
            "U01 pg URL ?api_key=",
            PG,
            "INSERT INTO t VALUES ('https://api.example.com/v1?api_key=Zq9wSecret&x=1')",
            Z,
        ),
        c4(
            'p',
            "U02 pg URL ?access_token=",
            PG,
            "INSERT INTO t VALUES ('https://h/?access_token=Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "U03 pg URL ?key= (Google)",
            PG,
            "INSERT INTO t VALUES ('https://maps.googleapis.com/maps/api/js?key=Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "U04 pg URL ?sig= (SAS)",
            PG,
            "INSERT INTO t VALUES ('https://a.blob.core.windows.net/c/b?sv=2020&sig=Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "U05 pg URL X-Amz-Signature",
            PG,
            "INSERT INTO t VALUES ('https://b.s3.amazonaws.com/k?X-Amz-Signature=Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "U06 pg URL fragment token",
            PG,
            "INSERT INTO t VALUES ('https://h/cb#access_token=Zq9wSecret&x=1')",
            Z,
        ),
        c4(
            'p',
            "U07 pg URL apiKey camel",
            PG,
            "INSERT INTO t VALUES ('https://h/?apiKey=Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "U08 pg URL client_secret",
            PG,
            "INSERT INTO t VALUES ('https://h/token?client_id=a&client_secret=Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "U09 pg URL in code (unquoted)",
            PG,
            "COPY t FROM 'x' -- see https://h/?token=Zq9wSecret",
            Z,
        ),
        c4(
            'p',
            "U10 pg URL percent-encoded '='",
            PG,
            "INSERT INTO t VALUES ('https://h/?password%3DZq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "U11 pg URL token in my double-quoted",
            MY,
            "INSERT INTO t VALUES (\"https://h/?token=Zq9wSecret\")",
            Z,
        ),
        c4(
            'p',
            "U12 pg URL user:pass in query param",
            PG,
            "INSERT INTO t VALUES ('https://h/?next=postgres://u:Zq9wSecret@db/x')",
            Z,
        ),
        c4(
            'p',
            "B31 my NBE: key after 'a\\'",
            MY,
            r"SELECT AES_DECRYPT(x, 'a\'), AES_DECRYPT(y, 'Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "B32 pg pgp_pub_decrypt_bytea psw",
            PG,
            "SELECT pgp_pub_decrypt_bytea(c, k, 'Zq9wSecret') FROM t",
            Z,
        ),
        c4(
            'p',
            "I21 my NBE init_connect",
            MY,
            r"SET GLOBAL init_connect = 'SELECT AES_DECRYPT(x, ''a\''), AES_DECRYPT(y, ''Zq9wSecret'')'",
            Z,
        ),
        c4(
            'p',
            "H15 my \\n then Authorization",
            MY,
            r"INSERT INTO t VALUES ('Host: h\nAuthorization: Bearer Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H16 my \\n then pw=",
            MY,
            r"INSERT INTO t VALUES ('host=h\npw=Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "H17 pg JSON \\n then Authorization",
            PG,
            r#"INSERT INTO t VALUES ('{"h": "Host: x\nAuthorization: Bearer Zq9wSecret"}')"#,
            Z,
        ),
        c4(
            'p',
            "C25 pg CREATE ROLE split over -- lines",
            PG,
            "-- CREATE ROLE r LOGIN\n--   PASSWORD 'Zq9wSecret'\nSELECT 1",
            Z,
        ),
        c4(
            'p',
            "N01 my NBE path then PASSWORD()",
            MY,
            r"SELECT 'C:\', PASSWORD('Zq9wSecret')",
            Z,
        ),
        c4(
            'p',
            "N02 my NBE path then ALTER USER",
            MY,
            r"SELECT 'C:\'; ALTER USER u IDENTIFIED BY 'Zq9wSecret'",
            Z,
        ),
    ]
}

/// The sixth audit's ordinary text near the new Full patterns.
fn sixth_ordinary() -> Vec<(SqlDialect, &'static str)> {
    vec![
        (
            PG,
            "SELECT encode(digest(email, 'sha256'), 'hex') FROM users",
        ),
        (
            MY,
            "SELECT AES_DECRYPT(secret_col, @key) FROM vault WHERE id = 1",
        ),
        (
            PG,
            "SELECT pgp_sym_decrypt(data, current_setting('app.key')) FROM t",
        ),
        (MY, "SET GLOBAL init_connect = 'SET NAMES utf8mb4'"),
        (MY, "SET GLOBAL init_connect = 'SET autocommit = 0'"),
        (MY, "SET GLOBAL init_connect = ''"),
        (PG, "SELECT 1 -- don't forget the index"),
        (
            PG,
            "/* compute password reset stats */ SELECT count(*) FROM resets",
        ),
        (
            PG,
            "-- TODO: the token column is nullable\nSELECT token FROM sessions",
        ),
        (
            PG,
            "SELECT * FROM t WHERE note = 'Authorization: required for admins'",
        ),
        (PG, "UPDATE posts SET is_secret = 1 WHERE id = 3"),
        (PG, "UPDATE trips SET compass = 'N' WHERE id = 1"),
        (PG, "UPDATE students SET pass = 1 WHERE score >= 50"),
        (PG, "SELECT * FROM api_keys WHERE api_key_id = 5"),
        (PG, "UPDATE models SET max_token = 4096 WHERE id = 1"),
        (PG, "SELECT * FROM t WHERE next_page_token IS NULL"),
        (
            PG,
            "INSERT INTO docs (body) VALUES ('{\"token_type\": \"bearer\", \"expires_in\": 3600}')",
        ),
        (
            PG,
            "INSERT INTO logs (msg) VALUES ('GET /?page=2&sort=asc')",
        ),
        (PG, "SELECT 'https://example.com/docs?lang=en' AS url"),
        (MY, "SELECT * FROM t WHERE msg LIKE '%password reset%'"),
        (MY, "SELECT DECODE(status, 1, 'on', 'off') FROM t"),
        (PG, "SELECT decode('aGVsbG8=', 'base64')"),
        (MY, "SELECT HEX(AES_ENCRYPT(name, @k)) FROM t"),
        (PG, "-- password policy: min 12 chars\nSELECT 1"),
        (PG, "SELECT 1 /* author's note: rotate keys yearly */"),
        (
            PG,
            "-- see https://wiki.example.com/db?section=passwords\nSELECT 1",
        ),
        (PG, "SELECT json_build_object('token_type', 'bearer')"),
        (PG, "SELECT * FROM users WHERE secret_question IS NOT NULL"),
        (PG, "SELECT count(*) FROM events WHERE type = 'bypass'"),
        (PG, "UPDATE flags SET bypass = false WHERE id = 1"),
        (MY, "SELECT * FROM t WHERE header = 'X-Request-Id: 42'"),
        (
            PG,
            "INSERT INTO notes (body) VALUES ('Remember: the API key rotation is on Monday')",
        ),
        (MY, "SELECT ENCRYPT('abc', 'ab')"),
        (
            PG,
            "SELECT * FROM t WHERE created_at > now() -- token bucket refill\n",
        ),
        (PG, "-- ALTER USER docs: see wiki\nSELECT 1"),
    ]
}

#[test]
fn no_secret_of_the_sixth_audit_reaches_the_log() -> TestResult {
    let cases = sixth_audit_cases();
    assert_eq!(cases.len(), 213);
    let found = audit4_leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:\n{}",
        found.len(),
        found.join("\n")
    );
    Ok(())
}

#[test]
fn sixth_audit_ordinary_text_is_not_redacted() {
    for (dialect, statement) in sixth_ordinary() {
        let normalized =
            prepare_statement_text(statement, dialect, TextLevel::Normalized).unwrap_or_default();
        let full = prepare_statement_text(statement, dialect, TextLevel::Full);
        assert!(
            !normalized.contains("[redacted]"),
            "{statement} => {normalized}"
        );
        assert_eq!(full.as_deref(), Some(statement));
    }
}

/// Markers of the eighth audit's leak cases.
const X8: &[&str] = &["XYZZY_SECRET_1", "xyzzysecretword"];

/// The eighth audit's 57 statements with a secret in a credential context
/// (`format` and `||` chains, `CONCAT`, procedure bodies, comments,
/// command lines, `UPDATE ... = ROW(...)`): nothing of the marker may
/// survive at any level.
fn eighth_audit_cases() -> Vec<Case4> {
    vec![
        c4(
            'p',
            "N01 format, account text in a later argument",
            PG,
            "SELECT format('%s %L', 'ALTER ROLE r PASSWORD', 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N02 format positional %2$L",
            PG,
            "SELECT format('ALTER ROLE %1$I PASSWORD %2$L', 'r', 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N03 format with || in arg 0",
            PG,
            "SELECT format('ALTER ROLE %I ' || 'PASSWORD %L', 'r', 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N04 format arg 0 from a variable-like concat",
            PG,
            "SELECT format(concat('ALTER ROLE %I ', 'PASSWORD %L'), 'r', 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N05 || chain with comment between",
            PG,
            "SELECT 'ALTER ROLE r PASSWORD ' || /* c */ 'XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N06 || chain via cast",
            PG,
            "SELECT 'ALTER ROLE r PASSWORD '::text || 'XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N07 || chain, head without PASSWORD then PASSWORD part",
            PG,
            "SELECT 'ALTER ROLE ' || 'r' || ' PASSWORD ' || 'XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N08 || chain, secret first then keyword",
            PG,
            "SELECT 'XYZZY_SECRET_1' || ' AS the new PASSWORD of ALTER ROLE r'",
            X8,
        ),
        c4(
            'p',
            "N09 || chain inside parentheses",
            PG,
            "SELECT ('ALTER ROLE r PASSWORD ' || ('XYZZY_SECRET_1'))",
            X8,
        ),
        c4(
            'p',
            "N10 || chain, upper/lower",
            PG,
            "select 'alter role r password ' || 'XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N11 CONCAT with QUOTE()",
            MY,
            "SET @q = CONCAT('ALTER USER u IDENTIFIED BY ', QUOTE('XYZZY_SECRET_1'))",
            X8,
        ),
        c4(
            'p',
            "N12 PREPARE FROM CONCAT",
            MY,
            "PREPARE s FROM CONCAT('ALTER USER u IDENTIFIED BY ''', 'XYZZY_SECRET_1', '''')",
            X8,
        ),
        c4(
            'p',
            "N13 CONCAT keyword in a later argument",
            MY,
            "SELECT CONCAT('ALTER USER u ', 'IDENTIFIED BY ', '''XYZZY_SECRET_1''')",
            X8,
        ),
        c4(
            'p',
            "N14 CONCAT_WS",
            MY,
            "SELECT CONCAT_WS(' ', 'SET PASSWORD FOR u =', '''XYZZY_SECRET_1''')",
            X8,
        ),
        c4(
            'p',
            "N15 procedure IF THEN CREATE USER",
            MY,
            "CREATE PROCEDURE p() BEGIN IF 1 THEN CREATE USER u IDENTIFIED BY 'XYZZY_SECRET_1'; END IF; END",
            X8,
        ),
        c4(
            'p',
            "N16 procedure ELSEIF THEN unquoted",
            MY,
            "CREATE PROCEDURE p() BEGIN IF a THEN SELECT 1; ELSEIF b THEN ALTER USER u IDENTIFIED BY XYZZY_SECRET_1; END IF; END",
            X8,
        ),
        c4(
            'p',
            "N17 handler body SET PASSWORD",
            MY,
            "CREATE PROCEDURE p() BEGIN DECLARE EXIT HANDLER FOR SQLEXCEPTION SET PASSWORD FOR u = 'XYZZY_SECRET_1'; END",
            X8,
        ),
        c4(
            'p',
            "N18 handler body ALTER USER unquoted",
            MY,
            "CREATE PROCEDURE p() BEGIN DECLARE EXIT HANDLER FOR SQLEXCEPTION ALTER USER u IDENTIFIED BY XYZZY_SECRET_1; END",
            X8,
        ),
        c4(
            'p',
            "N19 CASE THEN in select then account statement",
            MY,
            "SELECT CASE WHEN 1 THEN 2 END; ALTER USER u IDENTIFIED BY XYZZY_SECRET_1",
            X8,
        ),
        c4(
            'p',
            "N20 role statement with THEN inside",
            PG,
            "ALTER ROLE r PASSWORD 'a'; CREATE ROLE \"then\" LOGIN PASSWORD XYZZY_SECRET_1",
            X8,
        ),
        c4(
            'p',
            "N21 PASSWORD FOR across lines in comments",
            MY,
            "SELECT 1 -- SET PASSWORD FOR u\n-- = 'XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N22 two SET PASSWORD FOR in one comment",
            MY,
            "SELECT 1 /* SET PASSWORD FOR a = 'A1b2c3d4'; SET PASSWORD FOR b = 'XYZZY_SECRET_1' */",
            X8,
        ),
        c4(
            'p',
            "N23 comment CRLF split",
            PG,
            "SELECT 1\r\n-- ALTER ROLE r PASSWORD\r\n-- 'XYZZY_SECRET_1'\r\n",
            X8,
        ),
        c4(
            'p',
            "N24 comment unicode before",
            PG,
            "SELECT 1 -- \u{043f}\u{0430}\u{0440}\u{043e}\u{043b}\u{044c}: ALTER ROLE r PASSWORD 'XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N25 comment ALTER ROLE then PASSWORD unshaped",
            PG,
            "SELECT 1 -- ALTER ROLE app PASSWORD xyzzysecretword",
            X8,
        ),
        c4(
            'p',
            "N26 comment IDENTIFIED BY unquoted lowercase",
            MY,
            "SELECT 1 # alter user u identified by xyzzysecretword",
            X8,
        ),
        c4(
            'p',
            "N27 comment psql PGPASSWORD",
            PG,
            "SELECT 1 -- PGPASSWORD=XYZZY_SECRET_1 psql -h db",
            X8,
        ),
        c4(
            'p',
            "N28 comment mysql --password=",
            MY,
            "SELECT 1 -- mysql -u root --password=XYZZY_SECRET_1 db",
            X8,
        ),
        c4(
            'p',
            "N29 comment mysqldump -p",
            MY,
            "SELECT 1 -- mysqldump -uroot -pXYZZY_SECRET_1 db > x.sql",
            X8,
        ),
        c4(
            'p',
            "N30 comment redis-cli --pass",
            PG,
            "SELECT 1 -- redis-cli -h h --pass XYZZY_SECRET_1",
            X8,
        ),
        c4(
            'p',
            "N31 comment redis-cli -a after other flags",
            PG,
            "SELECT 1 -- redis-cli -h h -p 6379 -a XYZZY_SECRET_1 ping",
            X8,
        ),
        c4(
            'p',
            "N32 comment /usr/bin/mysql -p",
            MY,
            "SELECT 1 -- /usr/bin/mysql -h db -pXYZZY_SECRET_1",
            X8,
        ),
        c4(
            'p',
            "N33 bearer in literal lowercase",
            PG,
            "SELECT 'bearer XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N34 bearer token after newline escape",
            PG,
            "SELECT E'x\\nBearer XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N35 yaml quoted single",
            PG,
            "SELECT E'db:\\n  password: ''xyzzysecretword''\\n'",
            X8,
        ),
        c4(
            'p',
            "N36 yaml quoted inside comment",
            PG,
            "SELECT 1 -- password: \"xyzzysecretword\"",
            X8,
        ),
        c4(
            'p',
            "N37 nested do body CRLF",
            PG,
            "DO $$\r\nBEGIN\r\n  ALTER ROLE r PASSWORD 'XYZZY_SECRET_1';\r\nEND\r\n$$",
            X8,
        ),
        c4(
            'p',
            "N38 format() inside DO body",
            PG,
            "DO $$ BEGIN EXECUTE format('ALTER ROLE %I PASSWORD %L', 'r', 'XYZZY_SECRET_1'); END $$",
            X8,
        ),
        c4(
            'p',
            "N39 init_connect CONCAT",
            MY,
            "SET GLOBAL init_connect = CONCAT('ALTER USER u IDENTIFIED BY ', '''XYZZY_SECRET_1''')",
            X8,
        ),
        c4(
            'p',
            "N40 dollar literal holding account stmt (not a routine)",
            PG,
            "SELECT $q$ALTER ROLE r PASSWORD 'XYZZY_SECRET_1'$q$",
            X8,
        ),
        c4(
            'p',
            "N41 deep nested function then key arg",
            MY,
            "SELECT AES_ENCRYPT(f(g(h(1, 2), 3)), 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N42 key arg after nested CONCAT",
            MY,
            "SELECT AES_ENCRYPT(CONCAT('a', 'b'), CONCAT('XYZZY_', 'SECRET_1'))",
            X8,
        ),
        c4(
            'p',
            "N43 key function name quoted",
            PG,
            "SELECT \"pgp_sym_encrypt\"('d', 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N44 key function schema-qualified",
            PG,
            "SELECT public.pgp_sym_encrypt('d', 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N45 key function with comment before paren",
            PG,
            "SELECT pgp_sym_encrypt /* x */ ('d', 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N46 OPTIONS nested paren then password",
            PG,
            "CREATE SERVER s FOREIGN DATA WRAPPER w OPTIONS (host 'h', port '5', password 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N47 USER MAPPING OPTIONS ADD",
            PG,
            "ALTER USER MAPPING FOR u SERVER s OPTIONS (ADD password 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N48 INSERT secret column after nested group",
            PG,
            "INSERT INTO t (a, pw) VALUES ((1, 2), 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N49 INSERT ... ON CONFLICT DO UPDATE SET pw",
            PG,
            "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET pw = 'XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N50 UPDATE SET (a, pw) = ROW(...)",
            PG,
            "UPDATE t SET (a, pw) = ROW(1, 'XYZZY_SECRET_1')",
            X8,
        ),
        c4(
            'p',
            "N51 SET PASSWORD FOR with = in host",
            MY,
            "SET PASSWORD FOR 'u'@'h=x' = 'XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N52 SET PASSWORD FOR via CRLF",
            MY,
            "SET PASSWORD\r\nFOR u\r\n=\r\n'XYZZY_SECRET_1'",
            X8,
        ),
        c4(
            'p',
            "N53 role statement chunk with comment glued",
            PG,
            "ALTER ROLE r PASSWORD/**/XYZZY_SECRET_1",
            X8,
        ),
        c4(
            'p',
            "N54 json key at end of huge literal",
            PG,
            "SELECT '{\"a\": 1, \"client_secret\": \"XYZZY_SECRET_1\"}'",
            X8,
        ),
        c4(
            'p',
            "N55 URL in comment with CRLF",
            PG,
            "SELECT 1 -- postgres://u:XYZZY_SECRET_1@h/db\r\n",
            X8,
        ),
        c4(
            'p',
            "N56 redis AUTH CRLF",
            RD,
            "PING\r\nAUTH u XYZZY_SECRET_1\r\n",
            X8,
        ),
        c4(
            'p',
            "N57 redis CONFIG SET masterauth among many",
            RD,
            "CONFIG SET maxmemory 1 masterauth XYZZY_SECRET_1 appendonly yes",
            X8,
        ),
    ]
}

/// The eighth audit's 38 ordinary statements near the password rules
/// (commented account options, prose, notification text built with `||`,
/// `format` and `CONCAT`, password policy settings, Redis keys): stored
/// as written at `full`, and with no marker at `normalized`.
fn eighth_ordinary() -> Vec<(&'static str, SqlDialect, &'static str)> {
    vec![
        (
            "O01 commented PASSWORD EXPIRE",
            MY,
            "SELECT 1 -- ALTER USER bob PASSWORD EXPIRE NEVER",
        ),
        (
            "O02 commented PASSWORD NULL",
            PG,
            "SELECT 1 -- ALTER ROLE r PASSWORD NULL",
        ),
        (
            "O03 commented IDENTIFIED BY RANDOM PASSWORD",
            MY,
            "SELECT 1 -- CREATE USER u IDENTIFIED BY RANDOM PASSWORD",
        ),
        (
            "O04 commented IDENTIFIED WITH plugin",
            MY,
            "SELECT 1 -- ALTER USER u IDENTIFIED WITH caching_sha2_password",
        ),
        (
            "O05 prose PASSWORD FOR then later =",
            PG,
            "-- TODO: reset password for the reporting user\nSELECT 1;\n-- limit = 100\nSELECT 2",
        ),
        (
            "O06 prose --password flag",
            PG,
            "SELECT 1 -- run with --password to be prompted",
        ),
        (
            "O07 prose password policy",
            PG,
            "SELECT 1 -- the password must be rotated every 90 days",
        ),
        (
            "O08 prose ALTER ROLE PASSWORD",
            PG,
            "SELECT 1 -- ALTER ROLE app PASSWORD is managed by vault",
        ),
        (
            "O09 prose Bearer",
            PG,
            "SELECT 1 -- Bearer tokens expire after one hour",
        ),
        (
            "O10 prose mysql client",
            MY,
            "SELECT 1 -- connect with mysql -u root -p and enter it",
        ),
        (
            "O11 prose MySQL -pedantic",
            MY,
            "SELECT 1 -- MySQL docs -performance tips",
        ),
        (
            "O12 shell snippet psql",
            PG,
            "SELECT 1 -- psql -h db -U app -d app -c 'select 1'",
        ),
        (
            "O13 || notification text",
            PG,
            "SELECT 'Your password was reset at ' || now()::text || ' by ' || 'admin'",
        ),
        (
            "O14 || numbers",
            PG,
            "SELECT 'Password expires in ' || 30 || ' days'",
        ),
        (
            "O15 format notification",
            PG,
            "SELECT format('password reset for %s', 'alice')",
        ),
        (
            "O16 CONCAT notification",
            MY,
            "SELECT CONCAT('password reset for ', name, ' at ', '2024-01-01') FROM users",
        ),
        (
            "O17 role CONNECTION LIMIT",
            PG,
            "ALTER ROLE r WITH CONNECTION LIMIT 5",
        ),
        (
            "O18 role IN ROLE list",
            PG,
            "CREATE ROLE r WITH LOGIN IN ROLE admin, dev",
        ),
        (
            "O19 role IN DATABASE SET",
            PG,
            "ALTER ROLE r IN DATABASE d SET work_mem = '64MB'",
        ),
        (
            "O20 role SET TO",
            PG,
            "ALTER ROLE r SET statement_timeout TO '5min'",
        ),
        (
            "O21 user MAX_QUERIES",
            MY,
            "ALTER USER 'u'@'%' WITH MAX_QUERIES_PER_HOUR 100",
        ),
        ("O22 user ACCOUNT LOCK", MY, "ALTER USER u ACCOUNT LOCK"),
        (
            "O23 user FAILED_LOGIN_ATTEMPTS",
            MY,
            "ALTER USER u FAILED_LOGIN_ATTEMPTS 3 PASSWORD_LOCK_TIME 2",
        ),
        (
            "O24 user COMMENT",
            MY,
            "ALTER USER u COMMENT 'Reporting account'",
        ),
        (
            "O25 role VALID UNTIL",
            PG,
            "ALTER ROLE r VALID UNTIL 'infinity'",
        ),
        (
            "O26 role SET search_path list with quotes",
            PG,
            "ALTER ROLE r SET search_path = 'app', 'public'",
        ),
        (
            "O27 SET GLOBAL validate_password.policy",
            MY,
            "SET GLOBAL validate_password.policy = 'MEDIUM'",
        ),
        (
            "O28 SET password_history",
            MY,
            "SET GLOBAL password_history = 5",
        ),
        (
            "O29 SET default_password_lifetime",
            MY,
            "SET GLOBAL default_password_lifetime = 180",
        ),
        (
            "O30 redis key with password word",
            RD,
            "SET user:1:password_reset_at 1700000000",
        ),
        (
            "O31 redis HGET password_hint field",
            RD,
            "HGET user:1 password_hint",
        ),
        (
            "O32 yaml prose comment",
            PG,
            "SELECT 1 -- note: password: rotate it monthly",
        ),
        (
            "O34 comment --pass in prose",
            PG,
            "SELECT 1 -- use --pass through mode",
        ),
        (
            "O35 CASE THEN with password column",
            PG,
            "SELECT CASE WHEN password IS NULL THEN 'none' ELSE 'set' END FROM users",
        ),
        (
            "O36 Authorization prose in literal",
            PG,
            "SELECT 'Authorization: see docs'",
        ),
        (
            "O37 format() padding with password column",
            PG,
            "SELECT format('%s has a password', name) FROM users",
        ),
        (
            "O38 IDENTIFIED word in a comment then BY",
            PG,
            "SELECT 1 -- users are identified by email\nSELECT a FROM t ORDER BY 'x'",
        ),
    ]
}

#[test]
fn no_secret_of_the_eighth_audit_reaches_the_log() -> TestResult {
    let cases = eighth_audit_cases();
    assert_eq!(cases.len(), 57);
    let found = audit4_leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:
{}",
        found.len(),
        found.join(
            "
"
        )
    );
    Ok(())
}

#[test]
fn eighth_audit_ordinary_text_is_not_redacted() {
    let cases = eighth_ordinary();
    assert_eq!(cases.len(), 37);
    let mut changed = Vec::new();
    for (tag, dialect, statement) in cases {
        let normalized =
            prepare_statement_text(statement, dialect, TextLevel::Normalized).unwrap_or_default();
        let full = prepare_statement_text(statement, dialect, TextLevel::Full).unwrap_or_default();
        if normalized.contains("[redacted]") || full != statement {
            changed.push(format!("{tag}: {full} | {normalized}"));
        }
    }
    assert!(
        changed.is_empty(),
        "{}",
        changed.join(
            "
"
        )
    );
}

/// Markers of the ninth audit's leak cases.
const X9: &[&str] = &["XYZZY", "xyzzy", "secretword", "78797a7a79"];

/// Ninth-audit cases accepted at text level `full` (checked at `none`
/// and `normalized` only): a short, lowercase, digit-free password written
/// unquoted in prose cannot be told from an ordinary word (see the module
/// docs of the redactor, "Limits").
const NINTH_AUDIT_ACCEPTED: [&str; 12] = [
    "A01", "A04", "A05", "A06", "A07", "A10", "D01", "D02", "D03", "D09", "G04", "G08",
];

/// The ninth audit's 147 inputs aimed at each narrowed rule: comments,
/// command lines, `format`/`||`/`CONCAT` chains, JSON keys, escapes.
fn ninth_audit_cases() -> Vec<Case4> {
    let cases = vec![
        (
            "A01 comment: ALTER ROLE ...; then PASSWORD unshaped",
            PG,
            "SELECT 1 -- ALTER ROLE app LOGIN; PASSWORD xyzzysecretword",
        ),
        (
            "A02 block comment: CREATE USER on one line, IDENTIFIED BY next line",
            MY,
            "SELECT 1 /* CREATE USER app\n   IDENTIFIED BY xyzzysecretword */",
        ),
        (
            "A03 line comments: ALTER ROLE split over two lines, unshaped",
            PG,
            "SELECT 1\n-- ALTER ROLE app\n--   PASSWORD xyzzysecretword",
        ),
        (
            "A04 comment: password is <word>",
            PG,
            "SELECT 1 -- the app password is xyzzysecretword",
        ),
        (
            "A05 comment: password <word> (no account head)",
            PG,
            "SELECT 1 -- password xyzzysecretword",
        ),
        (
            "A06 comment: pwd: <word>",
            MY,
            "SELECT 1 # pwd: xyzzysecretword",
        ),
        (
            "A07 comment: password: <word>",
            PG,
            "SELECT 1 -- password: xyzzysecretword",
        ),
        (
            "A08 comment: credentials after ; inside a block",
            PG,
            "SELECT 1 /* step 1: ALTER ROLE app LOGIN; step 2: ALTER ROLE app PASSWORD xyzzysecretword; */",
        ),
        (
            "A09 comment: GRANT ... ; IDENTIFIED BY unshaped",
            MY,
            "SELECT 1 -- GRANT ALL ON db.* TO app IDENTIFIED BY xyzzysecretword",
        ),
        (
            "A10 comment: password word in NOT list then secret",
            PG,
            "SELECT 1 -- ALTER ROLE app PASSWORD to xyzzysecretword",
        ),
        (
            "A11 comment: quoted after ;",
            PG,
            "SELECT 1 -- ALTER ROLE a LOGIN; ALTER ROLE a PASSWORD 'xyzzysecretword'",
        ),
        (
            "A12 comment: PASSWORD = after ; (key=value)",
            PG,
            "SELECT 1 -- done; password = xyzzysecretword",
        ),
        (
            "A13 comment: SET PASSWORD = unshaped",
            MY,
            "SELECT 1 -- SET PASSWORD = xyzzysecretword",
        ),
        (
            "A14 comment: IDENTIFIED BY after newline in block",
            MY,
            "SELECT 1 /*\n ALTER USER 'app'@'%'\n IDENTIFIED BY xyzzysecretword\n*/",
        ),
        (
            "B01 sql: SET PASSWORD FOR u@h = REPLACE current",
            MY,
            "SET PASSWORD FOR 'u'@'%' = 'new_pw_1x' REPLACE 'xyzzysecretword' RETAIN CURRENT PASSWORD",
        ),
        (
            "B02 comment: SET PASSWORD FOR ... REPLACE current",
            MY,
            "SELECT 1 -- SET PASSWORD FOR u = 'new_pw_1x' REPLACE 'xyzzysecretword'",
        ),
        (
            "B03 comment: SET PASSWORD FOR two-part account unquoted",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app@localhost = xyzzysecretword",
        ),
        (
            "B04 comment: SET PASSWORD FOR, = on next comment line",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app\n-- = xyzzysecretword",
        ),
        (
            "B05 comment: SET PASSWORD FOR with PASSWORD()",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app = PASSWORD('xyzzysecretword')",
        ),
        (
            "B06 comment: SET PASSWORD FOR two accounts listed",
            MY,
            "SELECT 1 -- SET PASSWORD FOR a, b = xyzzysecretword",
        ),
        (
            "B07 comment: SET PASSWORD FOR account with spaces around @",
            MY,
            "SELECT 1 -- SET PASSWORD FOR 'app' @ 'localhost' = xyzzysecretword",
        ),
        (
            "B08 comment: SET PASSWORD FOR CURRENT_USER()",
            MY,
            "SELECT 1 -- SET PASSWORD FOR CURRENT_USER() = xyzzysecretword",
        ),
        (
            "B09 comment: SET PASSWORD FOR u TO RANDOM then IDENTIFIED",
            MY,
            "SELECT 1 -- SET PASSWORD FOR u TO RANDOM; ALTER USER u IDENTIFIED BY xyzzysecretword",
        ),
        (
            "B10 sql: SET PASSWORD FOR CURRENT_USER() =",
            MY,
            "SET PASSWORD FOR CURRENT_USER() = 'xyzzysecretword'",
        ),
        (
            "C01 comment: mysql --user root -p<secret>",
            MY,
            "SELECT 1 -- mysql --user root -pxyzzysecretword",
        ),
        (
            "C02 comment: mysql <db> -p<secret>",
            MY,
            "SELECT 1 -- mysql appdb -u root -pxyzzysecretword",
        ),
        (
            "C03 comment: mysql -e \"select 1\" -p<secret>",
            MY,
            "SELECT 1 -- mysql -u root -e \"select 1\" -pxyzzysecretword",
        ),
        (
            "C04 comment: mysql --host db.example.com -p",
            MY,
            "SELECT 1 -- mysql --host db.example.com --port 3306 -pxyzzysecretword",
        ),
        (
            "C05 comment: mysqldump db table -p",
            MY,
            "SELECT 1 -- mysqldump --single-transaction appdb users -pxyzzysecretword",
        ),
        (
            "C06 comment: sudo mysql ... -p",
            MY,
            "SELECT 1 -- sudo mysql -u root -pxyzzysecretword",
        ),
        (
            "C07 comment: mysql -u root -p'secret'",
            MY,
            "SELECT 1 -- mysql -u root -p'xyzzysecretword'",
        ),
        (
            "C08 comment: mysql < dump.sql -p",
            MY,
            "SELECT 1 -- mysql -u root appdb < dump.sql -pxyzzysecretword",
        ),
        (
            "C09 comment: mysql -hdb -P3306 -uapp -p glued",
            MY,
            "SELECT 1 -- mysql -hdb -P3306 -uapp -pxyzzysecretword appdb",
        ),
        (
            "C10 comment: mysql -u root --password <secret>",
            MY,
            "SELECT 1 -- mysql -u root --password xyzzysecretword",
        ),
        (
            "C11 comment: docker exec mysql -p",
            MY,
            "SELECT 1 -- docker exec -it db mysql -uroot -pxyzzysecretword",
        ),
        (
            "C12 comment: mariadb -u x -p",
            MY,
            "SELECT 1 -- mariadb -u app -pxyzzysecretword",
        ),
        (
            "C13 comment: mysql -u 'my user' -p",
            MY,
            "SELECT 1 -- mysql -u 'my user' -pxyzzysecretword",
        ),
        (
            "C14 literal: COPY FROM PROGRAM mysql -p",
            PG,
            "COPY t FROM PROGRAM 'mysql -u root -pxyzzysecretword -e \"select 1\"'",
        ),
        (
            "D01 comment: tool --password unshaped",
            PG,
            "SELECT 1 -- ./deploy.sh --password xyzzysecretword",
        ),
        (
            "D02 comment: pgcli --password unshaped",
            PG,
            "SELECT 1 -- pgcli --password xyzzysecretword",
        ),
        (
            "D03 comment: --pass unshaped",
            PG,
            "SELECT 1 -- vault login --pass xyzzysecretword",
        ),
        (
            "D04 comment: --password shaped",
            PG,
            "SELECT 1 -- ./deploy.sh --password XYZZY_SECRET_1",
        ),
        (
            "D05 comment: --password quoted",
            PG,
            "SELECT 1 -- ./deploy.sh --password 'xyzzysecretword'",
        ),
        (
            "D06 comment: --password=",
            PG,
            "SELECT 1 -- ./deploy.sh --password=xyzzysecretword",
        ),
        (
            "D07 comment: -a for redis-cli after other flags unshaped",
            PG,
            "SELECT 1 -- redis-cli -h h -a xyzzysecretword",
        ),
        (
            "D08 comment: --requirepass",
            RD,
            "# redis-server --requirepass xyzzysecretword\nPING",
        ),
        (
            "D09 comment: mongo -p unshaped",
            PG,
            "SELECT 1 -- mongosh -u admin -p xyzzysecretword",
        ),
        (
            "E01 sql: CREATE USER IDENTIFIED BY 'letmein'-like",
            MY,
            "CREATE USER 'app'@'%' IDENTIFIED BY 'xyzzysecretword'",
        ),
        (
            "E02 sql: ALTER USER IDENTIFIED WITH plugin BY",
            MY,
            "ALTER USER app IDENTIFIED WITH mysql_native_password BY 'xyzzysecretword'",
        ),
        (
            "E03 sql: GRANT ... IDENTIFIED BY",
            MY,
            "GRANT ALL ON *.* TO app@'%' IDENTIFIED BY 'xyzzysecretword'",
        ),
        (
            "E04 sql: MariaDB VIA ed25519 USING PASSWORD()",
            MY,
            "CREATE USER app IDENTIFIED VIA ed25519 USING PASSWORD('xyzzysecretword')",
        ),
        (
            "E05 sql: lowercase identified by",
            MY,
            "alter user app identified by 'xyzzysecretword'",
        ),
        (
            "E06 sql: IDENTIFIED BY with comment between",
            MY,
            "ALTER USER app IDENTIFIED /* new */ BY 'xyzzysecretword'",
        ),
        (
            "E07 sql: IDENTIFIED BY double-quoted",
            MY,
            "ALTER USER app IDENTIFIED BY \"xyzzysecretword\"",
        ),
        (
            "E08 sql: CREATE USER IF NOT EXISTS multiple accounts",
            MY,
            "CREATE USER IF NOT EXISTS a IDENTIFIED BY 'A1b2c3d4e5', b IDENTIFIED BY 'xyzzysecretword'",
        ),
        (
            "E09 sql: IDENTIFIED BY inside PREPARE string",
            MY,
            "PREPARE s FROM 'ALTER USER app IDENTIFIED BY ''xyzzysecretword'''",
        ),
        (
            "E10 sql: IDENTIFIED BY in a string (backslash quotes)",
            MY,
            "SET @s = 'ALTER USER app IDENTIFIED BY \\'xyzzysecretword\\''",
        ),
        (
            "E11 sql: PG CREATE USER WITH PASSWORD",
            PG,
            "CREATE USER app WITH ENCRYPTED PASSWORD 'xyzzysecretword'",
        ),
        (
            "E12 sql: IDENTIFIED BY in procedure body",
            MY,
            "CREATE PROCEDURE p() BEGIN ALTER USER app IDENTIFIED BY 'xyzzysecretword'; END",
        ),
        (
            "E13 sql: MySQL 8 ALTER USER ... DISCARD OLD/RETAIN",
            MY,
            "ALTER USER app IDENTIFIED BY 'xyzzysecretword' RETAIN CURRENT PASSWORD",
        ),
        (
            "E14 sql: ALTER USER REPLACE current",
            MY,
            "ALTER USER USER() IDENTIFIED BY 'new_pw_1x' REPLACE 'xyzzysecretword'",
        ),
        (
            "E15 sql: IDENTIFIED BY with string introducer",
            MY,
            "ALTER USER app IDENTIFIED BY _utf8mb4'xyzzysecretword'",
        ),
        (
            "E16 sql: IDENTIFIED BY X'hex'",
            MY,
            "ALTER USER app IDENTIFIED BY X'78797a7a79'",
        ),
        (
            "E17 sql: string in a SELECT naming credential stmt",
            PG,
            "SELECT 'CREATE USER app IDENTIFIED BY xyzzysecretword'",
        ),
        (
            "F01 || with quote_literal()",
            PG,
            "SELECT 'ALTER ROLE ' || quote_ident('r') || ' PASSWORD ' || quote_literal('xyzzysecretword')",
        ),
        (
            "F02 || doubled quotes around value",
            PG,
            "SELECT 'ALTER ROLE r PASSWORD ''' || 'xyzzysecretword' || ''''",
        ),
        (
            "F03 || ALTER split from ROLE",
            PG,
            "SELECT 'ALTER ' || 'ROLE r ' || 'PASSWORD ' || 'xyzzysecretword'",
        ),
        (
            "F04 concat() with quote_literal",
            PG,
            "SELECT concat('ALTER ROLE r', ' PASSWORD ', quote_literal('xyzzysecretword'))",
        ),
        (
            "F05 format with E'' value",
            PG,
            "SELECT format('ALTER USER %I WITH PASSWORD %L', 'r', E'xyzzysecretword')",
        ),
        (
            "F06 format ENCRYPTED PASSWORD",
            PG,
            "SELECT format('ALTER ROLE %I ENCRYPTED PASSWORD %L', 'r', 'xyzzysecretword')",
        ),
        (
            "F07 format E'' with newline",
            PG,
            "SELECT format(E'ALTER ROLE %I\\nPASSWORD %L', 'r', 'xyzzysecretword')",
        ),
        (
            "F08 format with %s and quote_literal",
            PG,
            "SELECT format('ALTER ROLE %I PASSWORD %s', 'r', quote_literal('xyzzysecretword'))",
        ),
        (
            "F09 format VARIADIC ARRAY",
            PG,
            "SELECT format('ALTER ROLE %I PASSWORD %L', VARIADIC ARRAY['r', 'xyzzysecretword'])",
        ),
        (
            "F10 format: verb in an argument",
            PG,
            "SELECT format('%s %I PASSWORD %L', 'ALTER ROLE', 'r', 'xyzzysecretword')",
        ),
        (
            "F11 CONCAT_WS word by word",
            MY,
            "SELECT CONCAT_WS(' ', 'ALTER', 'USER', 'app', 'IDENTIFIED', 'BY', '''xyzzysecretword''')",
        ),
        (
            "F12 CONCAT lower case with QUOTE()",
            MY,
            "SET @s = concat('alter user app identified by ', quote('xyzzysecretword'))",
        ),
        (
            "F13 CONCAT SET PASSWORD FOR",
            MY,
            "SET @s = CONCAT('SET PASSWORD FOR app = ', QUOTE('xyzzysecretword'))",
        ),
        (
            "F14 CONCAT with double-quoted value",
            MY,
            "SET @s = CONCAT('ALTER USER ', 'app', ' IDENTIFIED BY ', '\"xyzzysecretword\"')",
        ),
        (
            "F15 plpgsql: variable then EXECUTE ||",
            PG,
            "DO $$ DECLARE s text := 'ALTER ROLE r PASSWORD '; BEGIN EXECUTE s || quote_literal('xyzzysecretword'); END $$",
        ),
        (
            "F16 replace() template",
            PG,
            "SELECT replace('ALTER ROLE r PASSWORD $p', '$p', 'xyzzysecretword')",
        ),
        (
            "F17 || with parenthesized second part",
            PG,
            "SELECT 'ALTER ROLE r PASSWORD ' || ('''' || 'xyzzysecretword' || '''')",
        ),
        (
            "F18 || chain through a function call",
            PG,
            "SELECT 'ALTER ROLE r PASSWORD ' || lower('xyzzysecretword')",
        ),
        (
            "F19 format_string positional",
            PG,
            "SELECT format('ALTER ROLE %1$I PASSWORD %2$L VALID UNTIL %3$L', 'r', 'xyzzysecretword', 'infinity')",
        ),
        (
            "F20 MySQL || as concat in PIPES_AS_CONCAT",
            MY,
            "SELECT 'ALTER USER app IDENTIFIED BY ' || '''xyzzysecretword'''",
        ),
        (
            "F21 CONCAT with column then value",
            MY,
            "SELECT CONCAT('ALTER USER ', user, ' IDENTIFIED BY ''', 'xyzzysecretword', '''') FROM mysql.user",
        ),
        (
            "F22 Redis EVAL CONFIG SET requirepass",
            RD,
            "EVAL \"return redis.call('CONFIG','SET','requirepass','xyzzysecretword')\" 0",
        ),
        (
            "F23 Redis EVAL AUTH",
            RD,
            "EVAL \"return redis.call('AUTH','xyzzysecretword')\" 0",
        ),
        (
            "F24 format with password value first",
            PG,
            "SELECT format('ALTER ROLE r PASSWORD %2$L LOGIN %1$s', 'x', 'xyzzysecretword')",
        ),
        (
            "G01 COMMENT ON DATABASE with URL",
            PG,
            "COMMENT ON DATABASE d IS 'primary: postgres://app:xyzzysecretword@db:5432/d'",
        ),
        (
            "G02 ALTER USER COMMENT with URL",
            MY,
            "ALTER USER app COMMENT 'dsn mysql://app:xyzzysecretword@db/app'",
        ),
        (
            "G03 ALTER USER ATTRIBUTE json password",
            MY,
            "ALTER USER app ATTRIBUTE '{\"password\": \"xyzzysecretword\"}'",
        ),
        (
            "G04 COMMENT ON ROLE password: unshaped",
            PG,
            "COMMENT ON ROLE app IS 'password: xyzzysecretword'",
        ),
        (
            "G05 ALTER USER COMMENT conninfo",
            MY,
            "ALTER USER app COMMENT 'host=db user=app password=xyzzysecretword'",
        ),
        (
            "G06 CREATE USER ... COMMENT with pw=",
            MY,
            "CREATE USER app IDENTIFIED BY 'A1b2c3d4e5' COMMENT 'pw=xyzzysecretword'",
        ),
        (
            "G07 ATTRIBUTE with token key",
            MY,
            "CREATE USER app ATTRIBUTE '{\"api_token\": \"xyzzysecretword\"}'",
        ),
        (
            "G08 COMMENT keyword as password: IDENTIFIED BY COMMENT",
            MY,
            "ALTER USER app IDENTIFIED BY 'A1b2c3d4e5' PASSWORD EXPIRE COMMENT 'temp password xyzzysecretword'",
        ),
        (
            "G09 COMMENT ON ROLE with IDENTIFIED BY text",
            PG,
            "COMMENT ON ROLE app IS 'reset with: ALTER ROLE app PASSWORD ''xyzzysecretword'''",
        ),
        (
            "G10 ALTER USER COMMENT holding a full statement",
            MY,
            "ALTER USER app COMMENT 'ALTER USER app IDENTIFIED BY ''xyzzysecretword'''",
        ),
        (
            "G11 COMMENT ON COLUMN with Bearer",
            PG,
            "COMMENT ON COLUMN t.c IS 'call with Authorization: Bearer xyzzysecretword'",
        ),
        (
            "G12 user named comment",
            MY,
            "CREATE USER comment IDENTIFIED BY 'xyzzysecretword'",
        ),
        (
            "G13 user named attribute, quoted",
            MY,
            "ALTER USER 'attribute'@'%' IDENTIFIED BY 'xyzzysecretword'",
        ),
        (
            "G14 ATTRIBUTE before IDENTIFIED",
            MY,
            "CREATE USER app ATTRIBUTE '{}' IDENTIFIED BY 'xyzzysecretword'",
        ),
        (
            "G15 COMMENT then PASSWORD literal",
            MY,
            "ALTER USER app COMMENT 'x' IDENTIFIED BY 'xyzzysecretword'",
        ),
        (
            "H01 UPDATE password_reset_token",
            PG,
            "UPDATE users SET password_reset_token = 'xyzzysecretword' WHERE id = 1",
        ),
        (
            "H02 SET app.password_reset_token",
            PG,
            "SET app.password_reset_token = 'xyzzysecretword'",
        ),
        (
            "H03 MySQL user var @password_reset_token",
            MY,
            "SET @password_reset_token = 'xyzzysecretword'",
        ),
        (
            "H04 INSERT password_reset_token",
            MY,
            "INSERT INTO users (id, password_reset_token) VALUES (1, 'xyzzysecretword')",
        ),
        (
            "H05 numeric password unquoted",
            PG,
            "UPDATE users SET password = 314159265 WHERE id = 1",
        ),
        (
            "H06 numeric pin in password column insert",
            MY,
            "INSERT INTO users (id, password) VALUES (1, 314159265)",
        ),
        (
            "H07 password_salt",
            PG,
            "UPDATE users SET password_salt = 'xyzzysecretword'",
        ),
        (
            "H08 metadata-like name with string value",
            PG,
            "UPDATE users SET password_reset_at = 'xyzzysecretword'",
        ),
        (
            "H09 metadata name, numeric secret",
            PG,
            "UPDATE users SET password_reset_code = 314159265",
        ),
        (
            "H10 SET GLOBAL policy name with string value",
            MY,
            "SET GLOBAL password_history = 'xyzzysecretword'",
        ),
        (
            "H11 PG password_encryption non-switch",
            PG,
            "SET password_encryption = 'xyzzysecretword'",
        ),
        (
            "H12 Redis SET password_reset_token key",
            RD,
            "SET user:1:password_reset_token xyzzysecretword",
        ),
        (
            "H13 Redis HSET password_reset_token",
            RD,
            "HSET user:1 password_reset_token xyzzysecretword",
        ),
        (
            "H14 Redis key metadata ending, non-number",
            RD,
            "SET user:1:password_reset_at xyzzysecretword",
        ),
        (
            "H15 ALTER USER PASSWORD_LOCK_TIME then IDENTIFIED",
            MY,
            "ALTER USER app PASSWORD_LOCK_TIME 2 IDENTIFIED BY 'xyzzysecretword'",
        ),
        (
            "H16 SET validate_password.dictionary_file? no, SET PASSWORD var dotted",
            MY,
            "SET @@session.app_password = 'xyzzysecretword'",
        ),
        (
            "H17 json key password_history string",
            PG,
            "SELECT '{\"password_history\": \"xyzzysecretword\"}'",
        ),
        (
            "H18 UPDATE ... SET pw_hash = numeric",
            PG,
            "UPDATE users SET pin = 1234, password_hash = 314159265",
        ),
        (
            "I01 jsonb_build_object multi-word key ending password",
            PG,
            "SELECT jsonb_build_object('db password', 'xyzzysecretword')",
        ),
        (
            "I02 jsonb_build_object 3-word key",
            PG,
            "SELECT jsonb_build_object('admin db password', 'xyzzysecretword')",
        ),
        (
            "I03 jsonb_build_object 4-word key",
            PG,
            "SELECT jsonb_build_object('the main db password', 'xyzzysecretword')",
        ),
        (
            "I04 JSON_OBJECT 'Password Hash'",
            MY,
            "SELECT JSON_OBJECT('Password Hash', 'xyzzysecretword')",
        ),
        (
            "I05 key with password not last",
            PG,
            "SELECT jsonb_build_object('password for app', 'xyzzysecretword')",
        ),
        (
            "I06 JSON_SET path with space",
            MY,
            "SELECT JSON_SET(c, '$.\"db password\"', 'xyzzysecretword') FROM t",
        ),
        (
            "J01 E'' \\n before pwd=",
            PG,
            "SELECT E'host=h\\npwd=xyzzysecretword'",
        ),
        (
            "J02 E'' \\t before secret=",
            PG,
            "SELECT E'a=1\\tsecret=xyzzysecretword'",
        ),
        (
            "J03 E'' \\x0a before pwd=",
            PG,
            "SELECT E'host=h\\x0apwd=xyzzysecretword'",
        ),
        (
            "J04 E'' octal \\012 before pwd=",
            PG,
            "SELECT E'host=h\\012pwd=xyzzysecretword'",
        ),
        (
            "J05 E'' \\u000a before pwd=",
            PG,
            "SELECT E'host=h\\u000apwd=xyzzysecretword'",
        ),
        (
            "J06 MySQL \\0 before pwd=",
            MY,
            "SELECT 'host=h\\0pwd=xyzzysecretword'",
        ),
        (
            "J07 MySQL \\Z before pwd=",
            MY,
            "SELECT 'host=h\\Zpwd=xyzzysecretword'",
        ),
        (
            "J08 MySQL \\n before secret=",
            MY,
            "SELECT 'host=h\\nsecret=xyzzysecretword'",
        ),
        (
            "J09 JSON \\u0070assword key",
            PG,
            "SELECT '{\"\\u0070assword\": \"xyzzysecretword\"}'",
        ),
        (
            "J10 E'' escaped quote in password",
            PG,
            "ALTER ROLE r PASSWORD E'xyzzy\\'secretword'",
        ),
        (
            "J11 E'' \\x hex inside password",
            PG,
            "ALTER ROLE r PASSWORD E'xyzzy\\x41secretword'",
        ),
        (
            "J12 MySQL \\\\ inside password",
            MY,
            "ALTER USER app IDENTIFIED BY 'xyzzy\\\\secretword'",
        ),
        (
            "J13 MySQL \\Z inside password",
            MY,
            "ALTER USER app IDENTIFIED BY 'xyzzy\\Zsecretword'",
        ),
        (
            "J14 E'' octal inside password",
            PG,
            "ALTER ROLE r PASSWORD E'xyzzy\\101secretword'",
        ),
        (
            "J15 E'' \\u inside password",
            PG,
            "ALTER ROLE r PASSWORD E'xyzzy\\u0041secretword'",
        ),
        (
            "J16 U&'' password",
            PG,
            "ALTER ROLE r PASSWORD U&'xyzzy\\0041secretword'",
        ),
        (
            "J17 E'' unicode multibyte escaped",
            PG,
            "ALTER ROLE r PASSWORD E'xyzzy\\\u{00e9}secretword'",
        ),
        (
            "J18 init_connect with escapes",
            MY,
            "SET GLOBAL init_connect = 'ALTER USER app IDENTIFIED BY \\'xyzzy\\0secretword\\''",
        ),
        (
            "J19 E'' DO body format with escapes",
            PG,
            "DO E'BEGIN EXECUTE format(''ALTER ROLE r PASSWORD %L'', ''xyzzy\\\\nsecretword''); END'",
        ),
        (
            "J20 MySQL \\% \\_ in password",
            MY,
            "ALTER USER app IDENTIFIED BY 'xyzzy\\%secret\\_word'",
        ),
    ];
    cases
        .into_iter()
        .map(|(tag, d, stmt)| {
            let id = tag.split(' ').next().unwrap_or_default();
            let k = if NINTH_AUDIT_ACCEPTED.contains(&id) {
                'v'
            } else {
                'p'
            };
            c4(k, tag, d, stmt, X9)
        })
        .collect()
}

/// The ninth audit's 20 ordinary statements near the narrowed rules:
/// stored as written at `full`, and with no marker at `normalized`.
fn ninth_ordinary() -> Vec<(&'static str, SqlDialect, &'static str)> {
    vec![
        (
            "P01 runbook: mysql then prose",
            MY,
            "SELECT 1 -- see the mysql manual -p 12 for details",
        ),
        (
            "P02 prose IDENTIFIED BY name",
            MY,
            "SELECT 1 -- rows are identified by uuid",
        ),
        (
            "P03 SET PASSWORD FOR in prose",
            MY,
            "SELECT 1 -- remember to set password for new users = later",
        ),
        (
            "P04 comment ALTER ROLE then VALID UNTIL",
            PG,
            "SELECT 1 -- ALTER ROLE app VALID UNTIL 'infinity'",
        ),
        (
            "P05 COMMENT ON COLUMN password",
            PG,
            "COMMENT ON COLUMN users.password IS 'bcrypt hash, never the password'",
        ),
        (
            "P06 ALTER USER PASSWORD_LOCK_TIME UNBOUNDED",
            MY,
            "ALTER USER app PASSWORD_LOCK_TIME UNBOUNDED",
        ),
        (
            "P07 ALTER USER PASSWORD HISTORY DEFAULT",
            MY,
            "ALTER USER app PASSWORD HISTORY DEFAULT PASSWORD REUSE INTERVAL 365 DAY",
        ),
        (
            "P08 PG SET password_encryption md5",
            PG,
            "SET password_encryption = 'md5'",
        ),
        (
            "P09 || greeting with password word",
            PG,
            "SELECT 'Hello ' || name || ', your password expires soon' FROM users",
        ),
        (
            "P10 CONCAT reset link",
            MY,
            "SELECT CONCAT('https://x/reset?user=', id) FROM users",
        ),
        (
            "P11 format password policy text",
            PG,
            "SELECT format('password must have %s chars', 12)",
        ),
        (
            "P12 comment --password prompt",
            PG,
            "SELECT 1 -- psql --password prompts for it",
        ),
        (
            "P13 comment pg_dump example",
            PG,
            "SELECT 1 -- pg_dump -h db -U app appdb > dump.sql",
        ),
        (
            "P14 ALTER USER COMMENT plain",
            MY,
            "ALTER USER app COMMENT 'Reporting account, owner: data team'",
        ),
        (
            "P15 ATTRIBUTE plain json",
            MY,
            "ALTER USER app ATTRIBUTE '{\"team\": \"data\"}'",
        ),
        (
            "P16 SET GLOBAL validate_password.length",
            MY,
            "SET GLOBAL validate_password.length = 12",
        ),
        (
            "P17 UPDATE password_changed_at",
            PG,
            "UPDATE users SET password_changed_at = 1700000000",
        ),
        (
            "P18 comment with IDENTIFIED BY then ORDER BY",
            MY,
            "SELECT a FROM t ORDER BY b -- identified by a",
        ),
        (
            "P19 JSON prose value",
            PG,
            "SELECT '{\"hint\": \"the password is on the wiki\"}'",
        ),
        (
            "P20 Redis GET password_reset_at",
            RD,
            "GET user:1:password_reset_at",
        ),
    ]
}

#[test]
fn no_secret_of_the_ninth_audit_reaches_the_log() -> TestResult {
    let cases = ninth_audit_cases();
    assert_eq!(cases.len(), 147);
    let found = audit4_leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:
{}",
        found.len(),
        found.join(
            "
"
        )
    );
    Ok(())
}

#[test]
fn ninth_audit_ordinary_text_is_not_redacted() {
    let cases = ninth_ordinary();
    assert_eq!(cases.len(), 20);
    let mut changed = Vec::new();
    for (tag, dialect, statement) in cases {
        let normalized =
            prepare_statement_text(statement, dialect, TextLevel::Normalized).unwrap_or_default();
        let full = prepare_statement_text(statement, dialect, TextLevel::Full).unwrap_or_default();
        if normalized.contains("[redacted]") || full != statement {
            changed.push(format!("{tag}: {full} | {normalized}"));
        }
    }
    assert!(
        changed.is_empty(),
        "{}",
        changed.join(
            "
"
        )
    );
}

/// Markers of the tenth audit's secret cases.
const X10: &[&str] = &["XYZZY", "xyzzy", "secretword"];

/// Tenth-audit secret cases accepted at text level `full` (checked at
/// `none` and `normalized` only): a password set through a variable in an
/// earlier commented statement (M1-O02), `psql -W`, which takes no value
/// (M2-O13), and a MySQL literal whose quoting is valid in one backslash
/// mode, where the value is data (L1-B09).
const TENTH_AUDIT_ACCEPTED: [&str; 3] = ["M1-O02", "M2-O13", "L1-B09"];

/// The tenth audit's 92 inputs inside and just outside each rule.
fn tenth_audit_cases() -> Vec<Case4> {
    let cases = vec![
        (
            "M1-I01 SET PASSWORD FOR = PASSWORD('x')",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app = PASSWORD('XYZZY_SECRET_1')",
        ),
        (
            "M1-I02 FOR 'app'@'%' = PASSWORD(\"x\")",
            MY,
            "SELECT 1 -- SET PASSWORD FOR 'app'@'%' = PASSWORD(\"xyzzysecretword\")",
        ),
        (
            "M1-I03 OLD_PASSWORD('x')",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app = OLD_PASSWORD('xyzzysecretword')",
        ),
        (
            "M1-I04 PASSWORD ( 'x' ) spaced",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app = PASSWORD ( 'xyzzysecretword' )",
        ),
        (
            "M1-I05 lower password('x')",
            MY,
            "SELECT 1 -- set password for app = password('xyzzysecretword')",
        ),
        (
            "M1-I06 SET PASSWORD = PASSWORD('x') no FOR",
            MY,
            "SELECT 1 -- SET PASSWORD = PASSWORD('xyzzysecretword')",
        ),
        (
            "M1-I07 FOR a, b = x",
            MY,
            "SELECT 1 -- SET PASSWORD FOR a, b = xyzzysecretword",
        ),
        (
            "M1-I08 FOR a@h, 'b'@'%' = x",
            MY,
            "SELECT 1 -- SET PASSWORD FOR a@h, 'b'@'%' = xyzzysecretword",
        ),
        (
            "M1-I09 PASSWORD('it''s x')",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app = PASSWORD('it''s xyzzysecretword')",
        ),
        (
            "M1-I10 block comment PASSWORD('x')",
            MY,
            "SELECT 1 /* SET PASSWORD FOR app = PASSWORD('xyzzysecretword') */",
        ),
        (
            "M1-I11 FOR app=PASSWORD('x') no spaces",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app=PASSWORD('xyzzysecretword')",
        ),
        (
            "M1-O01 PASSWORD(CONCAT('a','b')) computed",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app = PASSWORD(CONCAT('xyzzysecret', 'word'))",
        ),
        (
            "M1-O02 PASSWORD(@var) then var set",
            MY,
            "SELECT 1 -- SET @p = 'xyzzysecretword'; SET PASSWORD FOR app = PASSWORD(@p)",
        ),
        (
            "M1-O03 FOR line break before =",
            MY,
            "SELECT 1 /* SET PASSWORD FOR app\n   = PASSWORD('xyzzysecretword') */",
        ),
        (
            "M1-O04 FOR the app user = shaped (prose)",
            MY,
            "SELECT 1 -- set password for the app user = XYZZY_SECRET_1",
        ),
        (
            "M1-O05 FOR app TO 'x' (wrong keyword)",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app TO 'xyzzysecretword'",
        ),
        (
            "M1-O06 ENCRYPT('x') value",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app = ENCRYPT('xyzzysecretword')",
        ),
        (
            "M1-O07 FOR app = unquoted, ; then text",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app = xyzzysecretword; done",
        ),
        (
            "L2-I01 SET PASSWORD FOR REPLACE 'cur'",
            MY,
            "SELECT 1 -- SET PASSWORD FOR u = 'n' REPLACE 'xyzzysecretword'",
        ),
        (
            "L2-I02 ALTER USER BY REPLACE 'cur'",
            MY,
            "SELECT 1 -- ALTER USER app IDENTIFIED BY 'n' REPLACE 'xyzzysecretword'",
        ),
        (
            "L2-I03 ALTER USER REPLACE unquoted",
            MY,
            "SELECT 1 -- ALTER USER app IDENTIFIED BY 'n' REPLACE xyzzysecretword",
        ),
        (
            "L2-I04 REPLACE on next comment line",
            MY,
            "SELECT 1 /* ALTER USER app IDENTIFIED BY 'n'\n   REPLACE 'xyzzysecretword' */",
        ),
        (
            "L2-O01 SET PASSWORD; REPLACE after ;",
            MY,
            "SELECT 1 -- SET PASSWORD = 'n'; ALTER USER app REPLACE 'xyzzysecretword'",
        ),
        (
            "M2-I01 mysql --user root -p",
            MY,
            "SELECT 1 -- mysql --user root -pXYZZY_SECRET_1",
        ),
        (
            "M2-I02 mysql -p direct",
            MY,
            "SELECT 1 -- mysql -pxyzzysecretword appdb",
        ),
        (
            "M2-I03 mysql -h db --port 3306 appdb -p",
            MY,
            "SELECT 1 -- mysql -h db --port 3306 appdb -pxyzzysecretword",
        ),
        (
            "M2-I04 mysql -u root -e \"select 1\" -p",
            MY,
            "SELECT 1 -- mysql -u root -e \"select 1\" -pxyzzysecretword",
        ),
        (
            "M2-I05 mysqldump flags db tbl -p > out",
            MY,
            "SELECT 1 -- mysqldump --single-transaction appdb users -pxyzzysecretword > out.sql",
        ),
        (
            "M2-I06 mysql -u 'my user' -p",
            MY,
            "SELECT 1 -- mysql -u 'my user' -pxyzzysecretword",
        ),
        (
            "M2-I07 mysql < dump -p",
            MY,
            "SELECT 1 -- mysql -u root appdb < dump.sql -pxyzzysecretword",
        ),
        (
            "M2-I08 mysql --password <x>",
            MY,
            "SELECT 1 -- mysql -u root --password xyzzysecretword",
        ),
        (
            "M2-I09 pipe to second client",
            MY,
            "SELECT 1 -- mysqldump -uroot -pxyzzysecretword db | mysql -uroot -pXYZZY_SECRET_1 db2",
        ),
        (
            "M2-I10 mysql -p\"two words\"",
            MY,
            "SELECT 1 -- mysql -uroot -p\"xyzzy secretword\" db",
        ),
        (
            "M2-I11 tabs",
            MY,
            "SELECT 1 -- mysql\t-uroot\t-pxyzzysecretword",
        ),
        (
            "M2-I12 in pg comment",
            PG,
            "SELECT 1 -- mysql -uroot -pxyzzysecretword",
        ),
        (
            "M2-I13 mysqlsh --uri -p",
            MY,
            "SELECT 1 -- mysqlsh --uri root@db -pxyzzysecretword",
        ),
        (
            "M2-I14 redis-cli -a",
            RD,
            "GET k # redis-cli -h cache -a xyzzysecretword",
        ),
        (
            "M2-O01 mysql db -p shaped (no other flag)",
            MY,
            "SELECT 1 -- mysql appdb -pXYZZY_SECRET_1",
        ),
        (
            "M2-O02 mysqldump db -p > out shaped",
            MY,
            "SELECT 1 -- mysqldump appdb -pXYZZY_SECRET_1 > out.sql",
        ),
        (
            "M2-O03 line continuation \\ in block comment",
            MY,
            "SELECT 1 /* mysql -h db.example.com -u admin \\\n     -pXYZZY_SECRET_1 appdb */",
        ),
        (
            "M2-O04 line continuation unshaped",
            MY,
            "SELECT 1 /* mysql -h db -u admin \\\n     -pxyzzysecretword appdb */",
        ),
        (
            "M2-O05 sh -c \"mysql -p\"",
            MY,
            "SELECT 1 -- docker exec db sh -c \"mysql -uroot -pXYZZY_SECRET_1 -e 'show databases'\"",
        ),
        (
            "M2-O06 ssh host 'mysqldump -p'",
            MY,
            "SELECT 1 -- ssh db1 'mysqldump -uroot -pxyzzysecretword appdb' > dump.sql",
        ),
        (
            "M2-O07 $(mysql -p)",
            MY,
            "SELECT 1 -- n=$(mysql -uroot -pxyzzysecretword -N -e 'select 1')",
        ),
        (
            "M2-O08 cd x&&mysql -p",
            MY,
            "SELECT 1 -- cd /tmp&&mysql -uroot -pxyzzysecretword",
        ),
        (
            "M2-O09 mysql.exe windows",
            MY,
            "SELECT 1 -- C:\\mysql\\bin\\mysql.exe -uroot -pxyzzysecretword",
        ),
        (
            "M2-O10 MYSQL_PWD env",
            MY,
            "SELECT 1 -- MYSQL_PWD=xyzzysecretword mysql -uroot appdb",
        ),
        (
            "M2-O11 `mysql -p` backticks",
            MY,
            "SELECT 1 -- run `mysql -uroot -pxyzzysecretword appdb` first",
        ),
        (
            "M2-O12 psql --password shaped word",
            PG,
            "SELECT 1 -- psql --password XYZZY_SECRET_1",
        ),
        (
            "M2-O13 psql -W shaped",
            PG,
            "SELECT 1 -- psql -h db -W XYZZY_SECRET_1",
        ),
        (
            "M2-O14 kubectl exec -- mysql -p",
            MY,
            "SELECT 1 -- kubectl exec -it db -- mysql -uroot -pxyzzysecretword",
        ),
        (
            "M2-O15 mysql in sql literal",
            PG,
            "COPY t FROM PROGRAM 'mysql appdb -pXYZZY_SECRET_1 -e \"select 1\"'",
        ),
        (
            "M2-O16 prose: run mysql with -p",
            MY,
            "SELECT 1 -- run mysql with -pXYZZY_SECRET_1",
        ),
        (
            "L4-I01 jsonb multi-word key",
            PG,
            "SELECT jsonb_build_object('the main db password', 'xyzzysecretword')",
        ),
        (
            "L4-I02 JSON_OBJECT 'Password Hash'",
            MY,
            "SELECT JSON_OBJECT('Password Hash', 'xyzzysecretword')",
        ),
        (
            "L4-I03 json_build_object 'password for app'",
            PG,
            "SELECT json_build_object('password for app', 'xyzzysecretword')",
        ),
        (
            "L4-I04 'api key'",
            PG,
            "SELECT jsonb_build_object('stripe api key', 'xyzzysecretword')",
        ),
        (
            "L4-I05 'db-password' hyphen",
            PG,
            "SELECT jsonb_build_object('db-password', 'xyzzysecretword')",
        ),
        (
            "L4-O01 key 'pass word' split",
            PG,
            "SELECT jsonb_build_object('pass word', 'xyzzysecretword')",
        ),
        (
            "L4-O02 JSON literal with spaced key",
            PG,
            "SELECT '{\"the db password\": \"xyzzysecretword\"}'::jsonb",
        ),
        (
            "L5-I01 E'' \\x0a",
            PG,
            "SELECT E'host=h\\x0apwd=xyzzysecretword'",
        ),
        (
            "L5-I02 E'' \\012",
            PG,
            "SELECT E'host=h\\012pwd=xyzzysecretword'",
        ),
        (
            "L5-I03 E'' \\u000a",
            PG,
            "SELECT E'host=h\\u000apwd=xyzzysecretword'",
        ),
        (
            "L5-I04 E'' \\U0000000a",
            PG,
            "SELECT E'host=h\\U0000000apwd=xyzzysecretword'",
        ),
        (
            "L5-I05 MySQL \\0",
            MY,
            "SELECT 'host=h\\0pwd=xyzzysecretword'",
        ),
        (
            "L5-I06 MySQL \\Z",
            MY,
            "SELECT 'host=h\\Zpwd=xyzzysecretword'",
        ),
        (
            "L5-I07 E'' \\b",
            PG,
            "SELECT E'host=h\\bpwd=xyzzysecretword'",
        ),
        (
            "L5-I08 E'' \\f",
            PG,
            "SELECT E'host=h\\fpwd=xyzzysecretword'",
        ),
        (
            "L5-I09 JSON \\u0022 quotes",
            PG,
            "SELECT E'{\\u0022password\\u0022: \\u0022xyzzysecretword\\u0022}'",
        ),
        (
            "L5-I10 E'' \\x encoded key",
            PG,
            "SELECT E'\\x70assword=xyzzysecretword'",
        ),
        (
            "L5-I11 \\t before key",
            PG,
            "SELECT E'host=h\\tpassword=xyzzysecretword'",
        ),
        (
            "L5-O01 double escape \\\\x0a",
            PG,
            "SELECT E'host=h\\\\x0apwd=xyzzysecretword'",
        ),
        (
            "L5-O02 E'' \\r\\n",
            PG,
            "SELECT E'host=h\\r\\npwd=xyzzysecretword'",
        ),
        (
            "L5-O03 four escapes deep",
            PG,
            "SELECT E'host=h\\x5cx0apwd=xyzzysecretword'",
        ),
        (
            "L5-O04 MySQL \\n",
            MY,
            "SELECT 'host=h\\npwd=xyzzysecretword'",
        ),
        (
            "L6-I01 E'password=x\\nhost=h'",
            PG,
            "SELECT E'password=xyzzysecretword\\nhost=h'",
        ),
        (
            "L7-I01 init_connect nested \\' and \\Z",
            MY,
            "SET GLOBAL init_connect = 'ALTER USER u IDENTIFIED BY \\'xyzzysecretword\\Z\\''",
        ),
        (
            "L7-I02 PG E'x\\''",
            PG,
            "ALTER ROLE r PASSWORD E'xyzzysecretword\\''",
        ),
        (
            "L7-I03 MySQL 'x\\''",
            MY,
            "ALTER USER u IDENTIFIED BY 'xyzzysecretword\\''",
        ),
        (
            "L7-I04 MySQL \"x\\\"\"",
            MY,
            "ALTER USER u IDENTIFIED BY \"xyzzysecretword\\\"\"",
        ),
        (
            "L1-B01 PG D'Angelo then hash",
            PG,
            "INSERT INTO users (id, name, password_hash) VALUES (21, 'D'Angelo', '$2b$12$XYZZYabc');\nINSERT INTO users (id, name, password_hash) VALUES (22, 'Bob', '$2b$12$XYZZYdef');",
        ),
        (
            "L1-B02 PG unterminated",
            PG,
            "ALTER ROLE r PASSWORD 'xyzzysecretword",
        ),
        (
            "L1-B03 MySQL it's both modes",
            MY,
            "INSERT INTO t (name, password) VALUES ('it's', 'XYZZY_SECRET_1')",
        ),
        (
            "L1-B04 PG scs-off style \\'",
            PG,
            "INSERT INTO t (a, password) VALUES ('it\\'s', 'XYZZY_SECRET_1')",
        ),
        (
            "L1-B05 PG unterminated $$",
            PG,
            "DO $$ BEGIN ALTER ROLE r PASSWORD 'xyzzysecretword'; END",
        ),
        (
            "L1-B06 PG unterminated /*",
            PG,
            "ALTER ROLE r PASSWORD 'XYZZY_SECRET_1' /* note",
        ),
        (
            "L1-B07 two unescaped quotes, no glue",
            PG,
            "INSERT INTO t (a, b, c) VALUES ('a ' b', '+31 XYZZY secretword', 'c ' d')",
        ),
        (
            "L1-B08 two unescaped quotes, no glue (my)",
            MY,
            "INSERT INTO t (a, b, c) VALUES ('a ' b', '+31 XYZZY secretword', 'c ' d')",
        ),
        (
            "L1-B09 MySQL one-mode break (nbe valid)",
            MY,
            "SELECT 'C:\\' , 'XYZZY_SECRET_1'",
        ),
        (
            "L1-B10 PG nested comment unterminated ending */",
            PG,
            "/* a /* b */ SELECT 'XYZZY_SECRET_1' /* c */",
        ),
        (
            "L1-B11 PG unescaped quote then number",
            PG,
            "INSERT INTO t VALUES ('5 o'9, '-XYZZY secretword', 'x')",
        ),
        (
            "L1-B12 PG two quotes value starts with symbol",
            PG,
            "INSERT INTO t VALUES ('a ' b', '#XYZZY_SECRET_1', 'c ' d');\nSELECT 1",
        ),
    ];
    cases
        .into_iter()
        .map(|(tag, d, stmt)| {
            let id = tag.split(' ').next().unwrap_or_default();
            let k = if TENTH_AUDIT_ACCEPTED.contains(&id) {
                'v'
            } else {
                'p'
            };
            c4(k, tag, d, stmt, X10)
        })
        .collect()
}

/// The tenth audit's 44 valid but unusual SQL texts (dollar tags, escape
/// strings, nested comments, literals glued to keywords): stored as
/// written at `full`, and their error message is not withheld.
fn tenth_valid() -> Vec<(&'static str, SqlDialect, &'static str)> {
    vec![
        (
            "V01 pg dollar tag holding quote",
            PG,
            "SELECT $fn$ it's $x$ inner $x$ $fn$; SELECT 'after'",
        ),
        (
            "V02 pg DO with $q$ and ''",
            PG,
            "DO $body$ BEGIN RAISE NOTICE 'it''s %', $q$a'b$q$; END $body$;",
        ),
        (
            "V03 pg E'' escapes",
            PG,
            "SELECT E'it\\'s \\\\ \\x41 \\n', 'next'",
        ),
        ("V04 pg E'a\\\\'", PG, "SELECT E'a\\\\', 'b'"),
        (
            "V05 pg U&''",
            PG,
            "SELECT U&'d\\0061t\\+000061' AS x, U&'\\0441\\043B' UESCAPE '\\', U&\"c\\0061\"",
        ),
        (
            "V06 pg nested comment with quotes",
            PG,
            "/* outer /* inner it's */ still don't */ SELECT 'a'",
        ),
        (
            "V07 pg standard string ending in \\",
            PG,
            "SELECT 'C:\\path\\', 'b'",
        ),
        (
            "V08 pg string continuation over newline",
            PG,
            "SELECT 'a'\n'b'",
        ),
        ("V09 pg $$||$tag$", PG, "SELECT $$a$$||$tag_1$b$tag_1$"),
        (
            "V10 pg typed literals",
            PG,
            "SELECT 'x'::text, '{\"a\":\"b''c\"}'::jsonb, B'1010', X'1F', N'abc'",
        ),
        (
            "V11 pg identifiers with $",
            PG,
            "SELECT col$1, a$b$c FROM t$x",
        ),
        (
            "V12 pg functions",
            PG,
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$;\nCREATE FUNCTION g() RETURNS text AS $g$ SELECT 'it''s' $g$ LANGUAGE sql",
        ),
        ("V13 pg literal glued to AS", PG, "SELECT 'a'AS x"),
        (
            "V14 pg params",
            PG,
            "SELECT $1::text, $2 FROM t WHERE a = $3",
        ),
        (
            "V15 pg O''Reilly",
            PG,
            "COMMENT ON TABLE t IS 'O''Reilly''s'",
        ),
        (
            "V16 pg empty and quote-only strings",
            PG,
            "SELECT '', '''', ''''''",
        ),
        (
            "V17 pg comments with apostrophes",
            PG,
            "-- it's a comment\nSELECT 1 -- don't\n; SELECT 'x'",
        ),
        ("V18 pg E'\\''", PG, "SELECT E'\\'', e'\\\\'"),
        ("V19 pg unicode dollar tag", PG, "SELECT $é$x'y$é$"),
        ("V20 pg empty comments", PG, "SELECT 1 /**/ /***/ /* * / */"),
        (
            "V21 pg quoted ident with quote",
            PG,
            "SELECT \"it's\"\"q\" FROM \"t'\"",
        ),
        ("V22 pg empty dollar string", PG, "SELECT $a$$a$, $$$$"),
        ("V23 pg literal glued to FROM", PG, "SELECT 'x'FROM t"),
        (
            "V24 pg dollar string glued to LANGUAGE",
            PG,
            "CREATE FUNCTION h() RETURNS int AS $$SELECT 1$$LANGUAGE sql",
        ),
        (
            "V25 pg E string with \\' then ;",
            PG,
            "SELECT E'a\\'; b'; SELECT 2",
        ),
        ("V26 pg $1 then string", PG, "PREPARE p AS SELECT $1||'x'"),
        (
            "M01 my escapes",
            MY,
            "SELECT 'it\\'s', \"say \\\"hi\\\"\", 'a''b'",
        ),
        ("M02 my nbe-mode valid 'C:\\'", MY, "SELECT 'C:\\', 'b'"),
        (
            "M03 my introducers",
            MY,
            "SELECT _utf8mb4'abc' COLLATE utf8mb4_bin, X'4142', b'01', N'x'",
        ),
        (
            "M04 my executable comment",
            MY,
            "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;",
        ),
        (
            "M05 my backtick with quote",
            MY,
            "SELECT `it's` FROM `t``x`",
        ),
        ("M06 my # comment with quote", MY, "SELECT 1 # it's\n, 'a'"),
        ("M07 my 5--1", MY, "SELECT 5--1, 'a'"),
        ("M08 my 'a\\\\'", MY, "SELECT 'a\\\\', 'b'"),
        ("M09 my LIKE escapes", MY, "SELECT 'a%' LIKE '\\%\\_'"),
        (
            "M10 my dump escapes",
            MY,
            "INSERT INTO t VALUES (1,'O\\'Brien','a\\\\b'),(2,'x\\0y','\\Z')",
        ),
        ("M11 my adjacent strings", MY, "SELECT 'a' 'b' 'c'"),
        ("M12 my literal glued to AS", MY, "SELECT 'x'AS y"),
        (
            "M13 my procedure",
            MY,
            "CREATE PROCEDURE p() BEGIN SELECT 'it''s'; SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='x'; END",
        ),
        ("M14 my mixed quotes", MY, "SELECT \"it's\" , 'say \"x\"'"),
        (
            "M15 my comment and literal lookalike",
            MY,
            "SELECT 1 /* it's */ , '/* not comment */'",
        ),
        ("M16 my $ identifiers", MY, "SELECT $col, a$b FROM t"),
        ("M17 my -- without space", MY, "SELECT 1--'x'\n, 2"),
        (
            "M18 my string with \\\\ before quote",
            MY,
            "SELECT 'a\\\\''b'",
        ),
    ]
}

/// The tenth audit's prose and comment lines without a secret, stored as
/// written at `full`. `mysqldump -pipe` (P11) is left out: a `-p` value
/// right after a client name is a password. P10 holds a password and is
/// checked in the unit tests (only its value goes, `host=h` stays).
fn tenth_prose() -> Vec<(&'static str, SqlDialect, &'static str)> {
    vec![
        (
            "P01 MySQL docs -performance tips",
            MY,
            "SELECT 1 -- MySQL docs -performance tips",
        ),
        (
            "P02 psql --password prompts",
            PG,
            "SELECT 1 -- psql --password prompts for it",
        ),
        (
            "P03 the PASSWORD() function is deprecated",
            MY,
            "SELECT 1 -- the PASSWORD() function is deprecated",
        ),
        (
            "P04 password reset then REPLACE()",
            MY,
            "SELECT 1 -- password reset, then REPLACE(name, 'a', 'b')",
        ),
        (
            "P05 mysql client then sentence",
            MY,
            "SELECT 1 -- the mysql client is used here; -p is optional",
        ),
        (
            "P06 mysql -u root -p (prompt) appdb",
            MY,
            "SELECT 1 -- mysql -u root -p appdb",
        ),
        (
            "P07 jsonb 'password length'",
            PG,
            "SELECT jsonb_build_object('password length', 12, 'password policy', 'strong')",
        ),
        (
            "P08 jsonb 'password changed at'",
            PG,
            "SELECT jsonb_build_object('password changed at', '2024-01-01')",
        ),
        (
            "P09 SET PASSWORD FOR app TO RANDOM",
            MY,
            "SELECT 1 -- SET PASSWORD FOR app TO RANDOM",
        ),
        (
            "P12 --password=... doc",
            PG,
            "SELECT 1 -- see the --password flag of pg_dump",
        ),
    ]
}

#[test]
fn no_secret_of_the_tenth_audit_reaches_the_log() -> TestResult {
    let cases = tenth_audit_cases();
    assert_eq!(cases.len(), 92);
    let found = audit4_leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:
{}",
        found.len(),
        found.join(
            "
"
        )
    );
    Ok(())
}

#[test]
fn tenth_audit_valid_sql_is_not_hidden() -> TestResult {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("audit"));
    recorder.set_text_level(TextLevel::Normalized);
    let cases = tenth_valid();
    assert_eq!(cases.len(), 44);
    let mut hidden = Vec::new();
    for (i, (tag, dialect, statement)) in cases.into_iter().enumerate() {
        let full = prepare_statement_text(statement, dialect, TextLevel::Full).unwrap_or_default();
        let connection = format!("v10-{i}");
        recorder.record(
            ActivityEvent::query_failed(
                statement,
                dialect,
                "x",
                &ErrorDetails {
                    code: Some("XX000".into()),
                    message: "something failed".into(),
                },
                1,
            )
            .connection(connection.as_str())
            .build()?,
        )?;
        let event = recorder.query_history(&connection, 1)?.remove(0);
        let message = event.details["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if full != statement || message != "something failed" {
            hidden.push(format!("{tag}: {full} | {message}"));
        }
    }
    assert!(
        hidden.is_empty(),
        "{}",
        hidden.join(
            "
"
        )
    );
    Ok(())
}

#[test]
fn tenth_audit_prose_is_not_redacted() {
    let mut changed = Vec::new();
    for (tag, dialect, statement) in tenth_prose() {
        let full = prepare_statement_text(statement, dialect, TextLevel::Full).unwrap_or_default();
        if full != statement {
            changed.push(format!("{tag}: {full}"));
        }
    }
    assert!(
        changed.is_empty(),
        "{}",
        changed.join(
            "
"
        )
    );
}

/// Every statement of the regression corpus (all audits, secret and
/// ordinary inputs).
fn corpus() -> Vec<(SqlDialect, String)> {
    let mut all: Vec<(SqlDialect, String)> = Vec::new();
    for case in audit_cases()
        .into_iter()
        .chain(second_audit_cases())
        .chain(third_audit_cases())
    {
        all.push((case.d, case.stmt));
    }
    for case in fourth_audit_cases()
        .into_iter()
        .chain(fifth_audit_cases())
        .chain(sixth_audit_cases())
        .chain(eighth_audit_cases())
        .chain(ninth_audit_cases())
        .chain(tenth_audit_cases())
    {
        all.push((case.d, case.stmt));
    }
    for (d, _, stmt) in ORDINARY {
        all.push((d, stmt.to_string()));
    }
    for (d, stmt) in fourth_ordinary().into_iter().chain(sixth_ordinary()) {
        all.push((d, stmt.to_string()));
    }
    for (_, d, stmt) in eighth_ordinary()
        .into_iter()
        .chain(ninth_ordinary())
        .chain(tenth_valid())
        .chain(tenth_prose())
    {
        all.push((d, stmt.to_string()));
    }
    all
}

/// Redacting redacted text changes nothing: `[redacted]` is never read as
/// a value again (`[redacted]]`), over the whole regression corpus.
#[test]
fn redaction_is_idempotent_over_the_corpus() {
    let corpus = corpus();
    assert!(corpus.len() > 1500, "{}", corpus.len());
    let mut unstable = Vec::new();
    for (dialect, statement) in corpus {
        let once = redact_secrets(&statement, dialect);
        let twice = redact_secrets(&once, dialect);
        if once != twice {
            unstable.push(format!("{statement}\n  once:  {once}\n  twice: {twice}"));
        }
    }
    assert!(unstable.is_empty(), "{}", unstable.join("\n"));
}

/// Accepted over-redaction: a MySQL client in command position takes every
/// `-p<value>` as a password, also when the line is prose (the eighth
/// audit's O33). `mysql appdb -pSECRET` is a common real command line.
#[test]
fn client_in_command_position_redacts_every_p_value() {
    for (text, expected) in [
        (
            "SELECT 1 -- mysql replication -primary only",
            "SELECT 1 -- mysql replication -p[redacted] only",
        ),
        (
            "SELECT 1 -- mysql appdb -phunter",
            "SELECT 1 -- mysql appdb -p[redacted]",
        ),
        (
            "SELECT 1 -- sudo MYSQL_HOST=db mysql appdb -phunter",
            "SELECT 1 -- sudo MYSQL_HOST=db mysql appdb -p[redacted]",
        ),
        (
            "SELECT 1 -- docker exec -it db mysqldump appdb -phunter > x.sql",
            "SELECT 1 -- docker exec -it db mysqldump appdb -p[redacted] > x.sql",
        ),
        // Not in command position, not shaped: prose stays.
        (
            "SELECT 1 -- MySQL docs -performance tips",
            "SELECT 1 -- MySQL docs -performance tips",
        ),
        (
            "SELECT 1 -- read about mysql replication -primary only",
            "SELECT 1 -- read about mysql replication -primary only",
        ),
    ] {
        assert_eq!(redact_secrets(text, MY), expected);
    }
}

/// Eleventh-audit command lines whose `-p` value is kept at text level
/// `full` (checked at `none` and `normalized` only): an upper-case tool
/// name, which no shell runs (S-O01, S-O02); an unquoted path with spaces,
/// which a shell splits (S-O11); `cmd` `^` and PowerShell backtick
/// continuations (S-O21, S-O22); a command in prose after a list number or
/// a label (S-O27, S-O28); `sh -c` nested deeper than the reader goes
/// (S-O29); and a command line stored as data in a literal (S-O71).
const ELEVENTH_AUDIT_ACCEPTED: [&str; 11] = [
    "S-O01", "S-O02", "S-O11", "S-O21", "S-O22", "S-O27", "S-O28", "S-O29", "S-O71",
    // `time` and `!` lead a command only on a line that reads as shell
    // (round 13): as English words they over-redacted prose.
    "S-I14", "S-O20",
];

/// Shell command lines in comments (and in Redis text): client paths,
/// quoting, wrappers, prompts, cron lines, routine bodies.
fn eleventh_shell_cases() -> Vec<(&'static str, SqlDialect, String)> {
    let v = "-pxyzzysecretword";
    let c = |tag: &'static str, line: &str| (tag, MY, format!("SELECT 1 -- {line}"));
    let mut out = vec![
        c("S-I01 control: mysql appdb -p", &format!("mysql appdb {v}")),
        c(
            "S-I02 NAME=value mysql",
            &format!("MYSQL_HOST=db mysql appdb {v}"),
        ),
        c(
            "S-I03 two assignments",
            &format!("A=1 B=2 mysqldump appdb {v} > x.sql"),
        ),
        c(
            "S-I04 docker compose exec",
            &format!("docker compose exec db mysql appdb {v}"),
        ),
        c(
            "S-I05 docker compose exec -T dump",
            &format!("docker compose exec -T db mysqldump appdb {v} > x.sql"),
        ),
        c(
            "S-I06 kubectl exec -- mysql",
            &format!("kubectl exec -it pod -- mysql appdb {v}"),
        ),
        c(
            "S-I07 kubectl -n x exec deploy/db --",
            &format!("kubectl -n prod exec deploy/db -- mysql appdb {v}"),
        ),
        c(
            "S-I08 xargs mysql",
            &format!("echo appdb | xargs mysql {v}x appdb"),
        ),
        c(
            "S-I09 xargs -I{{}} mysql",
            &format!("cat dbs | xargs -I{{}} mysqldump {{}} {v} --result-file={{}}.sql"),
        ),
        c(
            "S-I10 watch -n 5 mysql",
            &format!("watch -n 5 mysql appdb {v} -e 'show processlist'"),
        ),
        c(
            "S-I11 nice -n 19 mysqldump",
            &format!("nice -n 19 mysqldump appdb {v} > x.sql"),
        ),
        c("S-I12 env -i mysql", &format!("env -i mysql appdb {v}")),
        c("S-I13 command mysql", &format!("command mysql appdb {v}")),
        c("S-I14 time mysql", &format!("time mysql appdb {v}")),
        c(
            "S-I15 timeout 30 mysql",
            &format!("timeout 30 mysql appdb {v}"),
        ),
        c(
            "S-I16 nohup ... &",
            &format!("nohup mysqldump appdb {v} > x.sql &"),
        ),
        c(
            "S-I17 sudo -u mysql mysql",
            &format!("sudo -u mysql mysql appdb {v}"),
        ),
        c(
            "S-I18 ssh -i key user@db mysql",
            &format!("ssh -i key.pem admin@db mysql appdb {v}"),
        ),
        c(
            "S-I19 ssh db \"mysql\"",
            &format!("ssh db \"mysql appdb {v}\""),
        ),
        c(
            "S-I20 bash -lc 'mysqldump > x'",
            &format!("bash -lc 'mysqldump appdb {v} > x.sql'"),
        ),
        c(
            "S-I21 su - mysql -c \"mysql\"",
            &format!("su - mysql -c \"mysql appdb {v}\""),
        ),
        c(
            "S-I22 sh -c \"sh -c 'mysql'\" depth 2",
            &format!("sh -c \"sh -c 'mysql appdb {v}'\""),
        ),
        c("S-I23 heredoc after", &format!("mysql appdb {v} <<EOF")),
        c(
            "S-I24 cat heredoc | mysql",
            &format!("cat <<'EOF' | mysql appdb {v}"),
        ),
        c(
            "S-I25 a | mysql",
            &format!("gunzip < x.sql.gz | mysql appdb {v}"),
        ),
        c("S-I26 && mysql", &format!("cd /tmp && mysql appdb {v}")),
        c("S-I27 || mysql", &format!("false || mysql appdb {v}")),
        c("S-I28 ; mysql", &format!("cd /tmp; mysql appdb {v}")),
        c(
            "S-I29 $( mysql )",
            &format!("n=$(mysql appdb {v} -N -e 'select 1')"),
        ),
        c("S-I30 `mysql`", &format!("n=`mysql appdb {v}`")),
        c("S-I31 ( mysql )", &format!("(mysql appdb {v})")),
        c("S-I32 ;mysql glued", &format!("cd /tmp;mysql appdb {v}")),
        c("S-I33 /usr/bin/mysql", &format!("/usr/bin/mysql appdb {v}")),
        c(
            "S-I34 C:\\xampp\\...\\mysql.exe",
            &format!("C:\\xampp\\mysql\\bin\\mysql.exe appdb {v}"),
        ),
        c("S-I35 .\\mysql.exe", &format!(".\\mysql.exe appdb {v}")),
        c(
            "S-I36 mariadb-dump",
            &format!("mariadb-dump appdb {v} > x.sql"),
        ),
        c(
            "S-I37 mysqlbinlog",
            &format!("mysqlbinlog binlog.000001 {v}"),
        ),
        c("S-I38 tabs", &format!("mysql\tappdb\t{v}")),
        c("S-I39 -p'quoted'", "mysql appdb -p'xyzzy secretword'"),
        c("S-I40 -p\"quoted\"", "mysql appdb -p\"xyzzysecretword\""),
        c(
            "S-I41 escaped quotes in sh -c",
            "sh -c \"mysql -uroot -p\\\"xyzzy secretword\\\" appdb\"",
        ),
        c(
            "S-I42 --password value",
            "mysql appdb --password xyzzysecretword",
        ),
        c(
            "S-I43 --password=value",
            "mysql appdb --password=xyzzysecretword",
        ),
        c("S-I44 MYSQL_PWD=", "MYSQL_PWD=xyzzysecretword mysql appdb"),
        c(
            "S-I45 docker run -e MYSQL_ROOT_PASSWORD=",
            "docker run -d -e MYSQL_ROOT_PASSWORD=xyzzysecretword mysql:8",
        ),
        c(
            "S-I46 PGPASSWORD= psql",
            "PGPASSWORD=xyzzysecretword psql -h db appdb",
        ),
        c(
            "S-I47 psql URL",
            "psql \"postgresql://app:xyzzysecretword@db/appdb\"",
        ),
        c(
            "S-I48 pg_dump conninfo",
            "pg_dump \"host=db user=app password=xyzzysecretword\" > x.sql",
        ),
        c(
            "S-I49 redis-cli -a",
            "redis-cli -h cache -a xyzzysecretword",
        ),
        c(
            "S-I50 REDISCLI_AUTH=",
            "REDISCLI_AUTH=xyzzysecretword redis-cli -h cache",
        ),
        c(
            "S-I51 redis-cli --user --pass",
            "redis-cli --user app --pass xyzzysecretword",
        ),
        c(
            "S-I52 redis-cli -u URL",
            "redis-cli -u redis://app:xyzzysecretword@cache:6379",
        ),
        c(
            "S-I53 continuation over comment lines",
            &format!("mysql -h db appdb \\\n-- {v}"),
        ),
        c("S-I54 mysql -p then db", &format!("mysql {v} appdb")),
        // Forms outside the listed positions.
        c("S-O01 Mysql (mixed case)", &format!("Mysql appdb {v}")),
        c("S-O02 MYSQL upper", &format!("MYSQL appdb {v}")),
        c(
            "S-O03 /Applications/MAMP path",
            &format!("/Applications/MAMP/Library/bin/mysql appdb {v}"),
        ),
        c(
            "S-O04 $HOME/bin/mysql",
            &format!("$HOME/bin/mysql appdb {v}"),
        ),
        c(
            "S-O05 /opt/homebrew/Cellar/... (lower path, control)",
            &format!("/opt/homebrew/opt/mysql-client/bin/mysql appdb {v}"),
        ),
        c(
            "S-O06 \"/opt/my sql/bin/mysql\" quoted path",
            &format!("\"/opt/my sql/bin/mysql\" appdb {v}"),
        ),
        c(
            "S-O07 PowerShell & \"C:\\Program Files\\...\\mysql.exe\" db -p",
            &format!("& \"C:\\Program Files\\MySQL\\MySQL Server 8.0\\bin\\mysql.exe\" appdb {v}"),
        ),
        c(
            "S-O08 PowerShell & \"...mysql.exe\" -uroot -p (flagged)",
            &format!("& \"C:\\Program Files\\MySQL\\MySQL Server 8.0\\bin\\mysql.exe\" -uroot {v}"),
        ),
        c(
            "S-O09 cmd \"...mysqldump.exe\" -uroot -p",
            &format!(
                "\"C:\\Program Files\\MySQL\\MySQL Server 8.0\\bin\\mysqldump.exe\" -uroot {v} appdb > x.sql"
            ),
        ),
        c(
            "S-O10 PowerShell & 'C:\\...\\mysql.exe' -p",
            &format!("& 'C:\\Program Files\\MariaDB 11.4\\bin\\mysql.exe' appdb {v}"),
        ),
        c(
            "S-O11 unquoted C:\\Program Files path",
            &format!("C:\\Program Files\\MySQL\\MySQL Server 8.0\\bin\\mysql.exe appdb {v}"),
        ),
        c(
            "S-O12 stdbuf -oL mysql",
            &format!("stdbuf -oL mysql appdb {v}"),
        ),
        c("S-O13 $ prompt", &format!("$ mysql appdb {v}")),
        c("S-O14 # prompt", &format!("# mysqldump appdb {v} > x.sql")),
        c(
            "S-O15 PS C:\\> prompt",
            &format!("PS C:\\> mysql appdb {v}"),
        ),
        c(
            "S-O16 cron line",
            &format!("0 3 * * * mysqldump appdb {v} > /backup/x.sql"),
        ),
        c(
            "S-O17 for ...; do mysqldump",
            &format!("for db in a b; do mysqldump $db {v} > $db.sql; done"),
        ),
        c(
            "S-O18 if ...; then mysql",
            &format!("if true; then mysql appdb {v}; fi"),
        ),
        c("S-O19 { mysql; }", &format!("{{ mysql appdb {v}; }}")),
        c("S-O20 ! mysql", &format!("! mysql appdb {v}")),
        c(
            "S-O21 cmd ^ continuation",
            &format!("mysql.exe -uroot ^\n-- {v} appdb"),
        ),
        c(
            "S-O22 PowerShell ` continuation",
            &format!("mysql.exe -uroot `\n-- {v} appdb"),
        ),
        c(
            "S-O23 mysqladmin password NEW",
            "mysqladmin -uroot -pOld1 password xyzzysecretword",
        ),
        c(
            "S-O24 mysqladmin password 'NEW'",
            "mysqladmin -uroot password 'xyzzysecretword'",
        ),
        c(
            "S-O25 redis-cli AUTH as argument",
            "redis-cli -h cache AUTH app xyzzysecretword",
        ),
        c(
            "S-O26 redis-cli CONFIG SET requirepass",
            "redis-cli -h cache CONFIG SET requirepass xyzzysecretword",
        ),
        c("S-O27 1. numbered step", &format!("1. mysql appdb {v}")),
        c("S-O28 Run: mysql", &format!("Run: mysql appdb {v}")),
        c(
            "S-O29 sh -c 4 deep",
            &format!(
                "sh -c \"bash -c 'sh -c \\\"bash -c mysql_appdb\\\"'\" ; sh -c \"sh -c 'sh -c \\\"sh -c \\\\\\\"mysql appdb {v}\\\\\\\"\\\"'\""
            ),
        ),
        c(
            "S-O30 sudo -E env PATH=x mysql",
            &format!("sudo -E env PATH=/x mysql appdb {v}"),
        ),
        c(
            "S-O31 exec >log mysql",
            &format!("exec 2>&1 mysql appdb {v}"),
        ),
    ];
    // Same line in other comment and dialect forms.
    let line = format!("mysqldump appdb {v} > x.sql");
    out.push(("S-I60 block comment", MY, format!("SELECT 1 /* {line} */")));
    out.push(("S-I61 # comment (MySQL)", MY, format!("SELECT 1 # {line}")));
    out.push(("S-I62 PG -- comment", PG, format!("SELECT 1 -- {line}")));
    out.push((
        "S-I63 PG nested block comment",
        PG,
        format!("SELECT 1 /* x /* y */ {line} */"),
    ));
    out.push((
        "S-I64 javadoc block * line",
        MY,
        format!("SELECT 1 /*\n * {line}\n */"),
    ));
    out.push(("S-I65 Redis # line", RD, format!("# {line}")));
    out.push((
        "S-I66 Redis line after command",
        RD,
        format!("GET k # {line}"),
    ));
    out.push(("S-I67 Redis plain line", RD, format!("ECHO {line}")));
    out.push((
        "S-I68 redis-cli in Redis # line",
        RD,
        "# redis-cli -h cache -a xyzzysecretword".to_string(),
    ));
    out.push((
        "S-I69 two comments joined",
        MY,
        format!("SELECT 1; -- step 1\n-- {line}\nSELECT 2"),
    ));
    out.push((
        "S-I70 PG COPY PROGRAM unshaped",
        PG,
        "COPY t FROM PROGRAM 'mysql -uroot -pxyzzysecretword appdb -e \"select 1\"'".to_string(),
    ));
    out.push((
        "S-O71 literal at Full (data)",
        MY,
        "INSERT INTO runbook (cmd) VALUES ('mysql appdb -pxyzzysecretword')".to_string(),
    ));
    out.push((
        "S-O72 MySQL procedure comment",
        MY,
        format!("CREATE PROCEDURE p() BEGIN\n  -- {line}\n  SELECT 1;\nEND"),
    ));
    out.push((
        "S-I73 PG function body comment",
        PG,
        format!("CREATE FUNCTION f() RETURNS int AS $$\n-- {line}\nSELECT 1 $$ LANGUAGE sql"),
    ));
    out
}

/// Comment prose with `-p` look-alikes that must stay at text level
/// `full`.
fn eleventh_prose() -> Vec<(&'static str, SqlDialect, &'static str)> {
    vec![
        (
            "P01 MySQL docs -performance",
            MY,
            "SELECT 1 -- MySQL docs -performance tips",
        ),
        (
            "P02 read about mysql -primary",
            MY,
            "SELECT 1 -- read about mysql replication -primary only",
        ),
        (
            "P03 Mysql -prefixed (mixed case at start)",
            MY,
            "SELECT 1 -- Mysql -prefixed tables are legacy",
        ),
        (
            "P04 the mysql client -prints",
            MY,
            "SELECT 1 -- the mysql client -prints warnings",
        ),
        (
            "P05 psql -p 5432",
            PG,
            "SELECT 1 -- psql -h db -p 5432 appdb",
        ),
        ("P06 redis-cli -p 6379", RD, "# redis-cli -h cache -p 6379"),
        (
            "P07 mysql: -pretty",
            MY,
            "SELECT 1 -- mysql: -pretty output is off",
        ),
        (
            "P08 run mysql -P3306",
            MY,
            "SELECT 1 -- mysql -h db -P 3306 -u app -p appdb",
        ),
        (
            "P09 mysql -p (prompt) appdb",
            MY,
            "SELECT 1 -- mysql -uroot -p appdb",
        ),
        (
            "P10 sudo mysql (socket auth)",
            MY,
            "SELECT 1 -- sudo mysql -e 'select 1'",
        ),
        (
            "P11 path with -p in dir name",
            MY,
            "SELECT 1 -- ls /var/lib/mysql -lah",
        ),
        (
            "P12 mysql in table name",
            MY,
            "SELECT * FROM mysql.user WHERE host = '-pfoo'",
        ),
        (
            "P13 ps -p",
            MY,
            "SELECT 1 -- ps -p 123; mysql -e 'show processlist'",
        ),
        (
            "P14 grep -p after pipe",
            MY,
            "SELECT 1 -- mysql -e 'show tables' | grep -panic",
        ),
        (
            "P15 mysql then other cmd -p",
            MY,
            "SELECT 1 -- mysql -e 'select 1' && mkdir -p backups",
        ),
        (
            "P16 mkdir -p after mysql on same segment?",
            MY,
            "SELECT 1 -- mysqldump appdb > x.sql; mkdir -p /backup",
        ),
        ("P17 Redis # mkdir -p", RD, "# mkdir -p /var/lib/redis"),
    ]
}

/// Valid but unusual SQL that must not fall back.
fn eleventh_valid() -> Vec<(&'static str, SqlDialect, String)> {
    let s = |x: &str| x.to_string();
    vec![
        (
            "V01 PG nested dollar EXECUTE",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void AS $outer$ BEGIN EXECUTE $inner$ SELECT 'it''s' $inner$; END $outer$ LANGUAGE plpgsql",
            ),
        ),
        (
            "V02 PG $$ body with lone '",
            PG,
            s("CREATE FUNCTION f() RETURNS text AS $$ SELECT 'don''t' -- won't\n $$ LANGUAGE sql"),
        ),
        (
            "V03 PG body with apostrophe in comment only",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int AS $$\n-- it's the owner's call\nSELECT 1 $$ LANGUAGE sql",
            ),
        ),
        (
            "V04 PG format with $f$",
            PG,
            s("DO $$ BEGIN EXECUTE format($f$SELECT %L, 'x''y'$f$, 'z'); END $$"),
        ),
        (
            "V05 PG three nested tags",
            PG,
            s("DO $a$ BEGIN EXECUTE $b$ DO $c$ BEGIN RAISE NOTICE 'it''s'; END $c$ $b$; END $a$"),
        ),
        (
            "V06 PG body with $1 params",
            PG,
            s(
                "CREATE FUNCTION f(a text) RETURNS text AS $$ SELECT $1 || '''' || $1 $$ LANGUAGE sql",
            ),
        ),
        (
            "V07 PG trigger with NEW.x and quotes",
            PG,
            s(
                "CREATE OR REPLACE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $function$\nBEGIN\n  IF NEW.name ~ '^[a-z'']+$' THEN\n    RAISE EXCEPTION 'bad name: %', NEW.name;\n  END IF;\n  RETURN NEW;\nEND;\n$function$;",
            ),
        ),
        (
            "V08 PG E'' with \\' and \\\\",
            PG,
            s("SELECT E'it\\'s', E'C:\\\\', E'\\\\'"),
        ),
        (
            "V09 PG U&'' UESCAPE",
            PG,
            s("SELECT U&'d!0061t!+000061' UESCAPE '!'"),
        ),
        ("V10 PG standard 'C:\\'", PG, s("SELECT 'C:\\', 'b'")),
        (
            "V11 PG nested comment with quotes",
            PG,
            s("/* outer /* it's */ don't */ SELECT 1"),
        ),
        ("V12 PG 'a'\\n'b' continuation", PG, s("SELECT 'a'\n'b'")),
        (
            "V13 PG typed literals",
            PG,
            s("SELECT date'2024-01-01', interval'1 day', B'1010', X'FF', 1_000_000, 0x1F, .5e-3"),
        ),
        (
            "V14 PG 'x'::\"varchar\"",
            PG,
            s("SELECT 'x'::\"varchar\", 'y'COLLATE \"C\""),
        ),
        (
            "V15 PG pg_dump function",
            PG,
            s(
                "--\n-- Name: f(); Type: FUNCTION\n--\n\nCREATE FUNCTION public.f() RETURNS text\n    LANGUAGE plpgsql\n    AS $_$\nBEGIN\n  RETURN 'O''Brien' || $q$it's$q$;\nEND;\n$_$;\n\nALTER FUNCTION public.f() OWNER TO app;\nCOMMENT ON FUNCTION public.f() IS 'Returns the owner''s name';\n",
            ),
        ),
        (
            "V16 PG COPY FROM stdin with data rows",
            PG,
            s("COPY public.users (id, name) FROM stdin;\n1\tO'Brien\n2\tit's\n\\.\n"),
        ),
        (
            "V17 PG SELECT 'a'AS\"b\" (keyword glued to quote)",
            PG,
            s("SELECT 'a'AS\"b\""),
        ),
        ("V18 PG 'x'alias glued", PG, s("SELECT 'x'label")),
        (
            "V19 PG LIKE'x' minified",
            PG,
            s("SELECT * FROM t WHERE a LIKE'%x%'AND b='y'"),
        ),
        (
            "V20 PG a='x'OR b='y'",
            PG,
            s("SELECT * FROM t WHERE a='x'OR b='y'"),
        ),
        (
            "V21 MySQL dump backslash",
            MY,
            s(
                "INSERT INTO `t` VALUES (1,'O\\'Brien','C:\\\\path\\\\'),(2,'it\\'s','\\\\'),(3,'say \\\"hi\\\"','tab\\there');",
            ),
        ),
        (
            "V22 MySQL NBE-only 'C:\\'",
            MY,
            s("INSERT INTO t VALUES ('C:\\', 'O''Brien')"),
        ),
        (
            "V23 MySQL '\\'' (backslash only)",
            MY,
            s("SELECT '\\'', 'x'"),
        ),
        (
            "V24 MySQL introducers",
            MY,
            s("SELECT _utf8mb4'x' COLLATE utf8mb4_bin, _binary 0x41, N'y', X'41', b'01', 0b01"),
        ),
        (
            "V25 MySQL /*! */ and DELIMITER",
            MY,
            s(
                "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;\nDELIMITER ;;\nCREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 'it''s'; END ;;\nDELIMITER ;",
            ),
        ),
        (
            "V26 MySQL procedure with DECLARE",
            MY,
            s("CREATE PROCEDURE p() BEGIN DECLARE v VARCHAR(10) DEFAULT 'it''s'; SELECT v; END"),
        ),
        (
            "V27 MySQL non-nesting comments",
            MY,
            s("/* a /* b */ SELECT 'it''s'"),
        ),
        (
            "V28 MySQL # it's comment",
            MY,
            s("SELECT 1 # it's fine\n, 'b'"),
        ),
        ("V29 MySQL 5--'x'", MY, s("SELECT 5--'1'")),
        (
            "V30 MySQL \"it's\" dq string",
            MY,
            s("SELECT \"it's\", 'a' 'b'"),
        ),
        ("V31 MySQL 'x'AS`y`", MY, s("SELECT 'x'AS`y`")),
        (
            "V32 MySQL LIKE'x' minified",
            MY,
            s("SELECT * FROM t WHERE a LIKE'%x%'AND b='y'"),
        ),
        (
            "V33 MySQL mysqldump header",
            MY,
            s(
                "-- MySQL dump 10.13  Distrib 8.0.36\n/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */;\nLOCK TABLES `users` WRITE;\nINSERT INTO `users` VALUES (1,'D\\'Angelo','a@b.c','$2y$10$abc'),(2,'x','y','z');\nUNLOCK TABLES;",
            ),
        ),
        (
            "V34 MySQL backslash at end both modes",
            MY,
            s("SELECT 'a\\\\', 'b'"),
        ),
        (
            "V35 MySQL json with escaped quotes",
            MY,
            s("INSERT INTO t (j) VALUES ('{\"a\":\"it\\'s \\\\\"q\\\\\"\"}')"),
        ),
        (
            "V36 PG json with doubled quotes",
            PG,
            s("INSERT INTO t (j) VALUES ('{\"a\": \"it''s\"}'::jsonb)"),
        ),
        (
            "V37 PG dollar tag with digits/underscore",
            PG,
            s("SELECT $tag_1$it's$tag_1$, $TAG$x$TAG$"),
        ),
        (
            "V38 PG identifier with $ then literal",
            PG,
            s("SELECT a$b, 'it''s' FROM t$1"),
        ),
        (
            "V39 PG E'' newline escapes in function",
            PG,
            s("CREATE FUNCTION f() RETURNS text AS E'SELECT ''a\\nb''' LANGUAGE sql"),
        ),
        (
            "V40 MySQL view /*!50001",
            MY,
            s(
                "/*!50001 CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select 'it''s' AS `x` */;",
            ),
        ),
    ]
}

#[test]
fn no_secret_of_the_eleventh_audit_reaches_the_log() -> TestResult {
    let cases: Vec<Case4> = eleventh_shell_cases()
        .into_iter()
        .map(|(tag, d, stmt)| {
            let id = tag.split(' ').next().unwrap_or_default();
            let k = if ELEVENTH_AUDIT_ACCEPTED.contains(&id) {
                'v'
            } else {
                'p'
            };
            c4(k, tag, d, &stmt, X10)
        })
        .collect();
    assert_eq!(cases.len(), 99);
    let found = audit4_leaks(&cases)?;
    assert!(
        found.is_empty(),
        "{} leaks:
{}",
        found.len(),
        found.join(
            "
"
        )
    );
    Ok(())
}

#[test]
fn eleventh_audit_prose_and_valid_sql_stay() {
    let mut changed = Vec::new();
    for (tag, dialect, statement) in eleventh_prose() {
        let full = prepare_statement_text(statement, dialect, TextLevel::Full).unwrap_or_default();
        if full != statement {
            changed.push(format!("{tag}: {full}"));
        }
    }
    for (tag, dialect, statement) in eleventh_valid() {
        let full = prepare_statement_text(&statement, dialect, TextLevel::Full).unwrap_or_default();
        if full != statement {
            changed.push(format!("{tag}: {full}"));
        }
    }
    assert!(
        changed.is_empty(),
        "{}",
        changed.join(
            "
"
        )
    );
}

/// Twelfth-audit command lines inside a rule kept at text level `full`:
/// `!` leads a command only on a line that reads as shell (round 13).
const TWELFTH_AUDIT_ACCEPTED: [&str; 1] = ["L-I16"];

/// The twelfth audit's 141 inputs: `I` inside a rule (nothing of the
/// marker at any level), `O` outside one (checked at `none` and
/// `normalized`), `V` valid text or prose stored as written at `full`.
fn twelfth_cases() -> Vec<(&'static str, SqlDialect, String, char)> {
    let s = |x: &str| x.to_string();
    let c = |line: &str| format!("SELECT 1 -- {line}");
    let mut v: Vec<(&'static str, SqlDialect, String, char)> = vec![
        // ---- Medium A: tool basename lower case, quoted client paths.
        (
            "A-I01 C:\\xampp drive letter unflagged",
            MY,
            c("C:\\xampp\\mysql\\bin\\mysql.exe appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-I02 /Applications/MAMP path",
            MY,
            c("/Applications/MAMP/Library/bin/mysql appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-I03 $HOME/bin/mysql",
            MY,
            c("$HOME/bin/mysql appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-I04 PS & \"C:\\Program Files\\..\\mysql.exe\" flagged",
            MY,
            c(
                "& \"C:\\Program Files\\MySQL\\MySQL Server 8.0\\bin\\mysql.exe\" -uroot -pXYZZY_SECRET_1",
            ),
            'I',
        ),
        (
            "A-I05 PS & \"...mysql.exe\" unflagged unshaped",
            MY,
            c(
                "& \"C:\\Program Files\\MySQL\\MySQL Server 8.0\\bin\\mysql.exe\" appdb -pxyzzysecretword",
            ),
            'I',
        ),
        (
            "A-I06 PS & '...MariaDB...mysql.exe'",
            MY,
            c("& 'C:\\Program Files\\MariaDB 11.4\\bin\\mysql.exe' appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-I07 cmd \"...mysqldump.exe\" > x.sql",
            MY,
            c(
                "\"C:\\Program Files\\MySQL\\MySQL Server 8.0\\bin\\mysqldump.exe\" -uroot -pxyzzysecretword appdb > x.sql",
            ),
            'I',
        ),
        (
            "A-I08 \"/opt/my sql/bin/mysql\"",
            MY,
            c("\"/opt/my sql/bin/mysql\" appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-I09 MYSQL.EXE suffix upper",
            MY,
            c("C:\\bin\\mysql.EXE appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-I10 quoted path after sudo",
            MY,
            c("sudo \"/usr/local/mysql/bin/mysql\" appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-I11 PS call op with .\\ quoted",
            MY,
            c("& \".\\mysql.exe\" appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-I12 path in single quotes after ;",
            MY,
            c("cd x; '/usr/bin/mysqldump' appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-I13 PS prompt then & quoted",
            MY,
            c("PS C:\\> & \"C:\\Program Files\\MySQL\\bin\\mysql.exe\" appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "A-O01 Mysql.exe mixed case basename",
            MY,
            c("C:\\bin\\Mysql.exe appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "A-O02 \"C:\\...\\MYSQL.EXE\" upper basename",
            MY,
            c("& \"C:\\Program Files\\MYSQL.EXE\" appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "A-O03 path dir named mysql, tool other",
            MY,
            c("/opt/mysql/bin/backup appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "A-V01 prose: quoted path then -performance",
            MY,
            c("see \"C:\\Program Files\\MySQL\\docs\\mysql.txt\" for -performance tips"),
            'V',
        ),
        (
            "A-V02 prose: MySQL docs -performance",
            MY,
            c("MySQL docs -performance tips"),
            'V',
        ),
        // ---- Medium B: routine and dollar-quoted bodies.
        (
            "B-I01 PROCEDURE $$ flagged",
            PG,
            s(
                "CREATE PROCEDURE p() AS $$ BEGIN\n-- mysql -uroot -pXYZZY_SECRET_1\nCOMMIT; END $$ LANGUAGE plpgsql",
            ),
            'I',
        ),
        (
            "B-I02 FUNCTION $$ unflagged shaped",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int AS $$\n-- mysql appdb -pXYZZY_SECRET_1\nSELECT 1 $$ LANGUAGE sql",
            ),
            'I',
        ),
        (
            "B-I03 DO $$ mysqldump unshaped",
            PG,
            s("DO $$ BEGIN\n-- mysqldump appdb -pxyzzysecretword > x.sql\nPERFORM 1; END $$"),
            'I',
        ),
        (
            "B-I04 $fn$ tag",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $fn$ BEGIN\n-- mysqldump appdb -pxyzzysecretword\nRETURN 1; END $fn$",
            ),
            'I',
        ),
        (
            "B-I05 nested $a$ $b$",
            PG,
            s(
                "DO $a$ BEGIN EXECUTE $b$ SELECT 1 /* mysqldump appdb -pxyzzysecretword */ $b$; END $a$",
            ),
            'I',
        ),
        (
            "B-I06 old-style single-quoted body",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int AS '\n-- mysqldump appdb -pxyzzysecretword\nSELECT 1' LANGUAGE sql",
            ),
            'I',
        ),
        (
            "B-I07 BEGIN ATOMIC body comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC\n-- mysqldump appdb -pxyzzysecretword\nSELECT 1; END",
            ),
            'I',
        ),
        (
            "B-I08 body /* */ comment key=value",
            PG,
            s("DO $$ BEGIN /* token=xyzzysecretword */ PERFORM 1; END $$"),
            'I',
        ),
        (
            "B-I09 body comment redis-cli -a",
            PG,
            s("DO $$ BEGIN\n-- redis-cli -h cache -a xyzzysecretword ping\nPERFORM 1; END $$"),
            'I',
        ),
        (
            "B-I10 body comment password: shaped",
            PG,
            s("DO $$ BEGIN\n-- the app password: XYZZY_Secret_1\nPERFORM 1; END $$"),
            'I',
        ),
        (
            "B-I11 body EXECUTE 'ALTER ROLE ... PASSWORD'",
            PG,
            s("DO $$ BEGIN EXECUTE 'ALTER ROLE app PASSWORD ''xyzzysecretword'''; END $$"),
            'I',
        ),
        (
            "B-I12 MySQL proc comment",
            MY,
            s(
                "CREATE PROCEDURE p() BEGIN\n-- mysqldump appdb -pxyzzysecretword > x.sql\nSELECT 1; END",
            ),
            'I',
        ),
        (
            "B-I13 MySQL trigger body # comment",
            MY,
            s(
                "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN\n# mysqldump appdb -pxyzzysecretword\nSET NEW.a = 1; END",
            ),
            'I',
        ),
        (
            "B-I14 plpython body # mysql flagged",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\n# mysql -uroot -pxyzzysecretword appdb\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I15 plpython psycopg2 connect password=",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\nimport psycopg2\nc = psycopg2.connect(host='db', password='xyzzysecretword')\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I16 plv8 body // comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plv8 AS $$\n// mysql -uroot -pxyzzysecretword appdb\nreturn 1;\n$$",
            ),
            'I',
        ),
        (
            "B-V01 plpython body with it's in # comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS text LANGUAGE plpython3u AS $$\n# it's fine\nreturn 'x'\n$$",
            ),
            'V',
        ),
        (
            "B-V02 plperl body with regex quote",
            PG,
            s(
                "CREATE FUNCTION f(text) RETURNS text LANGUAGE plperl AS $$\nmy ($s) = @_; $s =~ s/'//g; return $s;\n$$",
            ),
            'V',
        ),
        (
            "B-V03 plv8 template literal",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS text LANGUAGE plv8 AS $$\nconst s = `it's ${1}`; return s;\n$$",
            ),
            'V',
        ),
        (
            "B-V04 dollar data literal (not a body)",
            PG,
            s("SELECT $$it's a note -- see docs$$ AS note"),
            'V',
        ),
        (
            "B-V05 body with E'' and '' ",
            PG,
            s("DO $$ BEGIN RAISE NOTICE 'it''s %', E'a\\'b'; END $$"),
            'V',
        ),
        (
            "B-V06 plpgsql body comment prose mysql",
            PG,
            s("DO $$ BEGIN\n-- MySQL docs -performance tips\nPERFORM 1; END $$"),
            'V',
        ),
        (
            "B-O01 dollar data literal with command line",
            PG,
            s("SELECT $$mysql appdb -pxyzzysecretword$$"),
            'O',
        ),
        // ---- Medium C: POSIX quoting.
        (
            "C-I01 sh -c \"..-p\\\"x\\\"..\"",
            MY,
            c("sh -c \"mysql -uroot -p\\\"xyzzysecretword\\\" appdb\""),
            'I',
        ),
        (
            "C-I02 sh -c \"...-p\\\"SHAPED\\\"\"",
            MY,
            c("sh -c \"mysql appdb -p\\\"XYZZY_SECRET_1\\\"\""),
            'I',
        ),
        (
            "C-I03 sh -c '... -p\"x y\"'",
            MY,
            c("sh -c 'mysql appdb -p\"xyzzy secretword\"'"),
            'I',
        ),
        (
            "C-I04 $'...' ANSI-C",
            MY,
            c("bash -c $'mysql appdb -p\\'xyzzy secretword\\''"),
            'I',
        ),
        (
            "C-I05 backslash in '' ends at '",
            MY,
            c("sh -c 'mysql appdb -pxyzzy\\' ; mysql appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "C-I06 \\\\ before closing \"",
            MY,
            c("sh -c \"mysql appdb -pxyzzysecretword\\\\\""),
            'I',
        ),
        (
            "C-I07 \\$ in double quotes",
            MY,
            c("sh -c \"mysql appdb -pxyzzy\\$secretword\""),
            'I',
        ),
        (
            "C-I08 ssh host \"sh -c 'mysql -p..'\"",
            MY,
            c("ssh db \"sh -c 'mysqldump appdb -pxyzzysecretword'\""),
            'I',
        ),
        (
            "C-I09 unquoted \\' inside word",
            MY,
            c("mysql appdb -pxyzzy\\'secretword"),
            'I',
        ),
        (
            "C-I10 unclosed \" to line end",
            MY,
            c("sh -c \"mysql appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "C-I11 -p'..' word containing # ",
            MY,
            c("mysql appdb -p'xyzzy#secretword'"),
            'I',
        ),
        (
            "C-I12 -p value with # unquoted",
            MY,
            c("mysql appdb -pxyzzy#secretword"),
            'I',
        ),
        (
            "C-I13 -p then comment end glued",
            MY,
            s("SELECT 1 /* mysql -uroot -pXYZZY_SECRET_1*/ , 2"),
            'I',
        ),
        (
            "C-O01 depth 4 nested",
            MY,
            c(
                "sh -c \"sh -c 'ssh h \\\"sh -c \\\\\\\"mysql appdb -pxyzzysecretword\\\\\\\"\\\"'\"",
            ),
            'O',
        ),
        // ---- Low A: command positions and command ends.
        (
            "L-I01 $ prompt",
            MY,
            c("$ mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I02 # prompt",
            MY,
            c("# mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I03 % prompt",
            MY,
            c("% mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I04 > prompt",
            MY,
            c("> mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I05 PS C:\\> prompt",
            MY,
            c("PS C:\\> mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I06 C:\\Users\\me> prompt",
            MY,
            c("C:\\Users\\me> mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I07 user@host:~$ prompt",
            MY,
            c("root@db1:~$ mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I08 [user@host ~]$ prompt",
            MY,
            c("[root@db1 ~]# mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I09 cron five fields",
            MY,
            c("0 3 * * * mysqldump appdb -pxyzzysecretword > /b/x.sql"),
            'I',
        ),
        (
            "L-I10 cron */5 and user field",
            MY,
            c("*/5 * * * * root mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I11 cron @daily",
            MY,
            c("@daily mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I12 comment block * line",
            MY,
            s("SELECT 1 /*\n * mysqldump appdb -pxyzzysecretword\n */"),
            'I',
        ),
        (
            "L-I13 for ...; do",
            MY,
            c("for db in a b; do mysqldump $db -pxyzzysecretword; done"),
            'I',
        ),
        (
            "L-I14 if ...; then ... else",
            MY,
            c("if true; then echo; else mysqldump appdb -pxyzzysecretword; fi"),
            'I',
        ),
        (
            "L-I15 { cmd; }",
            MY,
            c("{ mysqldump appdb -pxyzzysecretword; }"),
            'I',
        ),
        (
            "L-I16 ! cmd",
            MY,
            c("! mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I17 stdbuf -oL",
            MY,
            c("stdbuf -oL mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I18 builtin",
            MY,
            c("builtin exec mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I19 exec 2>&1",
            MY,
            c("exec 2>&1 mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I20 >log redirect first",
            MY,
            c(">log.txt mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I21 2>/dev/null first",
            MY,
            c("2>/dev/null mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I22 Redis # line",
            RD,
            s("# mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I23 Redis #mysql no space",
            RD,
            s("#mysql appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I24 Redis ECHO arg command line",
            RD,
            s("ECHO \"mysql -uroot -pxyzzysecretword appdb\""),
            'I',
        ),
        (
            "L-I25 Redis SET value command line",
            RD,
            s("SET note \"mysqldump appdb -pxyzzysecretword\""),
            'I',
        ),
        (
            "L-I26 elif",
            MY,
            c("if a; then x; elif b; then mysqldump appdb -pxyzzysecretword; fi"),
            'I',
        ),
        (
            "L-I27 while ...; do",
            MY,
            c("while true; do mysqldump appdb -pxyzzysecretword; sleep 60; done"),
            'I',
        ),
        (
            "L-I28 doas",
            MY,
            c("doas mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I29 ionice -c3",
            MY,
            c("ionice -c3 mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I30 cron with env in crontab",
            MY,
            c("MAILTO=x 0 3 * * * mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I31 PS> prompt glued",
            MY,
            c("PS> mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I32 mysql> client prompt then shell",
            MY,
            c("mysql> \\! mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I33 - list bullet",
            MY,
            c("- mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "L-I34 markdown ``` then line",
            MY,
            s("SELECT 1 /*\n```\nmysqldump appdb -pxyzzysecretword\n```\n*/"),
            'I',
        ),
        (
            "L-I35 Redis # line flagged shaped",
            RD,
            s("# mysql -uroot -pXYZZY_SECRET_1"),
            'I',
        ),
        (
            "L-I36 # ends command: -p before #",
            MY,
            c("mysqldump appdb -pxyzzysecretword # nightly"),
            'I',
        ),
        (
            "L-I37 /* */ ends command glued",
            MY,
            s("SELECT 1 /* mysqldump appdb -pxyzzysecretword*/"),
            'I',
        ),
        (
            "L-O01 prose: then run mysql -performance",
            MY,
            c("then check the mysql -performance schema"),
            'O',
        ),
        (
            "L-O02 prose: time mysql -p? word",
            MY,
            c("this time mysql -prefers hash joins"),
            'O',
        ),
        (
            "L-O03 # ends command: -p after #",
            MY,
            c("mysql appdb # -pnotapassword"),
            'O',
        ),
        (
            "L-O04 /* ends, -p in next comment",
            MY,
            s("SELECT 1 /* mysql appdb */ /* -pnotapassword */"),
            'O',
        ),
        (
            "L-O05 `1. mysql` numbered (accepted)",
            MY,
            c("1. mysqldump appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "L-O06 `Run: mysql` (accepted)",
            MY,
            c("Run: mysqldump appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "L-O07 uppercase MYSQL (accepted)",
            MY,
            c("MYSQL appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "L-O08 cmd ^ continuation (accepted)",
            MY,
            s("SELECT 1 /* mysql -h db appdb ^\n-pxyzzysecretword */"),
            'O',
        ),
        (
            "L-V01 prose: do not use mysql -performance",
            MY,
            c("do not use mysql -performance_schema here"),
            'V',
        ),
        (
            "L-V02 prose: then mysql -prompt",
            MY,
            c("then mysql -prompt changes"),
            'V',
        ),
        (
            "L-V03 prose: else mysql -p",
            MY,
            c("or else mysql -panics"),
            'V',
        ),
        (
            "L-V04 prose: ! mysql -pre",
            MY,
            c("note ! mysql -preloads"),
            'V',
        ),
        (
            "L-V05 prose: # then words",
            RD,
            s("# MySQL docs -performance tips"),
            'V',
        ),
        // ---- Low C: U&'' UESCAPE.
        (
            "U-I01 UESCAPE '!'",
            PG,
            s("DO $$ BEGIN RAISE EXCEPTION U&'!0058YZZY_SECRET_1' UESCAPE '!'; END $$"),
            'O',
        ),
        (
            "U-V01 U& with + form",
            PG,
            s("SELECT U&'d!0061t!+000061' UESCAPE '!'"),
            'V',
        ),
        (
            "U-V02 uescape lower, comment between",
            PG,
            s("SELECT U&'d#0061ta' /* c */ uescape '#'"),
            'V',
        ),
        // ---- Low E: redis-cli arguments, REDISCLI_AUTH, mysqladmin password.
        (
            "E-I01 redis-cli AUTH user pass",
            MY,
            c("redis-cli -h cache AUTH app XYZZY_SECRET_1"),
            'I',
        ),
        (
            "E-I02 redis-cli AUTH unshaped",
            MY,
            c("redis-cli -h cache AUTH xyzzysecretword"),
            'I',
        ),
        (
            "E-I03 REDISCLI_AUTH=",
            MY,
            c("REDISCLI_AUTH=xyzzysecretword redis-cli -h cache"),
            'I',
        ),
        (
            "E-I04 redis-cli CONFIG SET requirepass",
            MY,
            c("redis-cli -h cache CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "E-I05 redis-cli CONFIG SET masterauth",
            MY,
            c("redis-cli CONFIG SET masterauth xyzzysecretword"),
            'I',
        ),
        (
            "E-I06 redis-cli ACL SETUSER >pw",
            MY,
            c("redis-cli ACL SETUSER app on >xyzzysecretword ~* +@all"),
            'I',
        ),
        (
            "E-I07 redis-cli HELLO 3 AUTH",
            MY,
            c("redis-cli HELLO 3 AUTH app xyzzysecretword"),
            'I',
        ),
        (
            "E-I08 redis-cli MIGRATE AUTH",
            MY,
            c("redis-cli MIGRATE h 6379 k 0 5000 AUTH xyzzysecretword"),
            'I',
        ),
        (
            "E-I09 redis-cli MIGRATE AUTH2",
            MY,
            c("redis-cli MIGRATE h 6379 k 0 5000 AUTH2 app xyzzysecretword"),
            'I',
        ),
        (
            "E-I10 redis-cli --askpass then CONFIG SET",
            MY,
            c("redis-cli --askpass CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "E-I11 redis-cli -t 5 CONFIG SET",
            MY,
            c("redis-cli -t 5 CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "E-I12 redis-cli -D , CONFIG SET",
            MY,
            c("redis-cli -D , CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "E-I13 redis-cli --no-auth-warning -a",
            MY,
            c("redis-cli --no-auth-warning -a xyzzysecretword ping"),
            'I',
        ),
        (
            "E-I14 redis-cli lower auth",
            MY,
            c("redis-cli -n 1 auth xyzzysecretword"),
            'I',
        ),
        (
            "E-I15 redis-cli -c CONFIG SET",
            MY,
            c("redis-cli -c -h cache CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "E-I16 mysqladmin password NEW unshaped",
            MY,
            c("mysqladmin -uroot password xyzzysecretword"),
            'I',
        ),
        (
            "E-I17 mysqladmin password \"NEW\"",
            MY,
            c("mysqladmin -uroot password \"xyzzysecretword\""),
            'I',
        ),
        (
            "E-I18 mysqladmin -pOLD password NEW",
            MY,
            c("mysqladmin -uroot -pOld1 password XYZZY_SECRET_1"),
            'I',
        ),
        (
            "E-I19 mariadb-admin password",
            MY,
            c("mariadb-admin -uroot password xyzzysecretword"),
            'I',
        ),
        (
            "E-I20 export REDISCLI_AUTH=",
            MY,
            c("export REDISCLI_AUTH=xyzzysecretword"),
            'I',
        ),
        (
            "E-I21 Redis # redis-cli AUTH",
            RD,
            s("# redis-cli -h cache AUTH xyzzysecretword"),
            'I',
        ),
        (
            "E-I22 valkey-cli CONFIG SET",
            MY,
            c("valkey-cli CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "E-I23 redis-cli -a then AUTH both",
            MY,
            c("redis-cli -a xyzzysecretword AUTH xyzzysecretword2"),
            'I',
        ),
        (
            "E-I24 redis-cli CONFIG SET two pairs",
            MY,
            c("redis-cli CONFIG SET maxmemory 1gb requirepass xyzzysecretword"),
            'I',
        ),
        (
            "E-V01 redis-cli GET key",
            MY,
            c("redis-cli -h cache GET session:1"),
            'V',
        ),
        (
            "E-V02 mysqladmin status",
            MY,
            c("mysqladmin -uroot status"),
            'V',
        ),
        (
            "E-V03 prose: AUTH after redis-cli",
            MY,
            c("redis-cli supports AUTH since forever"),
            'V',
        ),
        (
            "E-O01 prose: mysqladmin password policy",
            MY,
            c("mysqladmin password reset is documented"),
            'O',
        ),
        // ---- Accepted at Full (ELEVENTH_AUDIT_ACCEPTED), must still be safe at Normalized.
        (
            "ACC unquoted path with spaces",
            MY,
            c("C:\\Program Files\\MySQL\\bin\\mysql.exe appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "ACC PowerShell backtick continuation",
            MY,
            s("SELECT 1 /* mysql appdb `\n-pxyzzysecretword */"),
            'O',
        ),
        (
            "ACC sh -c nested 4 deep",
            MY,
            c("sh -c \"sh -c 'sh -c \\\"sh -c mysql_appdb\\\"'\" -pxyzzysecretword"),
            'O',
        ),
        (
            "ACC literal data command line",
            PG,
            s("SELECT 'ssh db1 mysql appdb -pxyzzysecretword'"),
            'O',
        ),
    ];
    // Probe 11 Medium A/B/C and Low A/E forms, carried over.
    v.push((
        "P11 X06 mysqladmin password quoted",
        MY,
        c("mysqladmin -uroot password \"xyzzysecretword\""),
        'I',
    ));
    v
}

/// A `pg_dump` text with `tables` COPY blocks; the secret column values of
/// table `secret_at` carry the marker.
fn twelfth_dump(tables: usize, secret_at: usize, quotes: bool) -> String {
    let mut s = String::from(
        "--\n-- PostgreSQL database dump\n--\n\n-- Dumped from database version 16.4\n\nSET statement_timeout = 0;\nSET client_encoding = 'UTF8';\nSET standard_conforming_strings = on;\nSELECT pg_catalog.set_config('search_path', '', false);\n\n",
    );
    for t in 0..tables {
        s.push_str(&format!(
            "--\n-- Data for Name: t{t}; Type: TABLE DATA; Schema: public; Owner: app\n--\n\nCOPY public.t{t} (id, \"Email\", password_hash, \"api token\", note) FROM stdin;\n"
        ));
        for r in 0..3 {
            let hash = if t == secret_at {
                format!("XYZZYHASH{t}x{r}")
            } else {
                format!("h{t}x{r}")
            };
            let note = if quotes { "it's a note" } else { "plain note" };
            s.push_str(&format!(
                "{r}\tuser{r}@example.com\t{hash}\tXYZZYTOKEN{t}x{r}\t{note}\n"
            ));
        }
        s.push_str("\\.\n\n\n");
    }
    s.push_str("--\n-- Name: t0 t0_pkey; Type: CONSTRAINT\n--\n\nALTER TABLE ONLY public.t0\n    ADD CONSTRAINT t0_pkey PRIMARY KEY (id);\n\nALTER ROLE app PASSWORD 'XYZZY_SECRET_AFTER';\nCOMMENT ON TABLE public.t0 IS 'it''s the users';\n\n--\n-- PostgreSQL database dump complete\n--\n\n");
    s
}

#[test]
fn no_secret_of_the_twelfth_audit_reaches_the_log() -> TestResult {
    let all = twelfth_cases();
    assert_eq!(all.len(), 141);
    let mut checked = Vec::new();
    let mut changed = Vec::new();
    for (tag, d, stmt, kind) in all {
        let id = tag.split(' ').next().unwrap_or_default();
        match kind {
            'I' if !TWELFTH_AUDIT_ACCEPTED.contains(&id) => {
                checked.push(c4('p', tag, d, &stmt, X10))
            }
            'I' | 'O' => checked.push(c4('v', tag, d, &stmt, X10)),
            _ => {
                let full = prepare_statement_text(&stmt, d, TextLevel::Full).unwrap_or_default();
                if full != stmt {
                    changed.push(format!("{tag}: {full}"));
                }
            }
        }
    }
    let found = audit4_leaks(&checked)?;
    assert!(
        found.is_empty(),
        "{} leaks:\n{}",
        found.len(),
        found.join("\n")
    );
    assert!(changed.is_empty(), "{}", changed.join("\n"));
    Ok(())
}

/// COPY data of every form: no marker survives at `full` or `normalized`.
#[test]
fn copy_data_keeps_no_secret() {
    const PG: SqlDialect = SqlDialect::Postgres;
    let cases: Vec<(&str, String)> = vec![
        ("D06 no \\. terminator then ALTER ROLE", "COPY t (a) FROM stdin;\n1\nALTER ROLE app PASSWORD 'XYZZY_SECRET_1';\n".into()),
        ("D07 no \\. no rows then ALTER ROLE", "COPY t (a) FROM stdin;\n\nALTER ROLE app PASSWORD 'XYZZY_SECRET_1';".into()),
        ("D08 FORMAT csv quoted fields", "COPY t (id, name, password) FROM stdin WITH (FORMAT csv);\n1,\"alice\",\"XYZZY_SECRET_1\"\n\\.\nSELECT 1;\n".into()),
        ("D09 FORMAT csv unquoted fields", "COPY t (id, name, password) FROM stdin WITH (FORMAT csv);\n1,alice,XYZZY_SECRET_1\n\\.\nSELECT 1;\n".into()),
        ("D10 csv HEADER old syntax", "COPY t (id, password) FROM STDIN CSV HEADER;\nid,password\n1,XYZZY_SECRET_1\n\\.\n".into()),
        ("D11 FROM stdin (FORMAT binary) binary-looking", "COPY t (id, secret) FROM stdin (FORMAT binary);\nPGCOPY\n\u{ff}\r\n\u{1}\u{2}XYZZY_SECRET_1\u{7f}\n\\.\n".into()),
        ("D12 text format binary-looking bytes", "COPY t (id, secret) FROM stdin;\n1\t\\\\x50474350XYZZY_SECRET_1\u{1}\u{ff}\n\\.\n".into()),
        ("D13 literal holding stdin; starts nothing", "SELECT 'COPY t (a) FROM stdin;\n1\tit''s\n\\.\n', 2;\nALTER ROLE app PASSWORD 'XYZZY_SECRET_1';\n".into()),
        ("D14 comment holding stdin;", "-- COPY t (a) FROM stdin;\nSELECT 'it''s';\nALTER ROLE app PASSWORD 'XYZZY_SECRET_1';\n".into()),
        ("D15 dollar body holding stdin;", "DO $$ BEGIN PERFORM 1; -- COPY t (a) FROM stdin;\nEND $$;\nALTER ROLE app PASSWORD 'XYZZY_SECRET_1';\n".into()),
        ("D16 table named stdin", "SELECT * FROM stdin;\nALTER ROLE app PASSWORD 'XYZZY_SECRET_1';\n".into()),
        ("D17 data row: key=value + URL", "COPY t (id, note) FROM stdin;\n1\tpassword=XYZZY_SECRET_1\n2\tpostgres://app:XYZZY_URLPW@db/x\n\\.\n".into()),
        ("D18 secret column names", "COPY t (id, encrypted_password, passwd, pwd, api_key, client_secret, access_token, otp_secret, private_key, salt) FROM stdin;\n1\tXYZZY_A\tXYZZY_B\tXYZZY_C\tXYZZY_D\tXYZZY_E\tXYZZY_F\tXYZZY_G\tXYZZY_H\tXYZZY_I\n\\.\n".into()),
        ("D19 secret column quoted with comma", "COPY t (id, \"pass,word\", note) FROM stdin;\n1\tXYZZY_SECRET_1\tok\n\\.\n".into()),
        ("D20 \\N in secret column + CRLF", "COPY t (id, password) FROM stdin;\r\n1\t\\N\r\n2\tXYZZY_SECRET_1\r\n\\.\r\nSELECT 'it''s';\r\n".into()),
        ("D21 \\. not alone on line", "COPY t (id, password) FROM stdin;\n1\tab\\.\n2\tXYZZY_SECRET_1\n\\.\nSELECT 1;\n".into()),
        ("D22 COPY after comment on same line", "COPY t (id, password) FROM stdin; -- data\n1\tXYZZY_SECRET_1\n\\.\n".into()),
        ("D23 lower-case copy", "copy t (id, password) from stdin;\n1\tXYZZY_SECRET_1\n\\.\n".into()),
        ("D24 COPY with schema-qualified quoted table", "COPY \"My Schema\".\"Users\" (id, \"Password\") FROM stdin;\n1\tXYZZY_SECRET_1\n\\.\n".into()),
        ("D25 escaped tab in secret field", "COPY t (id, password, note) FROM stdin;\n1\tXYZZY\\tSECRET_1\tok\n\\.\n".into()),
        ("D26 MySQL dialect same text", "COPY t (id, password) FROM stdin;\n1\tXYZZY_SECRET_1\n\\.\n".into()),
        ("D27 COPY FROM stdin; then ALTER ROLE inside data", "COPY t (a) FROM stdin;\nALTER ROLE app PASSWORD 'XYZZY_SECRET_1';\n\\.\n".into()),
        ("D28 COPY column list missing, data has password=", "COPY t FROM stdin;\n1\tXYZZY_PLAIN\tpassword=XYZZY_SECRET_1\n\\.\n".into()),
    ];
    let mut kept = Vec::new();
    for (tag, text) in &cases {
        for level in [TextLevel::Normalized, TextLevel::Full] {
            let out = prepare_statement_text(text, PG, level).unwrap_or_default();
            // D28 keeps a plain data field of a table without a column list,
            // and D18 its `salt` column, which is no credential.
            let out = out.replace("XYZZY_PLAIN", "").replace("XYZZY_I", "");
            if out.contains("XYZZY") {
                kept.push(format!("{tag} at {level}: {out}"));
            }
        }
    }
    assert!(kept.is_empty(), "{}", kept.join("\n"));
}

/// Every COPY block of a long dump is data, however many there are (a
/// 200-table dump), and its secret columns go at `full`.
#[test]
fn every_copy_block_of_a_long_dump_is_data() {
    for (tables, secret_at) in [(20, 18), (200, 199), (200, 5)] {
        for quotes in [false, true] {
            let text = twelfth_dump(tables, secret_at, quotes);
            let full = prepare_statement_text(&text, SqlDialect::Postgres, TextLevel::Full)
                .unwrap_or_default();
            assert!(!full.contains("XYZZY"), "{tables} tables, quotes {quotes}");
            assert!(full.contains("ALTER ROLE app PASSWORD '[redacted]';"));
            assert!(full.contains("it''s the users"));
        }
    }
}

/// The thirteenth audit's 125 inputs: `I` inside a rule (nothing of the
/// marker at any level), `O` outside one (checked at `none` and
/// `normalized`), `V` valid text stored as written at `full`.
fn thirteenth_cases() -> Vec<(&'static str, SqlDialect, String, char)> {
    let s = |x: &str| x.to_string();
    let c = |line: &str| format!("SELECT 1 -- {line}");
    let v: Vec<(&'static str, SqlDialect, String, char)> = vec![
        // ---- COPY data sections.
        (
            "C-I01 CSV embedded quotes before secret",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT csv);\n1,\"a \"\"quoted\"\" note\",XYZZY_SECRET_1\n\\.\nSELECT 1;\n",
            ),
            'I',
        ),
        (
            "C-I02 CSV embedded newline before secret",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT csv);\n1,\"line one\nline two\",XYZZY_SECRET_2\n2,x,XYZZY_SECRET_3\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I03 CSV embedded delimiter before secret",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT csv);\n1,\"a,b,c\",XYZZY_SECRET_4\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I04 CSV secret field quoted with comma+newline",
            PG,
            s(
                "COPY t (id, password, note) FROM stdin WITH (FORMAT csv);\n1,\"XYZZY,PART\nXYZZY_PART2\",ok\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I05 CSV DELIMITER ';'",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, DELIMITER ';');\n1;XYZZY_SECRET_5\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I06 CSV HEADER false",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, HEADER false);\n1,XYZZY_SECRET_6\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I07 old DELIMITER AS ','",
            PG,
            s("COPY t (id, password) FROM stdin WITH DELIMITER AS ',';\n1,XYZZY_SECRET_7\n\\.\n"),
            'I',
        ),
        (
            "C-I08 FORMAT 'csv' quoted option value",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT 'csv');\n1,\"a\tb\",XYZZY_SECRET_8\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I09 CSV ESCAPE backslash",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT csv, ESCAPE '\\');\n1,\"x\\\"\",XYZZY_SECRET_9\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I10 CSV DELIMITER E'\\t'",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, DELIMITER E'\\t');\n1\tXYZZY_SECRET_10\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I11 text DELIMITER '|'",
            PG,
            s("COPY t (id, password) FROM stdin WITH (DELIMITER '|');\n1|XYZZY_SECRET_11\n\\.\n"),
            'I',
        ),
        (
            "C-I12 binary",
            PG,
            s(
                "COPY t (id, secret) FROM stdin WITH (FORMAT binary);\nPGCOPY\n\u{ff}\r\n\u{0}XYZZY_SECRET_12\u{1}\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I13 no terminator then INSERT secret column",
            PG,
            s(
                "COPY a (x) FROM stdin;\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_SECRET_13');\n",
            ),
            'I',
        ),
        (
            "C-I14 no terminator, INSERT, later COPY with \\.",
            PG,
            s(
                "COPY a (x) FROM stdin;\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_SECRET_14');\nCOPY b (y) FROM stdin;\n1\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I15 COPY TO STDOUT then INSERT and a \\. line",
            PG,
            s(
                "COPY t (password) TO STDOUT;\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_SECRET_15');\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I16 'stdin;' in literal then INSERT and \\.",
            PG,
            s(
                "SELECT 'COPY x FROM stdin;';\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_SECRET_16');\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I17 COPY (SELECT .. FROM stdin) TO STDOUT;",
            PG,
            s(
                "COPY (SELECT * FROM stdin) TO STDOUT;\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_SECRET_17');\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I18 COPY FROM stdin; SELECT on same line",
            PG,
            s(
                "COPY t (a) FROM stdin; SELECT 1;\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_SECRET_18');\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I19 data row with CREATE USER",
            PG,
            s(
                "COPY t (id, note) FROM stdin;\n1\tCREATE USER bob WITH PASSWORD 'XYZZY_SECRET_19'\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I20 data row PASSWORD 'x'",
            PG,
            s("COPY t (id, note) FROM stdin;\n1\tset PASSWORD 'XYZZY_SECRET_20' now\n\\.\n"),
            'I',
        ),
        (
            "C-I21 data row URL creds, CSV",
            PG,
            s(
                "COPY t (id, note) FROM stdin CSV;\n1,\"see mysql://app:XYZZY_SECRET_21@db/x\"\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I22 data row comment rule mysql -p",
            PG,
            s("COPY t (id, note) FROM stdin;\n1\tmysql -uroot -pxyzzysecretword appdb\n\\.\n"),
            'I',
        ),
        (
            "C-I23 psql \\copy from stdin",
            PG,
            s("\\copy t (id, password) from stdin\n1\tXYZZY_SECRET_23\n\\.\n"),
            'I',
        ),
        (
            "C-I24 CRLF CSV HEADER",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, HEADER);\r\nid,password\r\n1,XYZZY_SECRET_24\r\n\\.\r\n",
            ),
            'I',
        ),
        (
            "C-I25 HEADER true text",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (HEADER true);\nid\tpassword\n1\tXYZZY_SECRET_25\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I26 WHERE clause after stdin",
            PG,
            s("COPY t (id, password) FROM stdin WHERE id > 0;\n1\tXYZZY_SECRET_26\n\\.\n"),
            'I',
        ),
        (
            "C-I27 COPY ... FROM STDIN; /* c */",
            PG,
            s("COPY t (id, password) FROM STDIN; /* c */\n1\tXYZZY_SECRET_27\n\\.\n"),
            'I',
        ),
        (
            "C-I28 CSV FORCE_NOT_NULL list",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, FORCE_NOT_NULL (password));\n1,XYZZY_SECRET_28\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I29 CSV QUOTE ''''",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT csv, QUOTE '''');\n1,'a,b',XYZZY_SECRET_29\n\\.\n",
            ),
            'I',
        ),
        // Column names.
        (
            "C-I30 otp_secret",
            PG,
            s("COPY t (id, otp_secret) FROM stdin;\n1\tXYZZY_C30\n\\.\n"),
            'I',
        ),
        (
            "C-I31 encrypted_otp_secret (devise)",
            PG,
            s("COPY t (id, encrypted_otp_secret) FROM stdin;\n1\tXYZZY_C31\n\\.\n"),
            'I',
        ),
        (
            "C-I32 two_factor_secret (Fortify)",
            PG,
            s("COPY t (id, two_factor_secret) FROM stdin;\n1\tXYZZY_C32\n\\.\n"),
            'I',
        ),
        (
            "C-I33 remember_token (Laravel)",
            PG,
            s("COPY t (id, remember_token) FROM stdin;\n1\tXYZZY_C33\n\\.\n"),
            'I',
        ),
        (
            "C-I34 password_digest (Rails)",
            PG,
            s("COPY t (id, password_digest) FROM stdin;\n1\tXYZZY_C34\n\\.\n"),
            'I',
        ),
        (
            "C-I35 reset_password_token",
            PG,
            s("COPY t (id, reset_password_token) FROM stdin;\n1\tXYZZY_C35\n\\.\n"),
            'I',
        ),
        (
            "C-I36 secret (bare)",
            PG,
            s("COPY t (id, secret) FROM stdin;\n1\tXYZZY_C36\n\\.\n"),
            'I',
        ),
        (
            "C-I37 secret_key",
            PG,
            s("COPY t (id, secret_key) FROM stdin;\n1\tXYZZY_C37\n\\.\n"),
            'I',
        ),
        (
            "C-I38 session_token / auth_token",
            PG,
            s(
                "COPY t (id, session_token, auth_token) FROM stdin;\n1\tXYZZY_C38a\tXYZZY_C38b\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I39 two_factor_recovery_codes",
            PG,
            s("COPY t (id, two_factor_recovery_codes) FROM stdin;\n1\tXYZZY_C39\n\\.\n"),
            'I',
        ),
        (
            "C-I40 webhook_secret / signing_secret",
            PG,
            s(
                "COPY t (id, webhook_secret, signing_secret) FROM stdin;\n1\tXYZZY_C40a\tXYZZY_C40b\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I41 confirmation_token / unlock_token (devise)",
            PG,
            s(
                "COPY t (id, confirmation_token, unlock_token) FROM stdin;\n1\tXYZZY_C41a\tXYZZY_C41b\n\\.\n",
            ),
            'I',
        ),
        (
            "C-I42 salt / password_salt",
            PG,
            s("COPY t (id, password_salt) FROM stdin;\n1\tXYZZY_C42\n\\.\n"),
            'I',
        ),
        (
            "C-V01 is_secret / has_secret flags",
            PG,
            s("COPY t (id, is_secret, has_secret, no_secret) FROM stdin;\n1\tt\tf\tt\n\\.\n"),
            'V',
        ),
        (
            "C-V02 password_changed_at / token_count",
            PG,
            s("COPY t (id, password_changed_at, token_count) FROM stdin;\n1\t2024-01-01\t3\n\\.\n"),
            'V',
        ),
        (
            "C-V03 COPY TO STDOUT kept, SQL after kept",
            PG,
            s("COPY t (id, note) TO STDOUT;\nSELECT 'it''s';\n"),
            'V',
        ),
        (
            "C-V04 no column list fields kept",
            PG,
            s("COPY t FROM stdin;\n1\tplain\tit's\n\\.\nSELECT 1;\n"),
            'V',
        ),
        (
            "C-V05 CSV with quotes kept",
            PG,
            s("COPY t (id, note) FROM stdin CSV;\n1,\"a \"\"b\"\", c\"\n\\.\n"),
            'V',
        ),
        // ---- Script bodies.
        (
            "B-I01 plpython # comment mysql",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\n# mysql -uroot -pxyzzysecretword appdb\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I02 plpython code connect password=",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\nimport psycopg2\nc = psycopg2.connect(\"host=db password=XYZZY_SECRET_B2\")\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I03 plpython code kwarg password='x'",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\nc = psycopg2.connect(host='db', password='XYZZY_SECRET_B3')\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I04 plpython plpy.execute ALTER ROLE",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nplpy.execute(\"ALTER ROLE app PASSWORD 'XYZZY_SECRET_B4'\")\n$$",
            ),
            'I',
        ),
        (
            "B-I05 plpython URL creds in code",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nu = 'postgres://app:XYZZY_SECRET_B5@db/x'\n$$",
            ),
            'I',
        ),
        (
            "B-I06 plv8 // comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plv8 AS $$\n// mysql -uroot -pxyzzysecretword appdb\nreturn 1;\n$$",
            ),
            'I',
        ),
        (
            "B-I07 plv8 /* */ comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plv8 AS $$\n/* redis-cli -a xyzzysecretword ping */\nreturn 1;\n$$",
            ),
            'I',
        ),
        (
            "B-I08 plv8 execute ALTER ROLE",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plv8 AS $$\nplv8.execute(\"ALTER ROLE app PASSWORD 'XYZZY_SECRET_B8'\");\n$$",
            ),
            'I',
        ),
        (
            "B-I09 plv8 regex with ' then // comment",
            PG,
            s(
                "CREATE FUNCTION f(s text) RETURNS text LANGUAGE plv8 AS $$\nvar r = s.replace(/'/g, ''); // mysql -uroot -pxyzzysecretword appdb\nreturn r;\n$$",
            ),
            'I',
        ),
        (
            "B-I10 plcoffee # comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plcoffee AS $$\n# mysql -uroot -pxyzzysecretword appdb\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I11 plperl # comment + dsn password",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plperl AS $$\n# mysql -uroot -pxyzzysecretword appdb\nmy $d = 'dbi:Pg:dbname=x;password=XYZZY_SECRET_B11';\nreturn 1;\n$$",
            ),
            'I',
        ),
        (
            "B-I12 LANGUAGE 'plpython3u' quoted",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int AS $$\n# mysql -uroot -pxyzzysecretword appdb\nreturn 1\n$$ LANGUAGE 'plpython3u'",
            ),
            'I',
        ),
        (
            "B-I13 DO LANGUAGE plpython3u",
            PG,
            s("DO LANGUAGE plpython3u $$\n# mysql -uroot -pxyzzysecretword appdb\n$$"),
            'I',
        ),
        (
            "B-I14 DO $$ $$ LANGUAGE plv8",
            PG,
            s("DO $$\n// mysql -uroot -pxyzzysecretword appdb\n$$ LANGUAGE plv8"),
            'I',
        ),
        (
            "B-I15 plpython single-quoted body",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int AS '\n# mysql -uroot -pxyzzysecretword appdb\nreturn 1\n' LANGUAGE plpython3u",
            ),
            'I',
        ),
        (
            "B-I16 plpython triple-quoted with ' then #",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS text LANGUAGE plpython3u AS $$\nx = \"\"\"it's\nmulti\"\"\"\n# mysql -uroot -pxyzzysecretword appdb\nreturn x\n$$",
            ),
            'I',
        ),
        (
            "B-I17 pltcl # comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE pltcl AS $$\n# mysql -uroot -pxyzzysecretword appdb\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I18 plsh body is shell itself",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS text LANGUAGE plsh AS $$\n#!/bin/sh\nmysqldump appdb -pxyzzysecretword > /tmp/x.sql\n$$",
            ),
            'I',
        ),
        (
            "B-I19 plpython secret key=value in # comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\n# token=XYZZY_SECRET_B19\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I20 plpgsql body keeps SQL rules",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpgsql AS $$ BEGIN EXECUTE 'ALTER ROLE app PASSWORD ''XYZZY_SECRET_B20'''; END $$",
            ),
            'I',
        ),
        (
            "B-V01 plpython code kept",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS text LANGUAGE plpython3u AS $$\n# it's fine\nreturn 'x' if 1 else \"it's\"\n$$",
            ),
            'V',
        ),
        (
            "B-V02 plperl $#array",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plperl AS $$\nmy @a = (1,2); return $#a;\n$$",
            ),
            'V',
        ),
        (
            "B-V03 plv8 URL in comment no creds",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plv8 AS $$\n// see https://example.com/docs\nreturn 1;\n$$",
            ),
            'V',
        ),
        // ---- Prompts, cron, bullets.
        (
            "P-I01 user@host:~$",
            MY,
            c("deploy@db1:~$ mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I02 [root@db1 ~]#",
            MY,
            c("[root@db1 ~]# mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I03 PS>",
            MY,
            c("PS> mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I04 mysql> \\!",
            MY,
            c("mysql> \\! mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I05 /etc/crontab user field",
            MY,
            c("*/5 * * * * root mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I06 bullet -",
            MY,
            c("- mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I07 root@db1:/var/backups#",
            MY,
            c("root@db1:/var/backups# mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I08 zsh default prompt user@host ~ %",
            MY,
            c("kim@macbook ~ % mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I09 fish prompt user@host ~>",
            MY,
            c("kim@macbook ~> mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I10 @daily root (crontab with user)",
            MY,
            c("@daily root mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I11 cmd glued C:\\>mysqldump",
            MY,
            c("C:\\>mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I12 cmd glued ...\\bin>mysql -uroot",
            MY,
            c("C:\\Program Files\\MySQL\\MySQL Server 8.0\\bin>mysql -uroot -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I13 (venv) user@host:~$",
            MY,
            c("(venv) kim@db1:~$ mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I14 bash-5.2$",
            MY,
            c("bash-5.2$ mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I15 markdown quote >",
            MY,
            c("> mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I16 * bullet line in -- comment",
            MY,
            c("* mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I17 cron user then path",
            MY,
            c("0 3 * * * backup /usr/bin/mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I18 shell then on shell-looking line",
            MY,
            c("if true; then mysqldump appdb -pxyzzysecretword; fi"),
            'I',
        ),
        (
            "P-I19 sudo -u mysql mysqldump",
            MY,
            c("sudo -u mysql mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I20 kubectl exec -- mysqldump",
            MY,
            c("kubectl exec -it db-0 -- mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-O01 then on non-shell line",
            MY,
            c("then mysqldump appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "P-O02 ! on non-shell line (accepted)",
            MY,
            c("! mysqldump appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "P-O03 time on non-shell line (accepted)",
            MY,
            c("time mysql appdb -pxyzzysecretword"),
            'O',
        ),
        (
            "P-V01 prose do not use -performance",
            MY,
            c("do not use mysql -performance_schema here"),
            'V',
        ),
        (
            "P-V02 prose this time mysql -prefers",
            MY,
            c("this time mysql -prefers hash joins"),
            'V',
        ),
        (
            "P-V03 prose arrow -> mysql -performance",
            MY,
            c("result -> mysql -performance tips"),
            'V',
        ),
        (
            "P-V04 prose MySQL docs",
            MY,
            c("MySQL docs -performance tips"),
            'V',
        ),
        (
            "P-V05 prose list - mysql -performance",
            MY,
            c("- mysql -performance_schema is on"),
            'V',
        ),
        // ---- The dropped rule: -p right after the client.
        (
            "R-I01 own line mysql -pX",
            MY,
            c("mysql -pxyzzysecretword"),
            'I',
        ),
        (
            "R-I02 own line mysql -pX appdb",
            MY,
            c("mysql -pxyzzysecretword appdb"),
            'I',
        ),
        (
            "R-I03 backticks in prose",
            MY,
            c("use `mysql -pxyzzysecretword appdb` to connect"),
            'I',
        ),
        ("R-I04 $ prompt", MY, c("$ mysql -pxyzzysecretword"), 'I'),
        (
            "R-I05 prose with another option",
            MY,
            c("connect with mysql -h db -pxyzzysecretword"),
            'I',
        ),
        (
            "R-I06 prose shaped value",
            MY,
            c("run mysql -pXyzzy_Secret1"),
            'I',
        ),
        (
            "R-I07 MySQL # comment own line",
            MY,
            s("SELECT 1 # mysql -pxyzzysecretword"),
            'I',
        ),
        (
            "R-I08 block comment line",
            MY,
            s("SELECT 1 /* mysql -pxyzzysecretword */"),
            'I',
        ),
        (
            "R-I09 after ; in comment",
            MY,
            c("cd /tmp; mysql -pxyzzysecretword appdb"),
            'I',
        ),
        (
            "R-I10 && in comment",
            MY,
            c("cd /tmp && mysql -pxyzzysecretword appdb"),
            'I',
        ),
        (
            "R-O01 prose run mysql -pX (accepted)",
            MY,
            c("run mysql -pxyzzysecretword"),
            'O',
        ),
        (
            "R-O02 prose connect with mysql -pX appdb",
            MY,
            c("connect with mysql -pxyzzysecretword appdb"),
            'O',
        ),
        (
            "R-O03 prose e.g. mysql -pX",
            MY,
            c("e.g. mysql -pxyzzysecretword appdb"),
            'O',
        ),
        (
            "R-O04 prose try: mysql -pX",
            MY,
            c("try: mysql -pxyzzysecretword"),
            'O',
        ),
        (
            "R-O05 prose login via mysql -pX",
            MY,
            c("login via mysql -pxyzzysecretword"),
            'O',
        ),
        (
            "R-O06 prose Mysql capital at line start",
            MY,
            c("Mysql -pxyzzysecretword"),
            'O',
        ),
        // ---- Redis option table.
        (
            "X-I01 redis-cli -t 5 CONFIG SET",
            MY,
            c("redis-cli -t 5 CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "X-I02 redis-cli --askpass CONFIG SET",
            MY,
            c("redis-cli --askpass CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "X-I03 redis-cli -3 (flag) CONFIG SET",
            MY,
            c("redis-cli -3 CONFIG SET masterauth xyzzysecretword"),
            'I',
        ),
        (
            "X-I04 redis-cli --tls --cacert f ACL SETUSER",
            MY,
            c("redis-cli --tls --cacert ca.pem ACL SETUSER app on >xyzzysecretword"),
            'I',
        ),
        (
            "X-I05 redis-cli -e --no-raw CONFIG SET",
            MY,
            c("redis-cli -e --no-raw CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "X-I06 redis-cli --resp3 -c CONFIG SET",
            MY,
            c("redis-cli -c --resp3 CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "X-I07 redis-cli -u URL",
            MY,
            c("redis-cli -u redis://default:xyzzysecretword@cache:6379"),
            'I',
        ),
        (
            "X-I08 redis-cli --user u --pass p",
            MY,
            c("redis-cli --user app --pass xyzzysecretword ping"),
            'I',
        ),
        (
            "X-I09 Redis dialect: CONFIG SET via redis-cli in #",
            RD,
            s("# redis-cli -t 5 CONFIG SET requirepass xyzzysecretword"),
            'I',
        ),
        (
            "X-V01 prose redis-cli supports AUTH",
            MY,
            c("redis-cli supports AUTH since forever"),
            'V',
        ),
        (
            "X-V02 prose mysqladmin password reset",
            MY,
            c("mysqladmin password reset is documented"),
            'V',
        ),
    ];
    v
}

#[test]
fn no_secret_of_the_thirteenth_audit_reaches_the_log() -> TestResult {
    let all = thirteenth_cases();
    assert_eq!(all.len(), 125);
    let mut checked = Vec::new();
    let mut changed = Vec::new();
    for (tag, d, stmt, kind) in all {
        match kind {
            'I' => checked.push(c4('p', tag, d, &stmt, X10)),
            'O' => checked.push(c4('v', tag, d, &stmt, X10)),
            _ => {
                let full = prepare_statement_text(&stmt, d, TextLevel::Full).unwrap_or_default();
                if full != stmt {
                    changed.push(format!("{tag}: {full}"));
                }
            }
        }
    }
    let found = audit4_leaks(&checked)?;
    assert!(
        found.is_empty(),
        "{} leaks:\n{}",
        found.len(),
        found.join("\n")
    );
    assert!(changed.is_empty(), "{}", changed.join("\n"));
    Ok(())
}

/// Error messages at `normalized`: a statement that raises its own error
/// (by statement position, also in a body or through `plpy.error`) loses
/// every literal; a template word stays otherwise, also next to a column
/// named `signal` or a `raise` in a comment.
#[test]
fn thirteenth_audit_message_words() -> TestResult {
    const PG: SqlDialect = SqlDialect::Postgres;
    const MY: SqlDialect = SqlDialect::Mysql;
    let cases: Vec<(&str, SqlDialect, &str, &str, &str)> = vec![
        (
            "W01 RAISE custom ERRCODE 'Grant'",
            PG,
            "DO $$ BEGIN RAISE EXCEPTION 'customer % blocked', 'Grant' USING ERRCODE = '42501'; END $$",
            "42501",
            "customer Grant blocked",
        ),
        (
            "W02 SIGNAL MYSQL_ERRNO 1062 'admin'",
            MY,
            "SIGNAL SQLSTATE '23000' SET MYSQL_ERRNO = 1062, MESSAGE_TEXT = CONCAT('user ', 'admin', ' exists')",
            "1062",
            "user admin exists",
        ),
        (
            "W03 lower-case raise in function body",
            PG,
            "CREATE FUNCTION f() RETURNS void LANGUAGE plpgsql AS $$ begin raise exception using message = 'admin'; end $$",
            "P0001",
            "admin",
        ),
        (
            "W04 RESIGNAL in handler",
            MY,
            "CREATE PROCEDURE p() BEGIN DECLARE EXIT HANDLER FOR SQLEXCEPTION RESIGNAL SET MESSAGE_TEXT = 'secret'; SELECT 1; END",
            "1644",
            "secret",
        ),
        (
            "W05 plpython plpy.error",
            PG,
            "DO LANGUAGE plpython3u $$ plpy.error('admin') $$",
            "XX000",
            "admin",
        ),
        (
            "W06 column named signal (over-hide check)",
            PG,
            "UPDATE alarms SET signal = 'division' WHERE id = 1",
            "22012",
            "division by zero",
        ),
        (
            "W07 RAISE in a comment (over-hide check)",
            PG,
            "SELECT 'deadlock' -- raise later\n",
            "40P01",
            "deadlock detected",
        ),
        (
            "W08 plain 'division' keeps template word",
            PG,
            "SELECT 'division', 1/0",
            "22012",
            "division by zero",
        ),
        (
            "W09 raised_at column (no raise)",
            PG,
            "UPDATE t SET raised_at = now() WHERE note = 'deadlock'",
            "40P01",
            "deadlock detected",
        ),
        (
            "W10 RAISE NOTICE with secret-shaped",
            PG,
            "DO $$ BEGIN RAISE EXCEPTION '%', 'XYZZY_SECRET_W10'; END $$",
            "P0001",
            "XYZZY_SECRET_W10",
        ),
    ];
    let expected = [
        "customer ? blocked",
        "? ? ?",
        "?",
        "?",
        "?",
        "division by zero",
        "deadlock detected",
        "division by zero",
        "deadlock detected",
        "?",
    ];
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("audit"));
    recorder.set_text_level(TextLevel::Normalized);
    for (i, ((tag, d, stmt, code, msg), want)) in cases.into_iter().zip(expected).enumerate() {
        let connection = format!("w13-{i}");
        recorder.record(
            ActivityEvent::query_failed(
                stmt,
                d,
                "x",
                &ErrorDetails {
                    code: Some(code.into()),
                    message: msg.into(),
                },
                1,
            )
            .connection(connection.as_str())
            .build()?,
        )?;
        let event = recorder.query_history(&connection, 1)?.remove(0);
        assert_eq!(event.details["error"]["message"], want, "{tag}");
    }
    Ok(())
}

/// A `pg_dump` excerpt cut off before its `\.` line is still data: its
/// secret columns go at `full`, and so does SQL that sits in it.
#[test]
fn an_unterminated_copy_excerpt_keeps_no_secret() {
    for text in [
        "COPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tbob@example.com\t$2a$10$XYZZYabcdefghijklmnopqrstuv\txyzzytokenvalue\n2\tann@example.com\t$2a$10$XYZZYzzzzzz\tsecondxyzzytoken\n",
        "COPY public.users (id, password_hash) FROM stdin;\n1\t$2a$10$XYZZYabc\n\\. \n",
        "COPY t (a) FROM stdin;\nALTER ROLE app PASSWORD 'XYZZY_SECRET_1';\n",
    ] {
        let full =
            prepare_statement_text(text, SqlDialect::Postgres, TextLevel::Full).unwrap_or_default();
        assert!(!full.to_lowercase().contains("xyzzy"), "{full}");
        assert!(full.starts_with("COPY "), "{full}");
    }
}

/// Fourteenth-audit inputs inside a rule kept at text level `full` (checked
/// at `none` and `normalized`): SQL built at run time or held in code
/// structures of non-SQL bodies (B-I02, B-I06, B-I07, B-I10, B-I17, B-I20,
/// B-I22, B-I28, B-I30), unusual prompts and numbered lists (P-I08, P-I09,
/// P-I10, P-I17), and a `*` bullet with an unshaped `-p` value right after
/// the client, like the accepted `-` bullet (P-I16). See the redactor's
/// module docs, "Limits".
const FOURTEENTH_AUDIT_ACCEPTED: [&str; 14] = [
    "B-I02", "B-I06", "B-I07", "B-I10", "B-I17", "B-I20", "B-I22", "B-I28", "B-I30", "P-I08",
    "P-I09", "P-I10", "P-I16", "P-I17",
];

/// The fourteenth audit's 162 inputs: `I` inside a rule, `O` outside one,
/// `V` valid text stored as written at `full`.
fn fourteenth_cases() -> Vec<(&'static str, SqlDialect, String, char)> {
    let s = |x: &str| x.to_string();
    let c = |line: &str| format!("SELECT 1 -- {line}");
    let users = "COPY public.users (id, email, password_hash, api_token) FROM stdin;\n";
    vec![
        // ---- Unterminated COPY: the rest is data with all data-row rules.
        (
            "U-I01 r13 Medium A excerpt without \\.",
            PG,
            format!(
                "{users}1\tbob@example.com\t$2a$10$XYZZYabcdefghijklmnopqrstuv\txyzzytokenvalue\n2\tann@example.com\t$2a$10$XYZZYzzzzzz\tsecondxyzzytoken\n"
            ),
            'I',
        ),
        (
            "U-I02 excerpt, last row cut mid-field, no newline",
            PG,
            format!(
                "{users}1\tbob@example.com\tXYZZYHASH1\tXYZZYTOK1\n2\tann@example.com\tXYZZYHA"
            ),
            'I',
        ),
        (
            "U-I03 excerpt with '\\. ' (trailing blank)",
            PG,
            format!("{users}1\tb@x\tXYZZYHASH3\tXYZZYTOK3\n\\. \nSELECT 1;\n"),
            'I',
        ),
        (
            "U-I04 excerpt then INSERT secret col (data, SQL pass)",
            PG,
            format!(
                "{users}1\tb@x\tXYZZYHASH4\tXYZZYTOK4\nINSERT INTO accounts (name, password) VALUES ('bob', 'XYZZY_SECRET_U4');\n"
            ),
            'I',
        ),
        (
            "U-I05 excerpt then UPDATE SET password",
            PG,
            format!(
                "{users}1\tb@x\tXYZZYHASH5\tt\nUPDATE users SET password = 'XYZZY_SECRET_U5' WHERE id = 1;\n"
            ),
            'I',
        ),
        (
            "U-I06 excerpt CSV no \\.",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv);\n1,XYZZY_SECRET_U6\n2,\"XYZZY,U6b\"\n",
            ),
            'I',
        ),
        (
            "U-I07 excerpt row with ' then secret in later row",
            PG,
            format!("{users}1\tit's\tXYZZYHASH7\tXYZZYTOK7\n2\tok\tXYZZYHASH7b\tXYZZYTOK7b\n"),
            'I',
        ),
        (
            "U-I08 excerpt then SET PASSWORD (MySQL-like in data)",
            PG,
            format!("{users}1\tb\tXYZZYHASH8\tx\nALTER USER app PASSWORD 'XYZZY_SECRET_U8';\n"),
            'I',
        ),
        (
            "U-I09 unterminated binary",
            PG,
            s(
                "COPY t (id, secret) FROM stdin WITH (FORMAT binary);\nPGCOPY\n\u{ff}\r\n\u{0}XYZZY_SECRET_U9\u{1}\n",
            ),
            'I',
        ),
        (
            "U-I10 \\connect line right before COPY",
            PG,
            format!("\\connect appdb\n{users}1\tb@x\tXYZZYHASH10\tXYZZYTOK10\n\\.\n"),
            'I',
        ),
        (
            "U-I11 \\set line right before COPY",
            PG,
            format!("\\set ON_ERROR_STOP on\n{users}1\tb@x\tXYZZYHASH11\tXYZZYTOK11\n\\.\n"),
            'I',
        ),
        (
            "U-I12 \\restrict line (pg_dump 17.6+) before COPY",
            PG,
            format!(
                "\\restrict aBcDeF123\n{users}1\tb@x\tXYZZYHASH12\tXYZZYTOK12\n\\.\n\\unrestrict aBcDeF123\n"
            ),
            'I',
        ),
        (
            "U-I13 statement without ; before COPY",
            PG,
            format!("SELECT 1\n{users}1\tb@x\tXYZZYHASH13\tXYZZYTOK13\n\\.\n"),
            'I',
        ),
        (
            "U-V01 unterminated rows kept as written",
            PG,
            s("COPY t (id, note) FROM stdin;\n1\tit's fine\n2\t\"quoted\" \\t esc\n"),
            'V',
        ),
        (
            "U-V02 unterminated, SQL-looking note without secret",
            PG,
            s("COPY t (id, note) FROM stdin;\n1\tSELECT * FROM x WHERE a = 'b'\n"),
            'V',
        ),
        // ---- COPY option grammar.
        (
            "O-I01 HEADER false",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, HEADER false);\n1,XYZZY_O1\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I02 HEADER off",
            PG,
            s("COPY t (id, password) FROM stdin WITH (FORMAT csv, HEADER off);\n1,XYZZY_O2\n\\.\n"),
            'I',
        ),
        (
            "O-I03 HEADER 0",
            PG,
            s("COPY t (id, password) FROM stdin WITH (FORMAT csv, HEADER 0);\n1,XYZZY_O3\n\\.\n"),
            'I',
        ),
        (
            "O-I04 HEADER MATCH skips header only",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, HEADER MATCH);\nid,password\n1,XYZZY_O4\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I05 HEADER then DELIMITER",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, HEADER, DELIMITER ';');\nid;password\n1;XYZZY_O5\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I06 old CSV HEADER DELIMITER AS",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH CSV HEADER DELIMITER AS ';';\nid;password\n1;XYZZY_O6\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I07 FORMAT 'csv'",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT 'csv');\n1,\"a\tb\",XYZZY_O7\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I08 FORMAT \"csv\" (quoted ident)",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT \"csv\");\n1,\"a\tb\",XYZZY_O8\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I09 DELIMITER E'\\t' CSV",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, DELIMITER E'\\t');\n1\tXYZZY_O9\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I10 DELIMITER E'\\x7c' text",
            PG,
            s("COPY t (id, password) FROM stdin WITH (DELIMITER E'\\x7c');\n1|XYZZY_O10\n\\.\n"),
            'I',
        ),
        (
            "O-I11 ESCAPE '\\' CSV",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT csv, ESCAPE '\\');\n1,\"x\\\"\",XYZZY_O11\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I12 QUOTE '''' CSV",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FORMAT csv, QUOTE '''');\n1,'a,b',XYZZY_O12\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I13 old CSV QUOTE AS '''' ESCAPE AS '\\'",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin CSV QUOTE AS '''' ESCAPE AS '\\';\n1,'a\\',b',XYZZY_O13\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I14 NULL AS 'x' ENCODING then DELIMITER",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (NULL 'nil', ENCODING 'UTF8', DELIMITER ',');\n1,XYZZY_O14\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I15 FREEZE then FORMAT csv",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FREEZE, FORMAT csv);\n1,\"a\tb\",XYZZY_O15\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I16 FREEZE true, DEFAULT, FORMAT csv",
            PG,
            s(
                "COPY t (id, note, password) FROM stdin WITH (FREEZE true, DEFAULT '\\D', FORMAT csv);\n1,\"a\tb\",XYZZY_O16\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I17 /* */ comment after ;",
            PG,
            s("COPY t (id, password) FROM STDIN; /* c */\n1\tXYZZY_O17\n\\.\n"),
            'I',
        ),
        (
            "O-I18 -- comment after ;",
            PG,
            s("COPY t (id, password) FROM STDIN; -- load users\n1\tXYZZY_O18\n\\.\n"),
            'I',
        ),
        (
            "O-I19 comment inside options",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (/* x */ FORMAT csv -- y\n);\n1,XYZZY_O19\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I20 USING DELIMITERS ',' (old)",
            PG,
            s("COPY t (id, password) FROM stdin USING DELIMITERS ',';\n1,XYZZY_O20\n\\.\n"),
            'I',
        ),
        (
            "O-I21 COPY BINARY t (old prefix)",
            PG,
            s(
                "COPY BINARY t (id, secret) FROM stdin;\nPGCOPY\n\u{ff}\r\n\u{0}XYZZY_O21\u{1}\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I22 text, escaped delimiter \\| in a field",
            PG,
            s("COPY t (note, password) FROM stdin WITH (DELIMITER '|');\na\\|b|XYZZY_O22\n\\.\n"),
            'I',
        ),
        (
            "O-I23 text, backslash + newline in a field",
            PG,
            s("COPY t (id, note, password) FROM stdin;\n1\tline one\\\nline two\tXYZZY_O23\n\\.\n"),
            'I',
        ),
        (
            "O-I24 text, \\\\ before delimiter",
            PG,
            s("COPY t (note, password) FROM stdin WITH (DELIMITER '|');\nC:\\\\|XYZZY_O24\n\\.\n"),
            'I',
        ),
        (
            "O-I25 CSV FORCE_QUOTE *, HEADER",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, FORCE_QUOTE *, HEADER);\nid,password\n1,XYZZY_O25\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I26 old FORCE NOT NULL col",
            PG,
            s("COPY t (id, password) FROM stdin CSV FORCE NOT NULL password;\n1,XYZZY_O26\n\\.\n"),
            'I',
        ),
        (
            "O-I27 ON_ERROR ignore, LOG_VERBOSITY verbose",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv, ON_ERROR ignore, LOG_VERBOSITY verbose);\n1,XYZZY_O27\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I28 FORMAT text, DELIMITER ','",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT text, DELIMITER ',');\n1,XYZZY_O28\n\\.\n",
            ),
            'I',
        ),
        (
            "O-I29 options on next line",
            PG,
            s("COPY t (id, password)\n  FROM stdin\n  WITH (FORMAT csv);\n1,XYZZY_O29\n\\.\n"),
            'I',
        ),
        (
            "O-I30 CSV CRLF quoted secret with CRLF",
            PG,
            s("COPY t (id, password) FROM stdin CSV;\r\n1,\"XYZZY_O30\r\nmore\"\r\n\\.\r\n"),
            'I',
        ),
        (
            "O-I31 DELIMITER U&'\\007C'",
            PG,
            s("COPY t (id, password) FROM stdin WITH (DELIMITER U&'\\007C');\n1|XYZZY_O31\n\\.\n"),
            'I',
        ),
        (
            "O-I32 WHERE then rows",
            PG,
            s(
                "COPY t (id, password) FROM stdin WITH (FORMAT csv) WHERE id > 0;\n1,XYZZY_O32\n\\.\n",
            ),
            'I',
        ),
        (
            "O-V01 CSV quoted delimiter kept",
            PG,
            s("COPY t (id, note) FROM stdin WITH (FORMAT csv, HEADER false);\n1,\"a,b\"\n\\.\n"),
            'V',
        ),
        // ---- COPY TO and COPY (query).
        (
            "Q-I01 COPY (SELECT .. FROM stdin) TO STDOUT, INSERT, \\.",
            PG,
            s(
                "COPY (SELECT * FROM stdin) TO STDOUT;\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_Q1');\n\\.\n",
            ),
            'I',
        ),
        (
            "Q-I02 COPY (SELECT ..) TO STDOUT WITH CSV, INSERT",
            PG,
            s(
                "COPY (SELECT a FROM t) TO STDOUT WITH CSV;\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_Q2');\n",
            ),
            'I',
        ),
        (
            "Q-I03 COPY t TO STDOUT then COPY FROM stdin",
            PG,
            s(
                "COPY t (password) TO STDOUT;\nCOPY u (id, password) FROM stdin;\n1\tXYZZY_Q3\n\\.\n",
            ),
            'I',
        ),
        (
            "Q-I04 COPY t (a) TO 'f', then UPDATE",
            PG,
            s("COPY t (a) TO '/tmp/x';\nUPDATE users SET password = 'XYZZY_Q4';\n"),
            'I',
        ),
        (
            "Q-I05 COPY ((SELECT 1) UNION (SELECT 2)) TO STDOUT; INSERT",
            PG,
            s(
                "COPY ((SELECT 1) UNION (SELECT 2 FROM stdin)) TO STDOUT;\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_Q5');\n\\.\n",
            ),
            'I',
        ),
        (
            "Q-I06 COPY t FROM PROGRAM, INSERT",
            PG,
            s(
                "COPY t (a) FROM PROGRAM 'cat x';\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_Q6');\n\\.\n",
            ),
            'I',
        ),
        (
            "Q-I07 table named stdin (SELECT FROM stdin)",
            PG,
            s(
                "SELECT * FROM stdin;\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_Q7');\n\\.\n",
            ),
            'I',
        ),
        (
            "Q-O01 COPY TO STDOUT with pasted output rows",
            PG,
            s("COPY users (email, password_hash) TO STDOUT;\nbob@x\tXYZZYHASH_Q_O1\n\\.\n"),
            'O',
        ),
        (
            "Q-V01 COPY TO STDOUT kept, SQL after kept",
            PG,
            s("COPY t (id, note) TO STDOUT;\nSELECT 'it''s';\n"),
            'V',
        ),
        // ---- psql \copy.
        (
            "M-I01 \\copy from stdin csv",
            PG,
            s("\\copy users (id, password) from stdin with csv\n1,XYZZY_M1\n\\.\n"),
            'I',
        ),
        (
            "M-I02 \\COPY upper, text",
            PG,
            s("\\COPY users (id, password) FROM STDIN\n1\tXYZZY_M2\n\\.\n"),
            'I',
        ),
        (
            "M-I03 \\copy csv header delimiter ';'",
            PG,
            s(
                "\\copy users (id, password) from stdin csv header delimiter ';'\nid;password\n1;XYZZY_M3\n\\.\n",
            ),
            'I',
        ),
        (
            "M-I04 \\copy trailing ;",
            PG,
            s("\\copy users (id, password) from stdin;\n1\tXYZZY_M4\n\\.\n"),
            'I',
        ),
        (
            "M-I05 \\copy (select) to stdout then INSERT",
            PG,
            s(
                "\\copy (select * from stdin) to stdout\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_M5');\n\\.\n",
            ),
            'I',
        ),
        (
            "M-I06 \\copy from pstdin then INSERT",
            PG,
            s(
                "\\copy users (id, password) from pstdin\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_M6');\n\\.\n",
            ),
            'I',
        ),
        (
            "M-I07 \\copy from 'file' then INSERT",
            PG,
            s(
                "\\copy users (id, password) from 'u.csv' csv\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_M7');\n",
            ),
            'I',
        ),
        (
            "M-I08 \\copy after a SQL statement",
            PG,
            s(
                "SET client_encoding = 'UTF8';\n\\copy users (id, password) from stdin\n1\tXYZZY_M8\n\\.\n",
            ),
            'I',
        ),
        (
            "M-I09 \\copy unterminated",
            PG,
            s("\\copy users (id, password) from stdin\n1\tXYZZY_M9\n"),
            'I',
        ),
        // ---- Column names.
        (
            "N-I01 token",
            PG,
            s("COPY t (id, token) FROM stdin;\n1\tXYZZY_N1\n\\.\n"),
            'I',
        ),
        (
            "N-I02 remember_token",
            PG,
            s("INSERT INTO users (id, remember_token) VALUES (1, 'XYZZY_N2')"),
            'I',
        ),
        (
            "N-I03 confirmation_token/unlock_token COPY",
            PG,
            s(
                "COPY t (id, confirmation_token, unlock_token) FROM stdin;\n1\tXYZZY_N3a\tXYZZY_N3b\n\\.\n",
            ),
            'I',
        ),
        (
            "N-I04 two_factor_recovery_codes",
            PG,
            s("COPY t (id, two_factor_recovery_codes) FROM stdin;\n1\tXYZZY_N4\n\\.\n"),
            'I',
        ),
        (
            "N-I05 otp_backup_codes",
            PG,
            s("COPY t (id, otp_backup_codes) FROM stdin;\n1\tXYZZY_N5\n\\.\n"),
            'I',
        ),
        (
            "N-I06 mfa_recovery_code (singular)",
            PG,
            s("COPY t (id, mfa_recovery_code) FROM stdin;\n1\tXYZZY_N6\n\\.\n"),
            'I',
        ),
        (
            "N-I07 camelCase rememberToken",
            PG,
            s("COPY t (id, \"rememberToken\") FROM stdin;\n1\tXYZZY_N7\n\\.\n"),
            'I',
        ),
        (
            "N-I08 camelCase resetPasswordToken",
            PG,
            s("COPY t (id, \"resetPasswordToken\") FROM stdin;\n1\tXYZZY_N8\n\\.\n"),
            'I',
        ),
        (
            "N-I09 camelCase otpSecret",
            PG,
            s("COPY t (id, \"otpSecret\") FROM stdin;\n1\tXYZZY_N9\n\\.\n"),
            'I',
        ),
        (
            "N-I10 token_hash",
            PG,
            s("COPY t (id, token_hash) FROM stdin;\n1\tXYZZY_N10\n\\.\n"),
            'I',
        ),
        (
            "N-I11 totp_secret / secret_key_base",
            PG,
            s(
                "COPY t (id, totp_secret, secret_key_base) FROM stdin;\n1\tXYZZY_N11a\tXYZZY_N11b\n\\.\n",
            ),
            'I',
        ),
        (
            "N-I12 encrypted_password (devise)",
            PG,
            s("COPY t (id, encrypted_password) FROM stdin;\n1\tXYZZY_N12\n\\.\n"),
            'I',
        ),
        (
            "N-I13 client_secret / private_key",
            PG,
            s(
                "COPY t (id, client_secret, private_key) FROM stdin;\n1\tXYZZY_N13a\tXYZZY_N13b\n\\.\n",
            ),
            'I',
        ),
        (
            "N-I14 access_token_secret (oauth1)",
            PG,
            s("COPY t (id, access_token_secret) FROM stdin;\n1\tXYZZY_N14\n\\.\n"),
            'I',
        ),
        (
            "N-I15 password_changed_at with non-date value (UPDATE)",
            PG,
            s("UPDATE users SET password_changed_at = 'XYZZY_N15' WHERE id = 1"),
            'I',
        ),
        (
            "N-I16 token_count with non-number value (INSERT)",
            PG,
            s("INSERT INTO t (id, token_count) VALUES (1, 'XYZZY_N16')"),
            'I',
        ),
        (
            "N-I17 password_policy with secret-like value (UPDATE)",
            PG,
            s("UPDATE t SET password_policy = 'XYZZY_N17_hunter2'"),
            'I',
        ),
        (
            "N-I18 COPY metadata column with non-date value",
            PG,
            s("COPY t (id, password_changed_at) FROM stdin;\n1\tXYZZY_N18\n\\.\n"),
            'I',
        ),
        (
            "N-I19 secret_answer (security question)",
            PG,
            s("COPY t (id, secret_answer) FROM stdin;\n1\tXYZZY_N19\n\\.\n"),
            'I',
        ),
        (
            "N-I20 api_key / x_api_key",
            PG,
            s("COPY t (id, api_key, x_api_key) FROM stdin;\n1\tXYZZY_N20a\tXYZZY_N20b\n\\.\n"),
            'I',
        ),
        (
            "N-I21 \"Password\" quoted mixed case",
            PG,
            s("COPY \"Users\" (\"Id\", \"Password\") FROM stdin;\n1\tXYZZY_N21\n\\.\n"),
            'I',
        ),
        (
            "N-I22 password_reset_token (ends in token)",
            PG,
            s("COPY t (id, password_reset_token) FROM stdin;\n1\tXYZZY_N22\n\\.\n"),
            'I',
        ),
        (
            "N-I23 secret_token / auth_secret",
            PG,
            s(
                "COPY t (id, secret_token, auth_secret) FROM stdin;\n1\tXYZZY_N23a\tXYZZY_N23b\n\\.\n",
            ),
            'I',
        ),
        (
            "N-V01 tokens / token_type / max_tokens / has_token",
            PG,
            s(
                "COPY t (id, tokens, token_type, max_tokens, has_token) FROM stdin;\n1\t5\tbearer\t100\tt\n\\.\n",
            ),
            'V',
        ),
        (
            "N-V02 secret_santa / api_token_last4",
            PG,
            s("COPY t (id, secret_santa, api_token_last4) FROM stdin;\n1\tann\t9f3a\n\\.\n"),
            'V',
        ),
        (
            "N-V03 password_changed_at / token_expires_at / token_count",
            PG,
            s(
                "COPY t (id, password_changed_at, token_expires_at, token_count) FROM stdin;\n1\t2024-01-01 10:00:00+00\t2024-02-01\t3\n\\.\n",
            ),
            'V',
        ),
        (
            "N-V04 is_secret / has_password flags",
            PG,
            s("COPY t (id, is_secret, has_secret, no_secret) FROM stdin;\n1\tt\tf\tt\n\\.\n"),
            'V',
        ),
        (
            "N-V05 UPDATE password_changed_at date",
            PG,
            s("UPDATE users SET password_changed_at = '2024-01-01' WHERE id = 1"),
            'V',
        ),
        (
            "N-V06 UPDATE token_count number, password_length",
            PG,
            s("UPDATE t SET token_count = 3, password_length = 12"),
            'V',
        ),
        (
            "N-V07 next_page_token / api_key_id",
            PG,
            s("COPY t (id, next_page_token, api_key_id) FROM stdin;\n1\tabc\tkey_1\n\\.\n"),
            'V',
        ),
        (
            "N-V08 password_policy value 'strong'",
            PG,
            s("UPDATE t SET password_policy = 'strong'"),
            'V',
        ),
        (
            "N-V09 has_password / password_required flags",
            PG,
            s("COPY t (id, has_password, password_required) FROM stdin;\n1\tt\tf\n\\.\n"),
            'V',
        ),
        (
            "N-V10 token_type / secret_name INSERT",
            PG,
            s("INSERT INTO t (token_type, secret_santa) VALUES ('bearer', 'ann')"),
            'V',
        ),
        // ---- Script bodies.
        (
            "B-I01 plpy.execute INSERT secret col",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nplpy.execute(\"INSERT INTO users (name, password) VALUES ('bob', 'XYZZY_B1')\")\n$$",
            ),
            'I',
        ),
        (
            "B-I02 plpy.execute f-string INSERT",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nplpy.execute(f\"INSERT INTO users (name, password) VALUES ('{n}', 'XYZZY_B2')\")\n$$",
            ),
            'I',
        ),
        (
            "B-I03 plpy.execute triple-quoted UPDATE",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nplpy.execute(\"\"\"\nUPDATE users SET password = 'XYZZY_B3'\n\"\"\")\n$$",
            ),
            'I',
        ),
        (
            "B-I04 plpy.prepare + execute",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\np = plpy.prepare(\"UPDATE users SET password = 'XYZZY_B4' WHERE id = $1\", [\"int\"])\n$$",
            ),
            'I',
        ),
        (
            "B-I05 plpy.cursor",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nfor r in plpy.cursor(\"SELECT * FROM t WHERE password = 'XYZZY_B5'\"): pass\n$$",
            ),
            'I',
        ),
        (
            "B-I06 query in variable then execute",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nq = \"INSERT INTO users (name, password) VALUES ('bob', 'XYZZY_B6')\"\nplpy.execute(q)\n$$",
            ),
            'I',
        ),
        (
            "B-I07 subprocess arg list mysql -p",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nimport subprocess\nsubprocess.run([\"mysqldump\", \"-uroot\", \"-pxyzzysecretword\", \"appdb\"])\n$$",
            ),
            'I',
        ),
        (
            "B-I08 os.system mysqldump -p",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nimport os\nos.system(\"mysqldump appdb -pxyzzysecretword > /tmp/x\")\n$$",
            ),
            'I',
        ),
        (
            "B-I09 python dict password",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\ncfg = {'host': 'db', 'password': 'XYZZY_B9'}\n$$",
            ),
            'I',
        ),
        (
            "B-I10 python environ PGPASSWORD",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nimport os\nos.environ['PGPASSWORD'] = 'XYZZY_B10'\n$$",
            ),
            'I',
        ),
        (
            "B-I11 plv8.execute INSERT",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plv8 AS $$\nplv8.execute(\"INSERT INTO users (name, password) VALUES ('bob', 'XYZZY_B11')\");\n$$",
            ),
            'I',
        ),
        (
            "B-I12 plv8.execute template literal",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plv8 AS $$\nplv8.execute(`UPDATE users SET password = 'XYZZY_B12'`);\n$$",
            ),
            'I',
        ),
        (
            "B-I13 plv8 regex with ' then // comment",
            PG,
            s(
                "CREATE FUNCTION f(s text) RETURNS text LANGUAGE plv8 AS $$\nvar r = s.replace(/'/g, ''); // mysql -uroot -pxyzzysecretword appdb\nreturn r;\n$$",
            ),
            'I',
        ),
        (
            "B-I14 plv8 division then // comment",
            PG,
            s(
                "CREATE FUNCTION f(a int, b int) RETURNS int LANGUAGE plv8 AS $$\nvar x = a / b; // mysql -uroot -pxyzzysecretword appdb\nreturn x;\n$$",
            ),
            'I',
        ),
        (
            "B-I15 plv8 /* */ comment password",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plv8 AS $$\n/* ALTER ROLE app PASSWORD 'XYZZY_B15' */\nreturn 1;\n$$",
            ),
            'I',
        ),
        (
            "B-I16 plperl spi_exec_query INSERT",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plperl AS $$\nspi_exec_query(\"INSERT INTO users (name, password) VALUES ('bob', 'XYZZY_B16')\");\n$$",
            ),
            'I',
        ),
        (
            "B-I17 plperl spi_exec_query qq{}",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plperl AS $$\nspi_exec_query(qq{INSERT INTO users (name, password) VALUES ('bob', 'XYZZY_B17')});\n$$",
            ),
            'I',
        ),
        (
            "B-I18 plperl regex with ' then # comment",
            PG,
            s(
                "CREATE FUNCTION f(text) RETURNS text LANGUAGE plperl AS $$\nmy $s = shift; $s =~ s/'//g; # mysql -uroot -pxyzzysecretword appdb\nreturn $s;\n$$",
            ),
            'I',
        ),
        (
            "B-I19 plperl system mysqldump",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plperlu AS $$\nsystem(\"mysqldump appdb -pxyzzysecretword > /tmp/x\");\n$$",
            ),
            'I',
        ),
        (
            "B-I20 pltcl spi_exec \"...\" (no parens)",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE pltcl AS $$\nspi_exec \"INSERT INTO users (name, password) VALUES ('bob', 'XYZZY_B20')\"\n$$",
            ),
            'I',
        ),
        (
            "B-I21 pltcl spi_exec {...} braces",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE pltcl AS $$\nspi_exec {UPDATE users SET password = 'XYZZY_B21'}\n$$",
            ),
            'I',
        ),
        (
            "B-I22 pltcl ALTER ROLE in braces",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE pltcl AS $$\nspi_exec {ALTER ROLE app PASSWORD 'XYZZY_B22'}\n$$",
            ),
            'I',
        ),
        (
            "B-I23 plsh body mysqldump",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS text LANGUAGE plsh AS $$\n#!/bin/sh\nmysqldump appdb -pxyzzysecretword > /tmp/x.sql\n$$",
            ),
            'I',
        ),
        (
            "B-I24 plsh PGPASSWORD=",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS text LANGUAGE plsh AS $$\n#!/bin/sh\nPGPASSWORD=xyzzysecretword psql -h db -c 'select 1'\n$$",
            ),
            'I',
        ),
        (
            "B-I25 plsh with ' in a word before",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS text LANGUAGE plsh AS $$\n#!/bin/sh\necho \"it's\"\nmysql -pxyzzysecretword appdb\n$$",
            ),
            'I',
        ),
        (
            "B-I26 plcoffee # comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plcoffee AS $$\n# mysql -uroot -pxyzzysecretword appdb\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I27 plls # comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plls AS $$\n# mysql -uroot -pxyzzysecretword appdb\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I28 plr pg.spi.exec",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plr AS $$\npg.spi.exec(\"INSERT INTO users (name, password) VALUES ('bob', 'XYZZY_B28')\")\nreturn(1)\n$$",
            ),
            'I',
        ),
        (
            "B-I29 plpython single-quoted body, execute",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void AS 'plpy.execute(\"UPDATE users SET password = ''XYZZY_B29''\")' LANGUAGE plpython3u",
            ),
            'I',
        ),
        (
            "B-I30 plv8 bind params array",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plv8 AS $$\nplv8.execute('INSERT INTO users (name, password) VALUES ($1, $2)', ['bob', 'XYZZY_B30']);\n$$",
            ),
            'I',
        ),
        (
            "B-I31 plpython string with # then secret comment",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\nx = 'a#b'  # mysql -uroot -pxyzzysecretword appdb\nreturn 1\n$$",
            ),
            'I',
        ),
        (
            "B-I32 plpython raw string execute",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nplpy.execute(r\"UPDATE users SET password = 'XYZZY_B32'\")\n$$",
            ),
            'I',
        ),
        (
            "B-V01 plpython code kept",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS text LANGUAGE plpython3u AS $$\n# it's fine\nreturn 'x' if 1 else \"it's\"\n$$",
            ),
            'V',
        ),
        (
            "B-V02 plpy.execute plain SQL kept",
            PG,
            s(
                "CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS $$\nplpy.execute(\"UPDATE t SET note = 'hello' WHERE id = 1\")\n$$",
            ),
            'V',
        ),
        (
            "B-V03 plv8 regex kept",
            PG,
            s(
                "CREATE FUNCTION f(s text) RETURNS text LANGUAGE plv8 AS $$\nreturn s.replace(/[\"']/g, ''); // strip quotes\n$$",
            ),
            'V',
        ),
        (
            "B-V04 plperl hash and regex",
            PG,
            s(
                "CREATE FUNCTION f(text) RETURNS int LANGUAGE plperl AS $$\nmy %h = (a => 1); my $n = $#_; return scalar(keys %h);\n$$",
            ),
            'V',
        ),
        // ---- Prompts and bullets.
        (
            "P-I01 zsh kim@macbook ~ %",
            MY,
            c("kim@macbook ~ % mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I02 fish kim@macbook ~>",
            MY,
            c("kim@macbook ~> mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I03 @daily root",
            MY,
            c("@daily root mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I04 @reboot (no user)",
            MY,
            c("@reboot mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I05 cmd C:\\>mysqldump",
            MY,
            c("C:\\>mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I06 cmd C:\\Users\\kim>mysql",
            MY,
            c("C:\\Users\\kim>mysql -pxyzzysecretword appdb"),
            'I',
        ),
        (
            "P-I07 PS C:\\Users\\kim> mysql",
            MY,
            c("PS C:\\Users\\kim> mysql -pxyzzysecretword appdb"),
            'I',
        ),
        (
            "P-I08 zsh path prompt kim@mac project %",
            MY,
            c("kim@mac project % mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I09 starship prompt ❯",
            MY,
            c("❯ mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I10 oh-my-zsh ➜  ~",
            MY,
            c("➜  ~ mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I11 bullet - mysql -uroot -pX",
            MY,
            c("- mysql -uroot -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I12 bullet - mysqldump appdb -pX",
            MY,
            c("- mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I13 bullet - mysql -pShaped",
            MY,
            c("- mysql -pXyzzy_S3cret"),
            'I',
        ),
        (
            "P-I14 bullet + mysqldump appdb -pX",
            MY,
            c("+ mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I15 bullet • mysqldump appdb -pX",
            MY,
            c("• mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I16 bullet * mysql -pX (star)",
            MY,
            c("* mysql -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I17 numbered 1. mysqldump appdb -pX",
            MY,
            c("1. mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I18 ~/dir$ prompt",
            MY,
            c("~/backups$ mysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-I19 root@db1:~# with tab",
            MY,
            c("root@db1:~#\tmysqldump appdb -pxyzzysecretword"),
            'I',
        ),
        (
            "P-O01 bullet - mysql -pX appdb (unshaped, accepted)",
            MY,
            c("- mysql -pxyzzysecretword appdb"),
            'O',
        ),
        (
            "P-O02 bullet - mysql -pX (unshaped)",
            MY,
            c("- mysql -pxyzzysecretword"),
            'O',
        ),
        (
            "P-V01 - mysql -performance_schema",
            MY,
            c("- mysql -performance_schema is on"),
            'V',
        ),
        (
            "P-V02 * mysql -performance_schema",
            MY,
            c("* mysql -performance_schema is on"),
            'V',
        ),
        (
            "P-V03 + mysql -performance_schema",
            MY,
            c("+ mysql -performance_schema is on"),
            'V',
        ),
        (
            "P-V04 email then mysql -performance",
            MY,
            c("bob@example.com mysql -performance tips"),
            'V',
        ),
        (
            "P-V05 ~ approx mysql -performance",
            MY,
            c("~ mysql -performance is 2x"),
            'V',
        ),
        (
            "P-V06 @kim mention then mysql -prefers",
            MY,
            c("@kim mysql -prefers hash joins"),
            'V',
        ),
    ]
}

#[test]
fn no_secret_of_the_fourteenth_audit_reaches_the_log() -> TestResult {
    let all = fourteenth_cases();
    assert_eq!(all.len(), 162);
    let mut checked = Vec::new();
    let mut changed = Vec::new();
    for (tag, d, stmt, kind) in all {
        let id = tag.split(' ').next().unwrap_or_default();
        match kind {
            'I' if !FOURTEENTH_AUDIT_ACCEPTED.contains(&id) => {
                checked.push(c4('p', tag, d, &stmt, X10))
            }
            'I' | 'O' => checked.push(c4('v', tag, d, &stmt, X10)),
            _ => {
                let full = prepare_statement_text(&stmt, d, TextLevel::Full).unwrap_or_default();
                if full != stmt {
                    changed.push(format!("{tag}: {full}"));
                }
            }
        }
    }
    let found = audit4_leaks(&checked)?;
    assert!(
        found.is_empty(),
        "{} leaks:\n{}",
        found.len(),
        found.join("\n")
    );
    assert!(changed.is_empty(), "{}", changed.join("\n"));
    Ok(())
}

/// Error messages at `normalized` for statements that raise their own
/// error in every language, and template words next to look-alikes.
#[test]
fn fourteenth_audit_message_words() -> TestResult {
    const PG: SqlDialect = SqlDialect::Postgres;
    const MY: SqlDialect = SqlDialect::Mysql;
    let cases: Vec<(&str, SqlDialect, &str, &str, &str)> = vec![
        (
            "W01 column named signal: template word kept",
            PG,
            "UPDATE alarms SET signal = 'division' WHERE id = 1",
            "22012",
            "division by zero",
        ),
        (
            "W02 raise in a comment: template word kept",
            PG,
            "SELECT 'deadlock' -- raise later\n",
            "40P01",
            "deadlock detected",
        ),
        (
            "W03 RAISE after THEN",
            PG,
            "DO $$ BEGIN IF true THEN RAISE EXCEPTION 'admin'; END IF; END $$",
            "P0001",
            "admin",
        ),
        (
            "W04 RESIGNAL in DECLARE HANDLER",
            MY,
            "CREATE PROCEDURE p() BEGIN DECLARE EXIT HANDLER FOR SQLEXCEPTION RESIGNAL SET MESSAGE_TEXT = 'admin'; SELECT 1; END",
            "1644",
            "admin",
        ),
        (
            "W05 SIGNAL in CONTINUE HANDLER FOR SQLSTATE",
            MY,
            "CREATE PROCEDURE p() BEGIN DECLARE CONTINUE HANDLER FOR SQLSTATE '23000' SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'admin'; INSERT INTO t VALUES (1); END",
            "1644",
            "admin",
        ),
        (
            "W06 SIGNAL after label:",
            MY,
            "CREATE PROCEDURE p() lbl: BEGIN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'admin'; END",
            "1644",
            "admin",
        ),
        (
            "W07 RAISE after EXCEPTION WHEN .. THEN",
            PG,
            "DO $$ BEGIN PERFORM 1; EXCEPTION WHEN others THEN RAISE EXCEPTION 'admin'; END $$",
            "P0001",
            "admin",
        ),
        (
            "W08 RAISE after <<label>>",
            PG,
            "DO $$ <<blk>> BEGIN RAISE EXCEPTION 'admin'; END $$",
            "P0001",
            "admin",
        ),
        (
            "W09 plpy.error",
            PG,
            "DO LANGUAGE plpython3u $$ plpy.error('admin') $$",
            "XX000",
            "admin",
        ),
        (
            "W10 plpython raise Exception",
            PG,
            "DO LANGUAGE plpython3u $$\nraise Exception('admin')\n$$",
            "38000",
            "admin",
        ),
        (
            "W11 plv8 throw new Error",
            PG,
            "DO LANGUAGE plv8 $$ throw new Error('admin'); $$",
            "XX000",
            "Error: admin",
        ),
        (
            "W12 plv8.elog(ERROR, ..)",
            PG,
            "DO LANGUAGE plv8 $$ plv8.elog(ERROR, 'admin'); $$",
            "XX000",
            "admin",
        ),
        (
            "W13 pltcl elog ERROR (no parens)",
            PG,
            "DO LANGUAGE pltcl $$ elog ERROR \"admin\" $$",
            "XX000",
            "admin",
        ),
        (
            "W14 pltcl error command",
            PG,
            "CREATE FUNCTION f() RETURNS void LANGUAGE pltcl AS $$ error \"admin\" $$",
            "XX000",
            "admin",
        ),
        (
            "W15 plperl die",
            PG,
            "DO LANGUAGE plperl $$ die \"admin\\n\"; $$",
            "38000",
            "admin",
        ),
        (
            "W16 plperl elog(ERROR..)",
            PG,
            "DO LANGUAGE plperl $$ elog(ERROR, 'admin'); $$",
            "XX000",
            "admin",
        ),
        (
            "W17 'die' in an ordinary literal (over-hide check)",
            PG,
            "INSERT INTO movies (title) VALUES ('Die Hard')",
            "23505",
            "duplicate key value violates unique constraint \"movies_title_key\"",
        ),
        (
            "W18 'die ' literal + template word",
            PG,
            "SELECT 'die now', 'division' / 0",
            "22012",
            "division by zero",
        ),
        (
            "W19 raised_at column, no raise",
            PG,
            "UPDATE t SET raised_at = now() WHERE note = 'deadlock'",
            "40P01",
            "deadlock detected",
        ),
        (
            "W20 RAISE NOTICE then error elsewhere",
            PG,
            "DO $$ BEGIN RAISE NOTICE 'x'; PERFORM 'deadlock'; END $$",
            "40P01",
            "deadlock detected",
        ),
        (
            "W21 MySQL IF THEN SIGNAL",
            MY,
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN IF NEW.a < 0 THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'admin'; END IF; END",
            "1644",
            "admin",
        ),
        (
            "W22 MySQL CASE WHEN THEN SIGNAL",
            MY,
            "CREATE PROCEDURE p(x int) BEGIN CASE WHEN x < 0 THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'admin'; END CASE; END",
            "1644",
            "admin",
        ),
        (
            "W23 EXECUTE 'RAISE ...' dynamic",
            PG,
            "DO $$ BEGIN EXECUTE format('DO $x$ BEGIN RAISE EXCEPTION %L; END $x$', 'admin'); END $$",
            "P0001",
            "admin",
        ),
    ];
    let expected = EXPECTED_FOURTEENTH_WORDS;
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("audit"));
    recorder.set_text_level(TextLevel::Normalized);
    let mut wrong = Vec::new();
    for (i, ((tag, d, stmt, code, msg), want)) in cases.into_iter().zip(expected).enumerate() {
        let connection = format!("w14-{i}");
        recorder.record(
            ActivityEvent::query_failed(
                stmt,
                d,
                "x",
                &ErrorDetails {
                    code: Some(code.into()),
                    message: msg.into(),
                },
                1,
            )
            .connection(connection.as_str())
            .build()?,
        )?;
        let event = recorder.query_history(&connection, 1)?.remove(0);
        if event.details["error"]["message"] != want {
            wrong.push(format!("{tag}: {}", event.details["error"]["message"]));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    Ok(())
}

/// The messages stored for [`fourteenth_audit_message_words`].
const EXPECTED_FOURTEENTH_WORDS: [&str; 23] = [
    "division by zero",
    "deadlock detected",
    "?",
    "?",
    "?",
    "?",
    "?",
    "?",
    "?",
    "?",
    "Error: ?",
    "?",
    "?",
    "?",
    "?",
    "?",
    "duplicate key value violates unique constraint ?",
    "division by zero",
    "deadlock detected",
    "? detected",
    "?",
    "?",
    "?",
];

/// A dump near the size cap with a secret in every row stays redacted
/// field by field: the markers are counted against no cap, and a password
/// word in one field does not take the next field.
#[test]
fn copy_rows_are_redacted_field_by_field() {
    let mut text = String::from("COPY t (id, password, note) FROM stdin;\n");
    for i in 0..45_000 {
        text.push_str(&format!("{i}\tpw{i}\ta note\n"));
    }
    text.push_str("\\.\n");
    assert!(text.len() < 1 << 20);
    let full =
        prepare_statement_text(&text, SqlDialect::Postgres, TextLevel::Full).unwrap_or_default();
    assert!(
        full.contains("44999\t[redacted]\ta note"),
        "{}",
        &full[..200.min(full.len())]
    );
    let text = "COPY t (id, note, created) FROM stdin;\n1\tforgot my password\t2024-01-01\n\\.\n";
    assert_eq!(
        prepare_statement_text(text, SqlDialect::Postgres, TextLevel::Full).as_deref(),
        Some(text)
    );
}

// ---- Fifteenth audit ----------------------------------------------------

/// A fifteenth-audit input. `kind`: `I` inside a rule (no level may keep
/// a secret), `O` outside one (checked at `none` and `normalized`), `V`
/// valid text stored as written at `full`, `M` an error message, whose
/// `hide` part must not reach the stored message at `normalized`.
struct F15<'a> {
    tag: &'a str,
    d: SqlDialect,
    kind: char,
    text: &'a str,
    code: Option<&'a str>,
    msg: Option<&'a str>,
    hide: Option<&'a str>,
}

/// Metadata columns keep only a date, a number, `null` or a policy word,
/// so the audit's `ordinary value 7` in them is redacted by design
/// (design doc 9.3).
const FIFTEENTH_AUDIT_METADATA: [&str; 5] = ["N-V04", "N-V17", "N-V21", "N-V22", "N-V33"];

/// Message text built at run time (`` `xyzzy${1}tail` ``, `"xyzzy" +
/// "cat"`) or quoted in forms not read as literals (Perl `q{}`, Tcl
/// braces): documented limits (see the redactor's module docs, "Limits").
const FIFTEENTH_AUDIT_MESSAGE_LIMITS: [&str; 4] = ["B-L02", "B-L03", "B-L06", "B-L07"];

/// The fifteenth audit's 255 inputs.
#[rustfmt::skip]
fn fifteenth_cases() -> Vec<F15<'static>> {
    vec![
        F15 { tag: "A-I01 \\connect then COPY", d: PG, kind: 'I', text: "\\connect appdb\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHA1\tXYZZYTOKA1\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I02 \\c then INSERT", d: PG, kind: 'I', text: "\\c appdb\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_A2');\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I03 \\set then INSERT", d: PG, kind: 'I', text: "\\set ON_ERROR_STOP on\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_A3');\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I04 \\restrict then COPY", d: PG, kind: 'I', text: "\\restrict aBcDeF123\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHA4\tXYZZYTOKA4\n\\.\n\\unrestrict aBcDeF123\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I05 SELECT 1 without ; then COPY", d: PG, kind: 'I', text: "SELECT 1\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHA5\tXYZZYTOKA5\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I06 \\copy from pstdin then INSERT", d: PG, kind: 'I', text: "\\copy users (id, password) from pstdin\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_A6');\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I07 meta line with unmatched ' then COPY", d: PG, kind: 'I', text: "\\echo Loading user's table\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHA7\tXYZZYTOKA7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I08 meta line with unmatched ' then INSERT", d: PG, kind: 'I', text: "\\echo don't stop\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_A8');\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I09 meta line with $$ then COPY", d: PG, kind: 'I', text: "\\set body $$\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHA9\tXYZZYTOKA9\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I10 meta line with /* then COPY", d: PG, kind: 'I', text: "\\echo /* start\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHA10\tXYZZYTOKA10\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I11 meta line with \" then COPY", d: PG, kind: 'I', text: "\\echo say \"hi\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHA11\tXYZZYTOKA11\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I12 meta with ' then two COPY blocks", d: PG, kind: 'I', text: "\\echo it's\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHA12a\tXYZZYTOKA12a\n\\.\nCOPY t (id, password) FROM stdin;\n1\tXYZZY_A12b\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I13 \\set password var quoted", d: PG, kind: 'I', text: "\\set password 'XYZZY_A13'\nALTER ROLE app PASSWORD :'password';\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I14 \\set pw unquoted", d: PG, kind: 'I', text: "\\set pw XYZZY_A14secret\nALTER ROLE app PASSWORD :'pw';\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I15 \\connect conninfo with password", d: PG, kind: 'I', text: "\\connect \"host=db dbname=app user=app password=XYZZY_A15\"\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I16 \\c URL with password", d: PG, kind: 'I', text: "\\c postgresql://app:XYZZY_A16@db/app\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I17 SELECT \\gset then INSERT", d: PG, kind: 'I', text: "SELECT 1 AS x \\gset\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_A17');\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I18 SELECT \\g then COPY", d: PG, kind: 'I', text: "SELECT 1 \\g\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHA18\tXYZZYTOKA18\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I19 \\! shell with mysqldump -p", d: PG, kind: 'I', text: "\\! mysqldump appdb -uroot -pXYZZY_A19 > x.sql\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I20 \\gexec after SELECT then UPDATE", d: PG, kind: 'I', text: "SELECT format('VACUUM %I', t) FROM x\n\\gexec\nUPDATE users SET password = 'XYZZY_A20';\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I21 E-string with newline+\\connect inside", d: PG, kind: 'I', text: "INSERT INTO users (name, password) VALUES (E'a\n\\\\connect', 'XYZZY_A21');\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I22 plain literal with line starting \\", d: PG, kind: 'I', text: "INSERT INTO users (note, password) VALUES ('line one\n\\connect x', 'XYZZY_A22');\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I23 dollar body line starting \\", d: PG, kind: 'I', text: "CREATE FUNCTION f() RETURNS void LANGUAGE plpgsql AS $$\nBEGIN\n\\set x\nALTER ROLE app PASSWORD 'XYZZY_A23';\nEND $$;\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I24 \\ line in a block comment", d: PG, kind: 'I', text: "/* see\n\\connect db\n*/ ALTER ROLE app PASSWORD 'XYZZY_A24';\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I25 UPDATE mid-statement line starting \\", d: PG, kind: 'I', text: "UPDATE users SET note = 'x',\n\\ password = 'XYZZY_A25';\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I26 \\ line inside COPY column list", d: PG, kind: 'I', text: "COPY users (id,\n\\x\npassword) FROM stdin;\n1\tXYZZY_A26\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I27 ALTER ROLE split by \\ line", d: PG, kind: 'I', text: "ALTER ROLE app WITH\n\\echo x\nPASSWORD 'XYZZY_A27';\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I28 CREATE USER split by \\ line", d: PG, kind: 'I', text: "CREATE USER app\n\\echo hi\nIDENTIFIED BY 'XYZZY_A28';\n", code: None, msg: None, hide: None },
        F15 { tag: "A-I29 E string \\' then newline \\\\connect", d: PG, kind: 'I', text: "UPDATE users SET password = E'it\\'s\n\\\\connect XYZZY_A29';\n", code: None, msg: None, hide: None },
        F15 { tag: "A-V01 \\connect then SELECT kept", d: PG, kind: 'V', text: "\\connect appdb\nSELECT 'it''s';\n", code: None, msg: None, hide: None },
        F15 { tag: "A-V02 \\copyright then SELECT", d: PG, kind: 'V', text: "\\copyright\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "A-V03 \\c 日本db", d: PG, kind: 'V', text: "\\c 日本db\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "A-V04 \\ir ébauche.sql", d: PG, kind: 'V', text: "\\ir ébauche.sql\n", code: None, msg: None, hide: None },
        F15 { tag: "A-V05 SELECT then \\c 日本 on later line", d: PG, kind: 'V', text: "SELECT 1;\n\\c 日本\n", code: None, msg: None, hide: None },
        F15 { tag: "A-V06 \\cd ~/données", d: PG, kind: 'V', text: "\\cd ~/données\n", code: None, msg: None, hide: None },
        F15 { tag: "C-I01 COPY after SELECT no ;", d: PG, kind: 'I', text: "SELECT now()\nCOPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHC1\tXYZZYTOKC1\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "C-I02 COPY after SET no ;", d: PG, kind: 'I', text: "SET search_path = public\nCOPY t (id, password) FROM stdin;\n1\tXYZZY_C2\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "C-I03 column named copy at line start in INSERT", d: PG, kind: 'I', text: "INSERT INTO users (name,\ncopy, password) VALUES ('a', 'b', 'XYZZY_C3');\n", code: None, msg: None, hide: None },
        F15 { tag: "C-I04 column named copy at line start in UPDATE", d: PG, kind: 'I', text: "UPDATE users SET name = 'a',\ncopy = 'b', password = 'XYZZY_C4';\n", code: None, msg: None, hide: None },
        F15 { tag: "C-I05 copy at line start inside CREATE ROLE", d: PG, kind: 'I', text: "CREATE ROLE app\ncopy PASSWORD 'XYZZY_C5';\n", code: None, msg: None, hide: None },
        F15 { tag: "C-I06 copy column at line start, INSERT api_token", d: PG, kind: 'I', text: "INSERT INTO accounts (id, name\n, copy, api_token) VALUES (1, 'a', 'b', 'XYZZY_C6');\n", code: None, msg: None, hide: None },
        F15 { tag: "C-I07 COPY indented after statement", d: PG, kind: 'I', text: "SELECT 1\n   COPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASHC7\tXYZZYTOKC7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "C-I08 copy at line start before VALUES", d: PG, kind: 'I', text: "INSERT INTO users (name, password)\ncopy VALUES ('a', 'XYZZY_C8');\n", code: None, msg: None, hide: None },
        F15 { tag: "C-I09 INSERT col list: copy then password on next line", d: PG, kind: 'I', text: "INSERT INTO users (id,\ncopy,\npassword) VALUES (1, 'b', 'XYZZY_C9');\n", code: None, msg: None, hide: None },
        F15 { tag: "C-V01 copy not at line start", d: PG, kind: 'V', text: "SELECT 'a' AS copy, 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I01 CSV quoted field with delimiter then secret", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin CSV;\n\"a,b\",XYZZY_F1\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I02 CSV quoted field with newline then secret", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin CSV;\n\"line1\nline2,\",XYZZY_F2\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I03 CSV escaped quote then secret", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin CSV;\n\"say \"\"hi,\"\" now\",XYZZY_F3\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I04 CSV ESCAPE \\ escaped quote", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin WITH (FORMAT csv, ESCAPE '\\');\n\"a\\\",b\",XYZZY_F4\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I05 CSV secret quoted with delimiter", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin CSV;\n1,\"XYZZY,F5\"\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I06 CSV secret quoted with newline", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin CSV;\n1,\"XYZZY\nF6second\"\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I07 CSV stray quote mid note, secrets in rows", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin CSV;\n5\" screen,XYZZY_F7a\n2,XYZZY_F7b\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I08 CSV unbalanced quote at field start", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin CSV;\n\"open,XYZZY_F8a\n2,XYZZY_F8b\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I09 CSV quote mid-field balanced", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin CSV;\nab\"c,d\"e,XYZZY_F9\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I10 CSV header quoted names", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (FORMAT csv, HEADER true);\n\"id\",\"password\"\n1,XYZZY_F10\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I11 CSV header with newline in quoted header", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (FORMAT csv, HEADER);\n\"i\nd\",password\n1,XYZZY_F11\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I12 text escaped tab then secret", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin;\na\\\tb\tXYZZY_F12\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I13 text \\\\ then delimiter", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin;\nC:\\\\\tXYZZY_F13\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I14 text backslash-newline then secret", d: PG, kind: 'I', text: "COPY t (id, note, password) FROM stdin;\n1\tline one\\\nline two\tXYZZY_F14\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I15 secret ending with backslash before \\.", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin;\n1\tXYZZY_F15\\\n\\.\nINSERT INTO users (name, password) VALUES ('b', 'XYZZY_F15b');\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I16 CSV quoted secret spanning \\. line", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin CSV;\n1,\"XYZZY_F16\n\\.\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I17 text CRLF rows", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin;\r\n1\tXYZZY_F17\r\n\\.\r\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I18 field SQL INSERT", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin;\n1\tINSERT INTO users (name, password) VALUES ('b', 'XYZZY_F18');\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I19 field ALTER ROLE PASSWORD", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin;\n1\tALTER ROLE app PASSWORD 'XYZZY_F19'\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I20 field mysql -p", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin;\n1\tmysql -uroot -pXYZZY_F20 appdb\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I21 field URL", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin;\n1\tpostgres://u:XYZZY_F21@h/db\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I22 field password=x", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin;\n1\tpassword=XYZZY_F22\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I23 field JSON compact", d: PG, kind: 'I', text: "COPY t (id, cfg) FROM stdin;\n1\t{\"password\":\"XYZZY_F23\"}\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I24 field JSON spaced", d: PG, kind: 'I', text: "COPY t (id, cfg) FROM stdin;\n1\t{\"user\": \"a\", \"password\": \"XYZZY_F24\"}\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I25 field password: value", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin;\n1\tpassword: XYZZY_F25\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I26 CSV quoted UPDATE with commas", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin CSV;\n1,\"UPDATE users SET password = 'XYZZY_F26', name = 'x'\"\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I27 text \\n escape then ALTER ROLE", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin;\n1\tline\\nALTER ROLE app PASSWORD 'XYZZY_F27'\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I28 text password:\\t value (escaped tab)", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin;\n1\tpassword:\\tXYZZY_F28\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I29 CSV ';' JSON with doubled quotes", d: PG, kind: 'I', text: "COPY t (id, cfg) FROM stdin WITH (FORMAT csv, DELIMITER ';');\n1;\"{\"\"password\"\": \"\"XYZZY_F29\"\"}\"\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-I30 text JSON with escaped quotes \\\"", d: PG, kind: 'I', text: "COPY t (id, cfg) FROM stdin;\n1\t{\\\"password\\\": \\\"XYZZY_F30\\\"}\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-V01 CSV quoted notes kept", d: PG, kind: 'V', text: "COPY t (id, note) FROM stdin CSV;\n1,\"a, b, c\"\n2,\"say \"\"hi\"\"\"\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-V02 password word then date", d: PG, kind: 'V', text: "COPY t (id, note, created) FROM stdin;\n1\tforgot my password\t2024-01-01\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-V03 settings password_min_length 12", d: PG, kind: 'V', text: "COPY settings (name, value) FROM stdin;\npassword_min_length\t12\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-V04 row id password", d: PG, kind: 'V', text: "COPY t (a, b) FROM stdin;\nid\tpassword\n1\t2\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-V05 CSV header id,password", d: PG, kind: 'V', text: "COPY t (a, b) FROM stdin WITH (FORMAT csv, HEADER true);\nid,password\n1,2\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-V06 note mysql -performance", d: PG, kind: 'V', text: "COPY t (id, note) FROM stdin;\n1\tmysql -performance tips\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "F-V07 CSV no header id,password row", d: PG, kind: 'V', text: "COPY t (a, b) FROM stdin CSV;\nid,password\n1,2\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I01 FORMAT \"csv\"", d: PG, kind: 'I', text: "COPY t (id, note, password) FROM stdin WITH (FORMAT \"csv\");\n1,\"a\tb\",XYZZY_O1\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I02 USING DELIMITERS ','", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin USING DELIMITERS ',';\n1,XYZZY_O2\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I03 USING DELIMITERS '|' WITH NULL AS", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin USING DELIMITERS '|' WITH NULL AS '';\n1|XYZZY_O3\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I04 COPY BINARY t", d: PG, kind: 'I', text: "COPY BINARY t (id, secret) FROM stdin;\nPGCOPY\nÿ\r\n\u{0}XYZZY_O4\u{1}\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I05 U&'\\007C'", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER U&'\\007C');\n1|XYZZY_O5\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I06 U&'!007C' UESCAPE '!'", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER U&'!007C' UESCAPE '!');\n1|XYZZY_O6\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I07 E'\\t'", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER E'\\t');\n1\tXYZZY_O7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I08 E'\\174' octal", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER E'\\174');\n1|XYZZY_O8\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I09 E'\\u007C'", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER E'\\u007C');\n1|XYZZY_O9\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I10 DELIMITER 'é' (server error)", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER 'é');\n1éXYZZY_O10\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I11 QUOTE E'\\''", d: PG, kind: 'I', text: "COPY t (id, note, password) FROM stdin WITH (FORMAT csv, QUOTE E'\\'');\n1,'a,b',XYZZY_O11\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I12 \\copy delimiter '|'", d: PG, kind: 'I', text: "\\copy t (id, password) from stdin with delimiter '|'\n1|XYZZY_O12\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I13 \\copy csv quote ''''", d: PG, kind: 'I', text: "\\copy t (id, note, password) from stdin csv quote ''''\n1,'a,b',XYZZY_O13\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I14 FORMAT text DELIMITER '|'", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (FORMAT text, DELIMITER '|');\n1|XYZZY_O14\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I15 old DELIMITER '|' NULL ''", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin DELIMITER '|' NULL '';\n1|XYZZY_O15\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I16 (FORMAT binary)", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin (FORMAT binary);\nPGCOPY\nXYZZY_O16\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I17 lower copy binary t", d: PG, kind: 'I', text: "copy binary t (id, secret) from stdin;\nPGCOPY\nXYZZY_O17\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "O-I18 DELIMITER '\\' text (PG rejects)", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER '\\');\n1\\XYZZY_O18\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I01 COPY column \"rememberToken\"", d: PG, kind: 'I', text: "COPY t (id, \"rememberToken\") FROM stdin;\n1\tXYZZY_N01\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I02 COPY column \"otpSecret\"", d: PG, kind: 'I', text: "COPY t (id, \"otpSecret\") FROM stdin;\n1\tXYZZY_N02\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I03 COPY column token_hash", d: PG, kind: 'I', text: "COPY t (id, token_hash) FROM stdin;\n1\tXYZZY_N03\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I04 COPY column secret_key_base", d: PG, kind: 'I', text: "COPY t (id, secret_key_base) FROM stdin;\n1\tXYZZY_N04\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I05 COPY column mfa_recovery_code", d: PG, kind: 'I', text: "COPY t (id, mfa_recovery_code) FROM stdin;\n1\tXYZZY_N05\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I06 COPY column secret_answer", d: PG, kind: 'I', text: "COPY t (id, secret_answer) FROM stdin;\n1\tXYZZY_N06\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I07 COPY column \"APIToken\"", d: PG, kind: 'I', text: "COPY t (id, \"APIToken\") FROM stdin;\n1\tXYZZY_N07\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I08 COPY column \"oauth2Token\"", d: PG, kind: 'I', text: "COPY t (id, \"oauth2Token\") FROM stdin;\n1\tXYZZY_N08\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I09 COPY column \"apiV2Token\"", d: PG, kind: 'I', text: "COPY t (id, \"apiV2Token\") FROM stdin;\n1\tXYZZY_N09\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I10 COPY column \"csrfToken\"", d: PG, kind: 'I', text: "COPY t (id, \"csrfToken\") FROM stdin;\n1\tXYZZY_N10\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I11 COPY column \"s3SecretKey\"", d: PG, kind: 'I', text: "COPY t (id, \"s3SecretKey\") FROM stdin;\n1\tXYZZY_N11\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I12 COPY column \"twoFactorSecret\"", d: PG, kind: 'I', text: "COPY t (id, \"twoFactorSecret\") FROM stdin;\n1\tXYZZY_N12\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I13 COPY column \"TOTP_SECRET\"", d: PG, kind: 'I', text: "COPY t (id, \"TOTP_SECRET\") FROM stdin;\n1\tXYZZY_N13\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I14 COPY column \"PasswordHash\"", d: PG, kind: 'I', text: "COPY t (id, \"PasswordHash\") FROM stdin;\n1\tXYZZY_N14\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I15 COPY column \"resetToken\"", d: PG, kind: 'I', text: "COPY t (id, \"resetToken\") FROM stdin;\n1\tXYZZY_N15\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I16 COPY column \"passwordDigest\"", d: PG, kind: 'I', text: "COPY t (id, \"passwordDigest\") FROM stdin;\n1\tXYZZY_N16\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I17 COPY column \"privateKeyPem\"", d: PG, kind: 'I', text: "COPY t (id, \"privateKeyPem\") FROM stdin;\n1\tXYZZY_N17\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I18 COPY column \"webhookSecret\"", d: PG, kind: 'I', text: "COPY t (id, \"webhookSecret\") FROM stdin;\n1\tXYZZY_N18\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I19 COPY column \"securityAnswer\"", d: PG, kind: 'I', text: "COPY t (id, \"securityAnswer\") FROM stdin;\n1\tXYZZY_N19\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I20 COPY column \"backupCodes\"", d: PG, kind: 'I', text: "COPY t (id, \"backupCodes\") FROM stdin;\n1\tXYZZY_N20\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I21 COPY column \"jwt_secret_v2\"", d: PG, kind: 'I', text: "COPY t (id, \"jwt_secret_v2\") FROM stdin;\n1\tXYZZY_N21\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I22 COPY column \"tokenHash\"", d: PG, kind: 'I', text: "COPY t (id, \"tokenHash\") FROM stdin;\n1\tXYZZY_N22\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I23 COPY column \"secretKeyBase\"", d: PG, kind: 'I', text: "COPY t (id, \"secretKeyBase\") FROM stdin;\n1\tXYZZY_N23\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I24 COPY column \"pinCode\"", d: PG, kind: 'O', text: "COPY t (id, \"pinCode\") FROM stdin;\n1\tXYZZY_N24\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I25 COPY column \"sessionId\"", d: PG, kind: 'O', text: "COPY t (id, \"sessionId\") FROM stdin;\n1\tXYZZY_N25\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I26 COPY column \"signingSecret\"", d: PG, kind: 'I', text: "COPY t (id, \"signingSecret\") FROM stdin;\n1\tXYZZY_N26\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I27 COPY column \"recoveryCode\"", d: PG, kind: 'I', text: "COPY t (id, \"recoveryCode\") FROM stdin;\n1\tXYZZY_N27\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I28 COPY column \"totp2Secret\"", d: PG, kind: 'I', text: "COPY t (id, \"totp2Secret\") FROM stdin;\n1\tXYZZY_N28\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I29 COPY column \"github_pat\"", d: PG, kind: 'O', text: "COPY t (id, \"github_pat\") FROM stdin;\n1\tXYZZY_N29\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V01 COPY column \"tokenizerName\"", d: PG, kind: 'V', text: "COPY t (id, \"tokenizerName\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V02 COPY column \"keyboardLayout\"", d: PG, kind: 'V', text: "COPY t (id, \"keyboardLayout\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V03 COPY column \"tokenType\"", d: PG, kind: 'V', text: "COPY t (id, \"tokenType\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V04 COPY column \"tokenCount\"", d: PG, kind: 'V', text: "COPY t (id, \"tokenCount\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V05 COPY column \"keyId\"", d: PG, kind: 'V', text: "COPY t (id, \"keyId\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V06 COPY column \"publicKey\"", d: PG, kind: 'V', text: "COPY t (id, \"publicKey\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V07 COPY column \"primaryKey\"", d: PG, kind: 'V', text: "COPY t (id, \"primaryKey\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V08 COPY column \"sortKey\"", d: PG, kind: 'V', text: "COPY t (id, \"sortKey\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V09 COPY column \"cacheKey\"", d: PG, kind: 'V', text: "COPY t (id, \"cacheKey\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V10 COPY column \"hotkey\"", d: PG, kind: 'V', text: "COPY t (id, \"hotkey\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V11 COPY column \"passengerName\"", d: PG, kind: 'V', text: "COPY t (id, \"passengerName\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V12 COPY column \"passport\"", d: PG, kind: 'V', text: "COPY t (id, \"passport\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V13 COPY column \"bypassCache\"", d: PG, kind: 'V', text: "COPY t (id, \"bypassCache\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V14 COPY column \"compassHeading\"", d: PG, kind: 'V', text: "COPY t (id, \"compassHeading\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V15 COPY column \"secretaryName\"", d: PG, kind: 'V', text: "COPY t (id, \"secretaryName\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V16 COPY column \"tokenizedText\"", d: PG, kind: 'V', text: "COPY t (id, \"tokenizedText\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V17 COPY column \"lastTokenAt\"", d: PG, kind: 'V', text: "COPY t (id, \"lastTokenAt\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V18 COPY column \"hasPassword\"", d: PG, kind: 'V', text: "COPY t (id, \"hasPassword\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V19 COPY column \"passwordRequired\"", d: PG, kind: 'V', text: "COPY t (id, \"passwordRequired\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V20 COPY column \"isSecret\"", d: PG, kind: 'V', text: "COPY t (id, \"isSecret\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V21 COPY column \"tokenExpiresAt\"", d: PG, kind: 'V', text: "COPY t (id, \"tokenExpiresAt\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V22 COPY column \"passwordChangedAt\"", d: PG, kind: 'V', text: "COPY t (id, \"passwordChangedAt\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V23 COPY column \"syntaxToken\"", d: PG, kind: 'V', text: "COPY t (id, \"syntaxToken\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V24 COPY column \"firstToken\"", d: PG, kind: 'V', text: "COPY t (id, \"firstToken\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V25 COPY column \"topSecret\"", d: PG, kind: 'V', text: "COPY t (id, \"topSecret\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V26 COPY column \"passwordless\"", d: PG, kind: 'V', text: "COPY t (id, \"passwordless\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V27 COPY column \"keySecretary\"", d: PG, kind: 'V', text: "COPY t (id, \"keySecretary\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V28 COPY column \"tokenomics\"", d: PG, kind: 'V', text: "COPY t (id, \"tokenomics\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V29 COPY column secretion_rate", d: PG, kind: 'V', text: "COPY t (id, secretion_rate) FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V30 COPY column \"pwdLastSet\"", d: PG, kind: 'V', text: "COPY t (id, \"pwdLastSet\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V31 COPY column \"maxTokens\"", d: PG, kind: 'V', text: "COPY t (id, \"maxTokens\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V32 COPY column \"nextPageToken\"", d: PG, kind: 'V', text: "COPY t (id, \"nextPageToken\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V33 COPY column \"passwordHint\"", d: PG, kind: 'V', text: "COPY t (id, \"passwordHint\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V34 COPY column \"cancellationToken\"", d: PG, kind: 'V', text: "COPY t (id, \"cancellationToken\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V35 COPY column \"lexerToken\"", d: PG, kind: 'V', text: "COPY t (id, \"lexerToken\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V36 COPY column \"tokenStream\"", d: PG, kind: 'V', text: "COPY t (id, \"tokenStream\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V37 COPY column \"recoveryCodeCount\"", d: PG, kind: 'V', text: "COPY t (id, \"recoveryCodeCount\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V38 COPY column \"disasterRecoveryCode\"", d: PG, kind: 'V', text: "COPY t (id, \"disasterRecoveryCode\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V39 COPY column \"trade_secret\"", d: PG, kind: 'V', text: "COPY t (id, \"trade_secret\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V40 COPY column \"secretSanta\"", d: PG, kind: 'V', text: "COPY t (id, \"secretSanta\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V41 COPY column token_type", d: PG, kind: 'V', text: "COPY t (id, token_type) FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V42 COPY column \"monkeyName\"", d: PG, kind: 'V', text: "COPY t (id, \"monkeyName\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I40 INSERT metadata col non-date value", d: PG, kind: 'I', text: "INSERT INTO t (id, token_count) VALUES (1, 'XYZZY_N40')", code: None, msg: None, hide: None },
        F15 { tag: "N-I41 COPY metadata col non-date value", d: PG, kind: 'I', text: "COPY t (id, password_changed_at) FROM stdin;\n1\tXYZZY_N41\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-I42 INSERT camel metadata non-date", d: PG, kind: 'I', text: "INSERT INTO t (id, \"passwordChangedAt\") VALUES (1, 'XYZZY_N42')", code: None, msg: None, hide: None },
        F15 { tag: "N-I43 INSERT multirow metadata", d: PG, kind: 'I', text: "INSERT INTO t (id, password_reset_at) VALUES (1, '2024-01-01'), (2, 'XYZZY_N43')", code: None, msg: None, hide: None },
        F15 { tag: "N-I44 INSERT metadata letters+digits", d: PG, kind: 'I', text: "INSERT INTO t (id, token_version) VALUES (1, 'XYZZY44')", code: None, msg: None, hide: None },
        F15 { tag: "N-I45 metadata col digits/TZ + word", d: PG, kind: 'I', text: "INSERT INTO t (id, password_set_at) VALUES (1, '12:34:56-T-Z-xyzzyhunter')", code: None, msg: None, hide: None },
        F15 { tag: "N-I46 INSERT ... SELECT metadata", d: PG, kind: 'I', text: "INSERT INTO t (id, token_count) SELECT 1, 'XYZZY_N46'", code: None, msg: None, hide: None },
        F15 { tag: "N-I47 UPDATE metadata non-date", d: PG, kind: 'I', text: "UPDATE t SET \"tokenCount\" = 'XYZZY_N47'", code: None, msg: None, hide: None },
        F15 { tag: "N-I48 INSERT ON CONFLICT DO UPDATE metadata", d: PG, kind: 'I', text: "INSERT INTO t (id, n) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET password_changed_at = 'XYZZY_N48'", code: None, msg: None, hide: None },
        F15 { tag: "N-I49 MySQL INSERT metadata", d: MY, kind: 'I', text: "INSERT INTO t (id, token_count) VALUES (1, 'XYZZY_N49')", code: None, msg: None, hide: None },
        F15 { tag: "N-I50 MySQL INSERT SET form", d: MY, kind: 'I', text: "INSERT INTO t SET id = 1, token_count = 'XYZZY_N50'", code: None, msg: None, hide: None },
        F15 { tag: "N-V51 metadata digits (PIN-like) kept", d: PG, kind: 'V', text: "COPY t (id, password_changed_at) FROM stdin;\n1\t483920\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-V52 INSERT flags", d: PG, kind: 'V', text: "INSERT INTO t (id, has_password, password_required) VALUES (1, true, 'f')", code: None, msg: None, hide: None },
        F15 { tag: "N-V53 COPY flags", d: PG, kind: 'V', text: "COPY t (id, has_password, password_required) FROM stdin;\n1\tt\tf\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-O54 COPY password_set flag w/ secret-like value", d: PG, kind: 'O', text: "COPY t (id, password_set) FROM stdin;\n1\tXYZZY_N54\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "N-O55 INSERT use_password value", d: PG, kind: 'O', text: "INSERT INTO t (id, use_password) VALUES (1, 'XYZZY_N55')", code: None, msg: None, hide: None },
        F15 { tag: "N-O56 UPDATE has_password = secret", d: PG, kind: 'O', text: "UPDATE t SET has_password = 'XYZZY_N56'", code: None, msg: None, hide: None },
        F15 { tag: "N-V57 INSERT password_policy strong", d: PG, kind: 'V', text: "INSERT INTO t (id, password_policy) VALUES (1, 'strong')", code: None, msg: None, hide: None },
        F15 { tag: "N-V58 INSERT dates for metadata", d: PG, kind: 'V', text: "INSERT INTO t (id, token_expires_at, password_changed_at) VALUES (1, '2024-01-01 10:00:00+00', NULL)", code: None, msg: None, hide: None },
        F15 { tag: "B-M01 plpy.error double quoted", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ plpy.error(\"xyzzy\") $$", code: None, msg: Some("xyzzy"), hide: None },
        F15 { tag: "B-M02 plpy.error admin", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ plpy.error(\"admin\") $$", code: None, msg: Some("admin"), hide: None },
        F15 { tag: "B-M03 plv8.elog ERROR admin", d: PG, kind: 'M', text: "DO LANGUAGE plv8 $$ plv8.elog(ERROR, \"admin\"); $$", code: None, msg: Some("admin"), hide: None },
        F15 { tag: "B-M04 perl die admin", d: PG, kind: 'M', text: "DO LANGUAGE plperl $$ die \"admin\\n\"; $$", code: None, msg: Some("admin"), hide: None },
        F15 { tag: "B-M05 pltcl elog ERROR admin", d: PG, kind: 'M', text: "DO LANGUAGE pltcl $$ elog ERROR \"admin\" $$", code: None, msg: Some("admin"), hide: None },
        F15 { tag: "B-M06 pltcl error", d: PG, kind: 'M', text: "CREATE FUNCTION f() RETURNS void LANGUAGE pltcl AS $$ error \"admin\" $$", code: None, msg: Some("admin"), hide: None },
        F15 { tag: "B-M07 plv8 throw new Error", d: PG, kind: 'M', text: "DO LANGUAGE plv8 $$ throw new Error('admin'); $$", code: None, msg: Some("Error: admin"), hide: None },
        F15 { tag: "B-M08 plv8 throw template", d: PG, kind: 'M', text: "DO LANGUAGE plv8 $$ throw new Error(`admin`); $$", code: None, msg: Some("Error: admin"), hide: None },
        F15 { tag: "B-M09 python raise Exception", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$\nraise Exception(\"admin\")\n$$", code: None, msg: Some("Exception: admin"), hide: None },
        F15 { tag: "B-M10 triple quoted plpy.error", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ plpy.error(\"\"\"admin\"\"\") $$", code: None, msg: Some("admin"), hide: None },
        F15 { tag: "B-M11 plpy.error f-string", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ plpy.error(f\"admin\") $$", code: None, msg: Some("admin"), hide: None },
        F15 { tag: "B-M12 perl die single", d: PG, kind: 'M', text: "DO LANGUAGE plperl $$ die 'admin'; $$", code: None, msg: Some("admin at line 1."), hide: None },
        F15 { tag: "B-M13 perl croak", d: PG, kind: 'M', text: "DO LANGUAGE plperl $$ use Carp; croak(\"admin\"); $$", code: None, msg: Some("admin at line 1."), hide: None },
        F15 { tag: "B-M14 tcl return -code error", d: PG, kind: 'M', text: "DO LANGUAGE pltcl $$ return -code error \"admin\" $$", code: None, msg: Some("admin"), hide: None },
        F15 { tag: "B-M15 plv8 throw plain string", d: PG, kind: 'M', text: "DO LANGUAGE plv8 $$ throw \"admin\"; $$", code: None, msg: Some("admin"), hide: None },
        F15 { tag: "B-M16 python assert", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$\nassert False, \"admin\"\n$$", code: None, msg: Some("AssertionError: admin"), hide: None },
        F15 { tag: "B-M17 python division literal non-raising", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ x = \"division\"; y = 1/0 $$", code: None, msg: Some("ZeroDivisionError: division by zero"), hide: None },
        F15 { tag: "B-M18 plv8 string vs template word", d: PG, kind: 'M', text: "DO LANGUAGE plv8 $$ var s = \"deadlock\"; plv8.execute('SELECT 1/0'); $$", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-M19 SQL body double-quoted ident", d: PG, kind: 'M', text: "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT \"deadlock\" FROM t $$", code: None, msg: Some("column \"deadlock\" does not exist"), hide: None },
        F15 { tag: "B-M20 'die now' + template word", d: PG, kind: 'M', text: "SELECT 'die now', 'division' / 0", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-M21 'throw away' + template", d: PG, kind: 'M', text: "SELECT 'throw away', 'division' / 0", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-M22 pltcl elog NOTICE then error elsewhere", d: PG, kind: 'M', text: "DO LANGUAGE pltcl $$ elog NOTICE \"deadlock\"; spi_exec \"SELECT 1/0\" $$", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-M23 tcl set error var", d: PG, kind: 'M', text: "DO LANGUAGE pltcl $$ set error \"deadlock\"; spi_exec \"SELECT 1/0\" $$", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-M24 perl die in comment only", d: PG, kind: 'M', text: "DO LANGUAGE plperl $$ # never die here\nmy $x = \"deadlock\"; spi_exec_query('SELECT 1/0'); $$", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-M25 js throw in comment only", d: PG, kind: 'M', text: "DO LANGUAGE plv8 $$ // throw later\nvar x = \"deadlock\"; plv8.execute('SELECT 1/0'); $$", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-M26 python 'raise' in string only", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ x = \"raise it\"; y = \"deadlock\"; plpy.execute('SELECT 1/0') $$", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-M27 plpgsql var named error", d: PG, kind: 'M', text: "DO $$ DECLARE\n error text := 'deadlock';\nBEGIN PERFORM 1/0; END $$", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-M28 python error = assignment", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$\nerror = \"deadlock\"\nplpy.execute('SELECT 1/0')\n$$", code: None, msg: Some("division by zero"), hide: None },
        F15 { tag: "B-L01 xyzzy plpy.error double", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ plpy.error(\"xyzzyval\") $$", code: None, msg: Some("xyzzyval"), hide: Some("xyzzy") },
        F15 { tag: "B-L02 plv8 template ${} echoed", d: PG, kind: 'M', text: "DO LANGUAGE plv8 $$ var a = `xyzzy${1}tail`; throw new Error(a); $$", code: None, msg: Some("Error: xyzzy1tail"), hide: Some("xyzzy") },
        F15 { tag: "B-L03 plperl q{} echoed", d: PG, kind: 'M', text: "DO LANGUAGE plperl $$ die q{xyzzyq}; $$", code: None, msg: Some("xyzzyq at line 1."), hide: Some("xyzzy") },
        F15 { tag: "B-L04 plpython raw string", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ plpy.error(r\"xyzzyraw\") $$", code: None, msg: Some("xyzzyraw"), hide: Some("xyzzy") },
        F15 { tag: "B-L05 python escape decoded", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ plpy.error(\"xyzzy\\tesc\") $$", code: None, msg: Some("xyzzy\tesc"), hide: Some("xyzzy") },
        F15 { tag: "B-L06 python concat", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ plpy.error(\"xyzzy\" + \"cat\") $$", code: None, msg: Some("xyzzycat"), hide: Some("xyzzy") },
        F15 { tag: "B-L07 tcl braces echoed", d: PG, kind: 'M', text: "DO LANGUAGE pltcl $$ error {xyzzybrace} $$", code: None, msg: Some("xyzzybrace"), hide: Some("xyzzy") },
        F15 { tag: "B-L08 plpython body value in SQL call", d: PG, kind: 'M', text: "DO LANGUAGE plpython3u $$ plpy.execute(\"SELECT 'xyzzyval'::int\") $$", code: None, msg: Some("invalid input syntax for type integer: \"xyzzyval\""), hide: Some("xyzzy") },
        F15 { tag: "P-V01 bob@example.com prose", d: PG, kind: 'V', text: "SELECT 1 -- bob@example.com mysql -performance tips", code: None, msg: None, hide: None },
        F15 { tag: "P-V02 ~ prose", d: PG, kind: 'V', text: "SELECT 1 -- ~ mysql -performance is 2x", code: None, msg: None, hide: None },
        F15 { tag: "P-V03 @kim prose", d: PG, kind: 'V', text: "SELECT 1 -- @kim mysql -prefers hash joins", code: None, msg: None, hide: None },
        F15 { tag: "P-V04 * bullet prose", d: PG, kind: 'V', text: "SELECT 1 -- * mysql -performance_schema is on", code: None, msg: None, hide: None },
        F15 { tag: "P-V05 - bullet prose", d: PG, kind: 'V', text: "SELECT 1 -- - mysql -performance_schema is on", code: None, msg: None, hide: None },
        F15 { tag: "P-V06 @mention then cron-like", d: PG, kind: 'V', text: "SELECT 1 -- @kim 5 mysql -performance tips", code: None, msg: None, hide: None },
        F15 { tag: "P-I01 user@host:~$ mysqldump -p shaped", d: PG, kind: 'I', text: "SELECT 1 -- kim@db01:~$ mysqldump appdb -pXyzzy_S3cret", code: None, msg: None, hide: None },
        F15 { tag: "P-I02 user@host mysql -uroot -p lowercase", d: PG, kind: 'I', text: "SELECT 1 -- kim@db01 mysql -uroot -pxyzzysecretword", code: None, msg: None, hide: None },
        F15 { tag: "P-I03 ~ mysql -h db -p", d: PG, kind: 'I', text: "SELECT 1 -- ~ mysql -h db -pxyzzysecretword", code: None, msg: None, hide: None },
        F15 { tag: "P-I04 @daily mysqldump -p", d: PG, kind: 'I', text: "SELECT 1 -- @daily mysqldump appdb -pxyzzysecretword > /b.sql", code: None, msg: None, hide: None },
        F15 { tag: "P-O05 @mention mysqldump -p lowercase alone", d: PG, kind: 'O', text: "SELECT 1 -- @kim mysqldump -pxyzzysecretword", code: None, msg: None, hide: None },
        F15 { tag: "P-O06 - mysql -p lowercase alone", d: PG, kind: 'O', text: "SELECT 1 -- - mysql -pxyzzysecretword", code: None, msg: None, hide: None },
        F15 { tag: "P-I07 cron 0 3 * * *", d: PG, kind: 'I', text: "SELECT 1 -- 0 3 * * * mysqldump appdb -pxyzzysecretword", code: None, msg: None, hide: None },
        F15 { tag: "P-I08 $ prompt", d: PG, kind: 'I', text: "SELECT 1 -- $ mysql -pxyzzysecretword", code: None, msg: None, hide: None },
        F15 { tag: "P-I09 @reboot cron", d: PG, kind: 'I', text: "SELECT 1 -- @reboot mysqldump appdb -pxyzzysecretword", code: None, msg: None, hide: None },
        F15 { tag: "L-01 DELIMITER '|' second field", d: PG, kind: 'M', text: "COPY t (id, code, note) FROM stdin WITH (DELIMITER '|');\n1|xyzzysecond|n\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzysecond\""), hide: Some("xyzzy") },
        F15 { tag: "L-02 CSV ';' second field", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin WITH (FORMAT csv, DELIMITER ';');\n1;xyzzysemi\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzysemi\""), hide: Some("xyzzy") },
        F15 { tag: "L-03 CSV doubled quotes", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin CSV;\n1,\"say \"\"xyzzyq\"\"\"\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"say \"xyzzyq\"\""), hide: Some("xyzzy") },
        F15 { tag: "L-04 escaped delimiter", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin WITH (DELIMITER '|');\n1|ab\\|xyzzypipe\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"ab|xyzzypipe\""), hide: Some("xyzzy") },
        F15 { tag: "L-05 USING DELIMITERS '|'", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin USING DELIMITERS '|';\n1|xyzzyusing\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzyusing\""), hide: Some("xyzzy") },
        F15 { tag: "L-06 U&'\\007C'", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin WITH (DELIMITER U&'\\007C');\n1|xyzzyuni\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzyuni\""), hide: Some("xyzzy") },
        F15 { tag: "L-07 \\x escape", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin;\n1\t\\x78yzzyhex\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzyhex\""), hide: Some("xyzzy") },
        F15 { tag: "L-08 octal escape", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin;\n1\t\\170yzzyoct\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzyoct\""), hide: Some("xyzzy") },
        F15 { tag: "L-09 CSV ESCAPE '\\'", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin WITH (FORMAT csv, ESCAPE '\\');\n1,\"a\\\"xyzzyesc\"\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"a\"xyzzyesc\""), hide: Some("xyzzy") },
        F15 { tag: "L-10 HEADER MATCH header value", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin WITH (FORMAT csv, HEADER MATCH);\nid,xyzzyhdr\n\\.\n", code: Some("22001"), msg: Some("column name mismatch in header line field 2: got \"xyzzyhdr\", expected \"code\""), hide: Some("xyzzy") },
        F15 { tag: "L-11 \\copy delimiter '|'", d: PG, kind: 'M', text: "\\copy t (id, code) from stdin with delimiter '|'\n1|xyzzymeta\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzymeta\""), hide: Some("xyzzy") },
        F15 { tag: "L-12 meta then COPY '|'", d: PG, kind: 'M', text: "\\connect app\nCOPY t (id, code) FROM stdin WITH (DELIMITER '|');\n1|xyzzyafter\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzyafter\""), hide: Some("xyzzy") },
        F15 { tag: "L-13 CRLF", d: PG, kind: 'M', text: "COPY t (id, code) FROM stdin;\r\n1\txyzzycr\r\n\\.\r\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzycr\""), hide: Some("xyzzy") },
        F15 { tag: "L-14 meta unmatched quote then COPY", d: PG, kind: 'M', text: "\\echo it's\nCOPY t (id, code) FROM stdin;\n1\txyzzyq2\n\\.\n", code: Some("22001"), msg: Some("value too long for type character varying(5): \"xyzzyq2\""), hide: Some("xyzzy") },
    ]
}

fn audit_marker(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("xyzzy") || lower.contains("secretword")
}

/// Records `case` at `level` and returns the stored statement and message.
fn record15(
    recorder: &Recorder,
    case: &F15<'_>,
    connection: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let builder = match case.msg {
        Some(message) => ActivityEvent::query_failed(
            case.text,
            case.d,
            "x",
            &ErrorDetails {
                code: Some(case.code.unwrap_or("XX000").to_string()),
                message: message.to_string(),
            },
            1,
        ),
        None => ActivityEvent::query_executed(case.text, case.d, "x", Default::default(), 1),
    };
    recorder.record(builder.connection(connection).build()?)?;
    let event = recorder.query_history(connection, 1)?.remove(0);
    let text = |v: Option<&serde_json::Value>| {
        v.and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok((
        text(event.details.get("statement")),
        text(event.details.get("error").and_then(|e| e.get("message"))),
    ))
}

#[test]
fn no_secret_of_the_fifteenth_audit_reaches_the_log() -> TestResult {
    let all = fifteenth_cases();
    assert_eq!(all.len(), 255);
    let found = audit_findings(
        &all,
        &FIFTEENTH_AUDIT_METADATA,
        &FIFTEENTH_AUDIT_MESSAGE_LIMITS,
        &[],
    )?;
    assert!(
        found.is_empty(),
        "{} findings:\n{}",
        found.len(),
        found.join("\n")
    );
    Ok(())
}

/// Records every case at every level and returns what goes wrong: a
/// secret kept (`I` at any level, `O` below `full`), a `[redacted]` marker
/// at `normalized`, a hidden message part, valid text changed at `full`
/// (except `metadata`), or a second `full` pass that changes the text.
/// `message_limits` and `accepted` name cases whose message or `full`
/// statement may keep the marker.
fn audit_findings(
    all: &[F15<'_>],
    metadata: &[&str],
    message_limits: &[&str],
    accepted: &[&str],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("audit"));
    let mut found = Vec::new();
    for (i, case) in all.iter().enumerate() {
        let id = case.tag.split(' ').next().unwrap_or_default();
        for level in [TextLevel::None, TextLevel::Normalized, TextLevel::Full] {
            recorder.set_text_level(level);
            let (statement, message) = record15(&recorder, case, &format!("f{i}-{level}"))?;
            let checked = match case.kind {
                'I' => level != TextLevel::Full || !accepted.contains(&id),
                'O' => level != TextLevel::Full,
                _ => false,
            };
            if checked && audit_marker(&statement) {
                found.push(format!("{} at {level}: {statement:?}", case.tag));
            }
            if level == TextLevel::Normalized && statement.contains("[redacted]") {
                found.push(format!("{} at {level}: marker in {statement:?}", case.tag));
            }
            if let Some(hide) = case.hide
                && level == TextLevel::Normalized
                && message.to_lowercase().contains(hide)
                && !message_limits.contains(&id)
            {
                found.push(format!("{} message at {level}: {message:?}", case.tag));
            }
        }
        let full = prepare_statement_text(case.text, case.d, TextLevel::Full).unwrap_or_default();
        if case.kind == 'V' && full != case.text && !metadata.contains(&id) {
            found.push(format!("{} changed at full: {full:?}", case.tag));
        }
        let twice = prepare_statement_text(&full, case.d, TextLevel::Full).unwrap_or_default();
        if twice != full {
            found.push(format!(
                "{} not idempotent: {full:?} then {twice:?}",
                case.tag
            ));
        }
    }
    Ok(found)
}

/// Error messages of the fifteenth audit at `normalized`: what a routine
/// raises itself (`croak`, Tcl `return -code error`, Python `assert`) is
/// hidden, and a server message beside a body whose words only look like
/// a raise (`DECLARE error text`, `error = "deadlock"`) keeps its template
/// words.
#[test]
fn fifteenth_audit_messages() -> TestResult {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("audit"));
    recorder.set_text_level(TextLevel::Normalized);
    let mut wrong = Vec::new();
    for (i, case) in fifteenth_cases().iter().enumerate() {
        let (Some(msg), None) = (case.msg, case.hide) else {
            continue;
        };
        let (_, message) = record15(&recorder, case, &format!("m{i}"))?;
        let lower = msg.to_lowercase();
        let expected = if lower.contains("admin") || lower.contains("xyzzy") {
            // Raised by the routine: its words are hidden.
            !message.to_lowercase().contains("admin") && !audit_marker(&message)
        } else if msg.contains('"') {
            // Quoted parts of a server message are hidden at normalized.
            !message.contains('"') && message.contains('?')
        } else {
            message == msg
        };
        if !expected {
            wrong.push(format!("{}: {msg:?} stored as {message:?}", case.tag));
        }
    }
    // The over-hiding the audit found: a template word next to a variable
    // or an assignment named `error`.
    for (text, msg) in [
        (
            "DO $$ DECLARE\n error text := 'deadlock';\nBEGIN PERFORM 1/0; END $$",
            "deadlock detected",
        ),
        (
            "DO LANGUAGE plpython3u $$\nerror = \"deadlock\"\nplpy.execute('SELECT 1/0')\n$$",
            "deadlock detected",
        ),
    ] {
        let case = F15 {
            tag: "over-hidden template word",
            d: PG,
            kind: 'M',
            text,
            code: Some("40P01"),
            msg: Some(msg),
            hide: None,
        };
        let (_, message) = record15(&recorder, &case, &format!("w{}", text.len()))?;
        if message != msg {
            wrong.push(format!("{text:?}: {msg:?} stored as {message:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    Ok(())
}

/// Fifteenth audit, by finding: each input at every level and through the
/// recorder, with no audit marker kept anywhere.
fn assert_fifteenth_hidden(finding: &str, texts: &[&str]) -> TestResult {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("audit"));
    let mut found = Vec::new();
    for (i, text) in texts.iter().enumerate() {
        let case = F15 {
            tag: finding,
            d: PG,
            kind: 'I',
            text,
            code: Some("XX000"),
            msg: Some(text),
            hide: None,
        };
        for level in [TextLevel::Normalized, TextLevel::Full] {
            let out = prepare_statement_text(text, PG, level).unwrap_or_default();
            if audit_marker(&out) {
                found.push(format!("{finding} {text:?} at {level}: {out:?}"));
            }
            recorder.set_text_level(level);
            let (statement, _) = record15(&recorder, &case, &format!("{finding}-{i}-{level}"))?;
            if audit_marker(&statement) {
                found.push(format!(
                    "{finding} {text:?} recorded at {level}: {statement:?}"
                ));
            }
        }
        let full = prepare_statement_text(text, PG, TextLevel::Full).unwrap_or_default();
        let twice = prepare_statement_text(&full, PG, TextLevel::Full).unwrap_or_default();
        if twice != full {
            found.push(format!(
                "{finding} {text:?} not idempotent: {full:?} then {twice:?}"
            ));
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
    Ok(())
}

/// High A: a multi-byte character right after a meta-command backslash
/// (`\c 日本db`, `\ir ébauche.sql`, a line after `\.`) panicked in the
/// `\copy` check. The text is read, recorded, and kept as written.
#[test]
fn non_ascii_after_a_meta_command_backslash_does_not_panic() -> TestResult {
    let texts = [
        "\\c 日本db\nSELECT 1;\n",
        "\\ir ébauche.sql",
        "\\é",
        "\\日本",
        "SELECT 1 \\gset é\n",
        "COPY t (id, note) FROM stdin;\n1\tx\n\\.\nﬀ€ note\n",
        "COPY t (id, note) FROM stdin;\n1\tx\n\\.\n\\日本\n",
    ];
    assert_fifteenth_hidden("High A", &texts)?;
    for text in ["\\c 日本db\nSELECT 1;\n", "\\ir ébauche.sql"] {
        let full = prepare_statement_text(text, PG, TextLevel::Full).unwrap_or_default();
        assert_eq!(full, text);
        let _ = redact_secrets(text, PG);
    }
    Ok(())
}

/// High B: a COPY option of more than one byte (`DELIMITER 'é'`, which the
/// server rejects but which is still recorded) cut fields inside a
/// character. Such rows cannot be split: they are removed whole.
#[test]
fn multi_byte_copy_options_remove_the_rows_whole() -> TestResult {
    let texts = [
        "COPY t (id, password) FROM stdin WITH (DELIMITER 'é');\n1éXYZZY\n\\.\n",
        "COPY t (id, password) FROM stdin WITH (FORMAT csv, QUOTE 'ü');\n1,üXYZZY,aü\n\\.\n",
        "COPY t (id, password) FROM stdin WITH (FORMAT csv, ESCAPE '日');\n1,\"日\"XYZZY\"\n\\.\n",
        "COPY t (id, password) FROM stdin WITH (DELIMITER '||');\n1||XYZZY\n\\.\n",
        "COPY t (id, password) FROM stdin WITH (DELIMITER '\\');\n1\\XYZZY_O18\n\\.\n",
    ];
    assert_fifteenth_hidden("High B", &texts)?;
    let full = prepare_statement_text(texts[0], PG, TextLevel::Full).unwrap_or_default();
    assert_eq!(
        full,
        "COPY t (id, password) FROM stdin WITH (DELIMITER 'é');\n[redacted]\n\\.\n"
    );
    Ok(())
}

/// Medium C: a meta-command line with an unbalanced quote, `$$`, `/*` or
/// `"` ends at its line break, so later COPY blocks are still data
/// (A-I07, A-I09 to A-I12).
#[test]
fn meta_command_lines_end_at_their_line_break() -> TestResult {
    let users = "COPY public.users (id, email, password_hash, api_token) FROM stdin;\n1\tb@x\tXYZZYHASH\tXYZZYTOK\n\\.\n";
    let texts: Vec<String> = [
        "\\echo Loading user's table\n",
        "\\set body $$\n",
        "\\echo /* start\n",
        "\\echo say \"hi\n",
        "\\echo it's\n",
    ]
    .iter()
    .map(|meta| format!("{meta}{users}COPY t (id, password) FROM stdin;\n1\tXYZZY_b\n\\.\n"))
    .collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    assert_fifteenth_hidden("Medium C", &refs)?;
    let full = prepare_statement_text(&texts[0], PG, TextLevel::Full).unwrap_or_default();
    assert!(
        full.starts_with("\\echo Loading user's table\nCOPY public.users"),
        "{full}"
    );
    Ok(())
}

/// Medium D: a stray `"` in CSV data (the usual reason such a COPY fails)
/// opened a field that ran to the `\.` line, keeping every later secret
/// field (F-I07, F-I08).
#[test]
fn a_stray_csv_quote_hides_the_rest_of_the_block() -> TestResult {
    assert_fifteenth_hidden(
        "Medium D",
        &[
            "COPY t (note, password) FROM stdin CSV;\n5\" screen,XYZZY_F7a\n2,XYZZY_F7b\n\\.\n",
            "COPY t (note, password) FROM stdin CSV;\n\"open,XYZZY_F8a\n2,XYZZY_F8b\n\\.\n",
            "COPY t (note, password) FROM stdin CSV;\n5\" screen,XYZZY_F7a\n2,XYZZY_F7b\n",
            "COPY t (note, password) FROM stdin WITH (FORMAT csv, QUOTE '|');\n|open,XYZZY_q\n2,XYZZY_r\n\\.\n",
        ],
    )
}

/// Low E: `\gset` ends the query (A-I17), `\!` command lines and unquoted
/// `\set` secrets are redacted (A-I19, A-I14), a meta line inside a COPY
/// column list leaves the COPY open (A-I26), and a column named `copy` at
/// the start of a line does not split an INSERT (C-I03, C-I08, C-I09).
#[test]
fn meta_commands_and_copy_words_do_not_split_statements() -> TestResult {
    assert_fifteenth_hidden(
        "Low E",
        &[
            "SELECT 1 AS x \\gset\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_A17');\n",
            "\\! mysqldump appdb -uroot -pXYZZY_A19 > x.sql\nSELECT 1;\n",
            "\\set pw XYZZY_A14secret\nALTER ROLE app PASSWORD :'pw';\n",
            "COPY users (id,\n\\x\npassword) FROM stdin;\n1\tXYZZY_A26\n\\.\n",
            "INSERT INTO users (name,\ncopy, password) VALUES ('a', 'b', 'XYZZY_C3');\n",
            "INSERT INTO users (name, password)\ncopy VALUES ('a', 'XYZZY_C8');\n",
            "INSERT INTO users (id,\ncopy,\npassword) VALUES (1, 'b', 'XYZZY_C9');\n",
            "INSERT INTO users (id, password) OVERRIDING SYSTEM VALUE VALUES (1, 'XYZZY_os');\n",
            "SELECT 1\nCOPY t (id, password) FROM stdin;\n1\tXYZZY_split\n\\.\n",
            "SELECT 1\nCOPY BINARY public.\"t\" (id, password) FROM stdin;\n1\tXYZZY_bin\n\\.\n",
        ],
    )?;
    // `\x` keeps the buffer, `\g` sends it: the INSERT after it is read
    // as a statement of its own.
    let full = prepare_statement_text(
        "SELECT 1 \\g\nINSERT INTO users (name, password) VALUES ('a', 'XYZZY');\n",
        PG,
        TextLevel::Full,
    )
    .unwrap_or_default();
    assert!(!audit_marker(&full), "{full}");
    Ok(())
}

/// Low F: doubled CSV quotes are decoded before a field is checked
/// (F-I29), `\.` after a field ending in a backslash or inside an open
/// quoted field still ends the data (F-I15, F-I16), a delimiter given with
/// `UESCAPE` is read (O-I06), and `INSERT ... SELECT` into a metadata
/// column keeps only a date or number (N-I46).
#[test]
fn copy_fields_are_decoded_before_they_are_checked() -> TestResult {
    assert_fifteenth_hidden(
        "Low F",
        &[
            "COPY t (id, cfg) FROM stdin WITH (FORMAT csv, DELIMITER ';');\n1;\"{\"\"password\"\": \"\"XYZZY_F29\"\"}\"\n\\.\n",
            "COPY t (id, password) FROM stdin;\n1\tXYZZY_F15\\\n\\.\nINSERT INTO users (name, password) VALUES ('b', 'XYZZY_F15b');\n",
            "COPY t (id, password) FROM stdin CSV;\n1,\"XYZZY_F16\n\\.\nSELECT 1;\n",
            "COPY t (id, password) FROM stdin WITH (DELIMITER U&'!007C' UESCAPE '!');\n1|XYZZY_O6\n\\.\n",
            "INSERT INTO t (id, token_count) SELECT 1, 'XYZZY_N46'",
        ],
    )?;
    // A delimiter given with `UESCAPE` is decoded: the rows are split by
    // it and kept, not removed whole.
    let text = "COPY t (id, note) FROM stdin WITH (DELIMITER U&'!007C' UESCAPE '!');\n1|plain words\n\\.\n";
    assert_eq!(
        prepare_statement_text(text, PG, TextLevel::Full).unwrap_or_default(),
        text
    );
    // The data after `\.` is SQL again, kept as written.
    let full = prepare_statement_text(
        "COPY t (id, password) FROM stdin CSV;\n1,\"XYZZY\n\\.\nSELECT 1;\n",
        PG,
        TextLevel::Full,
    )
    .unwrap_or_default();
    assert!(full.ends_with("\\.\nSELECT 1;\n"), "{full}");
    Ok(())
}

/// Low G: column names with digits, versions and encodings are secret
/// (`oauth2Token`, `jwt_secret_v2`, `privateKeyPem`); words that only
/// contain a secret word are not (`passport`, `syntaxToken`,
/// `passwordless`).
#[test]
fn secret_column_names_with_digits_and_look_alikes() -> TestResult {
    let copy =
        |name: &str, value: &str| format!("COPY t (id, \"{name}\") FROM stdin;\n1\t{value}\n\\.\n");
    let secret: Vec<String> = [
        "oauth2Token",
        "apiV2Token",
        "totp2Secret",
        "jwt_secret_v2",
        "privateKeyPem",
        "password",
        "apiToken",
        "rememberToken",
        "client_secret",
        "recoveryCodes",
        "secretKeyBase",
    ]
    .iter()
    .map(|name| copy(name, "XYZZY_name"))
    .collect();
    let refs: Vec<&str> = secret.iter().map(String::as_str).collect();
    assert_fifteenth_hidden("Low G", &refs)?;
    for name in [
        "passport",
        "syntaxToken",
        "cancellationToken",
        "topSecret",
        "passwordless",
        "disasterRecoveryCode",
        "firstToken",
        "lexerToken",
        "trade_secret",
    ] {
        let text = copy(name, "ordinary value 7");
        let full = prepare_statement_text(&text, PG, TextLevel::Full).unwrap_or_default();
        assert_eq!(full, text, "{name}");
    }
    Ok(())
}

// ---- Sixteenth audit ----------------------------------------------------

/// Statements psql would not run, kept at `full`: a `COPY` head without
/// its `;` and rows after it (psql waits for the `;`, so the rows are
/// SQL text to it too).
const SIXTEENTH_AUDIT_ACCEPTED: [&str; 1] = ["R16-C12"];

/// Metadata columns given a value that is no date, number or policy word:
/// redacted by design.
const SIXTEENTH_AUDIT_METADATA: [&str; 0] = [];

/// The sixteenth audit's 106 new inputs (the other 255 are the fifteenth
/// audit's).
#[rustfmt::skip]
fn sixteenth_cases() -> Vec<F15<'static>> {
    vec![
        F15 { tag: "R16-S01 INSERT col list split by \\echo", d: PG, kind: 'I', text: "INSERT INTO users (name,\n\\echo x\npassword) VALUES ('a', 'XYZZY_S01');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S02 INSERT VALUES split by \\echo before secret", d: PG, kind: 'I', text: "INSERT INTO users (name, password) VALUES ('a',\n\\echo x\n'XYZZY_S02');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S03 UPDATE SET split", d: PG, kind: 'I', text: "UPDATE users SET\n\\echo x\npassword = 'XYZZY_S03';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S04 UPDATE SET key then value split", d: PG, kind: 'I', text: "UPDATE users SET password =\n\\echo x\n'XYZZY_S04';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S05 ALTER ROLE ... PASSWORD split", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_S05';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S06 CREATE USER IDENTIFIED BY split", d: PG, kind: 'I', text: "CREATE USER app\n\\echo hi\nIDENTIFIED BY 'XYZZY_S06';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S07 ALTER ROLE app split before WITH", d: PG, kind: 'I', text: "ALTER ROLE app\n\\x\nWITH LOGIN PASSWORD 'XYZZY_S07';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S08 COPY col list split then data", d: PG, kind: 'I', text: "COPY users (id,\n\\x\npassword) FROM stdin;\n1\tXYZZY_S08\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S09 INSERT multi-row with meta between rows", d: PG, kind: 'I', text: "INSERT INTO users (name, password) VALUES ('a', 'XYZZY_S09a'),\n\\echo next\n('b', 'XYZZY_S09b');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S10 SET password split", d: PG, kind: 'I', text: "SET\n\\x\npassword = 'XYZZY_S10';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S11 CREATE ROLE split twice", d: PG, kind: 'I', text: "CREATE ROLE\n\\x\napp\n\\x\nPASSWORD 'XYZZY_S11';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S12 SELECT set_config split", d: PG, kind: 'I', text: "SELECT set_config('app.password',\n\\echo x\n'XYZZY_S12', false);\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S13 INSERT secret col then \\r (buffer reset) then INSERT", d: PG, kind: 'I', text: "INSERT INTO users (name, password) VALUES ('a',\n\\r\nINSERT INTO users (name, password) VALUES ('b', 'XYZZY_S13');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S14 split by \\gset mid-statement? (ends it)", d: PG, kind: 'I', text: "INSERT INTO users (name, password)\n\\gset\nVALUES ('b', 'XYZZY_S14');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S15 dollar body split by meta line? (inside body: no)", d: PG, kind: 'I', text: "DO $$ BEGIN\n\\echo x\nALTER ROLE app PASSWORD 'XYZZY_S15'; END $$;\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S16 \\setenv PGPASSWORD", d: PG, kind: 'I', text: "\\setenv PGPASSWORD XYZZY_S16secret\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S17 \\set PGPASSWORD", d: PG, kind: 'I', text: "\\set PGPASSWORD XYZZY_S17secret\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S18 \\set password quoted", d: PG, kind: 'I', text: "\\set password 'XYZZY_S18'\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S19 \\set db_password unquoted", d: PG, kind: 'I', text: "\\set db_password XYZZY_S19secret\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S20 \\set pass (short name)", d: PG, kind: 'O', text: "\\set pass xyzzyS20secret\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S21 \\! export PGPASSWORD=", d: PG, kind: 'I', text: "\\! export PGPASSWORD=XYZZY_S21 && psql\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S22 \\! psql password conninfo", d: PG, kind: 'I', text: "\\! psql \"host=db password=XYZZY_S22\"\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S23 \\o pipe with mysql -p", d: PG, kind: 'I', text: "\\o | mysql -uroot -pXYZZY_S23 appdb\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S24 \\c with -reuse-previous and password", d: PG, kind: 'I', text: "\\c -reuse-previous=on \"dbname=app password=XYZZY_S24\"\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S25 \\connect URL", d: PG, kind: 'I', text: "\\connect postgres://u:XYZZY_S25@h/db\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S26 \\prompt then ALTER", d: PG, kind: 'I', text: "\\prompt 'Password: ' pw\nALTER ROLE app PASSWORD 'XYZZY_S26';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S27 \\set then \\\\ then ALTER on same line", d: PG, kind: 'I', text: "\\set x 1 \\\\ ALTER ROLE app PASSWORD 'XYZZY_S27'\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S28 SELECT \\gset then INSERT", d: PG, kind: 'I', text: "SELECT 1 AS x \\gset\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_S28');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S29 \\set pw with tab", d: PG, kind: 'I', text: "\\set\tpw\tXYZZY_S29secret\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S30 \\SET upper", d: PG, kind: 'O', text: "\\SET pw XYZZY_S30secret\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S31 meta at line middle after ;", d: PG, kind: 'I', text: "SELECT 1; \\set pw XYZZY_S31secret\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S32 \\echo unbalanced then COPY", d: PG, kind: 'I', text: "\\echo Loading user's table\nCOPY t (id, password) FROM stdin;\n1\tXYZZY_S32\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S33 \\echo $$ then COPY", d: PG, kind: 'I', text: "\\echo $$\nCOPY t (id, password) FROM stdin;\n1\tXYZZY_S33\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S34 \\set x E'\\' then ALTER", d: PG, kind: 'I', text: "\\set x E'abc\\'\nALTER ROLE app PASSWORD 'XYZZY_S34';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S35 \\copy with password column and \\. ", d: PG, kind: 'I', text: "\\copy users (id, password) from stdin csv\n1,XYZZY_S35\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S36 \\copy … stdin then INSERT after \\.", d: PG, kind: 'I', text: "\\copy users (id, password) from stdin\n1\tXYZZY_S36a\n\\.\nINSERT INTO users (name, password) VALUES ('b', 'XYZZY_S36b');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S37 \\ then CRLF", d: PG, kind: 'I', text: "\\c app\r\nINSERT INTO users (name, password) VALUES ('b', 'XYZZY_S37');\r\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S38 \\ at end of text", d: PG, kind: 'I', text: "INSERT INTO users (name, password) VALUES ('b', 'XYZZY_S38'); \\", code: None, msg: None, hide: None },
        F15 { tag: "R16-S39 backslash after literal glued", d: PG, kind: 'I', text: "INSERT INTO users (name, password) VALUES ('b', 'XYZZY_S39')\\g\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-S40 \\set pw then var used", d: PG, kind: 'I', text: "\\set pw 'XYZZY_S40'\nALTER ROLE app PASSWORD :'pw';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-V01 \\echo then SELECT kept", d: PG, kind: 'V', text: "\\echo hello world\nSELECT 'it''s';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-V02 \\x inside SELECT list kept", d: PG, kind: 'V', text: "SELECT a,\n\\x\nb FROM t;\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-V03 \\set ON_ERROR_STOP on", d: PG, kind: 'V', text: "\\set ON_ERROR_STOP on\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-V04 \\c 日本db", d: PG, kind: 'V', text: "\\c 日本db\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-V05 \\.\\nﬀ", d: PG, kind: 'V', text: "SELECT 1;\n\\.\nﬀ€ x\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-V06 \\set password_min_length 12", d: PG, kind: 'V', text: "\\set password_min_length 12\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C01 copy column at line start in INSERT", d: PG, kind: 'I', text: "INSERT INTO users (name,\ncopy, password) VALUES ('a', 'b', 'XYZZY_C01');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C02 copy before VALUES", d: PG, kind: 'I', text: "INSERT INTO users (name, password)\ncopy VALUES ('a', 'XYZZY_C02');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C03 COPY after SELECT no ;", d: PG, kind: 'I', text: "SELECT now()\nCOPY t (id, password) FROM stdin;\n1\tXYZZY_C03\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C04 COPY schema.\"T\" (cols) after no ;", d: PG, kind: 'I', text: "SELECT 1\nCOPY public.\"Users\" (id, \"apiToken\") FROM stdin;\n1\tXYZZY_C04\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C05 COPY BINARY after no ;", d: PG, kind: 'I', text: "SELECT 1\nCOPY BINARY t (id, secret) FROM stdin;\nPGCOPY\nXYZZY_C05\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C06 COPY with comment between", d: PG, kind: 'I', text: "SELECT 1\nCOPY /* c */ t (id, password) FROM stdin;\n1\tXYZZY_C06\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C07 COPY (query) TO after no ;, then INSERT", d: PG, kind: 'I', text: "SELECT 1\nCOPY (SELECT 1) TO STDOUT;\nINSERT INTO users (name, password) VALUES ('b', 'XYZZY_C07');\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C08 COPY t FROM on next line after no ;", d: PG, kind: 'I', text: "SELECT 1\nCOPY t (id, password)\nFROM stdin;\n1\tXYZZY_C08\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C09 copy word line start inside UPDATE SET (depth 0)", d: PG, kind: 'I', text: "UPDATE t SET a = 1,\ncopy = 2, password = 'XYZZY_C09';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C10 copy as table name 'copy t FROM' inside SELECT", d: PG, kind: 'I', text: "SELECT *\nFROM x\nWHERE id IN (SELECT id\ncopy FROM y) AND password = 'XYZZY_C10';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C11 CREATE ROLE then copy line", d: PG, kind: 'I', text: "CREATE ROLE app\ncopy t FROM stdin PASSWORD 'XYZZY_C11';\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-C12 copy line start, unterminated COPY head", d: PG, kind: 'I', text: "SELECT 1\nCOPY t (id, password) FROM stdin\n1\tXYZZY_C12\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U01 U&'!007C' UESCAPE '!'", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER U&'!007C' UESCAPE '!');\n1|XYZZY_U01\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U02 U&'\\+00007C'", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER U&'\\+00007C');\n1|XYZZY_U02\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U03 U&'#007C' UESCAPE '#' CSV", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (FORMAT csv, DELIMITER U&'#007C' UESCAPE '#');\n1|XYZZY_U03\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U04 UESCAPE lower case", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER u&'!007c' uescape '!');\n1|XYZZY_U04\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U05 U& with escaped escape", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER U&'\\\\');\n1\\XYZZY_U05\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U06 U& non-ASCII delimiter (server rejects)", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER U&'\\00E9');\n1éXYZZY_U06\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U07 QUOTE U&'\\0027' CSV", d: PG, kind: 'I', text: "COPY t (id, note, password) FROM stdin WITH (FORMAT csv, QUOTE U&'\\0027');\n1,'a,b',XYZZY_U07\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U08 DELIMITER 'é' (server rejects)", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER 'é');\n1éXYZZY_U08\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U09 QUOTE 'ü' CSV", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (QUOTE 'ü', FORMAT csv);\n1,üXYZZY_U09ü\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U10 ESCAPE 'ß' CSV", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (FORMAT csv, ESCAPE 'ß');\n1,XYZZY_U10\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U11 DELIMITER '' empty", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER '');\n1\tXYZZY_U11\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-U12 DELIMITER 'ab' two chars", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin WITH (DELIMITER 'ab');\n1abXYZZY_U12\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q01 CSV stray quote", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin CSV;\n5\" screen,XYZZY_Q01a\n2,XYZZY_Q01b\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q02 CSV open at field start", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin CSV;\n\"open,XYZZY_Q02a\n2,XYZZY_Q02b\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q03 CSV stray quote, no secret column, URL later", d: PG, kind: 'I', text: "COPY t (note, other) FROM stdin CSV;\n5\" screen,x\n2,postgres://u:XYZZY_Q03@h/db\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q04 CSV JSON doubled quotes ;", d: PG, kind: 'I', text: "COPY t (id, cfg) FROM stdin WITH (FORMAT csv, DELIMITER ';');\n1;\"{\"\"password\"\": \"\"XYZZY_Q04\"\"}\"\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q05 CSV JSON doubled quotes ,", d: PG, kind: 'I', text: "COPY t (id, cfg) FROM stdin CSV;\n1,\"{\"\"password\"\": \"\"XYZZY_Q05\"\"}\"\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q06 CSV ESCAPE \\ JSON", d: PG, kind: 'I', text: "COPY t (id, cfg) FROM stdin WITH (FORMAT csv, ESCAPE '\\');\n1,\"{\\\"password\\\": \\\"XYZZY_Q06\\\"}\"\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q07 secret ending in \\ before \\.", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin;\n1\tXYZZY_Q07\\\n\\.\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q08 CSV quoted secret across \\.", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin CSV;\n1,\"XYZZY_Q08\n\\.\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q09 CSV stray quote, unterminated (no \\.)", d: PG, kind: 'I', text: "COPY t (note, password) FROM stdin CSV;\n5\" screen,XYZZY_Q09a\n2,XYZZY_Q09b\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-Q10 text no secret column; secret-like JSON in text", d: PG, kind: 'I', text: "COPY t (id, cfg) FROM stdin;\n1\t{\"api_key\": \"XYZZY_Q10\"}\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-QV1 CSV stray quote, no secrets (kept?)", d: PG, kind: 'V', text: "COPY t (note, n) FROM stdin CSV;\n5\" screen,1\n2,3\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-QV2 CSV balanced data kept", d: PG, kind: 'V', text: "COPY t (id, note) FROM stdin CSV;\n1,\"a \"\"b\"\" c\"\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-N01 oauth2Token", d: PG, kind: 'I', text: "COPY t (id, \"oauth2Token\") FROM stdin;\n1\tXYZZY_N01\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-N02 apiV2Token", d: PG, kind: 'I', text: "COPY t (id, \"apiV2Token\") FROM stdin;\n1\tXYZZY_N02\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-N03 jwt_secret_v2", d: PG, kind: 'I', text: "COPY t (id, jwt_secret_v2) FROM stdin;\n1\tXYZZY_N03\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-N04 privateKeyPem", d: PG, kind: 'I', text: "COPY t (id, \"privateKeyPem\") FROM stdin;\n1\tXYZZY_N04\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-N05 INSERT SELECT metadata", d: PG, kind: 'I', text: "INSERT INTO t (id, token_count) SELECT 1, 'XYZZY_N05'", code: None, msg: None, hide: None },
        F15 { tag: "R16-N06 k3yZq9wSecret value as name", d: PG, kind: 'O', text: "COPY t (id, \"k3yZq9wSecret\") FROM stdin;\n1\tordinary\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV01 \"passport\"", d: PG, kind: 'V', text: "COPY t (id, \"passport\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV02 \"syntaxToken\"", d: PG, kind: 'V', text: "COPY t (id, \"syntaxToken\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV03 \"firstToken\"", d: PG, kind: 'V', text: "COPY t (id, \"firstToken\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV04 \"lexerToken\"", d: PG, kind: 'V', text: "COPY t (id, \"lexerToken\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV05 \"cancellationToken\"", d: PG, kind: 'V', text: "COPY t (id, \"cancellationToken\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV06 \"topSecret\"", d: PG, kind: 'V', text: "COPY t (id, \"topSecret\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV07 trade_secret", d: PG, kind: 'V', text: "COPY t (id, trade_secret) FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV08 \"passwordless\"", d: PG, kind: 'V', text: "COPY t (id, \"passwordless\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV09 \"disasterRecoveryCode\"", d: PG, kind: 'V', text: "COPY t (id, \"disasterRecoveryCode\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV10 \"utf8Token\"", d: PG, kind: 'V', text: "COPY t (id, \"utf8Token\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV11 \"v2Name\"", d: PG, kind: 'V', text: "COPY t (id, \"v2Name\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV12 \"sha256\"", d: PG, kind: 'V', text: "COPY t (id, \"sha256\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV13 \"md5Sum\"", d: PG, kind: 'V', text: "COPY t (id, \"md5Sum\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV14 \"ipv4Address\"", d: PG, kind: 'V', text: "COPY t (id, \"ipv4Address\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV15 \"s3Bucket\"", d: PG, kind: 'V', text: "COPY t (id, \"s3Bucket\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV16 \"oauth2Provider\"", d: PG, kind: 'V', text: "COPY t (id, \"oauth2Provider\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV17 \"tokenizerName\"", d: PG, kind: 'V', text: "COPY t (id, \"tokenizerName\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R16-NV18 \"keyboardLayout\"", d: PG, kind: 'V', text: "COPY t (id, \"keyboardLayout\") FROM stdin;\n1\tordinary value 7\n\\.\n", code: None, msg: None, hide: None },
    ]
}

#[test]
fn no_secret_of_the_sixteenth_audit_reaches_the_log() -> TestResult {
    let all = sixteenth_cases();
    assert_eq!(all.len(), 106);
    let found = audit_findings(
        &all,
        &SIXTEENTH_AUDIT_METADATA,
        &FIFTEENTH_AUDIT_MESSAGE_LIMITS,
        &SIXTEENTH_AUDIT_ACCEPTED,
    )?;
    assert!(
        found.is_empty(),
        "{} findings:\n{}",
        found.len(),
        found.join("\n")
    );
    Ok(())
}

/// Medium A: a meta-command line inside a statement (`\echo`, `\x`: psql
/// keeps its query buffer) no longer cuts the secret rules off from their
/// context (R16-S01, S02, S04, S05, S09, S12). The meta line itself stays
/// as written.
#[test]
fn a_meta_line_inside_a_statement_keeps_the_secret_rules_whole() -> TestResult {
    let texts = [
        "INSERT INTO users (name,\n\\echo x\npassword) VALUES ('a', 'XYZZY_S01');\n",
        "INSERT INTO users (name, password) VALUES ('a',\n\\echo x\n'XYZZY_S02');\n",
        "UPDATE users SET password =\n\\echo x\n'XYZZY_S04';\n",
        "ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_S05';\n",
        "INSERT INTO users (name, password) VALUES ('a', 'XYZZY_S09a'),\n\\echo next\n('b', 'XYZZY_S09b');\n",
        "SELECT set_config('app.password',\n\\echo x\n'XYZZY_S12', false);\n",
        "ALTER ROLE app WITH PASSWORD\n\\echo it's\n'XYZZY_q';\n",
        "ALTER ROLE app WITH PASSWORD\n\\x\n\\echo /* two\n'XYZZY_two';\nSELECT 'ok';\n",
    ];
    assert_fifteenth_hidden("Medium A", &texts)?;
    let full = prepare_statement_text(texts[3], PG, TextLevel::Full).unwrap_or_default();
    assert_eq!(
        full,
        "ALTER ROLE app WITH PASSWORD\n\\echo x\n'[redacted]';\n"
    );
    let full = prepare_statement_text(texts[7], PG, TextLevel::Full).unwrap_or_default();
    assert!(full.ends_with("'[redacted]';\nSELECT 'ok';\n"), "{full}");
    Ok(())
}

/// Low B: `\gset` alone inside a statement sends the part before it; the
/// rest is still read with the statement's context (R16-S14), and a
/// complete query before `\gset` still leaves the next statement its own
/// (R16-S28).
#[test]
fn a_sending_meta_line_inside_a_statement_keeps_the_context() -> TestResult {
    assert_fifteenth_hidden(
        "Low B",
        &[
            "INSERT INTO users (name, password)\n\\gset\nVALUES ('b', 'XYZZY_S14');\n",
            "SELECT 1 AS x \\gset\nINSERT INTO users (name, password) VALUES ('bob', 'XYZZY_S28');\n",
            "SELECT count(*) AS n FROM t\n\\gset\nALTER ROLE app PASSWORD 'XYZZY_g';\n",
        ],
    )
}

/// Low C: a secret found on a meta-command line never takes its line
/// break, so the next line stays a statement of its own (R16-S26).
#[test]
fn a_meta_line_secret_keeps_its_line_break() -> TestResult {
    let text = "\\prompt 'Password: ' pw\nALTER ROLE app PASSWORD 'XYZZY_S26';\n";
    assert_fifteenth_hidden("Low C", &[text])?;
    let full = prepare_statement_text(text, PG, TextLevel::Full).unwrap_or_default();
    assert!(
        full.ends_with("\nALTER ROLE app PASSWORD '[redacted]';\n"),
        "{full}"
    );
    Ok(())
}

/// Low D: `\set` names are read like column names, in any case, with the
/// short password names psql scripts use (R16-S20, S30); a number in a
/// metadata variable and a `utf8Token` column are kept (R16-V06, NV10).
#[test]
fn set_variables_and_token_columns_are_read_like_names() -> TestResult {
    assert_fifteenth_hidden(
        "Low D",
        &[
            "\\set pass xyzzyS20secret\n",
            "\\SET pw XYZZY_S30secret\n",
            "\\set db_pass xyzzy_db\n",
            "\\set password_min_length xyzzy\n",
        ],
    )?;
    for text in [
        "\\set password_min_length 12\n",
        "\\set passes 3\n",
        "SELECT 1 -- password_min_length 12, password_expires_at 2026-01-01",
        "COPY t (id, \"utf8Token\") FROM stdin;\n1\tordinary value 7\n\\.\n",
    ] {
        let full = prepare_statement_text(text, PG, TextLevel::Full).unwrap_or_default();
        assert_eq!(full, text);
    }
    Ok(())
}

// ---- Seventeenth audit --------------------------------------------------

/// Valid text whose one password literal is removed (the `X` cases check
/// that nothing else is): compared in full by
/// `a_secret_found_through_a_meta_line_is_removed_where_it_was_found`.
const SEVENTEENTH_AUDIT_ONE_SECRET: [&str; 11] = [
    "R17-X01", "R17-X02", "R17-X03", "R17-X04", "R17-X05", "R17-X06", "R17-X07", "R17-X08",
    "R17-X09", "R17-X10", "R17-X11",
];

/// The seventeenth audit's 37 new inputs.
#[rustfmt::skip]
fn seventeenth_cases() -> Vec<F15<'static>> {
    vec![
        F15 { tag: "R17-W01 S05 then broken quote later", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_W01';\nSELECT 'D'Angelo';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W02 S04 then unterminated literal later", d: PG, kind: 'I', text: "UPDATE users SET password =\n\\echo x\n'XYZZY_W02';\nSELECT 'oops\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W03 S01 then unterminated block comment", d: PG, kind: 'I', text: "INSERT INTO users (name,\n\\echo x\npassword) VALUES ('a', 'XYZZY_W03');\n/* never closed\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W04 broken quote BEFORE the split statement", d: PG, kind: 'I', text: "SELECT 'D'Angelo';\nALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_W04';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W05 split stmt, COPY block after", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_W05';\nCOPY t (id, note) FROM stdin;\n1\tit's\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W06 COPY block before split stmt (rows with quote)", d: PG, kind: 'I', text: "COPY t (id, note) FROM stdin;\n1\tit's\n\\.\nALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_W06';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W07 meta inside dollar quote (not a meta)", d: PG, kind: 'I', text: "DO $$ BEGIN\n\\echo x\nALTER ROLE app PASSWORD 'XYZZY_W07'; END $$;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W08 meta-like line inside COPY data", d: PG, kind: 'I', text: "COPY t (id, password) FROM stdin;\n1\tXYZZY_W08a\n\\echo x\n2\tXYZZY_W08b\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W09 two stmts per line, meta between", d: PG, kind: 'I', text: "SELECT 1; ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_W09'; SELECT 2;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W10 meta containing secret inside stmt", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\set pw XYZZY_W10a\n'XYZZY_W10b';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W11 meta with conninfo inside stmt", d: PG, kind: 'I', text: "UPDATE users SET password =\n\\connect \"password=XYZZY_W11a\"\n'XYZZY_W11b';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W12 two metas inside stmt", d: PG, kind: 'I', text: "INSERT INTO users (name,\n\\echo a\npassword)\n\\echo b\nVALUES ('a', 'XYZZY_W12');\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W13 short meta line (len<4) inside stmt", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\x\n'XYZZY_W13';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W14 meta inside then \\g ends", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_W14'\n\\g\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W15 meta inside stmt, CRLF", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\r\n\\echo x\r\n'XYZZY_W15';\r\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W16 meta inside with */ in it", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\echo a */ b\n'XYZZY_W16';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W17 meta inside with /* in it", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\echo /* b\n'XYZZY_W17';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W18 meta inside with unicode", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\echo 日本\n'XYZZY_W18';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W19 meta inside MySQL dialect (no meta)", d: MY, kind: 'I', text: "ALTER USER app IDENTIFIED BY\n\\echo x\n'XYZZY_W19';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W20 meta inside INSERT multirow, many rows", d: PG, kind: 'I', text: "INSERT INTO users (name, password) VALUES ('a', 'XYZZY_W20a'),\n\\echo n\n('b', 'XYZZY_W20b'),\n\\echo n\n('c', 'XYZZY_W20c');\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W21 meta inside, unquoted pw", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\nXyzzyW21Secret;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W22 meta inside CREATE USER IN ROLE", d: PG, kind: 'I', text: "CREATE ROLE app LOGIN\n\\echo x\nPASSWORD 'XYZZY_W22' IN ROLE admins;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W23 meta inside, statement-level comment after", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD -- set it\n\\echo x\n'XYZZY_W23';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W24 meta inside, value split E-string", d: PG, kind: 'I', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\nE'XYZZY\\'W24';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-W25 meta inside set_config", d: PG, kind: 'I', text: "SELECT set_config('app.password',\n\\echo x\n'XYZZY_W25', false);\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X01 secret equal to a table name", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'users';\nSELECT * FROM users;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X02 secret equal to a word in meta line", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo hello\n'hello';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X03 one-char secret 'a'", d: PG, kind: 'V', text: "INSERT INTO users (name,\n\\echo x\npassword) VALUES ('a', 'a');\nSELECT a FROM t;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X04 secret 'x' equal to meta arg", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'x';\nSELECT x FROM t;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X05 secret equal to SQL keyword", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'select';\nSELECT 1;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X06 secret equal to COPY row value", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'bob';\nCOPY t (id, name) FROM stdin;\n1\tbob\n\\.\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X07 secret equal to [redacted]", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'[redacted]';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X08 secret equal to redacted", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'redacted';\nSELECT 'redacted';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X09 secret with regex-ish chars", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'a.b*c';\nSELECT 'a.b*c', 'aXbbc';\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X10 secret = 1 digit, numbers elsewhere", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'1';\nSELECT 1, 11, 1.5;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-X11 secret substring of other word", d: PG, kind: 'V', text: "ALTER ROLE app WITH PASSWORD\n\\echo x\n'pass';\nSELECT password_policy, passport FROM t;\n", code: None, msg: None, hide: None },
        F15 { tag: "R17-G01 GRANT TO GROUP a, b normalized", d: PG, kind: 'V', text: "GRANT SELECT ON t TO GROUP a, b;\n", code: None, msg: None, hide: None },
    ]
}

#[test]
fn no_secret_of_the_seventeenth_audit_reaches_the_log() -> TestResult {
    let all = seventeenth_cases();
    assert_eq!(all.len(), 37);
    let found = audit_findings(&all, &SEVENTEENTH_AUDIT_ONE_SECRET, &[], &[])?;
    assert!(
        found.is_empty(),
        "{} findings:\n{}",
        found.len(),
        found.join("\n")
    );
    Ok(())
}

/// Medium A: broken quoting elsewhere in the script (a later `'D'Angelo'`,
/// an unterminated literal, an unclosed comment) no longer turns the
/// whole-statement reading off. The statement holding the meta line is
/// read up to the break, the rest is hidden, and a second pass changes
/// nothing (R17-W01, W02, W03 and the prefixes of W20).
#[test]
fn a_meta_line_statement_is_read_whole_before_a_later_break() -> TestResult {
    let w20 = "INSERT INTO users (name, password) VALUES ('a', 'XYZZY_W20a'),\n\\echo n\n('b', 'XYZZY_W20b'),\n\\echo n\n('c', 'XYZZY_W20c');\n";
    let mut texts: Vec<String> = vec![
        "ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_W01';\nSELECT 'D'Angelo';\n".into(),
        "UPDATE users SET password =\n\\echo x\n'XYZZY_W02';\nSELECT 'oops\n".into(),
        "INSERT INTO users (name,\n\\echo x\npassword) VALUES ('a', 'XYZZY_W03');\n/* never closed\n".into(),
        "ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_c';\nCOPY t (id, note) FROM stdin;\n1\tit's\n".into(),
    ];
    // Every prefix ending inside a later row: a failed statement cut short.
    texts.extend(
        (1..w20.len())
            .filter(|&n| w20.is_char_boundary(n) && n > w20.find("('b'").unwrap_or(0))
            .map(|n| w20[..n].to_string()),
    );
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    assert_fifteenth_hidden("Medium A", &refs)?;
    let full = prepare_statement_text(&texts[0], PG, TextLevel::Full).unwrap_or_default();
    assert_eq!(
        full,
        "ALTER ROLE app WITH PASSWORD\n\\echo x\n'[redacted]';\n[redacted]\n"
    );
    Ok(())
}

/// Low B: a secret found through a meta line inside a statement is removed
/// where it was found, not as a word everywhere (R17-X01 to X11).
#[test]
fn a_secret_found_through_a_meta_line_is_removed_where_it_was_found() {
    let alter = "ALTER ROLE app WITH PASSWORD\n\\echo x\n";
    for (rest, kept) in [
        (
            "'users';\nSELECT * FROM users;\n",
            "\nSELECT * FROM users;\n",
        ),
        ("'select';\nSELECT 1;\n", "\nSELECT 1;\n"),
        (
            "'bob';\nCOPY t (id, name) FROM stdin;\n1\tbob\n\\.\n",
            "\nCOPY t (id, name) FROM stdin;\n1\tbob\n\\.\n",
        ),
        ("'x';\nSELECT x FROM t;\n", "\nSELECT x FROM t;\n"),
        (
            "'redacted';\nSELECT 'redacted';\n",
            "\nSELECT 'redacted';\n",
        ),
        ("'1';\nSELECT 1, 11, 1.5;\n", "\nSELECT 1, 11, 1.5;\n"),
        (
            "'a.b*c';\nSELECT 'a.b*c', 'aXbbc';\n",
            "\nSELECT 'a.b*c', 'aXbbc';\n",
        ),
        ("'[redacted]';\n", "\n"),
    ] {
        let text = format!("{alter}{rest}");
        let full = prepare_statement_text(&text, PG, TextLevel::Full).unwrap_or_default();
        assert_eq!(full, format!("{alter}'[redacted]';{kept}"), "{text:?}");
        let twice = prepare_statement_text(&full, PG, TextLevel::Full).unwrap_or_default();
        assert_eq!(twice, full);
    }
    let text = "ALTER ROLE app WITH PASSWORD\n\\echo hello\n'hello';\n";
    assert_eq!(
        prepare_statement_text(text, PG, TextLevel::Full).unwrap_or_default(),
        "ALTER ROLE app WITH PASSWORD\n\\echo hello\n'[redacted]';\n"
    );
    let text =
        "INSERT INTO users (name,\n\\echo x\npassword) VALUES ('a', 'a');\nSELECT a FROM t;\n";
    assert_eq!(
        prepare_statement_text(text, PG, TextLevel::Full).unwrap_or_default(),
        "INSERT INTO users (name,\n\\echo x\npassword) VALUES ('a', '[redacted]');\nSELECT a FROM t;\n"
    );
}

// ---- Eighteenth audit ---------------------------------------------------

/// Low B: a query sent by `\gset` before a later broken statement in the
/// same run is no part of the broken one: it stays, the broken statement
/// is hidden (R18-B18).
#[test]
fn a_sent_query_before_a_later_break_is_kept() -> TestResult {
    let text =
        "ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_B18';\nSELECT 1 \\gset\nSELECT 'oops\n";
    assert_fifteenth_hidden("Low B", &[text])?;
    assert_eq!(
        prepare_statement_text(text, PG, TextLevel::Full).unwrap_or_default(),
        "ALTER ROLE app WITH PASSWORD\n\\echo x\n'[redacted]';\nSELECT 1 \\gset\n[redacted]\n"
    );
    // A break before the `\gset` still hides from its statement on.
    let text =
        "ALTER ROLE app WITH PASSWORD\n\\echo x\n'XYZZY_c';\nSELECT 'oops \\gset\nSELECT 2;\n";
    assert_fifteenth_hidden("Low B", &[text])?;
    Ok(())
}
