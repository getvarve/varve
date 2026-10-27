//! Adversarial nesting must never abort the process: every text level is
//! computed on a 256 KiB thread stack for inputs nested 100,000 levels deep
//! (and at sizes around the redaction cap), and nested routine bodies stay
//! linear in time.

use std::time::{Duration, Instant};

use varve_activity::{MAX_REDACTED_BYTES, SqlDialect, TextLevel, prepare_statement_text};

const LEVELS: [TextLevel; 3] = [TextLevel::None, TextLevel::Normalized, TextLevel::Full];
const STACK: usize = 256 * 1024;
const DEPTH: usize = 100_000;

/// Runs every level for `text` on a small stack; returns the outputs.
fn on_small_stack(text: String, dialect: SqlDialect) -> Vec<Option<String>> {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            LEVELS
                .iter()
                .map(|level| prepare_statement_text(&text, dialect, *level))
                .collect()
        })
        .and_then(|handle| {
            handle
                .join()
                .map_err(|_| std::io::Error::other("the redactor panicked"))
        })
        .unwrap_or_else(|error| panic!("{error}"))
}

fn inputs(depth: usize) -> Vec<(&'static str, String)> {
    vec![
        (
            "nested comments",
            format!(
                "SELECT 1 {}PASSWORD 'Zq9wSecret'{}",
                "/* ".repeat(depth),
                " */".repeat(depth)
            ),
        ),
        (
            "dollar quotes",
            format!(
                "DO {}PASSWORD 'Zq9wSecret'{}",
                (0..depth).map(|i| format!("$t{i}$ ")).collect::<String>(),
                (0..depth)
                    .rev()
                    .map(|i| format!(" $t{i}$"))
                    .collect::<String>()
            ),
        ),
        (
            "parentheses",
            format!(
                "SELECT * FROM t WHERE pw = {}'Zq9wSecret'{}",
                "(".repeat(depth),
                ")".repeat(depth)
            ),
        ),
        (
            "CASE",
            format!(
                "SELECT {}'Zq9wSecret'{} FROM t",
                "CASE WHEN a THEN ".repeat(depth),
                " END".repeat(depth)
            ),
        ),
        (
            "sub-selects",
            format!(
                "SELECT * FROM t WHERE x IN {}'Zq9wSecret'{}",
                "(SELECT x FROM t WHERE x IN ".repeat(depth),
                ")".repeat(depth)
            ),
        ),
        // Just under the parser's token limit: the syntax tree is built.
        (
            "OR chain",
            format!(
                "SELECT * FROM t WHERE {}pw = 'Zq9wSecret'",
                "a = 1 OR ".repeat(depth.min(495))
            ),
        ),
        (
            "+ chain",
            format!("SELECT {}'Zq9wSecret'", "1 + ".repeat(depth.min(995))),
        ),
        (
            "line comments",
            format!(
                "{}-- PASSWORD 'Zq9wSecret'\nSELECT 1",
                "-- don't\n".repeat(depth)
            ),
        ),
        (
            "BINARY chain",
            format!(
                "SELECT * FROM t WHERE password = {}'Zq9wSecret'",
                "BINARY ".repeat(depth)
            ),
        ),
        (
            "init_connect in init_connect",
            (0..depth.min(12)).fold(
                "ALTER USER u IDENTIFIED BY 'Zq9wSecret'".to_string(),
                |inner, _| format!("SET GLOBAL init_connect = '{}'", inner.replace('\'', "''")),
            ),
        ),
    ]
}

#[test]
fn deep_nesting_never_aborts_and_never_leaks() {
    for depth in [10, 1_000, DEPTH] {
        for (name, text) in inputs(depth) {
            for dialect in [SqlDialect::Postgres, SqlDialect::Mysql] {
                let outputs = on_small_stack(text.clone(), dialect);
                assert_eq!(outputs[0], None, "{name} at none");
                // In the parentheses, CASE and sub-select inputs the literal
                // is plain data, which `full` keeps as written: only
                // `normalized` must hide it there.
                let plain = matches!(
                    name,
                    "parentheses" | "CASE" | "sub-selects" | "OR chain" | "+ chain"
                );
                let checked = if plain { &outputs[1..2] } else { &outputs[..] };
                for output in checked.iter().flatten() {
                    assert!(
                        !output.contains("Zq9wSecret"),
                        "{name} depth {depth} {dialect:?} leaked: {}",
                        &output[..output.len().min(200)]
                    );
                }
                if text.len() > MAX_REDACTED_BYTES {
                    // Above the cap no text is kept at any level.
                    assert!(outputs.iter().all(Option::is_none), "{name}");
                }
            }
        }
    }
}

/// The time to prepare `depth` nested dollar-quoted bodies at every
/// level: the best of three runs, so a burst of load on the machine does
/// not count.
fn nested_bodies_time(depth: usize) -> Duration {
    let text = format!(
        "DO {}PASSWORD 'x'{}",
        (0..depth).map(|i| format!("$t{i}$ ")).collect::<String>(),
        (0..depth)
            .rev()
            .map(|i| format!(" $t{i}$"))
            .collect::<String>()
    );
    (0..3)
        .map(|_| {
            let start = Instant::now();
            for level in LEVELS {
                let _ = prepare_statement_text(&text, SqlDialect::Postgres, level);
            }
            start.elapsed()
        })
        .min()
        .unwrap_or_default()
}

/// Linear, measured against itself in the same run rather than a wall
/// clock bound: four times the nesting may take at most ten times as long
/// (a quadratic reading takes sixteen times as long).
#[test]
fn nested_routine_bodies_take_linear_time() {
    let small = nested_bodies_time(750);
    let large = nested_bodies_time(3_000);
    println!("750 nested dollar quotes: {small:?}, 3,000: {large:?}");
    let base = small.max(Duration::from_millis(1));
    assert!(large < base * 10, "750: {small:?}, 3,000: {large:?}");
}
