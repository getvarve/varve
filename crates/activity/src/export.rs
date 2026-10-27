//! CSV and JSON Lines export of the activity log (design doc 9.6).

use std::io::{self, Write};

use crate::event::ActivityEvent;
use crate::query::format_utc;

/// Header row of the CSV export.
pub(crate) const CSV_HEADER: &str = "id,occurred_at,occurred_at_utc,actor_id,device_id,\
workspace_id,connection_id,server_key,category,action,outcome,duration_ms,target,details\r\n";

/// Writes one CSV field (RFC 4180 quoting). Text that a spreadsheet would
/// run as a formula (`=`, `+`, `-`, `@`, tab, carriage return at the start)
/// is prefixed with `'`.
fn write_field(writer: &mut impl Write, value: &str, text: bool) -> io::Result<()> {
    let guarded;
    let value = if text && value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        guarded = format!("'{value}");
        guarded.as_str()
    } else {
        value
    };
    if value.contains([',', '"', '\n', '\r']) {
        writer.write_all(b"\"")?;
        writer.write_all(value.replace('"', "\"\"").as_bytes())?;
        writer.write_all(b"\"")
    } else {
        writer.write_all(value.as_bytes())
    }
}

/// Writes one event as a CSV row.
pub(crate) fn write_csv_row(writer: &mut impl Write, event: &ActivityEvent) -> io::Result<()> {
    let opt = |v: &Option<String>| v.clone().unwrap_or_default();
    let fields: [(String, bool); 14] = [
        (event.id.to_string(), false),
        (event.occurred_at.to_string(), false),
        (format_utc(event.occurred_at), false),
        (event.actor_id.clone(), true),
        (opt(&event.device_id), true),
        (opt(&event.workspace_id), true),
        (opt(&event.connection_id), true),
        (opt(&event.server_key), true),
        (event.category.as_str().to_string(), false),
        (event.action.clone(), true),
        (event.outcome.as_str().to_string(), false),
        (
            event.duration_ms.map(|d| d.to_string()).unwrap_or_default(),
            false,
        ),
        (opt(&event.target), true),
        (event.details.to_string(), true),
    ];
    for (i, (value, text)) in fields.iter().enumerate() {
        if i > 0 {
            writer.write_all(b",")?;
        }
        write_field(writer, value, *text)?;
    }
    writer.write_all(b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(value: &str, text: bool) -> io::Result<String> {
        let mut out = Vec::new();
        write_field(&mut out, value, text)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn csv_quoting() -> io::Result<()> {
        assert_eq!(field("plain", true)?, "plain");
        assert_eq!(field("a,b", true)?, "\"a,b\"");
        assert_eq!(field("say \"hi\"", true)?, "\"say \"\"hi\"\"\"");
        assert_eq!(field("line\nbreak", true)?, "\"line\nbreak\"");
        assert_eq!(field("=cmd()", true)?, "'=cmd()");
        assert_eq!(field("-5", false)?, "-5");
        Ok(())
    }
}
