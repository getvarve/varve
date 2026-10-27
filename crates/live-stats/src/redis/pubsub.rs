//! Pub/Sub snapshot: active channels with subscriber counts, pattern
//! subscription count, and shard channels (7.0 and newer).
//!
//! Live message streaming needs a connection in subscriber mode, which the
//! request/response [`Session`] cannot provide (the driver refuses
//! `SUBSCRIBE` in the console). It belongs in a dedicated driver connection
//! mode and is outside this crate.

use serde::{Deserialize, Serialize};
use varve_catalog::CatalogResult;
use varve_driver::{ServerInfo, Session, Value};

use super::reply::{Cmd, array, bytes, int, name_value, run, run_batch, unexpected};

/// Channels per `PUBSUB NUMSUB` call.
const NUMSUB_BATCH: usize = 500;

/// A channel and its subscribers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSubscribers {
    /// Channel name (text, or bytes when not UTF-8).
    pub channel: Value,
    /// Subscribed clients.
    pub subscribers: i64,
}

/// Pub/Sub state at one moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PubSubSnapshot {
    /// Channels with at least one subscriber, most subscribers first.
    pub channels: Vec<ChannelSubscribers>,
    /// True when more than `max_channels` channels matched.
    pub truncated: bool,
    /// Pattern subscriptions (`PUBSUB NUMPAT`).
    pub pattern_subscriptions: i64,
    /// Shard channels (7.0 and newer); `None` on older servers.
    pub shard_channels: Option<Vec<ChannelSubscribers>>,
}

/// Parses a `PUBSUB NUMSUB` / `SHARDNUMSUB` reply.
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Unexpected`] for a malformed reply.
pub fn parse_numsub(reply: &Value) -> CatalogResult<Vec<ChannelSubscribers>> {
    // Channel names can be binary, so pair the raw values instead of text keys.
    let items = array(reply).ok_or_else(|| unexpected("NUMSUB is not an array"))?;
    let flat: Vec<(&Value, &Value)> = if items.first().is_some_and(|i| matches!(i, Value::Array(_)))
    {
        items
            .iter()
            .filter_map(|i| match array(i) {
                Some([k, v]) => Some((k, v)),
                _ => None,
            })
            .collect()
    } else {
        items
            .chunks(2)
            .filter_map(|c| match c {
                [k, v] => Some((k, v)),
                _ => None,
            })
            .collect()
    };
    flat.into_iter()
        .map(|(k, v)| {
            Ok(ChannelSubscribers {
                channel: name_value(bytes(k).ok_or_else(|| unexpected("channel is not text"))?),
                subscribers: int(v)
                    .ok_or_else(|| unexpected("subscriber count is not a number"))?,
            })
        })
        .collect()
}

async fn counts(
    session: &dyn Session,
    sub: &str,
    channels: &[Vec<u8>],
) -> CatalogResult<Vec<ChannelSubscribers>> {
    let cmds: Vec<Cmd> = channels
        .chunks(NUMSUB_BATCH)
        .map(|chunk| Cmd::new("PUBSUB").arg(sub).args(chunk))
        .collect();
    let mut out = Vec::with_capacity(channels.len());
    for reply in run_batch(session, &cmds).await? {
        out.extend(parse_numsub(&reply)?);
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.subscribers));
    Ok(out)
}

async fn channel_list(
    session: &dyn Session,
    sub: &str,
    pattern: Option<&str>,
) -> CatalogResult<Vec<Vec<u8>>> {
    let mut cmd = Cmd::new("PUBSUB").arg(sub);
    if let Some(pattern) = pattern {
        cmd = cmd.arg(pattern);
    }
    let reply = run(session, &cmd).await?;
    Ok(array(&reply)
        .ok_or_else(|| unexpected("PUBSUB channel list is not an array"))?
        .iter()
        .filter_map(bytes)
        .collect())
}

/// Reads the Pub/Sub state: channels matching `pattern` (all when `None`)
/// with subscriber counts, at most `max_channels` of them, the number of
/// pattern subscriptions and, on 7.0 and newer, shard channels.
///
/// # Errors
///
/// Driver errors.
pub async fn pubsub_snapshot(
    session: &dyn Session,
    server: &ServerInfo,
    pattern: Option<&str>,
    max_channels: usize,
) -> CatalogResult<PubSubSnapshot> {
    let mut names = channel_list(session, "CHANNELS", pattern).await?;
    let mut truncated = names.len() > max_channels;
    names.truncate(max_channels);
    let channels = counts(session, "NUMSUB", &names).await?;
    let numpat = run(session, &Cmd::new("PUBSUB").arg("NUMPAT")).await?;
    let shard_channels = if server.at_least(7, 0) {
        let mut shard = channel_list(session, "SHARDCHANNELS", pattern).await?;
        truncated |= shard.len() > max_channels;
        shard.truncate(max_channels);
        Some(counts(session, "SHARDNUMSUB", &shard).await?)
    } else {
        None
    };
    Ok(PubSubSnapshot {
        channels,
        truncated,
        pattern_subscriptions: int(&numpat)
            .ok_or_else(|| unexpected("PUBSUB NUMPAT is not a number"))?,
        shard_channels,
    })
}
