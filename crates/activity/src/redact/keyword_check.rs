//! Checks the generated keyword lists against the servers (ignored: needs
//! the M2 database matrix, `docker compose -p varve-m2`).

use std::collections::BTreeSet;

use mysql_async::prelude::Queryable;

use super::keywords::{MYSQL_KEYWORDS, POSTGRES_KEYWORDS};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn word_shaped(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

async fn postgres_keywords(port: u16) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let config = format!(
        "host=127.0.0.1 port={port} user=varve password=varve_dev dbname=varve connect_timeout=10"
    );
    let (client, connection) = tokio_postgres::connect(&config, tokio_postgres::NoTls).await?;
    let task = tokio::spawn(connection);
    let rows = client
        .query("SELECT upper(word) FROM pg_get_keywords()", &[])
        .await?;
    drop(client);
    task.await??;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

async fn mysql_keywords(port: u16) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("root"))
        .pass(Some("varve_dev"));
    let mut conn = mysql_async::Conn::new(opts).await?;
    let words: Vec<String> = conn
        .query("SELECT upper(WORD) FROM information_schema.KEYWORDS")
        .await?;
    conn.disconnect().await?;
    Ok(words.into_iter().filter(|w| word_shaped(w)).collect())
}

fn compare(name: &str, committed: &[&str], live: &BTreeSet<String>) -> Result<(), String> {
    let committed: BTreeSet<String> = committed.iter().map(|w| (*w).to_string()).collect();
    if &committed == live {
        return Ok(());
    }
    let list: String = live.iter().map(|w| format!("    \"{w}\",\n")).collect();
    Err(format!(
        "{name} differs from the servers. Missing: {:?}. Extra: {:?}. New list:\n{list}",
        live.difference(&committed).collect::<Vec<_>>(),
        committed.difference(live).collect::<Vec<_>>()
    ))
}

#[tokio::test]
#[ignore = "needs the M2 database matrix (docker compose -p varve-m2)"]
async fn keyword_lists_match_the_servers() -> TestResult {
    let mut postgres = BTreeSet::new();
    for (var, default) in [("VARVE_M2_PG13_PORT", 55513), ("VARVE_M2_PG18_PORT", 55518)] {
        postgres.extend(postgres_keywords(port(var, default)).await?);
    }
    let mut mysql = BTreeSet::new();
    for (var, default) in [
        ("VARVE_M2_MYSQL80_PORT", 35080),
        ("VARVE_M2_MYSQL97_PORT", 35097),
        ("VARVE_M2_MARIADB106_PORT", 35106),
        ("VARVE_M2_MARIADB123_PORT", 35123),
    ] {
        mysql.extend(mysql_keywords(port(var, default)).await?);
    }
    compare("POSTGRES_KEYWORDS", POSTGRES_KEYWORDS, &postgres)?;
    compare("MYSQL_KEYWORDS", MYSQL_KEYWORDS, &mysql)?;
    Ok(())
}

#[test]
fn committed_lists_are_sorted_keywords_only() {
    for list in [POSTGRES_KEYWORDS, MYSQL_KEYWORDS] {
        assert!(list.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
        assert!(list.iter().all(|w| word_shaped(w)));
    }
    // Type names and role options that are not server keywords.
    for word in [
        "BIGSERIAL",
        "BYTEA",
        "TIMESTAMPTZ",
        "JSONB",
        "UUID",
        "PLPGSQL",
        "PERFORM",
        "BYPASSRLS",
        "CREATEDB",
        "CREATEROLE",
        "LOGIN",
        "NOLOGIN",
        "NOINHERIT",
        "SUPERUSER",
        "PUBLIC",
    ] {
        assert!(!POSTGRES_KEYWORDS.contains(&word), "{word}");
        assert!(!MYSQL_KEYWORDS.contains(&word), "{word}");
    }
}
