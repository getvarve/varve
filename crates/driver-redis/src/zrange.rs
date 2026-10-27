//! Sorted set ranges by score and by lex (`ZRANGE ... BYSCORE|BYLEX [REV]
//! LIMIT`, Redis 6.2 and newer, Valkey), paged by offset.
//!
//! The range is read on the key browser's side connection as a stream: a
//! page keeps at most `count` members and about [`MAX_PAGE_BYTES`] of
//! member data (it ends early then), and a member over
//! [`crate::MAX_REPLY_BYTES`] becomes a placeholder, so nothing larger is
//! ever held.

use serde::{Deserialize, Serialize};
use varve_driver::{DriverError, ErrorKind, Value};

use crate::browser::{MAX_PAGE_BYTES, MAX_PAGE_COUNT, ScoredMember, u64_text};
use crate::edit::{EditError, score, value_bytes};
use crate::measure::{self, Head, RawConn};
use crate::oversized::placeholder;
use crate::reply::{bytes_value, double_value};
use crate::session::{MAX_REPLY_BYTES, RedisSession};

/// What a range is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RangeBy {
    /// Scores.
    Score,
    /// Members, byte by byte (meaningful when all scores are equal).
    Lex,
}

/// One end of a range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ZsetBound {
    /// No limit at this end.
    Unbounded,
    /// Up to and including `value`.
    Inclusive {
        /// A score (by score) or a member (by lex).
        value: Value,
    },
    /// Up to but not including `value`.
    Exclusive {
        /// A score (by score) or a member (by lex).
        value: Value,
    },
}

/// A range of a sorted set to read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZsetRange {
    /// Score or lex.
    pub by: RangeBy,
    /// The low end.
    pub min: ZsetBound,
    /// The high end.
    pub max: ZsetBound,
    /// Highest first.
    pub reverse: bool,
    /// Members of the range to skip.
    #[serde(with = "u64_text")]
    pub offset: u64,
    /// Members to read, clamped to 1..=[`MAX_PAGE_COUNT`].
    pub count: u32,
}

/// A page of a range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZsetRangePage {
    /// Members in range order; the score is [`Value::Null`] for lex ranges.
    pub members: Vec<ScoredMember>,
    /// Offset of the next page of the same range; `None` when the range is
    /// done.
    pub next_offset: Option<String>,
}

fn bound_arg(bound: &ZsetBound, by: RangeBy, low: bool) -> Result<Vec<u8>, EditError> {
    Ok(match (bound, by) {
        (ZsetBound::Unbounded, RangeBy::Score) => {
            if low {
                b"-inf".to_vec()
            } else {
                b"+inf".to_vec()
            }
        }
        (ZsetBound::Unbounded, RangeBy::Lex) => {
            if low {
                b"-".to_vec()
            } else {
                b"+".to_vec()
            }
        }
        (ZsetBound::Inclusive { value }, RangeBy::Score) => score(value, "bound")?.1.into_bytes(),
        (ZsetBound::Exclusive { value }, RangeBy::Score) => {
            format!("({}", score(value, "bound")?.1).into_bytes()
        }
        (ZsetBound::Inclusive { value }, RangeBy::Lex) => {
            let mut arg = b"[".to_vec();
            arg.extend(value_bytes(value, "bound")?);
            arg
        }
        (ZsetBound::Exclusive { value }, RangeBy::Lex) => {
            let mut arg = b"(".to_vec();
            arg.extend(value_bytes(value, "bound")?);
            arg
        }
    })
}

/// The command for a range (`count + 1` members, to know whether more
/// follow).
fn range_command(key: &[u8], range: &ZsetRange) -> Result<Vec<Vec<u8>>, EditError> {
    let min = bound_arg(&range.min, range.by, true)?;
    let max = bound_arg(&range.max, range.by, false)?;
    let (first, second) = if range.reverse {
        (max, min)
    } else {
        (min, max)
    };
    let mut command = vec![b"ZRANGE".to_vec(), key.to_vec(), first, second];
    command.push(match range.by {
        RangeBy::Score => b"BYSCORE".to_vec(),
        RangeBy::Lex => b"BYLEX".to_vec(),
    });
    if range.reverse {
        command.push(b"REV".to_vec());
    }
    let count = range.count.clamp(1, MAX_PAGE_COUNT);
    command.push(b"LIMIT".to_vec());
    command.push(range.offset.to_string().into_bytes());
    command.push((u64::from(count) + 1).to_string().into_bytes());
    if range.by == RangeBy::Score {
        command.push(b"WITHSCORES".to_vec());
    }
    Ok(command)
}

/// Reads the range reply: members while they fit the page, the rest
/// skipped.
async fn read_range(
    raw: &mut RawConn,
    key: &[u8],
    command: &[Vec<u8>],
    range: &ZsetRange,
) -> Result<Option<ZsetRangePage>, DriverError> {
    raw.send(&[b"TYPE", key]).await?;
    match raw.next().await? {
        Head::Simple(name) if name == "none" => return Ok(None),
        Head::Simple(name) if name == "zset" => {}
        Head::Simple(_) => {
            return Err(DriverError::new(
                ErrorKind::Query,
                "WRONGTYPE Operation against a key holding the wrong kind of value",
            )
            .with_code("WRONGTYPE"));
        }
        other => return Err(measure::unexpected(&format!("{other:?}"))),
    }
    let args: Vec<&[u8]> = command.iter().map(Vec::as_slice).collect();
    raw.send(&args).await?;
    let parts = raw.array().await?;
    let scored = range.by == RangeBy::Score;
    let per = if scored { 2 } else { 1 };
    if parts % per != 0 {
        return Err(measure::unexpected(
            "a range reply with an odd number of parts",
        ));
    }
    let found = parts / per;
    let count = u64::from(range.count.clamp(1, MAX_PAGE_COUNT));
    let mut members = Vec::new();
    let mut bytes = 0u64;
    let mut full = false;
    for index in 0..found {
        let keep = !full && index < count;
        let member = match raw.next().await? {
            Head::Bulk(Some(len)) if keep && len > MAX_REPLY_BYTES => {
                raw.skip(len).await?;
                bytes = bytes.saturating_add(64);
                Some(placeholder(Some(len)))
            }
            Head::Bulk(Some(len))
                if keep && (members.is_empty() || bytes.saturating_add(len) <= MAX_PAGE_BYTES) =>
            {
                bytes = bytes.saturating_add(len);
                Some(bytes_value(raw.keep(len).await?))
            }
            Head::Bulk(Some(len)) => {
                raw.skip(len).await?;
                if keep {
                    full = true;
                }
                None
            }
            other => return Err(measure::unexpected(&format!("{other:?}"))),
        };
        let score = if scored {
            let text = raw.short().await?;
            Some(text)
        } else {
            None
        };
        if let Some(member) = member {
            let score = match score {
                Some(text) => double_value(
                    crate::edit::parse_score(&text)
                        .ok_or_else(|| measure::unexpected(&format!("score {text}")))?,
                ),
                None => Value::Null,
            };
            members.push(ScoredMember { member, score });
        }
    }
    let kept = members.len() as u64;
    let next_offset = if full || found > count {
        Some(range.offset.saturating_add(kept).to_string())
    } else {
        None
    };
    Ok(Some(ZsetRangePage {
        members,
        next_offset,
    }))
}

impl RedisSession {
    /// A page of a sorted set range by score or lex; `None` when the key
    /// does not exist.
    ///
    /// # Errors
    ///
    /// Query errors for malformed bounds (code `INVALID_RANGE`) or another
    /// type (`WRONGTYPE`), connection errors, Cancelled.
    pub async fn zset_range(
        &self,
        key: &[u8],
        range: &ZsetRange,
    ) -> Result<Option<ZsetRangePage>, DriverError> {
        let command = range_command(key, range).map_err(|e| {
            DriverError::new(ErrorKind::Query, e.to_string()).with_code("INVALID_RANGE")
        })?;
        let mut call = self.begin().await?;
        let key = key.to_vec();
        let range = range.clone();
        measure::with_side(&mut call, move |raw| {
            Box::pin(async move { read_range(raw, &key, &command, &range).await })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(command: &[Vec<u8>]) -> String {
        command
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn commands_follow_the_bounds() -> Result<(), Box<dyn std::error::Error>> {
        let range: ZsetRange = serde_json::from_value(serde_json::json!({
            "by": "score",
            "min": { "kind": "exclusive", "value": { "t": "float", "v": 1.5 } },
            "max": { "kind": "unbounded" },
            "reverse": true, "offset": "10", "count": 5
        }))?;
        assert_eq!(
            lines(&range_command(b"z", &range)?),
            "ZRANGE z +inf (1.5 BYSCORE REV LIMIT 10 6 WITHSCORES"
        );
        let lex = ZsetRange {
            by: RangeBy::Lex,
            min: ZsetBound::Inclusive {
                value: Value::Text("a".into()),
            },
            max: ZsetBound::Exclusive {
                value: Value::Text("c".into()),
            },
            reverse: false,
            offset: 0,
            count: 0,
        };
        assert_eq!(
            lines(&range_command(b"z", &lex)?),
            "ZRANGE z [a (c BYLEX LIMIT 0 2"
        );
        let bad = ZsetRange {
            min: ZsetBound::Inclusive {
                value: Value::Bool(true),
            },
            ..lex
        };
        assert!(range_command(b"z", &bad).is_err());
        Ok(())
    }
}
