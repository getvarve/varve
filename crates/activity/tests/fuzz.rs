//! Deterministic fuzzing of the redactor: random mixes of non-ASCII text,
//! psql meta-command lines, COPY blocks with odd delimiters, quotes, `\.`
//! lines, comments and account statements. No input may panic, at any
//! level, through the recorder too, and a second pass at text level
//! `full` must not change the first one's output.

use std::sync::Arc;

use varve_activity::{
    ActivityEvent, ErrorDetails, MAX_REDACTED_BYTES, Recorder, RecorderSettings, SqlDialect,
    TextLevel, prepare_error_message, prepare_statement_text, redact_secrets,
};
use varve_store::{Store, core_migrations};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A small xorshift generator: the same texts on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        let len = u64::try_from(items.len()).unwrap_or(1);
        let index = usize::try_from(self.next() % len).unwrap_or(0);
        items[index]
    }
}

/// Pieces the texts are built from.
const PIECES: &[&str] = &[
    "\\c 日本db\n",
    "\\ir ébauche.sql\n",
    "\\set\u{a0}x 1\n",
    "\\echo Loading user's table\n",
    "\\set body $$\n",
    "\\echo /* start\n",
    "\\echo say \"hi\n",
    "\\set pw Zq9wSecret\n",
    "\\! mysqldump appdb -uroot -pZq9wSecret > x.sql\n",
    "SELECT 1 AS x \\gset\n",
    "\\copy users (id, password) from stdin\n",
    "\\.\n",
    "\\.\nﬀ€\n",
    "COPY t (id, password) FROM stdin;\n",
    "COPY t (id, password) FROM stdin WITH (DELIMITER 'é');\n",
    "COPY t (id, password) FROM stdin WITH (DELIMITER '|');\n",
    "COPY t (id, note, password) FROM stdin CSV;\n",
    "COPY t (id, password) FROM stdin WITH (DELIMITER U&'!007C' UESCAPE '!');\n",
    "COPY t (id, password) FROM stdin WITH (QUOTE 'ü', FORMAT csv);\n",
    "1\tZq9wSecret\n",
    "1|Zq9wSecret\n",
    "1éZq9wSecret\n",
    "5\" screen,Zq9wSecret\n",
    "1,\"{\"\"password\"\": \"\"Zq9wSecret\"\"}\"\n",
    "2\tline one\\\n",
    "ALTER ROLE app PASSWORD 'Zq9wSecret';\n",
    "INSERT INTO users (name,\ncopy, password) VALUES ('a', 'b', 'Zq9wSecret');\n",
    "CREATE USER u IDENTIFIED BY 'Zq9wSecret';\n",
    "-- mysql -uroot -pZq9wSecret\n",
    "/* ü */",
    "'",
    "\"",
    "$$",
    "$q$",
    "E'\\''",
    "U&'\\00e9'",
    "日本語",
    "é",
    "€",
    "🙂",
    "\u{0}",
    "\r\n",
    "\n",
    " ",
    "\t",
    ";",
    "(",
    ")",
    "\\",
    "AUTH Zq9wSecret\n",
    "DO $$ BEGIN RAISE EXCEPTION 'x'; END $$;\n",
    "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\nplpy.error(\"é\")\n$$;\n",
];

fn text(rng: &mut Rng) -> String {
    let pieces = 1 + usize::try_from(rng.next() % 12).unwrap_or(0);
    (0..pieces).map(|_| rng.pick(PIECES)).collect()
}

#[test]
fn random_texts_never_panic_and_are_stable() -> TestResult {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("fuzz"));
    let rounds = if cfg!(debug_assertions) {
        3_000
    } else {
        30_000
    };
    let mut rng = Rng(0x5eed_1234_abcd_0042);
    let mut unstable = Vec::new();
    for round in 0..rounds {
        let text = text(&mut rng);
        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Redis] {
            let full = prepare_statement_text(&text, dialect, TextLevel::Full).unwrap_or_default();
            let _ = prepare_statement_text(&text, dialect, TextLevel::Normalized);
            let _ = redact_secrets(&text, dialect);
            let _ = prepare_error_message(&text, Some("42601"), Some(dialect), TextLevel::Full);
            if full.len() <= MAX_REDACTED_BYTES {
                let twice =
                    prepare_statement_text(&full, dialect, TextLevel::Full).unwrap_or_default();
                if twice != full && unstable.len() < 20 {
                    unstable.push(format!(
                        "{dialect:?} {text:?}\n  once:  {full:?}\n  twice: {twice:?}"
                    ));
                }
            }
            if round % 10 == 0 {
                for level in [TextLevel::Normalized, TextLevel::Full] {
                    recorder.set_text_level(level);
                    recorder.record(
                        ActivityEvent::query_failed(
                            &text,
                            dialect,
                            "x",
                            &ErrorDetails {
                                code: Some("XX000".into()),
                                message: text.clone(),
                            },
                            1,
                        )
                        .connection("fuzz")
                        .build()?,
                    )?;
                }
            }
        }
    }
    assert!(unstable.is_empty(), "{}", unstable.join("\n"));
    Ok(())
}

/// Texts the fuzzer found unstable: a hidden broken statement swallowed
/// the line break before `COPY` or the `COPY` head itself, so a second
/// pass read the rows as SQL.
#[test]
fn hidden_statements_keep_the_script_shape() {
    for text in [
        "'1éZq9wSecret\n\\echo Loading user's table\nCOPY t (id, password) FROM stdin WITH (DELIMITER '|');\nU&'\\00e9'CREATE USER u IDENTIFIED BY 'Zq9wSecret';\n'",
        "\\COPY t (id, note, password) FROM stdin CSV;\n\\.\n€1|Zq9wSecret\nU&'\\00e9'1\tZq9wSecret\nE'\\''\\.\nCOPY t (id, password) FROM stdin WITH (QUOTE 'ü', FORMAT csv);\nE'\\''",
        "U&'\\00e9'€INSERT INTO users (name,\ncopy, password) VALUES ('a', 'b', 'Zq9wSecret');\n\r\nCOPY t (id, note, password) FROM stdin CSV;\n'CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\nplpy.error(\"é\")\n$$;\n🙂\0",
        "U&'\\00e9'€SELECT 1;\nCOPY t (id, password) FROM stdin;\n1\tZq9wSecret\n\\.\nSELECT 2;\n",
    ] {
        let once =
            prepare_statement_text(text, SqlDialect::Postgres, TextLevel::Full).unwrap_or_default();
        assert!(!once.contains("Zq9wSecret"), "{once:?}");
        let twice = prepare_statement_text(&once, SqlDialect::Postgres, TextLevel::Full)
            .unwrap_or_default();
        assert_eq!(twice, once, "{text:?}");
    }
}
