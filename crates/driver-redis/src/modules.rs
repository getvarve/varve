//! Redis modules (design doc 4.2, "JSON, Search, TimeSeries, Bloom":
//! browse): which module features a server has, readers for TimeSeries and
//! probabilistic keys (Bloom, Cuckoo, Count-Min sketch, Top-K, t-digest),
//! their read-only queries, and the Search indexes.
//!
//! Features are detected per server with one `COMMAND INFO` of a command of
//! each feature (it works for every ACL user that may run `COMMAND INFO`,
//! on Redis 6.2 and newer and on Valkey); `MODULE LIST` adds the module
//! names and versions when the user may run it. A feature the server lacks
//! is reported with the reason, so the UI can hide it and say why.
//!
//! Every reader is bounded: a TimeSeries page holds at most
//! [`MAX_PAGE_COUNT`] samples, a Top-K list holds `k` items (the size the
//! key was created with), info replies are small. Module keys never change
//! here: edits are planned like every other value edit
//! ([`crate::KeyEdit`]).

use redis::Value as Resp;
use serde::{Deserialize, Serialize};
use varve_driver::{DriverError, ErrorKind, Value};

use crate::browser::{KeyType, MAX_PAGE_COUNT, ValueData, query_error, u64_text};
use crate::conn::ServerFlavor;
use crate::error;
use crate::reply::{ValueBudget, as_int, as_text, bytes_value, double_value, to_value};
use crate::session::{Call, RedisSession};

/// Error code of a time series read refused while a console `WATCH` is
/// open on the session (see [`RedisSession::read_value`]).
pub const WATCH_OPEN: &str = "WATCH_OPEN";

/// Most inputs of one [`ModuleQuery`].
pub const MAX_QUERY_INPUTS: usize = 1000;

/// Most bytes of a module info reply read on a side connection (TS.INFO
/// inside an edit check; labels can be long).
pub(crate) const INFO_REPLY_LIMIT: u64 = 4 * 1024 * 1024;

/// A module key type the browser reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModuleKind {
    /// RedisTimeSeries (`TSDB-TYPE`).
    TimeSeries,
    /// Bloom filter (`MBbloom--`; Valkey's `bloomfltr`).
    Bloom,
    /// Cuckoo filter (`MBbloomCF`).
    Cuckoo,
    /// Count-Min sketch (`CMSk-TYPE`).
    CountMin,
    /// Top-K (`TopK-TYPE`).
    TopK,
    /// t-digest (`TDIS-TYPE`).
    TDigest,
}

impl ModuleKind {
    /// The kind of a `TYPE` name, `None` for other names.
    #[must_use]
    pub fn from_type_name(name: &str) -> Option<Self> {
        Some(match name {
            "TSDB-TYPE" => Self::TimeSeries,
            "MBbloom--" | "bloomfltr" => Self::Bloom,
            "MBbloomCF" => Self::Cuckoo,
            "CMSk-TYPE" => Self::CountMin,
            "TopK-TYPE" => Self::TopK,
            "TDIS-TYPE" => Self::TDigest,
            _ => return None,
        })
    }

    /// The kind of a key type, `None` for core types and unknown modules.
    #[must_use]
    pub fn of(key_type: &KeyType) -> Option<Self> {
        match key_type {
            KeyType::Other(name) => Self::from_type_name(name),
            _ => None,
        }
    }

    /// The command that describes a key of this kind.
    #[must_use]
    pub fn info_command(self) -> &'static str {
        match self {
            Self::TimeSeries => "TS.INFO",
            Self::Bloom => "BF.INFO",
            Self::Cuckoo => "CF.INFO",
            Self::CountMin => "CMS.INFO",
            Self::TopK => "TOPK.INFO",
            Self::TDigest => "TDIGEST.INFO",
        }
    }

    /// The feature the kind belongs to.
    #[must_use]
    pub fn feature(self) -> ModuleFeature {
        match self {
            Self::TimeSeries => ModuleFeature::TimeSeries,
            Self::Bloom => ModuleFeature::Bloom,
            Self::Cuckoo => ModuleFeature::Cuckoo,
            Self::CountMin => ModuleFeature::CountMin,
            Self::TopK => ModuleFeature::TopK,
            Self::TDigest => ModuleFeature::TDigest,
        }
    }

    /// A name for messages.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::TimeSeries => "time series",
            Self::Bloom => "Bloom filter",
            Self::Cuckoo => "Cuckoo filter",
            Self::CountMin => "Count-Min sketch",
            Self::TopK => "Top-K",
            Self::TDigest => "t-digest",
        }
    }
}

/// A module feature the UI shows or hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModuleFeature {
    /// JSON documents (`JSON.*`).
    Json,
    /// Search indexes (`FT.*`).
    Search,
    /// Time series (`TS.*`).
    TimeSeries,
    /// Bloom filters (`BF.*`).
    Bloom,
    /// Cuckoo filters (`CF.*`).
    Cuckoo,
    /// Count-Min sketches (`CMS.*`).
    CountMin,
    /// Top-K (`TOPK.*`).
    TopK,
    /// t-digest (`TDIGEST.*`).
    TDigest,
}

impl ModuleFeature {
    /// Every feature, in the order the UI lists them.
    pub const ALL: [ModuleFeature; 8] = [
        Self::Json,
        Self::Search,
        Self::TimeSeries,
        Self::Bloom,
        Self::Cuckoo,
        Self::CountMin,
        Self::TopK,
        Self::TDigest,
    ];

    /// The command whose presence (`COMMAND INFO`) shows the feature.
    #[must_use]
    pub fn probe_command(self) -> &'static str {
        match self {
            Self::Json => "JSON.GET",
            Self::Search => "FT._LIST",
            Self::TimeSeries => "TS.INFO",
            Self::Bloom => "BF.INFO",
            Self::Cuckoo => "CF.INFO",
            Self::CountMin => "CMS.INFO",
            Self::TopK => "TOPK.INFO",
            Self::TDigest => "TDIGEST.INFO",
        }
    }

    /// The name `TYPE` reports (and `SCAN ... TYPE` takes) for the keys of
    /// this feature on a server of `flavor`; `None` for Search, which
    /// stores no keys of its own. valkey-bloom names its type `bloomfltr`;
    /// valkey-json keeps RedisJSON's `ReJSON-RL`.
    #[must_use]
    pub fn key_type(self, flavor: ServerFlavor) -> Option<&'static str> {
        Some(match (self, flavor) {
            (Self::Json, _) => "ReJSON-RL",
            (Self::Search, _) => return None,
            (Self::TimeSeries, _) => "TSDB-TYPE",
            (Self::Bloom, ServerFlavor::Redis) => "MBbloom--",
            (Self::Bloom, ServerFlavor::Valkey) => "bloomfltr",
            (Self::Cuckoo, _) => "MBbloomCF",
            (Self::CountMin, _) => "CMSk-TYPE",
            (Self::TopK, _) => "TopK-TYPE",
            (Self::TDigest, _) => "TDIS-TYPE",
        })
    }

    /// Module names (`MODULE LIST`) that provide the feature.
    fn module_names(self) -> &'static [&'static str] {
        match self {
            Self::Json => &["rejson", "json"],
            Self::Search => &["search", "ft", "searchlight"],
            Self::TimeSeries => &["timeseries"],
            Self::Bloom => &["bf", "bloom"],
            Self::Cuckoo | Self::CountMin | Self::TopK | Self::TDigest => &["bf"],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Search => "Search",
            Self::TimeSeries => "TimeSeries",
            Self::Bloom => "Bloom filters",
            Self::Cuckoo => "Cuckoo filters",
            Self::CountMin => "Count-Min sketches",
            Self::TopK => "Top-K",
            Self::TDigest => "t-digest",
        }
    }

    /// Why the feature is missing on a server of `flavor`.
    fn missing_reason(self, flavor: ServerFlavor) -> String {
        let prefix = self.probe_command().split('.').next().unwrap_or_default();
        let needs = match (flavor, self) {
            (ServerFlavor::Valkey, Self::Json) => "the valkey-json module",
            (ServerFlavor::Valkey, Self::Search) => "the valkey-search module",
            (ServerFlavor::Valkey, Self::Bloom) => "the valkey-bloom module",
            (ServerFlavor::Valkey, _) => "Redis 8 or Redis Stack (Valkey has no module for it)",
            (ServerFlavor::Redis, Self::Json) => "Redis 8, Redis Stack or the RedisJSON module",
            (ServerFlavor::Redis, Self::Search) => "Redis 8, Redis Stack or the RediSearch module",
            (ServerFlavor::Redis, Self::TimeSeries) => {
                "Redis 8, Redis Stack or the RedisTimeSeries module"
            }
            (ServerFlavor::Redis, _) => "Redis 8, Redis Stack or the RedisBloom module",
        };
        format!(
            "This server has no {prefix}.* commands: {} need {needs}",
            self.label()
        )
    }
}

/// Whether a server has one feature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureSupport {
    /// The feature.
    pub feature: ModuleFeature,
    /// True when its commands are there.
    pub available: bool,
    /// Why it is not available (or cannot be told); `None` when available.
    pub reason: Option<String>,
}

/// A loaded module from `MODULE LIST`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedModule {
    /// Module name.
    pub name: String,
    /// Version as the server reports it (for example `81001`).
    pub version: Option<String>,
}

/// The module features of a server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleReport {
    /// One entry per [`ModuleFeature::ALL`], in that order.
    pub features: Vec<FeatureSupport>,
    /// `MODULE LIST`; `None` when the user may not run it (or it is
    /// renamed away).
    pub modules: Option<Vec<LoadedModule>>,
    /// The `TYPE` names of the module keys the available features store
    /// ([`ModuleFeature::key_type`]), in feature order: the key types the
    /// browser's `SCAN ... TYPE` filter can offer on this server (Valkey
    /// refuses a type name it does not know).
    #[serde(default)]
    pub key_types: Vec<String>,
}

impl ModuleReport {
    /// Whether `feature` is available.
    #[must_use]
    pub fn has(&self, feature: ModuleFeature) -> bool {
        self.features
            .iter()
            .any(|f| f.feature == feature && f.available)
    }
}

/// A name and value of an info reply (`BF.INFO`, `FT.INFO`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoField {
    /// Field name as the server reports it.
    pub name: String,
    /// Value.
    pub value: Value,
}

/// A label of a time series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsLabel {
    /// Label name.
    pub name: String,
    /// Label value.
    pub value: String,
}

/// A compaction rule of a time series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsRule {
    /// Destination key, display text.
    pub dest_key: String,
    /// Bucket duration, ms.
    #[serde(with = "u64_text")]
    pub bucket_ms: u64,
    /// Aggregation (`avg`, `sum`, ...).
    pub aggregation: String,
    /// Bucket alignment timestamp (`0` when not set).
    #[serde(with = "i64_text")]
    pub align_ms: i64,
}

/// What `TS.ALTER` changes, as read: compared before an alter runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsMeta {
    /// Retention in ms; `0` keeps samples forever.
    #[serde(with = "u64_text")]
    pub retention_ms: u64,
    /// Labels in the order the server reports them.
    pub labels: Vec<TsLabel>,
    /// Duplicate policy (`block`, `first`, `last`, `min`, `max`, `sum`);
    /// `None` when the series uses the server default.
    pub duplicate_policy: Option<String>,
}

/// `TS.INFO` of a time series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsInfo {
    /// Samples in the series.
    #[serde(with = "u64_text")]
    pub total_samples: u64,
    /// First and last timestamps; `None` while the series is empty.
    #[serde(with = "crate::browser::opt_i64_text")]
    pub first_timestamp: Option<i64>,
    /// See `first_timestamp`.
    #[serde(with = "crate::browser::opt_i64_text")]
    pub last_timestamp: Option<i64>,
    /// Retention, labels and duplicate policy (what `TS.ALTER` changes).
    pub meta: TsMeta,
    /// The series this one is compacted from, display text.
    pub source_key: Option<String>,
    /// Compaction rules to other series.
    pub rules: Vec<TsRule>,
    /// Every scalar field of the reply (memory, chunks, ...), for display.
    pub fields: Vec<InfoField>,
}

/// A sample of a time series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TsSample {
    /// Timestamp, ms.
    #[serde(with = "i64_text")]
    pub timestamp: i64,
    /// Value: [`Value::Float`], or [`Value::Other`] with `nan`.
    pub value: Value,
}

/// A page of samples to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsRangeQuery {
    /// Lowest timestamp (ms, inclusive); `None` from the first sample.
    #[serde(with = "crate::browser::opt_i64_text", default)]
    pub from: Option<i64>,
    /// Highest timestamp (ms, inclusive); `None` to the last sample.
    #[serde(with = "crate::browser::opt_i64_text", default)]
    pub to: Option<i64>,
    /// Newest first.
    #[serde(default)]
    pub reverse: bool,
    /// Samples to read, clamped to 1..=[`MAX_PAGE_COUNT`].
    pub count: u32,
    /// The previous page's `next`; `None` for the first page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// A page of samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsRangePage {
    /// Samples in range order.
    pub samples: Vec<TsSample>,
    /// Cursor of the next page of the same range; `None` when done.
    pub next: Option<String>,
}

/// An item of a Top-K list with its count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopKItem {
    /// Item.
    pub item: Value,
    /// Estimated count.
    #[serde(with = "u64_text")]
    pub count: u64,
}

/// A read-only query of a probabilistic key, tagged by `op`. Inputs are
/// text or bytes ([`Value::Text`], [`Value::Bytes`]); numbers may also be
/// [`Value::Int`] or [`Value::Float`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum ModuleQuery {
    /// `BF.MEXISTS`: whether each item may be in the filter.
    BloomExists {
        /// Items.
        items: Vec<Value>,
    },
    /// `CF.MEXISTS`.
    CuckooExists {
        /// Items.
        items: Vec<Value>,
    },
    /// `CF.COUNT` per item.
    CuckooCount {
        /// Items.
        items: Vec<Value>,
    },
    /// `CMS.QUERY`: estimated counts.
    CmsQuery {
        /// Items.
        items: Vec<Value>,
    },
    /// `TOPK.QUERY`: whether each item is in the top list.
    TopkQuery {
        /// Items.
        items: Vec<Value>,
    },
    /// `TDIGEST.QUANTILE`: values at fractions 0 to 1.
    TdigestQuantile {
        /// Fractions.
        quantiles: Vec<Value>,
    },
    /// `TDIGEST.CDF`: fraction of observations at or below each value.
    TdigestCdf {
        /// Values.
        values: Vec<Value>,
    },
    /// `TDIGEST.RANK`: estimated rank of each value.
    TdigestRank {
        /// Values.
        values: Vec<Value>,
    },
    /// `TDIGEST.REVRANK`.
    TdigestRevRank {
        /// Values.
        values: Vec<Value>,
    },
    /// `TDIGEST.BYRANK`: value at each rank.
    TdigestByRank {
        /// Ranks (0 is the smallest).
        ranks: Vec<Value>,
    },
    /// `TDIGEST.TRIMMED_MEAN` between two fractions.
    TdigestTrimmedMean {
        /// Low fraction.
        low: Value,
        /// High fraction.
        high: Value,
    },
}

impl ModuleQuery {
    /// The key kind the query reads.
    #[must_use]
    pub fn kind(&self) -> ModuleKind {
        match self {
            Self::BloomExists { .. } => ModuleKind::Bloom,
            Self::CuckooExists { .. } | Self::CuckooCount { .. } => ModuleKind::Cuckoo,
            Self::CmsQuery { .. } => ModuleKind::CountMin,
            Self::TopkQuery { .. } => ModuleKind::TopK,
            Self::TdigestQuantile { .. }
            | Self::TdigestCdf { .. }
            | Self::TdigestRank { .. }
            | Self::TdigestRevRank { .. }
            | Self::TdigestByRank { .. }
            | Self::TdigestTrimmedMean { .. } => ModuleKind::TDigest,
        }
    }
}

/// One input of a query and what the server answered for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryRow {
    /// The input (for a trimmed mean, the two fractions as text).
    pub input: Value,
    /// The answer: [`Value::Bool`] for exists queries, a number otherwise
    /// (`nan`, `inf` and `-inf` as [`Value::Other`]).
    pub output: Value,
}

/// The answer of a [`ModuleQuery`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    /// The command line that ran (inputs shortened).
    pub command: String,
    /// One row per input.
    pub rows: Vec<QueryRow>,
}

/// An attribute (field) of a Search index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchAttribute {
    /// The hash field or JSON path.
    pub identifier: String,
    /// The name queries use (the alias after `AS`).
    pub attribute: String,
    /// `TEXT`, `TAG`, `NUMERIC`, `GEO`, `GEOSHAPE`, `VECTOR`, ...
    pub field_type: String,
    /// The options as `FT.CREATE` takes them, in its order, for example
    /// `["NOSTEM", "WEIGHT", "2", "SORTABLE"]` or, for a vector,
    /// `["HNSW", "6", "TYPE", "FLOAT32", ...]`.
    pub options: Vec<String>,
    /// Settings of `FT.INFO` Varve cannot give back to `FT.CREATE`; when
    /// any attribute has one, the index has no rebuilt definition.
    pub unknown: Vec<String>,
}

/// A Search index (`FT.INFO`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchIndex {
    /// Index name.
    pub name: String,
    /// The module that answered (its `FT.INFO` shape differs).
    pub engine: SearchEngine,
    /// `HASH` or `JSON`.
    pub key_type: Option<String>,
    /// Key prefixes the index follows (empty: every key).
    pub prefixes: Vec<String>,
    /// The `FILTER` expression.
    pub filter: Option<String>,
    /// The `LANGUAGE` when the server reports one.
    pub language: Option<String>,
    /// The `LANGUAGE_FIELD`.
    pub language_field: Option<String>,
    /// `INDEXALL ENABLE`: every key is indexed, also one without the
    /// attributes.
    pub indexes_all: bool,
    /// The `SCORE` (default document score).
    pub default_score: Option<String>,
    /// The `SCORE_FIELD`.
    pub score_field: Option<String>,
    /// The `PAYLOAD_FIELD`.
    pub payload_field: Option<String>,
    /// Index options (`NOOFFSETS`, `NOFREQS`, ...).
    pub index_options: Vec<String>,
    /// `STOPWORDS`, when the index has its own list.
    pub stopwords: Option<Vec<String>>,
    /// Attributes in schema order.
    pub attributes: Vec<SearchAttribute>,
    /// Documents indexed.
    #[serde(with = "crate::browser::opt_u64_text")]
    pub num_docs: Option<u64>,
    /// Documents that failed to index.
    #[serde(with = "crate::browser::opt_u64_text")]
    pub indexing_failures: Option<u64>,
    /// Fraction indexed (0 to 1) while a background scan runs.
    pub percent_indexed: Option<f64>,
    /// Scalar statistics of the reply (nested groups as `group.name`).
    pub stats: Vec<InfoField>,
    /// The definition as one `FT.CREATE` command line, quoted the way the
    /// console and `redis-cli` read it (for copying and running); `None`
    /// when `FT.INFO` holds settings that would not recreate the index (see
    /// `notes`; `raw` has the reply).
    pub ddl: Option<String>,
    /// The same command over several lines (one per attribute), for
    /// display only.
    pub ddl_display: Option<String>,
    /// What the rebuilt definition cannot carry, or why there is none.
    pub notes: Vec<String>,
    /// The `FT.INFO` reply as indented text.
    pub raw: String,
}

/// Signed 64-bit numbers as decimal strings (timestamps can exceed 2^53).
pub(crate) mod i64_text {
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Text {
        Number(i64),
        Text(String),
    }

    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        match Text::deserialize(deserializer)? {
            Text::Number(n) => Ok(n),
            Text::Text(text) => text.parse().map_err(|_| {
                serde::de::Error::custom(format!("not a signed 64-bit number: {text:?}"))
            }),
        }
    }
}

// ---- Reply parsing ----

/// The items of an array reply (RESP2 arrays; RESP3 maps flattened).
fn items(reply: &Resp) -> Vec<&Resp> {
    match reply {
        Resp::Array(items) | Resp::Set(items) => items.iter().collect(),
        Resp::Map(pairs) => pairs.iter().flat_map(|(k, v)| [k, v]).collect(),
        _ => Vec::new(),
    }
}

/// Name and value pairs of a flat `name, value, ...` reply.
pub(crate) fn info_pairs(reply: &Resp) -> Vec<(String, &Resp)> {
    let list = items(reply);
    list.chunks(2)
        .filter_map(|pair| match pair {
            [name, value] => Some((as_text(name)?, *value)),
            _ => None,
        })
        .collect()
}

fn find<'a>(pairs: &'a [(String, &'a Resp)], name: &str) -> Option<&'a Resp> {
    pairs.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
}

/// A scalar reply as a cell value; `None` for arrays.
fn scalar(reply: &Resp) -> Option<Value> {
    match reply {
        Resp::Array(_) | Resp::Set(_) | Resp::Map(_) => None,
        Resp::Nil => Some(Value::Null),
        Resp::Int(i) => Some(Value::Int(*i)),
        Resp::Double(f) => Some(double_value(*f)),
        other => {
            let text = as_text(other)?;
            Some(Value::Text(text))
        }
    }
}

/// Text of a reply, also for nil (`None`).
fn text_of(reply: Option<&Resp>) -> Option<String> {
    reply.and_then(as_text).filter(|t| !t.is_empty())
}

fn display_of(reply: &Resp) -> Option<String> {
    match reply {
        Resp::BulkString(bytes) => Some(crate::parse::key_display(bytes)),
        other => as_text(other),
    }
}

/// Flat `[a, b, a, b]` or nested `[[a, b], [a, b]]` pairs.
fn pair_list(reply: Option<&Resp>) -> Vec<(&Resp, &Resp)> {
    let Some(reply) = reply else {
        return Vec::new();
    };
    let list = items(reply);
    if !list.is_empty() && list.iter().all(|i| matches!(i, Resp::Array(_))) {
        return list
            .into_iter()
            .filter_map(|pair| match items(pair).as_slice() {
                [a, b, ..] => Some((*a, *b)),
                _ => None,
            })
            .collect();
    }
    list.chunks(2)
        .filter_map(|pair| match pair {
            [a, b] => Some((*a, *b)),
            _ => None,
        })
        .collect()
}

/// A float from a reply (`inf`, `nan` included).
fn number(reply: &Resp) -> Option<Value> {
    match reply {
        Resp::Int(i) => Some(Value::Int(*i)),
        Resp::Double(f) => Some(double_value(*f)),
        other => {
            let text = as_text(other)?;
            let lower = text.trim().to_ascii_lowercase();
            match lower.as_str() {
                "nan" | "-nan" => Some(Value::Other("nan".into())),
                "inf" | "+inf" | "infinity" => Some(Value::Other("inf".into())),
                "-inf" | "-infinity" => Some(Value::Other("-inf".into())),
                _ => lower.parse::<f64>().ok().map(double_value),
            }
        }
    }
}

/// The meta part of a `TS.INFO` reply.
pub(crate) fn ts_meta(pairs: &[(String, &Resp)]) -> TsMeta {
    TsMeta {
        retention_ms: find(pairs, "retentionTime")
            .and_then(as_int)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0),
        labels: pair_list(find(pairs, "labels"))
            .into_iter()
            .filter_map(|(name, value)| {
                Some(TsLabel {
                    name: as_text(name)?,
                    value: as_text(value).unwrap_or_default(),
                })
            })
            .collect(),
        duplicate_policy: text_of(find(pairs, "duplicatePolicy")).map(|p| p.to_ascii_lowercase()),
    }
}

/// Parses a `TS.INFO` reply.
///
/// # Errors
///
/// A protocol error when it lacks `totalSamples`.
pub(crate) fn parse_ts_info(reply: &Resp) -> Result<TsInfo, DriverError> {
    let pairs = info_pairs(reply);
    let total_samples = find(&pairs, "totalSamples")
        .and_then(as_int)
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| error::protocol("TS.INFO returned no totalSamples"))?;
    let stamp = |name: &str| {
        find(&pairs, name)
            .and_then(as_int)
            .filter(|_| total_samples > 0)
    };
    let rules = find(&pairs, "rules")
        .map(items)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|rule| {
            let parts = items(rule);
            Some(TsRule {
                dest_key: display_of(parts.first()?)?,
                bucket_ms: parts
                    .get(1)
                    .and_then(|r| as_int(r))
                    .and_then(|n| u64::try_from(n).ok())?,
                aggregation: parts.get(2).and_then(|r| as_text(r))?,
                align_ms: parts.get(3).and_then(|r| as_int(r)).unwrap_or(0),
            })
        })
        .collect();
    let fields = pairs
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "labels" | "rules"))
        .filter_map(|(name, value)| {
            Some(InfoField {
                name: name.clone(),
                value: scalar(value)?,
            })
        })
        .collect();
    Ok(TsInfo {
        total_samples,
        first_timestamp: stamp("firstTimestamp"),
        last_timestamp: stamp("lastTimestamp"),
        meta: ts_meta(&pairs),
        source_key: find(&pairs, "sourceKey")
            .and_then(display_of)
            .filter(|s| !s.is_empty()),
        rules,
        fields,
    })
}

/// Samples of a `TS.RANGE` reply.
fn parse_samples(reply: &Resp) -> Result<Vec<TsSample>, DriverError> {
    items(reply)
        .into_iter()
        .map(|sample| match items(sample).as_slice() {
            [timestamp, value] => Ok(TsSample {
                timestamp: as_int(timestamp)
                    .ok_or_else(|| error::protocol("a sample without a timestamp"))?,
                value: number(value)
                    .ok_or_else(|| error::protocol("a sample value that is not a number"))?,
            }),
            _ => Err(error::protocol("a sample that is not a pair")),
        })
        .collect()
}

/// Every name and value of a module info reply as fields.
fn info_fields(reply: &Resp) -> Vec<InfoField> {
    info_pairs(reply)
        .into_iter()
        .filter_map(|(name, value)| {
            Some(InfoField {
                name,
                value: scalar(value)
                    .or_else(|| to_value(value.clone(), &mut ValueBudget::new()).ok())?,
            })
        })
        .collect()
}

// ---- Search ----

/// Field flags of RediSearch's `FT.INFO`, all taken by `FT.CREATE`.
const FIELD_FLAGS: &[&str] = &[
    "NOSTEM",
    "WITHSUFFIXTRIE",
    "INDEXEMPTY",
    "INDEXMISSING",
    "CASESENSITIVE",
    "SORTABLE",
    "UNF",
    "NOINDEX",
];

/// Vector settings `FT.CREATE` takes, by their `FT.INFO` names (RediSearch
/// in lower case, valkey-search in its `index` and `algorithm` maps).
fn vector_param(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "data_type" => "TYPE",
        "dim" | "dimensions" => "DIM",
        "distance_metric" => "DISTANCE_METRIC",
        "initial_cap" | "capacity" => "INITIAL_CAP",
        "block_size" => "BLOCK_SIZE",
        "m" => "M",
        "ef_construction" => "EF_CONSTRUCTION",
        "ef_runtime" => "EF_RUNTIME",
        "epsilon" => "EPSILON",
        "compression" => "COMPRESSION",
        "construction_window_size" => "CONSTRUCTION_WINDOW_SIZE",
        "graph_max_degree" => "GRAPH_MAX_DEGREE",
        "search_window_size" => "SEARCH_WINDOW_SIZE",
        "training_threshold" => "TRAINING_THRESHOLD",
        "reduce" => "REDUCE",
        _ => return None,
    })
}

/// Statistics valkey-search reports per attribute (not settings).
const VALKEY_FIELD_STATS: &[&str] = &["user_indexed_memory", "size"];

/// Which search module answered `FT.INFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchEngine {
    /// RediSearch (Redis 8, Redis Stack): flat attribute lists with flags.
    RediSearch,
    /// valkey-search: every attribute setting a `name value` pair, vector
    /// settings nested in `index`.
    ValkeySearch,
}

/// What one attribute's `FT.INFO` entry says, in `FT.CREATE` terms.
#[derive(Debug, Default)]
struct FieldDef {
    identifier: Option<String>,
    attribute: Option<String>,
    field_type: Option<String>,
    flags: Vec<String>,
    weight: Option<String>,
    separator: Option<String>,
    phonetic: Option<String>,
    coord_system: Option<String>,
    vector_algorithm: Option<String>,
    vector_params: Vec<(&'static str, String)>,
    /// Settings Varve does not know how to give back to `FT.CREATE`.
    unknown: Vec<String>,
}

/// SVS-VAMANA compressions `FT.CREATE` takes (their canonical spelling).
const SVS_COMPRESSIONS: &[&str] = &[
    "LVQ8",
    "LVQ4",
    "LVQ4x4",
    "LVQ4x8",
    "LeanVec4x8",
    "LeanVec8x8",
];

impl FieldDef {
    /// Turns what `FT.INFO` says about vector compression into what
    /// `FT.CREATE` takes: `NO_COMPRESSION` (and its training threshold) is
    /// left out, a compression `FT.CREATE` names is kept, anything else
    /// (`GlobalSQ8`, which the server reports for a requested LVQ on some
    /// builds) cannot be given back and makes the definition unusable.
    fn normalize_vector(&mut self) {
        let Some(at) = self
            .vector_params
            .iter()
            .position(|(name, _)| *name == "COMPRESSION")
        else {
            return;
        };
        let value = self.vector_params[at].1.clone();
        if value.eq_ignore_ascii_case("NO_COMPRESSION") {
            self.vector_params.retain(|(name, _)| {
                !matches!(*name, "COMPRESSION" | "TRAINING_THRESHOLD" | "REDUCE")
            });
            return;
        }
        match SVS_COMPRESSIONS
            .iter()
            .find(|c| c.eq_ignore_ascii_case(&value))
        {
            Some(canonical) => self.vector_params[at].1 = (*canonical).to_string(),
            None => self.unknown.push(format!(
                "compression {value} (FT.CREATE takes {})",
                SVS_COMPRESSIONS.join(", ")
            )),
        }
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// The type-specific options and flags in the order `FT.CREATE` parses
    /// them: type settings first, then `SORTABLE [UNF]`, `NOINDEX` last.
    fn options(&self) -> Vec<String> {
        let mut out = Vec::new();
        let push_flag = |out: &mut Vec<String>, name: &str| {
            if self.flag(name) {
                out.push(name.to_string());
            }
        };
        match self.field_type.as_deref().unwrap_or_default() {
            "VECTOR" => {
                if let Some(algorithm) = &self.vector_algorithm {
                    out.push(algorithm.clone());
                    out.push((self.vector_params.len() * 2).to_string());
                    for (name, value) in &self.vector_params {
                        out.push((*name).to_string());
                        out.push(value.clone());
                    }
                }
                push_flag(&mut out, "INDEXMISSING");
                return out;
            }
            "GEOSHAPE" => {
                if let Some(system) = &self.coord_system {
                    out.push(system.to_ascii_uppercase());
                }
            }
            "TEXT" => {
                push_flag(&mut out, "NOSTEM");
                if let Some(weight) = &self.weight {
                    out.push("WEIGHT".into());
                    out.push(weight.clone());
                }
                if let Some(phonetic) = &self.phonetic {
                    out.push("PHONETIC".into());
                    out.push(phonetic.clone());
                }
                push_flag(&mut out, "WITHSUFFIXTRIE");
            }
            "TAG" => {
                // JSON tags have no separator (FT.INFO says `""`).
                if let Some(separator) = self.separator.as_ref().filter(|s| !s.is_empty()) {
                    out.push("SEPARATOR".into());
                    out.push(separator.clone());
                }
                push_flag(&mut out, "CASESENSITIVE");
                push_flag(&mut out, "WITHSUFFIXTRIE");
            }
            _ => {}
        }
        push_flag(&mut out, "INDEXEMPTY");
        push_flag(&mut out, "INDEXMISSING");
        if self.flag("SORTABLE") {
            out.push("SORTABLE".into());
            // NUMERIC and GEO sort values are never normalized; FT.INFO
            // still says UNF for them, which FT.CREATE refuses there.
            if self.flag("UNF") && matches!(self.field_type.as_deref(), Some("TEXT" | "TAG")) {
                out.push("UNF".into());
            }
        }
        push_flag(&mut out, "NOINDEX");
        out
    }
}

/// Parses a RediSearch attribute: `identifier`, `attribute`, `type`,
/// valued options (`WEIGHT`, `SEPARATOR`, `PHONETIC`), lower-case `name
/// value` settings (vectors, `coord_system`) and bare flags.
fn parse_redisearch_field(tokens: &[String]) -> FieldDef {
    let mut def = FieldDef::default();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        let next = tokens.get(i + 1).cloned();
        let pair = |slot: &mut Option<String>| *slot = next.clone();
        match token {
            "identifier" => pair(&mut def.identifier),
            "attribute" => pair(&mut def.attribute),
            "type" => pair(&mut def.field_type),
            "WEIGHT" => pair(&mut def.weight),
            "SEPARATOR" => pair(&mut def.separator),
            "PHONETIC" => pair(&mut def.phonetic),
            "coord_system" => pair(&mut def.coord_system),
            "algorithm" => pair(&mut def.vector_algorithm),
            // Vector settings come in both cases (`dim 4`, `M 16`).
            other
                if def.field_type.as_deref() == Some("VECTOR")
                    && next.is_some()
                    && vector_param(other).is_some() =>
            {
                if let Some(name) = vector_param(other) {
                    def.vector_params
                        .push((name, next.clone().unwrap_or_default()));
                }
            }
            flag if FIELD_FLAGS.contains(&flag) => {
                def.flags.push(flag.to_string());
                i += 1;
                continue;
            }
            other if other.chars().any(|c| c.is_ascii_lowercase()) && next.is_some() => {
                match vector_param(other) {
                    Some(name) if def.field_type.as_deref() == Some("VECTOR") => {
                        def.vector_params
                            .push((name, next.clone().unwrap_or_default()));
                    }
                    _ => def
                        .unknown
                        .push(format!("{other} {}", next.clone().unwrap_or_default())),
                }
            }
            other => {
                def.unknown.push(other.to_string());
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    def
}

/// Parses a valkey-search attribute: every setting a `name value` pair;
/// a vector's settings in `index` (and its algorithm in `algorithm`).
fn parse_valkey_field(reply: &Resp) -> FieldDef {
    let mut def = FieldDef::default();
    for (name, value) in info_pairs(reply) {
        match name.as_str() {
            "identifier" => def.identifier = as_text(value),
            "attribute" => def.attribute = as_text(value),
            "type" => def.field_type = as_text(value),
            "SEPARATOR" => def.separator = as_text(value),
            "CASESENSITIVE" => {
                if as_int(value) == Some(1) || as_text(value).as_deref() == Some("1") {
                    def.flags.push("CASESENSITIVE".into());
                }
            }
            "index" => {
                for (setting, inner) in info_pairs(value) {
                    match setting.as_str() {
                        "algorithm" => {
                            for (key, v) in info_pairs(inner) {
                                if key == "name" {
                                    def.vector_algorithm = as_text(v);
                                } else if let Some(param) = vector_param(&key) {
                                    def.vector_params
                                        .push((param, as_text(v).unwrap_or_default()));
                                } else {
                                    def.unknown.push(format!("algorithm {key}"));
                                }
                            }
                        }
                        // A statistic, not a setting.
                        "size" => {}
                        other => match vector_param(other) {
                            Some(param) => {
                                def.vector_params
                                    .push((param, as_text(inner).unwrap_or_default()));
                            }
                            None => def.unknown.push(format!("index {other}")),
                        },
                    }
                }
            }
            stat if VALKEY_FIELD_STATS.contains(&stat) => {}
            other => def.unknown.push(other.to_string()),
        }
    }
    // FT.CREATE wants TYPE, DIM and DISTANCE_METRIC first; the order of
    // the others does not matter.
    let rank = |name: &str| match name {
        "TYPE" => 0,
        "DIM" => 1,
        "DISTANCE_METRIC" => 2,
        _ => 3,
    };
    def.vector_params.sort_by_key(|(name, _)| rank(name));
    def
}

/// True when a reply's text holds U+FFFD: bytes that were not valid UTF-8
/// (simple strings are decoded with replacement characters).
fn raw_has_replacement(reply: &Resp) -> bool {
    match reply {
        Resp::Array(list) | Resp::Set(list) => list.iter().any(raw_has_replacement),
        Resp::Map(pairs) => pairs
            .iter()
            .any(|(k, v)| raw_has_replacement(k) || raw_has_replacement(v)),
        Resp::BulkString(bytes) => std::str::from_utf8(bytes).is_err(),
        Resp::SimpleString(text) => text.contains('\u{FFFD}'),
        _ => false,
    }
}

/// The text of a reply as indented lines, for a raw `FT.INFO` view.
fn raw_text(reply: &Resp, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    match reply {
        Resp::Array(list) | Resp::Set(list) => {
            let mut iter = list.iter().peekable();
            while let Some(item) = iter.next() {
                let is_name = matches!(item, Resp::BulkString(_) | Resp::SimpleString(_));
                match iter.peek() {
                    Some(next @ (Resp::Array(_) | Resp::Set(_) | Resp::Map(_))) if is_name => {
                        out.push_str(&format!("{pad}{}:\n", as_text(item).unwrap_or_default()));
                        raw_text(next, depth + 1, out);
                        iter.next();
                    }
                    Some(next) if is_name => {
                        let value = scalar(next)
                            .map_or_else(String::new, |v| v.to_plain_text().unwrap_or_default());
                        out.push_str(&format!(
                            "{pad}{} {value}\n",
                            as_text(item).unwrap_or_default()
                        ));
                        iter.next();
                    }
                    _ => match item {
                        Resp::Array(_) | Resp::Set(_) | Resp::Map(_) => {
                            out.push_str(&format!("{pad}-\n"));
                            raw_text(item, depth + 1, out);
                        }
                        other => out.push_str(&format!(
                            "{pad}{}\n",
                            scalar(other)
                                .and_then(|v| v.to_plain_text())
                                .unwrap_or_default()
                        )),
                    },
                }
            }
        }
        Resp::Map(pairs) => {
            for (k, v) in pairs {
                out.push_str(&format!("{pad}{}:\n", as_text(k).unwrap_or_default()));
                raw_text(v, depth + 1, out);
            }
        }
        other => out.push_str(&format!(
            "{pad}{}\n",
            scalar(other)
                .and_then(|v| v.to_plain_text())
                .unwrap_or_default()
        )),
    }
}

fn flatten_stats(prefix: &str, reply: &Resp, out: &mut Vec<InfoField>) {
    for (name, value) in info_pairs(reply) {
        let full = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}.{name}")
        };
        match scalar(value) {
            Some(value) => out.push(InfoField { name: full, value }),
            None if prefix.is_empty() => flatten_stats(&full, value, out),
            None => {}
        }
    }
}

/// `index_definition` entries the rebuild gives back to `FT.CREATE`.
const KNOWN_DEFINITION: &[&str] = &[
    "key_type",
    "prefixes",
    "filter",
    "default_language",
    "language",
    "language_field",
    "default_score",
    "score_field",
    "payload_field",
    "indexes_all",
];

/// Settings `FT.CREATE` takes that `FT.INFO` never reports, so a rebuilt
/// definition cannot carry them.
pub const UNRECOVERABLE_OPTIONS: &str = "TEMPORARY, SKIPINITIALSCAN and the PHONETIC matcher of \
     TEXT attributes (and LANGUAGE when it is the default, english)";

/// Parses an `FT.INFO` reply of index `name`.
pub(crate) fn parse_search_index(name: &str, reply: &Resp) -> SearchIndex {
    let pairs = info_pairs(reply);
    let definition = find(&pairs, "index_definition")
        .map(info_pairs)
        .unwrap_or_default();
    let attribute_list: Vec<&Resp> = find(&pairs, "attributes").map(items).unwrap_or_default();
    // valkey-search gives each attribute as name/value pairs only, with
    // statistics such as `user_indexed_memory`.
    let engine = if attribute_list.iter().any(|a| {
        info_pairs(a)
            .iter()
            .any(|(n, _)| VALKEY_FIELD_STATS.contains(&n.as_str()) || n == "index")
    }) || find(&pairs, "backfill_in_progress").is_some()
    {
        SearchEngine::ValkeySearch
    } else {
        SearchEngine::RediSearch
    };
    let key_type = text_of(find(&definition, "key_type"));
    let prefixes: Vec<String> = find(&definition, "prefixes")
        .map(|p| match p {
            Resp::Array(_) | Resp::Set(_) | Resp::Map(_) => items(p),
            single => vec![single],
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(as_text)
        .filter(|p| !p.is_empty())
        .collect();
    let filter = text_of(find(&definition, "filter"));
    let language = text_of(find(&definition, "default_language"))
        .or_else(|| text_of(find(&definition, "language")))
        .or_else(|| text_of(find(&pairs, "language")));
    let language_field = text_of(find(&definition, "language_field"));
    let default_score = text_of(find(&definition, "default_score"));
    let score_field = text_of(find(&definition, "score_field"));
    let payload_field = text_of(find(&definition, "payload_field"));
    // `INDEXALL ENABLE` (index every key, also those without the
    // attributes) shows as `indexes_all true`.
    let indexes_all = text_of(find(&definition, "indexes_all")).as_deref() == Some("true");
    let unknown_definition: Vec<String> = definition
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| !KNOWN_DEFINITION.contains(&name.as_str()))
        .collect();
    let index_options: Vec<String> = find(&pairs, "index_options")
        .map(items)
        .unwrap_or_default()
        .into_iter()
        .filter_map(as_text)
        .filter(|o| !o.is_empty())
        .collect();
    let stopwords = find(&pairs, "stopwords_list").map(|list| {
        items(list)
            .into_iter()
            .filter_map(as_text)
            .collect::<Vec<_>>()
    });
    let mut fields: Vec<FieldDef> = attribute_list
        .iter()
        .map(|a| match engine {
            SearchEngine::ValkeySearch => parse_valkey_field(a),
            SearchEngine::RediSearch => parse_redisearch_field(
                &items(a)
                    .into_iter()
                    .map(|t| as_text(t).unwrap_or_default())
                    .collect::<Vec<_>>(),
            ),
        })
        .collect();
    for field in &mut fields {
        field.normalize_vector();
    }
    let attributes: Vec<SearchAttribute> = fields
        .iter()
        .filter_map(|f| {
            let identifier = f.identifier.clone()?;
            Some(SearchAttribute {
                attribute: f.attribute.clone().unwrap_or_else(|| identifier.clone()),
                identifier,
                field_type: f.field_type.clone().unwrap_or_default(),
                options: f.options(),
                unknown: f.unknown.clone(),
            })
        })
        .collect();
    let mut stats = Vec::new();
    for (field, value) in &pairs {
        if matches!(
            field.as_str(),
            "index_name"
                | "index_options"
                | "index_definition"
                | "attributes"
                | "stopwords_list"
                | "field statistics"
        ) {
            continue;
        }
        match scalar(value) {
            Some(value) => stats.push(InfoField {
                name: field.clone(),
                value,
            }),
            None => flatten_stats(field, value, &mut stats),
        }
    }
    let stat = |name: &str| {
        stats
            .iter()
            .find(|f| f.name == name)
            .and_then(|f| f.value.to_plain_text())
    };
    let num_docs = stat("num_docs").and_then(|t| t.parse().ok());
    let indexing_failures = stat("hash_indexing_failures")
        .or_else(|| stat("Index Errors.indexing failures"))
        .and_then(|t| t.parse().ok());
    let percent_indexed = stat("percent_indexed")
        .or_else(|| stat("backfill_complete_percent"))
        .and_then(|t| t.parse().ok());

    // The command, as arguments.
    let mut head: Vec<String> = vec!["FT.CREATE".into(), name.to_string()];
    if let Some(key_type) = &key_type {
        head.push("ON".into());
        head.push(key_type.clone());
    }
    if !prefixes.is_empty() {
        head.push("PREFIX".into());
        head.push(prefixes.len().to_string());
        head.extend(prefixes.iter().cloned());
    }
    if let Some(filter) = &filter {
        head.push("FILTER".into());
        head.push(filter.clone());
    }
    if let Some(language) = language
        .as_ref()
        .filter(|l| !l.eq_ignore_ascii_case("english"))
    {
        head.push("LANGUAGE".into());
        head.push(language.clone());
    }
    if let Some(field) = language_field
        .as_ref()
        .filter(|f| f.as_str() != "__language")
    {
        head.push("LANGUAGE_FIELD".into());
        head.push(field.clone());
    }
    if let Some(score) = default_score.as_ref().filter(|s| s.as_str() != "1") {
        head.push("SCORE".into());
        head.push(score.clone());
    }
    if let Some(field) = score_field.as_ref().filter(|f| f.as_str() != "__score") {
        head.push("SCORE_FIELD".into());
        head.push(field.clone());
    }
    if let Some(field) = payload_field.as_ref().filter(|f| f.as_str() != "__payload") {
        head.push("PAYLOAD_FIELD".into());
        head.push(field.clone());
    }
    // NOHL is implied by NOOFFSETS and reported with it.
    let offsets_off = index_options.iter().any(|o| o == "NOOFFSETS");
    let mut unknown_options = Vec::new();
    for option in &index_options {
        match option.as_str() {
            "NOHL" if offsets_off => {}
            "MAXTEXTFIELDS" | "NOOFFSETS" | "NOHL" | "NOFIELDS" | "NOFREQS" => {
                head.push(option.clone());
            }
            other => unknown_options.push(other.to_string()),
        }
    }
    if let Some(words) = &stopwords {
        head.push("STOPWORDS".into());
        head.push(words.len().to_string());
        head.extend(words.iter().cloned());
    }
    if indexes_all {
        head.push("INDEXALL".into());
        head.push("ENABLE".into());
    }
    head.push("SCHEMA".into());
    let lines: Vec<Vec<String>> = attributes
        .iter()
        .map(|a| {
            let mut line = vec![a.identifier.clone()];
            if a.attribute != a.identifier {
                line.push("AS".into());
                line.push(a.attribute.clone());
            }
            line.push(a.field_type.clone());
            line.extend(a.options.iter().cloned());
            line
        })
        .collect();

    let mut notes = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    for a in &attributes {
        if !a.unknown.is_empty() {
            problems.push(format!(
                "attribute {} has settings Varve cannot rebuild: {}",
                a.attribute,
                a.unknown.join(", ")
            ));
        }
        if a.field_type == "VECTOR" && !a.options.first().is_some_and(|o| o != "INDEXMISSING") {
            problems.push(format!("vector attribute {} has no algorithm", a.attribute));
        }
        if a.field_type.is_empty() {
            problems.push(format!("attribute {} has no type", a.attribute));
        }
    }
    if attributes.len() != attribute_list.len() {
        problems.push("an attribute has no name".into());
    }
    if !unknown_definition.is_empty() {
        problems.push(format!(
            "index settings Varve cannot rebuild: {}",
            unknown_definition.join(", ")
        ));
    }
    // Names, prefixes or settings that are not valid UTF-8 were read with
    // replacement characters: their bytes are lost.
    if raw_has_replacement(reply) {
        problems.push(
            "a name, prefix or setting is not valid UTF-8, so its exact bytes cannot be shown"
                .into(),
        );
    }
    if !unknown_options.is_empty() {
        problems.push(format!(
            "index options Varve cannot rebuild: {}",
            unknown_options.join(", ")
        ));
    }
    let (ddl, ddl_display) = if problems.is_empty() {
        let quote = |args: &[String]| varve_redis_syntax::join_args(args);
        let mut all = head.clone();
        for line in &lines {
            all.extend(line.iter().cloned());
        }
        let mut display = quote(&head);
        for line in &lines {
            display.push_str("\n  ");
            display.push_str(&quote(line));
        }
        notes.push(match engine {
            SearchEngine::RediSearch => format!(
                "Rebuilt from FT.INFO, which does not report every setting: \
                 {UNRECOVERABLE_OPTIONS} cannot be recovered."
            ),
            SearchEngine::ValkeySearch => "Rebuilt from valkey-search's FT.INFO: attribute \
                                           types, separators, case sensitivity and vector \
                                           settings; settings it does not report cannot be \
                                           recovered."
                .to_string(),
        });
        (Some(quote(&all)), Some(display))
    } else {
        notes.push(format!(
            "No FT.CREATE command is shown, because it would not recreate this index: {}. The \
             raw FT.INFO reply is shown instead.",
            problems.join("; ")
        ));
        (None, None)
    };
    let mut raw = String::new();
    raw_text(reply, 0, &mut raw);
    SearchIndex {
        name: name.to_string(),
        engine,
        key_type,
        prefixes,
        filter,
        language,
        language_field,
        indexes_all,
        default_score,
        score_field,
        payload_field,
        index_options,
        stopwords,
        attributes,
        num_docs,
        indexing_failures,
        percent_indexed,
        stats,
        ddl,
        ddl_display,
        notes,
        raw,
    }
}

// ---- Commands ----

/// The bytes of a query input.
fn input_bytes(value: &Value, what: &str) -> Result<Vec<u8>, DriverError> {
    match value {
        Value::Text(text) => Ok(text.as_bytes().to_vec()),
        Value::Bytes(bytes) => Ok(bytes.clone()),
        Value::Int(i) => Ok(i.to_string().into_bytes()),
        Value::Float(f) => Ok(f.to_string().into_bytes()),
        _ => Err(invalid(format!("The {what} must be text or bytes"))),
    }
}

/// A finite number argument.
fn number_arg(value: &Value, what: &str) -> Result<String, DriverError> {
    let parsed = match value {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Text(text) | Value::Other(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    match parsed {
        Some(f) if f.is_finite() => Ok(match value {
            Value::Text(text) | Value::Other(text) => text.trim().to_string(),
            _ => format!("{f}"),
        }),
        _ => Err(invalid(format!("The {what} must be a number"))),
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> DriverError {
    DriverError::new(ErrorKind::Query, message).with_code("INVALID_RANGE")
}

/// One command as its arguments.
type CommandArgs = Vec<Vec<u8>>;

/// The commands of a query, and its inputs for the rows.
fn query_command(
    key: &[u8],
    query: &ModuleQuery,
) -> Result<(Vec<CommandArgs>, Vec<Value>), DriverError> {
    let one = |name: &str, args: Vec<Vec<u8>>| {
        let mut command = vec![name.as_bytes().to_vec(), key.to_vec()];
        command.extend(args);
        command
    };
    let items = |list: &[Value], what: &str| -> Result<Vec<Vec<u8>>, DriverError> {
        if list.is_empty() {
            return Err(invalid(format!("Give at least one {what}")));
        }
        if list.len() > MAX_QUERY_INPUTS {
            return Err(invalid(format!(
                "A query takes at most {MAX_QUERY_INPUTS} inputs"
            )));
        }
        list.iter().map(|v| input_bytes(v, what)).collect()
    };
    let numbers = |list: &[Value], what: &str| -> Result<Vec<Vec<u8>>, DriverError> {
        if list.is_empty() {
            return Err(invalid(format!("Give at least one {what}")));
        }
        if list.len() > MAX_QUERY_INPUTS {
            return Err(invalid(format!(
                "A query takes at most {MAX_QUERY_INPUTS} inputs"
            )));
        }
        list.iter()
            .map(|v| number_arg(v, what).map(String::into_bytes))
            .collect()
    };
    let rows = |list: &[Value]| list.to_vec();
    Ok(match query {
        ModuleQuery::BloomExists { items: list } => {
            (vec![one("BF.MEXISTS", items(list, "item")?)], rows(list))
        }
        ModuleQuery::CuckooExists { items: list } => {
            (vec![one("CF.MEXISTS", items(list, "item")?)], rows(list))
        }
        ModuleQuery::CuckooCount { items: list } => (
            items(list, "item")?
                .into_iter()
                .map(|item| one("CF.COUNT", vec![item]))
                .collect(),
            rows(list),
        ),
        ModuleQuery::CmsQuery { items: list } => {
            (vec![one("CMS.QUERY", items(list, "item")?)], rows(list))
        }
        ModuleQuery::TopkQuery { items: list } => {
            (vec![one("TOPK.QUERY", items(list, "item")?)], rows(list))
        }
        ModuleQuery::TdigestQuantile { quantiles } => {
            for q in quantiles {
                let f: f64 = number_arg(q, "fraction")?.parse().unwrap_or(-1.0);
                if !(0.0..=1.0).contains(&f) {
                    return Err(invalid("A quantile is a fraction from 0 to 1"));
                }
            }
            (
                vec![one("TDIGEST.QUANTILE", numbers(quantiles, "fraction")?)],
                rows(quantiles),
            )
        }
        ModuleQuery::TdigestCdf { values } => (
            vec![one("TDIGEST.CDF", numbers(values, "value")?)],
            rows(values),
        ),
        ModuleQuery::TdigestRank { values } => (
            vec![one("TDIGEST.RANK", numbers(values, "value")?)],
            rows(values),
        ),
        ModuleQuery::TdigestRevRank { values } => (
            vec![one("TDIGEST.REVRANK", numbers(values, "value")?)],
            rows(values),
        ),
        ModuleQuery::TdigestByRank { ranks } => {
            for rank in ranks {
                let text = number_arg(rank, "rank")?;
                if text.parse::<u64>().is_err() {
                    return Err(invalid("A rank is a whole number of 0 or more"));
                }
            }
            (
                vec![one("TDIGEST.BYRANK", numbers(ranks, "rank")?)],
                rows(ranks),
            )
        }
        ModuleQuery::TdigestTrimmedMean { low, high } => {
            let lo = number_arg(low, "low fraction")?;
            let hi = number_arg(high, "high fraction")?;
            let (l, h): (f64, f64) = (lo.parse().unwrap_or(-1.0), hi.parse().unwrap_or(-1.0));
            if !(0.0..=1.0).contains(&l) || !(0.0..=1.0).contains(&h) || l >= h {
                return Err(invalid(
                    "The trimmed mean needs two fractions from 0 to 1, the low one first",
                ));
            }
            (
                vec![one(
                    "TDIGEST.TRIMMED_MEAN",
                    vec![lo.clone().into_bytes(), hi.clone().into_bytes()],
                )],
                vec![Value::Text(format!("{lo} to {hi}"))],
            )
        }
    })
}

/// The output of one row from the reply items.
fn row_output(query: &ModuleQuery, reply: &Resp) -> Value {
    match query {
        ModuleQuery::BloomExists { .. }
        | ModuleQuery::CuckooExists { .. }
        | ModuleQuery::TopkQuery { .. } => match as_int(reply) {
            Some(n) => Value::Bool(n != 0),
            None => number(reply).unwrap_or(Value::Null),
        },
        _ => number(reply).unwrap_or(Value::Null),
    }
}

impl RedisSession {
    /// Which module features the server has (see the module
    /// documentation). Read on the session connection.
    ///
    /// # Errors
    ///
    /// `MULTI_OPEN` while a console `MULTI` block is open, connection
    /// errors, Cancelled.
    pub async fn modules(&self) -> Result<ModuleReport, DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        let flavor = call.details().flavor;
        let mut info = redis::cmd("COMMAND");
        info.arg("INFO");
        for feature in ModuleFeature::ALL {
            info.arg(feature.probe_command());
        }
        let probed = match call.send(&info).await? {
            Resp::Array(entries) if entries.len() == ModuleFeature::ALL.len() => Some(
                entries
                    .iter()
                    .map(|e| !matches!(e, Resp::Nil))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let modules = match call.send(redis::cmd("MODULE").arg("LIST")).await? {
            Resp::Array(entries) => Some(
                entries
                    .iter()
                    .filter_map(|entry| {
                        let pairs = info_pairs(entry);
                        Some(LoadedModule {
                            name: text_of(find(&pairs, "name"))?,
                            version: find(&pairs, "ver").and_then(as_text),
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let features = ModuleFeature::ALL
            .iter()
            .enumerate()
            .map(|(index, feature)| {
                let available = match (&probed, &modules) {
                    (Some(found), _) => found.get(index).copied().unwrap_or(false),
                    (None, Some(list)) => list.iter().any(|m| {
                        feature
                            .module_names()
                            .contains(&m.name.to_ascii_lowercase().as_str())
                    }),
                    (None, None) => false,
                };
                let reason = (!available).then(|| match (&probed, &modules) {
                    // The UI (packages/ui/src/api/redis.ts `hasModules`) reads this
                    // "Varve cannot tell" prefix as "unknown, keep the panel".
                    (None, None) => format!(
                        "Varve cannot tell whether this server has {}: this user may not run \
                         COMMAND INFO or MODULE LIST",
                        feature.label()
                    ),
                    _ => feature.missing_reason(flavor),
                });
                FeatureSupport {
                    feature: *feature,
                    available,
                    reason,
                }
            })
            .collect::<Vec<FeatureSupport>>();
        let key_types = features
            .iter()
            .filter(|f| f.available)
            .filter_map(|f| f.feature.key_type(flavor))
            .map(str::to_owned)
            .collect();
        Ok(ModuleReport {
            features,
            modules,
            key_types,
        })
    }

    /// A page of samples of a time series; `None` when the key does not
    /// exist.
    ///
    /// # Errors
    ///
    /// Query errors (`WRONGTYPE`, an invalid cursor with code
    /// `INVALID_RANGE`), connection errors, Cancelled.
    pub async fn ts_range(
        &self,
        key: &[u8],
        query: &TsRangeQuery,
    ) -> Result<Option<TsRangePage>, DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        if !key_exists(&mut call, key).await? {
            return Ok(None);
        }
        ts_page(&mut call, key, query).await.map(Some)
    }

    /// A read-only query of a probabilistic key; `None` when the key does
    /// not exist.
    ///
    /// # Errors
    ///
    /// Invalid inputs (`INVALID_RANGE`), a key of another type
    /// (`WRONGTYPE`), connection errors, Cancelled.
    pub async fn module_query(
        &self,
        key: &[u8],
        query: &ModuleQuery,
    ) -> Result<Option<QueryResult>, DriverError> {
        let (commands, inputs) = query_command(key, query)?;
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        let type_reply = call.query(redis::cmd("TYPE").arg(key)).await?;
        let Some(key_type) = as_text(&type_reply).and_then(|t| KeyType::from_type_name(&t)) else {
            return Ok(None);
        };
        if ModuleKind::of(&key_type) != Some(query.kind()) {
            return Err(DriverError::new(
                ErrorKind::Query,
                format!(
                    "The key is a {}, not a {}",
                    key_type.type_name(),
                    query.kind().label()
                ),
            )
            .with_code("WRONGTYPE"));
        }
        let mut pipe = redis::pipe();
        for command in &commands {
            pipe.add_command(crate::session::to_cmd(command));
        }
        let replies = call.send_pipeline(&pipe).await?;
        let mut outputs = Vec::new();
        for reply in &replies {
            if let Resp::ServerError(e) = reply {
                return Err(query_error(e));
            }
            match reply {
                Resp::Array(_) | Resp::Set(_) => outputs.extend(items(reply).into_iter().cloned()),
                other => outputs.push(other.clone()),
            }
        }
        let rows = inputs
            .into_iter()
            .zip(outputs.iter())
            .map(|(input, output)| QueryRow {
                input,
                output: row_output(query, output),
            })
            .collect();
        let command = commands
            .iter()
            .take(1)
            .map(|c| crate::edit::display_line(c))
            .chain((commands.len() > 1).then(|| format!("(and {} more)", commands.len() - 1)))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(Some(QueryResult { command, rows }))
    }

    /// The names of the Search indexes (`FT._LIST`).
    ///
    /// # Errors
    ///
    /// Unsupported when the server has no Search commands, server errors
    /// (`NOPERM`), connection errors, Cancelled.
    pub async fn search_indexes(&self) -> Result<Vec<String>, DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        // Read on a side connection with Varve's own reply reader: RediSearch
        // sends names as simple strings, and a name that is not valid UTF-8
        // would make the protocol library drop the session connection.
        match crate::measure::side_query(&mut call, &[b"FT._LIST"], SEARCH_REPLY_LIMIT).await {
            Err(e) if is_unknown_command(&e) => Err(no_search()),
            Err(e) => Err(e),
            Ok(reply) => {
                let mut names: Vec<String> =
                    items(&reply).into_iter().filter_map(as_text).collect();
                names.sort();
                Ok(names)
            }
        }
    }

    /// One Search index (`FT.INFO`); `None` when it does not exist. Read
    /// like [`Self::search_indexes`]; text that is not valid UTF-8 shows
    /// with replacement characters, and such an index has no rebuilt
    /// definition.
    ///
    /// # Errors
    ///
    /// As [`Self::search_indexes`].
    pub async fn search_index(&self, name: &str) -> Result<Option<SearchIndex>, DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        match crate::measure::side_query(
            &mut call,
            &[b"FT.INFO", name.as_bytes()],
            SEARCH_REPLY_LIMIT,
        )
        .await
        {
            Err(e) if is_unknown_command(&e) => Err(no_search()),
            Err(e) if e.code.is_some() => {
                let text = e.message.to_ascii_lowercase();
                if text.contains("not found")
                    || text.contains("unknown index")
                    || text.contains("no such index")
                {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
            Ok(reply) => Ok(Some(parse_search_index(name, &reply))),
        }
    }
}

/// Most bytes of strings of an `FT._LIST` or `FT.INFO` reply (16 MiB).
const SEARCH_REPLY_LIMIT: u64 = 16 * 1024 * 1024;

fn is_unknown_command(e: &DriverError) -> bool {
    e.code.is_some() && e.message.to_ascii_lowercase().contains("unknown command")
}

fn no_search() -> DriverError {
    DriverError::new(
        ErrorKind::Unsupported,
        "This server has no Search commands (FT.*): Search needs Redis 8, Redis Stack, the \
         RediSearch module or valkey-search",
    )
    .with_code("SEARCH_UNAVAILABLE")
}

async fn key_exists(call: &mut Call<'_>, key: &[u8]) -> Result<bool, DriverError> {
    Ok(as_int(&call.query(redis::cmd("EXISTS").arg(key)).await?) == Some(1))
}

/// A TimeSeries cursor: the next timestamp to read from (forward) or to
/// (reverse).
fn ts_cursor(cursor: &str) -> Result<i64, DriverError> {
    cursor
        .parse()
        .map_err(|_| invalid(format!("Invalid page cursor {cursor:?}")))
}

async fn ts_page(
    call: &mut Call<'_>,
    key: &[u8],
    query: &TsRangeQuery,
) -> Result<TsRangePage, DriverError> {
    let count = query.count.clamp(1, MAX_PAGE_COUNT);
    let mut from = query.from;
    let mut to = query.to;
    if let (Some(a), Some(b)) = (from, to)
        && a > b
    {
        return Err(invalid("The range starts after it ends"));
    }
    if let Some(cursor) = query.cursor.as_deref().filter(|c| !c.is_empty()) {
        let at = ts_cursor(cursor)?;
        if query.reverse {
            to = Some(to.map_or(at, |t| t.min(at)));
        } else {
            from = Some(from.map_or(at, |f| f.max(at)));
        }
    }
    let bound = |b: Option<i64>, low: bool| match b {
        Some(n) => n.to_string(),
        None if low => "-".into(),
        None => "+".into(),
    };
    let mut cmd = redis::cmd(if query.reverse {
        "TS.REVRANGE"
    } else {
        "TS.RANGE"
    });
    cmd.arg(key)
        .arg(bound(from, true))
        .arg(bound(to, false))
        .arg("COUNT")
        .arg(u64::from(count) + 1);
    let reply = call.query(&cmd).await?;
    let mut samples = parse_samples(&reply)?;
    let more = samples.len() > count as usize;
    samples.truncate(count as usize);
    let next = if more {
        samples.last().and_then(|s| {
            if query.reverse {
                s.timestamp.checked_sub(1)
            } else {
                s.timestamp.checked_add(1)
            }
            .map(|n| n.to_string())
        })
    } else {
        None
    };
    Ok(TsRangePage { samples, next })
}

/// The browser page of a module key: `(total, next cursor, data)`.
pub(crate) async fn read_module(
    call: &mut Call<'_>,
    key: &[u8],
    kind: ModuleKind,
    cursor: &str,
    count: u32,
) -> Result<Option<(Option<u64>, Option<String>, ValueData)>, DriverError> {
    // TS.INFO counts as a change of the key for every client's WATCH (it
    // opens the key for writing); with a console WATCH on this key in any
    // session of the app, reading it would make that transaction fail.
    if kind == ModuleKind::TimeSeries && call.key_is_watched(key) {
        return Err(DriverError::new(
            ErrorKind::Query,
            "A console WATCH holds this key (in this tab or another tab of this connection). \
             Reading a time series' settings (TS.INFO) counts as a change of the key for Redis, \
             so it would make that transaction fail",
        )
        .with_code(WATCH_OPEN));
    }
    let info = match call.send(redis::cmd(kind.info_command()).arg(key)).await? {
        Resp::ServerError(e) if e.details().is_some_and(|d| d.contains("does not exist")) => {
            return Ok(None);
        }
        Resp::ServerError(e) => return Err(query_error(&e)),
        reply => reply,
    };
    if kind == ModuleKind::TimeSeries {
        let info = parse_ts_info(&info)?;
        let page = ts_page(
            call,
            key,
            &TsRangeQuery {
                from: None,
                to: None,
                reverse: false,
                count,
                cursor: (!cursor.is_empty()).then(|| cursor.to_string()),
            },
        )
        .await?;
        return Ok(Some((
            Some(info.total_samples),
            page.next,
            ValueData::TimeSeries {
                info,
                samples: page.samples,
            },
        )));
    }
    let mut fields = info_fields(&info);
    let mut top = Vec::new();
    match kind {
        ModuleKind::TopK => {
            let reply = call
                .query(redis::cmd("TOPK.LIST").arg(key).arg("WITHCOUNT"))
                .await?;
            for pair in items(&reply).chunks(2) {
                if let [item, count] = pair {
                    top.push(TopKItem {
                        item: match item {
                            Resp::BulkString(bytes) => bytes_value(bytes.clone()),
                            other => Value::Text(as_text(other).unwrap_or_default()),
                        },
                        count: as_int(count)
                            .and_then(|n| u64::try_from(n).ok())
                            .unwrap_or(0),
                    });
                }
            }
        }
        ModuleKind::TDigest => {
            let mut pipe = redis::pipe();
            pipe.cmd("TDIGEST.MIN").arg(key);
            pipe.cmd("TDIGEST.MAX").arg(key);
            let replies = call.send_pipeline(&pipe).await?;
            for (name, reply) in ["Min", "Max"].into_iter().zip(replies.iter()) {
                if let Some(value) = number(reply) {
                    fields.push(InfoField {
                        name: name.into(),
                        value,
                    });
                }
            }
        }
        _ => {}
    }
    Ok(Some((
        None,
        None,
        ValueData::Probabilistic {
            module_kind: kind,
            fields,
            top,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(text: &str) -> Resp {
        Resp::BulkString(text.as_bytes().to_vec())
    }

    #[test]
    fn feature_key_types_per_flavor() {
        assert_eq!(
            ModuleFeature::Bloom.key_type(ServerFlavor::Redis),
            Some("MBbloom--")
        );
        assert_eq!(
            ModuleFeature::Bloom.key_type(ServerFlavor::Valkey),
            Some("bloomfltr")
        );
        assert_eq!(ModuleFeature::Search.key_type(ServerFlavor::Redis), None);
        // Every key type maps back to the reader of its kind (JSON has the JSON editor).
        for flavor in [ServerFlavor::Redis, ServerFlavor::Valkey] {
            for feature in ModuleFeature::ALL {
                let Some(name) = feature.key_type(flavor) else {
                    continue;
                };
                let parsed = KeyType::from_type_name(name);
                if feature == ModuleFeature::Json {
                    assert_eq!(parsed, Some(KeyType::Json));
                } else {
                    assert!(ModuleKind::from_type_name(name).is_some(), "{name}");
                }
            }
        }
        let report = ModuleReport {
            features: Vec::new(),
            modules: None,
            key_types: vec!["TSDB-TYPE".into()],
        };
        let json = serde_json::to_value(&report).unwrap_or_default();
        assert_eq!(json["keyTypes"], serde_json::json!(["TSDB-TYPE"]));
    }

    #[test]
    fn module_kinds_from_type_names() {
        assert_eq!(
            ModuleKind::from_type_name("TSDB-TYPE"),
            Some(ModuleKind::TimeSeries)
        );
        assert_eq!(
            ModuleKind::from_type_name("bloomfltr"),
            Some(ModuleKind::Bloom)
        );
        assert_eq!(ModuleKind::from_type_name("ReJSON-RL"), None);
        assert_eq!(
            ModuleKind::of(&KeyType::Other("TDIS-TYPE".into())),
            Some(ModuleKind::TDigest)
        );
        assert_eq!(ModuleKind::of(&KeyType::Hash), None);
    }

    #[test]
    fn ts_info_parses_flat_and_nested_labels() -> Result<(), DriverError> {
        let reply = Resp::Array(vec![
            bulk("totalSamples"),
            Resp::Int(2),
            bulk("firstTimestamp"),
            Resp::Int(1000),
            bulk("lastTimestamp"),
            Resp::Int(2000),
            bulk("retentionTime"),
            Resp::Int(60000),
            bulk("duplicatePolicy"),
            bulk("BLOCK"),
            bulk("labels"),
            Resp::Array(vec![
                Resp::Array(vec![bulk("sensor"), bulk("a")]),
                Resp::Array(vec![bulk("room"), bulk("b")]),
            ]),
            bulk("sourceKey"),
            Resp::Nil,
            bulk("rules"),
            Resp::Array(vec![Resp::Array(vec![
                bulk("ts:avg"),
                Resp::Int(60000),
                bulk("AVG"),
                Resp::Int(0),
            ])]),
        ]);
        let info = parse_ts_info(&reply)?;
        assert_eq!(info.total_samples, 2);
        assert_eq!(info.first_timestamp, Some(1000));
        assert_eq!(info.meta.retention_ms, 60000);
        assert_eq!(info.meta.duplicate_policy.as_deref(), Some("block"));
        assert_eq!(info.meta.labels.len(), 2);
        assert_eq!(info.meta.labels[1].name, "room");
        assert_eq!(info.source_key, None);
        assert_eq!(info.rules[0].aggregation, "AVG");
        assert!(info.fields.iter().any(|f| f.name == "retentionTime"));
        let empty = parse_ts_info(&Resp::Array(vec![
            bulk("totalSamples"),
            Resp::Int(0),
            bulk("firstTimestamp"),
            Resp::Int(0),
            bulk("labels"),
            Resp::Array(vec![bulk("a"), bulk("b")]),
        ]))?;
        assert_eq!(empty.first_timestamp, None);
        assert_eq!(empty.meta.labels[0].value, "b");
        assert!(parse_ts_info(&Resp::Array(vec![])).is_err());
        Ok(())
    }

    #[test]
    fn samples_keep_nan() -> Result<(), DriverError> {
        let reply = Resp::Array(vec![
            Resp::Array(vec![Resp::Int(1), Resp::SimpleString("1.5".into())]),
            Resp::Array(vec![Resp::Int(2), Resp::SimpleString("NaN".into())]),
        ]);
        let samples = parse_samples(&reply)?;
        assert_eq!(samples[0].value, Value::Float(1.5));
        assert_eq!(samples[1].value, Value::Other("nan".into()));
        let json = serde_json::to_value(&samples[0]).map_err(|e| error::protocol(e.to_string()))?;
        assert_eq!(json["timestamp"], "1");
        Ok(())
    }

    fn ft_info() -> Resp {
        let attrs = |tokens: &[&str]| Resp::Array(tokens.iter().map(|t| bulk(t)).collect());
        Resp::Array(vec![
            bulk("index_name"),
            bulk("idx1"),
            bulk("index_options"),
            attrs(&["NOOFFSETS", "NOHL"]),
            bulk("index_definition"),
            attrs(&[
                "key_type",
                "HASH",
                "prefixes",
                "user:",
                "default_score",
                "0.5",
                "language_field",
                "lang",
                "score_field",
                "__score",
                "payload_field",
                "pl",
                "indexes_all",
                "false",
            ]),
            bulk("attributes"),
            Resp::Array(vec![
                attrs(&[
                    "identifier",
                    "name",
                    "attribute",
                    "name",
                    "type",
                    "TEXT",
                    "WEIGHT",
                    "2",
                    "SORTABLE",
                    "UNF",
                    "NOSTEM",
                ]),
                attrs(&[
                    "identifier",
                    "$.tags",
                    "attribute",
                    "tags",
                    "type",
                    "TAG",
                    "SEPARATOR",
                    ";",
                    "SORTABLE",
                    "UNF",
                    "CASESENSITIVE",
                    "INDEXEMPTY",
                ]),
                attrs(&[
                    "identifier",
                    "vec",
                    "attribute",
                    "vec",
                    "type",
                    "VECTOR",
                    "algorithm",
                    "HNSW",
                    "data_type",
                    "FLOAT32",
                    "dim",
                    "4",
                    "distance_metric",
                    "COSINE",
                    "M",
                    "16",
                ]),
                attrs(&[
                    "identifier",
                    "n",
                    "attribute",
                    "n",
                    "type",
                    "NUMERIC",
                    "SORTABLE",
                    "UNF",
                    "NOINDEX",
                ]),
                attrs(&[
                    "identifier",
                    "g",
                    "attribute",
                    "g",
                    "type",
                    "GEOSHAPE",
                    "coord_system",
                    "FLAT",
                ]),
            ]),
            bulk("num_docs"),
            Resp::Int(3),
            bulk("gc_stats"),
            attrs(&["bytes_collected", "0"]),
            bulk("Index Errors"),
            Resp::Array(vec![bulk("indexing failures"), Resp::Int(1)]),
            bulk("percent_indexed"),
            bulk("1"),
        ])
    }

    #[test]
    fn search_index_rebuilds_its_definition() -> Result<(), varve_driver::DriverError> {
        let index = parse_search_index("idx1", &ft_info());
        assert_eq!(index.engine, SearchEngine::RediSearch);
        assert_eq!(index.key_type.as_deref(), Some("HASH"));
        assert_eq!(index.prefixes, vec!["user:"]);
        assert_eq!(index.num_docs, Some(3));
        assert_eq!(index.indexing_failures, Some(1));
        assert_eq!(index.percent_indexed, Some(1.0));
        assert_eq!(index.attributes.len(), 5);
        assert_eq!(index.attributes[1].identifier, "$.tags");
        // FT.CREATE order: type settings first, SORTABLE [UNF] and NOINDEX
        // last; GEOSHAPE takes its system as a bare word; UNF only for
        // TEXT and TAG; default fields (`__score`) left out.
        let display = index.ddl_display.clone().unwrap_or_default();
        assert_eq!(
            display,
            "FT.CREATE idx1 ON HASH PREFIX 1 user: LANGUAGE_FIELD lang SCORE 0.5 PAYLOAD_FIELD pl \
             NOOFFSETS SCHEMA\n  \
             name TEXT NOSTEM WEIGHT 2 SORTABLE UNF\n  \
             $.tags AS tags TAG SEPARATOR ; CASESENSITIVE INDEXEMPTY SORTABLE UNF\n  \
             vec VECTOR HNSW 8 TYPE FLOAT32 DIM 4 DISTANCE_METRIC COSINE M 16\n  \
             n NUMERIC SORTABLE NOINDEX\n  \
             g GEOSHAPE FLAT"
        );
        // The copyable form is one console line with the same arguments.
        let ddl = index.ddl.clone().unwrap_or_default();
        assert!(!ddl.contains('\n'));
        let parsed = crate::parse_script(&ddl)?;
        assert_eq!(parsed.len(), 1);
        let words: Vec<String> = display.split_whitespace().map(str::to_string).collect();
        let args: Vec<String> = parsed[0]
            .args
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        assert_eq!(args, words);
        assert!(index.notes[0].contains("TEMPORARY"));
        assert!(index.raw.contains("index_definition:"));
        assert!(
            index
                .stats
                .iter()
                .any(|f| f.name == "gc_stats.bytes_collected")
        );
        Ok(())
    }

    #[test]
    fn quoting_survives_the_console_parser() -> Result<(), varve_driver::DriverError> {
        let attrs = |tokens: &[&str]| Resp::Array(tokens.iter().map(|t| bulk(t)).collect());
        let reply = Resp::Array(vec![
            bulk("index_definition"),
            attrs(&["key_type", "HASH", "filter", "@title:\"a b\" | 'x'"]),
            bulk("attributes"),
            Resp::Array(vec![attrs(&[
                "identifier",
                "my field",
                "attribute",
                "f",
                "type",
                "TAG",
                "SEPARATOR",
                "\"",
            ])]),
        ]);
        let index = parse_search_index("idx \"q\"", &reply);
        let ddl = index.ddl.unwrap_or_default();
        let parsed = crate::parse_script(&ddl)?;
        let args: Vec<String> = parsed[0]
            .args
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "FT.CREATE",
                "idx \"q\"",
                "ON",
                "HASH",
                "FILTER",
                "@title:\"a b\" | 'x'",
                "SCHEMA",
                "my field",
                "AS",
                "f",
                "TAG",
                "SEPARATOR",
                "\"",
            ]
        );
        Ok(())
    }

    fn one_attribute(definition: &[&str], attribute: &[&str]) -> Resp {
        let attrs = |tokens: &[&str]| Resp::Array(tokens.iter().map(|t| bulk(t)).collect());
        Resp::Array(vec![
            bulk("index_definition"),
            attrs(definition),
            bulk("attributes"),
            Resp::Array(vec![attrs(attribute)]),
        ])
    }

    #[test]
    fn svs_compression_takes_the_form_ft_create_accepts() {
        let svs = |compression: &str, more: &[&str]| {
            let mut tokens = vec![
                "identifier",
                "v",
                "attribute",
                "v",
                "type",
                "VECTOR",
                "algorithm",
                "SVS-VAMANA",
                "data_type",
                "FLOAT32",
                "dim",
                "16",
                "distance_metric",
                "L2",
                "compression",
            ];
            tokens.push(compression);
            tokens.extend_from_slice(more);
            parse_search_index("i", &one_attribute(&["key_type", "HASH"], &tokens))
        };
        // No compression: left out, with its training threshold.
        let none = svs("NO_COMPRESSION", &["training_threshold", "10240"]);
        assert_eq!(
            none.ddl.as_deref(),
            Some(
                "FT.CREATE i ON HASH SCHEMA v VECTOR SVS-VAMANA 6 TYPE FLOAT32 DIM 16 \
                 DISTANCE_METRIC L2"
            )
        );
        // A compression FT.CREATE names: kept in its spelling.
        let lvq = svs("lvq8", &["training_threshold", "10240"]);
        assert!(
            lvq.ddl
                .as_deref()
                .is_some_and(|d| d.ends_with("COMPRESSION LVQ8 TRAINING_THRESHOLD 10240")),
            "{:?}",
            lvq.ddl
        );
        // What the server reports for a requested LVQ on some builds.
        let sq8 = svs("GlobalSQ8", &["training_threshold", "10240"]);
        assert_eq!(sq8.ddl, None);
        assert!(
            sq8.notes[0].contains("compression GlobalSQ8"),
            "{:?}",
            sq8.notes
        );
    }

    #[test]
    fn indexall_is_written_back_and_unknown_settings_block_the_rebuild() {
        let all = parse_search_index(
            "i",
            &one_attribute(
                &["key_type", "HASH", "indexes_all", "true"],
                &["identifier", "t", "attribute", "t", "type", "TEXT"],
            ),
        );
        assert!(all.indexes_all);
        assert_eq!(
            all.ddl.as_deref(),
            Some("FT.CREATE i ON HASH INDEXALL ENABLE SCHEMA t TEXT")
        );
        let off = parse_search_index(
            "i",
            &one_attribute(
                &["key_type", "HASH", "indexes_all", "false"],
                &["identifier", "t", "attribute", "t", "type", "TEXT"],
            ),
        );
        assert!(!off.indexes_all);
        assert_eq!(
            off.ddl.as_deref(),
            Some("FT.CREATE i ON HASH SCHEMA t TEXT")
        );
        let future = parse_search_index(
            "i",
            &one_attribute(
                &["key_type", "HASH", "future_setting", "1"],
                &["identifier", "t", "attribute", "t", "type", "TEXT"],
            ),
        );
        assert_eq!(future.ddl, None);
        assert!(future.notes[0].contains("future_setting"));
    }

    #[test]
    fn text_that_is_not_utf8_gives_no_definition() {
        let reply = Resp::Array(vec![
            bulk("index_definition"),
            Resp::Array(vec![
                bulk("key_type"),
                bulk("HASH"),
                bulk("prefixes"),
                Resp::Array(vec![Resp::SimpleString("p\u{FFFD}:".into())]),
            ]),
            bulk("attributes"),
            Resp::Array(vec![Resp::Array(
                ["identifier", "t", "attribute", "t", "type", "TEXT"]
                    .iter()
                    .map(|t| bulk(t))
                    .collect(),
            )]),
        ]);
        let index = parse_search_index("i", &reply);
        assert_eq!(index.ddl, None);
        assert!(index.notes[0].contains("UTF-8"), "{:?}", index.notes);
    }

    #[test]
    fn unknown_settings_give_no_definition() {
        let attrs = |tokens: &[&str]| Resp::Array(tokens.iter().map(|t| bulk(t)).collect());
        let reply = Resp::Array(vec![
            bulk("index_definition"),
            attrs(&["key_type", "HASH"]),
            bulk("attributes"),
            Resp::Array(vec![attrs(&[
                "identifier",
                "t",
                "attribute",
                "t",
                "type",
                "TEXT",
                "FUTUREFLAG",
            ])]),
        ]);
        let index = parse_search_index("idx", &reply);
        assert_eq!(index.ddl, None);
        assert_eq!(index.ddl_display, None);
        assert!(index.notes[0].contains("FUTUREFLAG"), "{:?}", index.notes);
        assert!(index.raw.contains("FUTUREFLAG"));
    }

    #[test]
    fn valkey_search_info_is_read_in_its_own_shape() {
        let attrs = |tokens: &[&str]| Resp::Array(tokens.iter().map(|t| bulk(t)).collect());
        let vector = Resp::Array(vec![
            bulk("identifier"),
            bulk("v"),
            bulk("attribute"),
            bulk("v"),
            bulk("user_indexed_memory"),
            Resp::Int(0),
            bulk("type"),
            bulk("VECTOR"),
            bulk("index"),
            Resp::Array(vec![
                bulk("capacity"),
                Resp::Int(10240),
                bulk("dimensions"),
                Resp::Int(2),
                bulk("distance_metric"),
                bulk("L2"),
                bulk("size"),
                bulk("0"),
                bulk("data_type"),
                bulk("FLOAT32"),
                bulk("algorithm"),
                Resp::Array(vec![
                    bulk("name"),
                    bulk("HNSW"),
                    bulk("m"),
                    Resp::Int(16),
                    bulk("ef_construction"),
                    Resp::Int(200),
                ]),
            ]),
        ]);
        let tag = Resp::Array(vec![
            bulk("identifier"),
            bulk("t"),
            bulk("attribute"),
            bulk("t"),
            bulk("user_indexed_memory"),
            Resp::Int(0),
            bulk("type"),
            bulk("TAG"),
            bulk("SEPARATOR"),
            bulk(";"),
            bulk("CASESENSITIVE"),
            bulk("0"),
            bulk("size"),
            bulk("0"),
        ]);
        let reply = Resp::Array(vec![
            bulk("index_definition"),
            Resp::Array(vec![
                bulk("key_type"),
                bulk("HASH"),
                bulk("prefixes"),
                attrs(&["p:"]),
                bulk("default_score"),
                bulk("1"),
            ]),
            bulk("attributes"),
            Resp::Array(vec![vector, tag]),
            bulk("backfill_in_progress"),
            bulk("0"),
            bulk("language"),
            bulk("english"),
        ]);
        let index = parse_search_index("vi", &reply);
        assert_eq!(index.engine, SearchEngine::ValkeySearch);
        assert_eq!(
            index.ddl.as_deref(),
            Some(
                "FT.CREATE vi ON HASH PREFIX 1 p: SCHEMA v VECTOR HNSW 12 TYPE FLOAT32 DIM 2 \
                 DISTANCE_METRIC L2 INITIAL_CAP 10240 M 16 EF_CONSTRUCTION 200 t TAG SEPARATOR ;"
            )
        );
        // CASESENSITIVE 0 is a value there, not a flag.
        assert_eq!(index.attributes[1].options, ["SEPARATOR", ";"]);
        // The note fits the engine: no RediSearch-only settings named.
        assert!(
            index.notes[0].contains("valkey-search"),
            "{:?}",
            index.notes
        );
        assert!(!index.notes[0].contains("PHONETIC"));
    }

    #[test]
    fn queries_validate_inputs() {
        let text = |s: &str| Value::Text(s.into());
        assert!(query_command(b"k", &ModuleQuery::BloomExists { items: vec![] }).is_err());
        assert!(
            query_command(
                b"k",
                &ModuleQuery::TdigestQuantile {
                    quantiles: vec![text("1.5")]
                }
            )
            .is_err()
        );
        assert!(
            query_command(
                b"k",
                &ModuleQuery::TdigestTrimmedMean {
                    low: text("0.9"),
                    high: text("0.1")
                }
            )
            .is_err()
        );
        let (commands, inputs) = query_command(
            b"k",
            &ModuleQuery::CuckooCount {
                items: vec![text("a"), text("b")],
            },
        )
        .unwrap_or_default();
        assert_eq!(commands.len(), 2);
        assert_eq!(inputs.len(), 2);
        let (commands, _) = query_command(
            b"k",
            &ModuleQuery::TdigestByRank {
                ranks: vec![Value::Int(0), text("3")],
            },
        )
        .unwrap_or_default();
        assert_eq!(
            commands[0],
            vec![
                b"TDIGEST.BYRANK".to_vec(),
                b"k".to_vec(),
                b"0".to_vec(),
                b"3".to_vec()
            ]
        );
    }

    #[test]
    fn exists_answers_are_booleans() {
        let query = ModuleQuery::BloomExists { items: vec![] };
        assert_eq!(row_output(&query, &Resp::Int(1)), Value::Bool(true));
        let query = ModuleQuery::TdigestCdf { values: vec![] };
        assert_eq!(
            row_output(&query, &Resp::SimpleString("inf".into())),
            Value::Other("inf".into())
        );
    }
}
