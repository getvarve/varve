//! Functions (`FUNCTION LIST`, `LOAD` and `DELETE` plans; Redis 7.0 and
//! newer and Valkey) and the script cache (`SCRIPT EXISTS`, `SCRIPT FLUSH`
//! plan).

use serde::{Deserialize, Serialize};
use varve_catalog::CatalogResult;
use varve_driver::{ServerInfo, Session, Value};

use super::CommandPlan;
use super::reply::{Cmd, Fields, array, int, invalid, run, text, unexpected, unsupported};

/// One function of a library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionInfo {
    /// Function name.
    pub name: String,
    /// Description, when registered with one.
    pub description: Option<String>,
    /// Flags (`no-writes`, `allow-oom`, `allow-stale`, `no-cluster`,
    /// `allow-cross-slot-keys`).
    pub flags: Vec<String>,
}

/// A function library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionLibrary {
    /// Library name.
    pub name: String,
    /// Engine (`LUA`).
    pub engine: String,
    /// Functions.
    pub functions: Vec<FunctionInfo>,
    /// Source code, when listed `WITHCODE`.
    pub code: Option<String>,
}

fn require_functions(server: &ServerInfo) -> CatalogResult<()> {
    if server.at_least(7, 0) {
        Ok(())
    } else {
        Err(unsupported(format!(
            "Functions need Redis 7.0 or newer; this server is {}",
            server.version
        )))
    }
}

fn words(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(array)
        .unwrap_or_default()
        .iter()
        .filter_map(text)
        .collect()
}

/// Parses a `FUNCTION LIST` reply.
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Unexpected`] for a malformed reply.
pub fn parse_function_list(reply: &Value) -> CatalogResult<Vec<FunctionLibrary>> {
    array(reply)
        .ok_or_else(|| unexpected("FUNCTION LIST is not an array"))?
        .iter()
        .map(|lib| {
            let f = Fields::of(lib).ok_or_else(|| unexpected("library is not a map"))?;
            let functions = f
                .get("functions")
                .and_then(array)
                .unwrap_or_default()
                .iter()
                .filter_map(Fields::of)
                .map(|func| FunctionInfo {
                    name: func.text("name").unwrap_or_default(),
                    description: func.get("description").and_then(|d| match d {
                        Value::Null => None,
                        other => text(other),
                    }),
                    flags: words(func.get("flags")),
                })
                .collect();
            Ok(FunctionLibrary {
                name: f
                    .text("library_name")
                    .ok_or_else(|| unexpected("library without a name"))?,
                engine: f.text("engine").unwrap_or_default(),
                functions,
                code: f.text("library_code"),
            })
        })
        .collect()
}

/// Lists function libraries, optionally filtered by a library name
/// pattern, with their code when `with_code`.
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Unsupported`] before Redis 7.0; driver
/// errors.
pub async fn function_list(
    session: &dyn Session,
    server: &ServerInfo,
    library_pattern: Option<&str>,
    with_code: bool,
) -> CatalogResult<Vec<FunctionLibrary>> {
    require_functions(server)?;
    let mut cmd = Cmd::new("FUNCTION").arg("LIST");
    if let Some(pattern) = library_pattern {
        cmd = cmd.arg("LIBRARYNAME").arg(pattern);
    }
    if with_code {
        cmd = cmd.arg("WITHCODE");
    }
    parse_function_list(&run(session, &cmd).await?)
}

/// The library name from the `#!<engine> name=<library>` first line of
/// function code.
#[must_use]
pub fn library_name(code: &str) -> Option<String> {
    let first = code.lines().next()?.trim();
    let header = first.strip_prefix("#!")?;
    header
        .split_whitespace()
        .skip(1)
        .find_map(|part| part.strip_prefix("name="))
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

fn plan(cmd: &Cmd, destructive: bool, warnings: Vec<String>) -> CommandPlan {
    let line = cmd.line();
    CommandPlan {
        commands: vec![line.clone()],
        display: vec![line],
        destructive,
        sensitive: false,
        warnings,
    }
}

/// Builds the `FUNCTION LOAD [REPLACE]` plan for library code.
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Unsupported`] before Redis 7.0;
/// [`varve_catalog::CatalogError::Invalid`] when the code does not start
/// with a `#!<engine> name=<library>` line.
pub fn function_load_plan(
    server: &ServerInfo,
    code: &str,
    replace: bool,
) -> CatalogResult<CommandPlan> {
    require_functions(server)?;
    let name = library_name(code)
        .ok_or_else(|| invalid("Function code must start with a line like #!lua name=mylib"))?;
    let mut cmd = Cmd::new("FUNCTION").arg("LOAD");
    if replace {
        cmd = cmd.arg("REPLACE");
    }
    cmd = cmd.arg(code);
    let warnings = if replace {
        vec![format!(
            "Replaces library {name} if it exists; its functions are replaced on every caller at once."
        )]
    } else {
        Vec::new()
    };
    Ok(plan(&cmd, replace, warnings))
}

/// Builds the `FUNCTION DELETE` plan for a library.
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Unsupported`] before Redis 7.0;
/// [`varve_catalog::CatalogError::Invalid`] for an empty name.
pub fn function_delete_plan(server: &ServerInfo, library: &str) -> CatalogResult<CommandPlan> {
    require_functions(server)?;
    if library.trim().is_empty() {
        return Err(invalid("The library name is empty"));
    }
    Ok(plan(
        &Cmd::new("FUNCTION").arg("DELETE").arg(library),
        true,
        vec![format!(
            "Deletes library {library} and all its functions; FCALL of them fails afterwards."
        )],
    ))
}

/// Whether a script is in the script cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptPresence {
    /// SHA1 digest, lower case.
    pub sha: String,
    /// Cached on the server.
    pub exists: bool,
}

fn check_sha(sha: &str) -> CatalogResult<String> {
    let sha = sha.trim().to_ascii_lowercase();
    if sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(sha)
    } else {
        Err(invalid(format!("{sha:?} is not a SHA1 digest")))
    }
}

/// Checks which scripts are cached (`SCRIPT EXISTS`).
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Invalid`] for a digest that is not 40
/// hex characters; driver errors.
pub async fn script_exists(
    session: &dyn Session,
    server: &ServerInfo,
    shas: &[String],
) -> CatalogResult<Vec<ScriptPresence>> {
    let _ = server;
    let shas: Vec<String> = shas
        .iter()
        .map(|s| check_sha(s))
        .collect::<CatalogResult<_>>()?;
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let reply = run(session, &Cmd::new("SCRIPT").arg("EXISTS").args(&shas)).await?;
    let flags = array(&reply).ok_or_else(|| unexpected("SCRIPT EXISTS is not an array"))?;
    if flags.len() != shas.len() {
        return Err(unexpected(
            "SCRIPT EXISTS returned the wrong number of answers",
        ));
    }
    Ok(shas
        .into_iter()
        .zip(flags)
        .map(|(sha, flag)| ScriptPresence {
            exists: int(flag) == Some(1) || matches!(flag, Value::Bool(true)),
            sha,
        })
        .collect())
}

/// How `SCRIPT FLUSH` frees the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FlushMode {
    /// The server default (`lazyfree-lazy-user-flush`).
    #[default]
    Default,
    /// `ASYNC`.
    Async,
    /// `SYNC`.
    Sync,
}

/// Builds the `SCRIPT FLUSH` plan.
#[must_use]
pub fn script_flush_plan(server: &ServerInfo, mode: FlushMode) -> CommandPlan {
    let _ = server;
    let cmd = match mode {
        FlushMode::Default => Cmd::new("SCRIPT").arg("FLUSH"),
        FlushMode::Async => Cmd::new("SCRIPT").arg("FLUSH").arg("ASYNC"),
        FlushMode::Sync => Cmd::new("SCRIPT").arg("FLUSH").arg("SYNC"),
    };
    plan(
        &cmd,
        true,
        vec![
            "Empties the script cache: EVALSHA of any script fails with NOSCRIPT until clients \
             load it again."
                .into(),
        ],
    )
}
