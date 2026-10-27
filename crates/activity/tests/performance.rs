//! The activity panel stays fast on a large log (design doc 9.5: the log is
//! kept forever by default).

use std::sync::Arc;
use std::time::{Duration, Instant};

use varve_activity::{
    ActivityEvent, ActivityFilter, Category, Outcome, Recorder, RecorderSettings, RowCounts,
    SqlDialect,
};
use varve_store::{Store, core_migrations};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const EVENTS: i64 = 50_000;
const START: i64 = 1_760_000_000_000;

/// The target is 200 ms in release builds. Debug builds compile SQLite
/// without optimizations, so the bound there is generous; the timing is
/// printed either way.
fn bound() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_millis(2_000)
    } else {
        Duration::from_millis(200)
    }
}

#[test]
fn filtered_page_over_50k_events_is_fast() -> TestResult {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let recorder = Recorder::new(store, RecorderSettings::local("device-1"));

    let mut batch = Vec::with_capacity(5_000);
    for i in 0..EVENTS {
        let connection = format!("conn-{}", i % 20);
        let builder = match i % 10 {
            0 => ActivityEvent::builder(Category::Connection, "connection.opened"),
            9 => ActivityEvent::query_failed(
                "SELECT * FROM orders WHERE id = 1",
                SqlDialect::Postgres,
                "select",
                &varve_activity::ErrorDetails {
                    code: Some("42P01".into()),
                    message: "relation does not exist".into(),
                },
                3,
            ),
            _ => ActivityEvent::query_executed(
                "SELECT * FROM orders WHERE customer_id = 42 AND status = 'open'",
                SqlDialect::Postgres,
                "select",
                RowCounts {
                    returned: Some(10),
                    affected: None,
                },
                2,
            ),
        };
        // One event deep in the log that only a full scan finds.
        let builder = if i == 1_234 {
            builder.target("public.needle_table")
        } else {
            builder
        };
        batch.push(
            builder
                .occurred_at(START + i * 1_000)
                .connection(connection)
                .build()?,
        );
        if batch.len() == 5_000 {
            recorder.record_batch(std::mem::take(&mut batch))?;
        }
    }
    recorder.record_batch(batch)?;

    // (name, filter, expected rows on the first page)
    let filters = [
        (
            "connection + category",
            ActivityFilter {
                connection_id: Some("conn-7".into()),
                categories: vec![Category::Query],
                limit: 100,
                ..ActivityFilter::default()
            },
            100,
        ),
        (
            "outcome + time range",
            ActivityFilter {
                outcomes: vec![Outcome::Failure],
                from: Some(START + 10_000_000),
                to: Some(START + 40_000_000),
                limit: 100,
                ..ActivityFilter::default()
            },
            100,
        ),
        (
            "text search",
            ActivityFilter {
                text: Some("42P01".into()),
                limit: 100,
                ..ActivityFilter::default()
            },
            100,
        ),
        (
            "text search, single match (full scan)",
            ActivityFilter {
                text: Some("needle".into()),
                limit: 100,
                ..ActivityFilter::default()
            },
            1,
        ),
        (
            "largest page",
            ActivityFilter {
                limit: 500,
                ..ActivityFilter::default()
            },
            500,
        ),
    ];
    for (name, filter, expected) in filters {
        let started = Instant::now();
        let page = recorder.list(&filter)?;
        let elapsed = started.elapsed();
        println!(
            "list ({name}) over {EVENTS} events: {} rows in {elapsed:?}",
            page.events.len()
        );
        assert_eq!(page.events.len(), expected, "{name}");
        assert!(
            elapsed < bound(),
            "{name}: {elapsed:?} exceeds {:?}",
            bound()
        );
        let Some(cursor) = page.next_cursor else {
            continue;
        };

        // The second page through the cursor is just as fast.
        let next = ActivityFilter {
            before_id: Some(cursor),
            ..filter
        };
        let started = Instant::now();
        let page2 = recorder.list(&next)?;
        let elapsed = started.elapsed();
        println!("  next page: {} rows in {elapsed:?}", page2.events.len());
        assert!(elapsed < bound(), "{name} page 2: {elapsed:?}");
    }

    let started = Instant::now();
    let days = recorder.count_by_day(&ActivityFilter {
        actions: vec!["connection.opened".into()],
        ..ActivityFilter::default()
    })?;
    println!(
        "count_by_day over {EVENTS} events: {} days in {:?}",
        days.len(),
        started.elapsed()
    );
    assert_eq!(days.iter().map(|d| d.count).sum::<u64>(), 5_000);
    Ok(())
}
