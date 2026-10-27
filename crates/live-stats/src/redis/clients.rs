//! Client list (`CLIENT LIST`) with the session's own connection marked
//! (`CLIENT ID`), and `CLIENT KILL ID`.
//!
//! Every release adds fields (`ssub` and `watch` in 7.0, `lib-name` in 7.2,
//! `io-thread` and network totals in Redis 8, `capa` in Valkey 8); the
//! common ones are typed and the rest kept in [`ClientInfo::other`].

use serde::{Deserialize, Serialize};
use varve_catalog::CatalogResult;
use varve_driver::{ServerInfo, Session};

use super::info::{InfoField, Take};
use super::reply::{Cmd, int, invalid, run, run_batch, text, unexpected};
use varve_catalog::CatalogError;

/// One client connection.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// `id`.
    pub id: i64,
    /// True for the connection this session runs on.
    pub is_self: bool,
    /// `addr`: client address.
    pub addr: Option<String>,
    /// `laddr`: server address the client connected to.
    pub laddr: Option<String>,
    /// `name`; `None` when not set.
    pub name: Option<String>,
    /// `user` (ACL user).
    pub user: Option<String>,
    /// `age`, seconds connected.
    pub age: Option<i64>,
    /// `idle`, seconds since the last command.
    pub idle: Option<i64>,
    /// `flags` as reported (`N` for none).
    pub flags: String,
    /// The flags spelled out.
    pub flag_names: Vec<String>,
    /// `db`.
    pub db: Option<i64>,
    /// `sub`: channel subscriptions.
    pub sub: Option<i64>,
    /// `psub`: pattern subscriptions.
    pub psub: Option<i64>,
    /// `ssub`: shard channel subscriptions (7.0 and newer).
    pub ssub: Option<i64>,
    /// `multi`: commands queued in `MULTI`, -1 outside a transaction.
    pub multi: Option<i64>,
    /// `qbuf`: query buffer bytes.
    pub qbuf: Option<i64>,
    /// `omem`: output buffer bytes.
    pub omem: Option<i64>,
    /// `tot-mem`: total memory of the client, bytes.
    pub tot_mem: Option<i64>,
    /// `cmd`: last command (`client|list` for subcommands).
    pub cmd: Option<String>,
    /// `resp`: protocol version (7.0 and newer).
    pub resp: Option<i64>,
    /// `lib-name` (7.2 and newer).
    pub lib_name: Option<String>,
    /// `lib-ver` (7.2 and newer).
    pub lib_ver: Option<String>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

fn flag_name(flag: char) -> Option<&'static str> {
    Some(match flag {
        'A' => "close as soon as possible",
        'b' => "blocked",
        'c' => "close after reply",
        'd' => "watched keys modified",
        'e' => "excluded from eviction",
        'i' => "waiting for I/O",
        'M' => "primary",
        'N' => return None,
        'O' => "monitor",
        'P' => "pub/sub",
        'r' => "read-only cluster mode",
        'R' => "tracking target invalid",
        'S' => "replica",
        't' => "tracking",
        'B' => "broadcast tracking",
        'T' => "no touch",
        'u' => "unblocked",
        'U' => "unix socket",
        'x' => "in MULTI/EXEC",
        'I' => "import source",
        _ => "unknown flag",
    })
}

/// Parses `CLIENT LIST` (or `CLIENT INFO`) text. `own_id` marks the
/// session's connection.
#[must_use]
pub fn parse_client_list(text: &str, own_id: Option<i64>) -> Vec<ClientInfo> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let fields = line
                .split(' ')
                .filter_map(|part| part.split_once('='))
                .map(|(k, v)| InfoField {
                    key: k.to_string(),
                    value: v.to_string(),
                })
                .collect();
            let mut t = Take::new(fields);
            let id = t.int(&["id"])?;
            let flags = t.str(&["flags"]).unwrap_or_default();
            let non_empty = |v: Option<String>| v.filter(|s| !s.is_empty());
            Some(ClientInfo {
                id,
                is_self: own_id == Some(id),
                addr: t.str(&["addr"]),
                laddr: t.str(&["laddr"]),
                name: non_empty(t.str(&["name"])),
                user: t.str(&["user"]),
                age: t.int(&["age"]),
                idle: t.int(&["idle"]),
                flag_names: flags
                    .chars()
                    .filter_map(flag_name)
                    .map(str::to_string)
                    .collect(),
                flags,
                db: t.int(&["db"]),
                sub: t.int(&["sub"]),
                psub: t.int(&["psub"]),
                ssub: t.int(&["ssub"]),
                multi: t.int(&["multi"]),
                qbuf: t.int(&["qbuf"]),
                omem: t.int(&["omem"]),
                tot_mem: t.int(&["tot-mem"]),
                cmd: t.str(&["cmd"]),
                resp: t.int(&["resp"]),
                lib_name: non_empty(t.str(&["lib-name"])),
                lib_ver: non_empty(t.str(&["lib-ver"])),
                other: t.rest(),
            })
        })
        .collect()
}

/// Lists the connected clients, the session's own connection marked.
///
/// # Errors
///
/// Driver errors.
pub async fn client_list(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<Vec<ClientInfo>> {
    let _ = server;
    let replies = run_batch(
        session,
        &[Cmd::new("CLIENT").arg("ID"), Cmd::new("CLIENT").arg("LIST")],
    )
    .await?;
    let own = int(&replies[0]).ok_or_else(|| unexpected("CLIENT ID is not a number"))?;
    let list = text(&replies[1]).ok_or_else(|| unexpected("CLIENT LIST did not return text"))?;
    Ok(parse_client_list(&list, Some(own)))
}

/// Closes client connection `id` (`CLIENT KILL ID`). Refuses the
/// session's own connection.
///
/// # Errors
///
/// [`CatalogError::Invalid`] for the own connection,
/// [`CatalogError::NotFound`] when no client has that id, driver errors
/// (a read-only connection refuses the kill).
pub async fn kill_client(session: &dyn Session, server: &ServerInfo, id: i64) -> CatalogResult<()> {
    let _ = server;
    let own = run(session, &Cmd::new("CLIENT").arg("ID")).await?;
    if int(&own) == Some(id) {
        return Err(invalid(
            "This is the connection Varve uses for this panel; it cannot be closed from here",
        ));
    }
    let reply = run(
        session,
        &Cmd::new("CLIENT").arg("KILL").arg("ID").arg(id.to_string()),
    )
    .await?;
    match int(&reply) {
        Some(0) => Err(CatalogError::NotFound {
            message: format!("No client with id {id} is connected"),
        }),
        Some(_) => Ok(()),
        None => Err(unexpected("CLIENT KILL did not return a number")),
    }
}
