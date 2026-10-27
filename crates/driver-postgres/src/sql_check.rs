//! Checks that every SQL text the driver sends itself names functions,
//! operators and types with their schema (`pg_catalog.count(*)`,
//! `OPERATOR(pg_catalog.=)`, `::pg_catalog.int8`), so objects a user put in
//! front of `pg_catalog` in the `search_path` (for example a
//! `public.=(text, text)` operator) never run in the driver's own queries.
//!
//! The scan reads the string literals of the crate's sources that start a
//! query (`SELECT`, `(SELECT`, `WITH`), outside test modules, and the type
//! lookup queries of the vendored tokio-postgres (`vendor/tokio-postgres`,
//! patched to qualify them), which must equal `tap::TYPEINFO_QUERIES`.

/// The unqualified names, operators and casts in `sql`. `{...}` format
/// placeholders, string literals and quoted identifiers are skipped.
fn unqualified(sql: &str) -> Vec<String> {
    const KEYWORDS: [&str; 17] = [
        "ANY", "ALL", "ARRAY", "OPERATOR", "EXISTS", "VALUES", "AND", "OR", "NOT", "WHERE",
        "SELECT", "FROM", "AS", "THEN", "ELSE", "WHEN", "IN",
    ];
    const OPERATOR_CHARS: &str = "=<>!~|+@#%^&/";
    let chars: Vec<char> = sql.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    // The previous word and whether `pg_catalog.` came right before it.
    let mut previous: Option<(String, bool)> = None;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' => {
                i += 1;
                while i < chars.len() && chars[i] != c {
                    i += 1;
                }
                i += 1;
                previous = None;
            }
            '{' => {
                while i < chars.len() && chars[i] != '}' {
                    i += 1;
                }
                i += 1;
                previous = None;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let qualified = start >= 11
                    && chars[start - 11..start].iter().collect::<String>() == "pg_catalog.";
                if word.eq_ignore_ascii_case("IN") {
                    found.push("IN (uses an unqualified =)".to_owned());
                }
                previous = Some((word, qualified));
            }
            '(' => {
                if let Some((word, qualified)) = previous.take()
                    && !qualified
                    && word != "pg_catalog"
                    && !KEYWORDS.iter().any(|k| word.eq_ignore_ascii_case(k))
                {
                    found.push(format!("{word}("));
                }
                i += 1;
            }
            ':' if chars.get(i + 1) == Some(&':') => {
                let rest: String = chars[i + 2..].iter().collect();
                if !rest.trim_start().starts_with("pg_catalog.") {
                    found.push(format!(
                        "::{}",
                        rest.split_whitespace().next().unwrap_or("")
                    ));
                }
                i += 2;
                previous = None;
            }
            c if OPERATOR_CHARS.contains(c) => {
                // Allowed only as the name inside OPERATOR(pg_catalog.x).
                let start = i;
                while i < chars.len() && OPERATOR_CHARS.contains(chars[i]) {
                    i += 1;
                }
                let before: String = chars[..start].iter().collect();
                if !before.ends_with("OPERATOR(pg_catalog.") {
                    found.push(chars[start..i].iter().collect());
                }
                previous = None;
            }
            _ => {
                if !c.is_whitespace() && c != '.' {
                    previous = None;
                }
                i += 1;
            }
        }
    }
    found
}

/// The string literals of Rust `source` (outside `#[cfg(test)]` modules),
/// with line continuations joined.
fn literals(source: &str) -> Vec<String> {
    let code = source
        .split("#[cfg(test)]\nmod tests")
        .next()
        .unwrap_or(source);
    let chars: Vec<char> = code.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            // Line comments and char literals hold no queries.
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '\'' if chars.get(i + 2) == Some(&'\'') || chars.get(i + 1) == Some(&'\\') => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                let mut text = String::new();
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        match chars.get(i + 1) {
                            Some('\n') => {
                                i += 2;
                                while i < chars.len() && chars[i].is_whitespace() {
                                    i += 1;
                                }
                                continue;
                            }
                            Some(next) => text.push(*next),
                            None => {}
                        }
                        i += 2;
                        continue;
                    }
                    text.push(chars[i]);
                    i += 1;
                }
                i += 1;
                out.push(text);
            }
            _ => i += 1,
        }
    }
    out
}

/// True for a literal that starts a query the driver sends.
fn is_query(text: &str) -> bool {
    let start = text
        .trim_start()
        .trim_start_matches('(')
        .to_ascii_uppercase();
    start.starts_with("SELECT ") || start.starts_with("WITH ")
}

#[test]
fn the_checker_finds_unqualified_names() {
    assert_eq!(
        unqualified("SELECT count(*) = 1 FROM t WHERE a IN (1) AND b::int8 <> 'x(y)'"),
        ["count(", "=", "IN (uses an unqualified =)", "::int8", "<>"]
    );
    assert!(
        unqualified(
            "SELECT pg_catalog.count(*) OPERATOR(pg_catalog.=) {} FROM pg_catalog.t \
             WHERE a OPERATOR(pg_catalog.<>) 'x' AND b::pg_catalog.int8 IS NULL \
             AND c OPERATOR(pg_catalog.=) ANY (ARRAY[{}]::pg_catalog.text[])"
        )
        .is_empty()
    );
}

#[test]
fn the_driver_sends_only_qualified_sql() {
    let sources = [
        ("guard.rs", include_str!("guard.rs")),
        ("session.rs", include_str!("session.rs")),
        ("error.rs", include_str!("error.rs")),
        ("convert.rs", include_str!("convert.rs")),
        ("tls.rs", include_str!("tls.rs")),
        // tap.rs also holds tokio-postgres's type lookup texts.
        ("tap.rs", include_str!("tap.rs")),
    ];
    let mut queries = 0;
    for (file, source) in sources {
        for text in literals(source).into_iter().filter(|t| is_query(t)) {
            queries += 1;
            let found = unqualified(&text);
            assert!(found.is_empty(), "{file}: `{text}` has {found:?}");
        }
    }
    // The settings checks, the multirange lookup and the ping at least.
    assert!(queries >= 5, "only {queries} queries found");
}

#[test]
fn tokio_postgres_type_lookups_are_qualified() {
    // The vendored, patched tokio-postgres (see its VARVE-PATCH.md): its
    // type lookup queries are qualified and are the texts the tap knows.
    let source = include_str!("../../../vendor/tokio-postgres/src/prepare.rs");
    let lookups: Vec<String> = literals(source)
        .into_iter()
        .filter(|t| is_query(t))
        .collect();
    assert_eq!(lookups.len(), crate::tap::TYPEINFO_QUERIES.len());
    for text in &lookups {
        let found = unqualified(text);
        assert!(found.is_empty(), "`{text}` has {found:?}");
        assert!(
            crate::tap::TYPEINFO_QUERIES.contains(&text.as_str()),
            "the tap does not know `{text}`"
        );
    }
}

#[test]
fn every_guard_query_is_qualified() -> Result<(), String> {
    let sql = crate::guard::full_query(&["s0".into(), "s1".into()]).ok_or("no query")?;
    let found = unqualified(&sql);
    assert!(found.is_empty(), "{sql}: {found:?}");
    Ok(())
}
