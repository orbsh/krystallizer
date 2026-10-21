//! Configuration loading (ADR-0007): one unified KDL document,
//! `krystallizer.kdl`, one top-level node per subsystem. The file's
//! LOCATION is the assembly site's decision (the binary reads the one
//! next to itself, or the one `--config` names); this module only knows
//! its shape. Each node is a struct here; secrets are NEVER in the file
//! — a node that needs one names the environment variable that holds
//! it, and the process resolves it at call time. Parsing goes through
//! knus; callers never see knus types.

use knus::ast::{SpannedNode, Value};
use knus::decode::Context;
use knus::errors::DecodeError;
use knus::traits::{DecodeScalar, ErrorSpan};
use knus::Decode;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};

/// Root of `krystallizer.kdl`. New subsystems are new top-level nodes
/// here (and new fields here) — never new config files.
#[derive(Decode, Debug, Clone)]
pub struct KrystallizerConfig {
    /// `storage { ... }` — the database's home on disk.
    #[knus(child)]
    pub storage: StorageConfig,
    /// `embedding { ... }` — the built-in local embedding model
    /// (ADR-0003: in-process, no endpoint, no key).
    #[knus(child)]
    pub embedding: EmbeddingConfig,
    /// `compression { ... }` — the Phase 2.6 policy engine's numbers
    /// (ADR-0008 §4: every threshold is a parameter, never a constant).
    #[knus(child)]
    pub compression: CompressionConfig,
    /// `logs { ... }` — the structured log's ceiling (ADR-0011 §5: the
    /// table is disposable diagnostics, so it is bounded).
    #[knus(child)]
    pub logs: LogsConfig,
}

/// Where the database lives: one directory, one lock file (ADR-0002 — k10r
/// always owns its Fjall directory).
///
/// ```kdl
/// storage {
///     data-dir "$HOME/.local/share/krystallizer/data"
/// }
/// ```
///
/// It is configuration and not a flag because it is the service's own state:
/// a process manager must be able to start the service without reproducing a
/// command line, and the value must not be whatever directory the process
/// happened to start from.
#[derive(Decode, Debug, Clone)]
pub struct StorageConfig {
    /// The database directory (created if absent). `$HOME` is allowed — see
    /// [`expand_home`].
    #[knus(child, unwrap(argument))]
    pub data_dir: String,
}

impl StorageConfig {
    /// The directory in force, `$HOME` expanded.
    pub fn data_dir(&self) -> Result<PathBuf, ConfigError> {
        expand_home(&self.data_dir)
    }
}

/// The one placeholder a configured path may carry: `$HOME`.
///
/// A path value is deployment data — this machine's home directory is not a
/// property of the software — so the file states `$HOME/...` and the process
/// expands it. Only a LEADING `$HOME` that ends the value or a path segment is
/// a placeholder: `$HOMELESS` is a directory literally named that, and any
/// other `$FOO` is literal text (an unknown placeholder is never silently
/// expanded to nothing).
pub const HOME_PLACEHOLDER: &str = "$HOME";

/// Expand a leading `$HOME` in a configured path. An unset `HOME` is an error,
/// not a directory called `$HOME`: the service must not create one.
pub fn expand_home(raw: &str) -> Result<PathBuf, ConfigError> {
    let home = std::env::var("HOME").ok().filter(|home| !home.is_empty());
    expand_home_with(raw, home.as_deref())
}

/// The expansion itself, with the home directory STATED. The environment is read
/// once, in [`expand_home`], so a test can state the value instead of mutating a
/// process-wide variable every other test shares.
fn expand_home_with(raw: &str, home: Option<&str>) -> Result<PathBuf, ConfigError> {
    let Some(rest) = raw.strip_prefix(HOME_PLACEHOLDER) else {
        return Ok(PathBuf::from(raw));
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return Ok(PathBuf::from(raw));
    }
    let home = home.ok_or_else(|| ConfigError::HomeUnset {
        raw: raw.to_string(),
    })?;
    let home = PathBuf::from(home);
    Ok(if rest.is_empty() {
        home
    } else {
        home.join(rest.trim_start_matches('/'))
    })
}

/// The structured log's ceiling (ADR-0011 §5). Every append past it deletes
/// the oldest rows, so the table never becomes a second storage problem.
///
/// ```kdl
/// logs {
///     max-rows 10000
/// }
/// ```
#[derive(Decode, Debug, Clone)]
pub struct LogsConfig {
    /// Rows kept. Must equal [`crate::log::DEFAULT_MAX_ROWS`] in the shipped
    /// file — the same no-drift rule the compression numbers follow.
    #[knus(child, unwrap(argument))]
    pub max_rows: usize,
}

/// The compression policy engine's numbers (ADR-0008 §4). Tuning is
/// deferred until feedback labels exist — with no labels, fitting the
/// policy to traces fits noise — so these are defaults a deployment
/// overrides, not constants in code.
///
/// ```kdl
/// compression {
///     context-bytes 131072
///     budget-percent 50
///     cache-ttl-ms 300000
///     floor-bytes 2048
/// }
/// ```
#[derive(Decode, Debug, Clone)]
pub struct CompressionConfig {
    /// The consumer's model window in BYTES (k10r holds no tokenizer for
    /// another model, so bytes are the honest unit).
    #[knus(child, unwrap(argument))]
    pub context_bytes: usize,
    /// Fire at this share of the window — a mid band, not 90%.
    #[knus(child, unwrap(argument))]
    pub budget_percent: u8,
    /// The provider's prompt-cache lifetime.
    #[knus(child, unwrap(argument))]
    pub cache_ttl_ms: u64,
    /// Below this the view is too small for a summary to buy anything.
    #[knus(child, unwrap(argument))]
    pub floor_bytes: usize,
}

impl CompressionConfig {
    /// The engine's parameter set.
    pub fn params(&self) -> crate::compress::Params {
        crate::compress::Params {
            context_bytes: self.context_bytes,
            budget_percent: self.budget_percent,
            cache_ttl_ms: self.cache_ttl_ms,
            floor_bytes: self.floor_bytes,
        }
    }
}

/// WHICH embedding engine the deployment runs, and where (ADR-0003). Both modes
/// produce what the store needs — one vector per text, UNIT length, at the declared
/// width — so the seam is the model's, not the transport's.
///
/// The node is TAGGED by its `mode` child: the tag picks the variant, and that
/// variant's own struct reads the rest of the node. A field of the other mode is
/// therefore not something a list of names has to remember to refuse — it is an
/// unknown child of a struct that never had it, so the decode fails at that line.
///
/// ```kdl
/// // in this process (the default; no endpoint, no key)
/// embedding {
///     mode "local"
///     model-dir "$HOME/.local/share/krystallizer/models/bge-small-zh-v1.5"
///     dimensions 512
///     metric "cosine"
/// }
///
/// // an OpenAI-shaped embeddings service (`POST {base-url}/embeddings`)
/// embedding {
///     mode "remote"
///     base-url "http://gpu-box:8000/v1"
///     model "bge-small-zh-v1.5"
///     api-key-env "KRYSTALLIZER_EMBEDDING_KEY"   // the VARIABLE's name, never the key
///     timeout-ms 10000
///     dimensions 512
///     metric "cosine"
/// }
/// ```
#[derive(Debug, Clone)]
pub enum EmbeddingConfig {
    /// In-process: the model's directory, and the width that model emits.
    Local(LocalEmbedding),
    /// An OpenAI-shaped embeddings service, and the width it must answer with.
    Remote(RemoteEmbedding),
}

/// `mode "local"`: the artifact directory is the only thing a deployment states.
#[derive(Decode, Debug, Clone)]
pub struct LocalEmbedding {
    /// Vector width the store declares for a row's vector frame.
    #[knus(child, unwrap(argument))]
    pub dimensions: u32,
    /// Ranking metric downstream; both modes emit unit-length vectors, so `cosine`
    /// is an inner product.
    #[knus(child, unwrap(argument))]
    pub metric: String,
    /// The artifact directory. A deployment parameter, so the environment overrides
    /// it — see `resolved_model_dir`.
    #[knus(child, unwrap(argument))]
    pub model_dir: String,
}

/// `mode "remote"`: a SERVICE, not an artifact.
#[derive(Decode, Debug, Clone)]
pub struct RemoteEmbedding {
    /// Vector width the store declares; a service answering with another width is a
    /// boot failure, because two vector spaces in one table is not survivable.
    #[knus(child, unwrap(argument))]
    pub dimensions: u32,
    /// Ranking metric downstream (see [`LocalEmbedding::metric`]).
    #[knus(child, unwrap(argument))]
    pub metric: String,
    /// The service's base URL, WITHOUT the `/embeddings` suffix (the client appends
    /// it, so the file names a deployment, not a route).
    #[knus(child, unwrap(argument))]
    pub base_url: String,
    /// The model NAME the service should use.
    #[knus(child, unwrap(argument))]
    pub model: String,
    /// The environment variable holding the bearer token. Absent = no
    /// `Authorization` header (a self-hosted service behind a private network).
    #[knus(child, unwrap(argument))]
    pub api_key_env: Option<String>,
    /// Per-request ceiling. Absent = [`DEFAULT_TIMEOUT_MS`].
    #[knus(child, unwrap(argument))]
    pub timeout_ms: Option<u64>,
}

/// The tag child's name, in one place: the decode reads it and the error spells it.
pub const MODE_NODE: &str = "mode";
/// The tag's first word: the built-in model, in this process.
pub const MODE_LOCAL: &str = "local";
/// The tag's second word: an embeddings service over HTTP.
pub const MODE_REMOTE: &str = "remote";

/// The tag picks the variant, then the variant's struct decodes the WHOLE node.
///
/// Written by hand because knus selects an enum variant by NODE NAME, and this node's
/// name is `embedding` (ADR-0007: one top-level node per subsystem) — the choice lives
/// in the tag, not in the node. The `mode` child is removed before the variant sees the
/// node, so the tag is stated once, in the file, and never restated per variant.
impl<S: ErrorSpan> Decode<S> for EmbeddingConfig {
    fn decode_node(node: &SpannedNode<S>, ctx: &mut Context<S>) -> Result<Self, DecodeError<S>> {
        let (tag, tag_value, rest) = split_tag(node, ctx)?;
        match tag.as_str() {
            MODE_LOCAL => Ok(Self::Local(LocalEmbedding::decode_node(&rest, ctx)?)),
            MODE_REMOTE => Ok(Self::Remote(RemoteEmbedding::decode_node(&rest, ctx)?)),
            other => Err(DecodeError::unexpected(
                &tag_value.literal,
                "argument",
                format!(
                    "`{MODE_NODE}` `{other}` is not one of \"{MODE_LOCAL}\" / \"{MODE_REMOTE}\""
                ),
            )),
        }
    }
}

/// What the tag split hands back: the tag's word, its AST (for the error span), and
/// the node the variant reads.
type SplitTag<'a, S> = (String, &'a Value<S>, SpannedNode<S>);

/// The tag's value, the value's AST (for the error), and the node WITHOUT the tag —
/// what the variant's struct is written against.
fn split_tag<'a, S: ErrorSpan>(
    node: &'a SpannedNode<S>,
    ctx: &mut Context<S>,
) -> Result<SplitTag<'a, S>, DecodeError<S>> {
    let missing = || {
        DecodeError::missing(
            node,
            format!(
                "`embedding` needs a `{MODE_NODE}` child: \"{MODE_LOCAL}\" or \"{MODE_REMOTE}\""
            ),
        )
    };
    let tag = node
        .children
        .as_ref()
        .and_then(|children| {
            children
                .iter()
                .find(|child| &**child.node_name == MODE_NODE)
        })
        .ok_or_else(missing)?;
    let value = tag.arguments.first().ok_or_else(|| {
        DecodeError::missing(
            tag,
            format!(
                "`{MODE_NODE}` needs a value: \"{MODE_LOCAL}\" or \"{MODE_REMOTE}\""
            ),
        )
    })?;
    let text = String::decode(value, ctx)?;
    let mut rest = node.clone();
    if let Some(children) = rest.deref_mut().children.as_mut() {
        children.retain(|child| &**child.node_name != MODE_NODE);
    }
    Ok((text, value, rest))
}

/// A remote call's default ceiling: long enough for a cold model on the other side,
/// short enough that a dead service fails a door instead of hanging it.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// The engine the file asks for, with its own fields RESOLVED and the other
/// mode's fields refused. What an assembly site matches on instead of reading
/// `mode` and then hoping the right strings are present.
#[derive(Debug, Clone, PartialEq)]
pub enum EmbeddingEngine {
    /// In-process: the model directory, `$HOME` expanded.
    Local { model_dir: PathBuf },
    /// An OpenAI-shaped embeddings service.
    Remote {
        /// `{base_url}/embeddings` is the endpoint; the suffix is the client's.
        base_url: String,
        model: String,
        /// The VARIABLE name; the key itself is read at call time.
        api_key_env: Option<String>,
        timeout_ms: u64,
    },
}

/// Environment variable that overrides the configured model directory.
/// The artifact is a deployment parameter (a ~96 MB, host-local file),
/// so the file's value is the default and the environment wins.
pub const MODEL_DIR_ENV: &str = "KRYSTALLIZER_MODEL_DIR";

/// Fixed artifact file names inside the model directory (the layout the
/// model ships in, HuggingFace's).
pub const ARCH_FILE: &str = "config.json";
pub const MODEL_FILE: &str = "model.safetensors";
pub const TOKENIZER_FILE: &str = "tokenizer.json";

impl EmbeddingConfig {
    /// The vector width the store declares. Both modes state it; a remote service
    /// that answers with another width fails the boot.
    pub fn dimensions(&self) -> u32 {
        match self {
            Self::Local(local) => local.dimensions,
            Self::Remote(remote) => remote.dimensions,
        }
    }

    /// The ranking metric downstream. Both modes emit unit-length vectors, so
    /// `cosine` is an inner product.
    pub fn metric(&self) -> &str {
        match self {
            Self::Local(local) => &local.metric,
            Self::Remote(remote) => &remote.metric,
        }
    }

    /// The engine this file asks for, RESOLVED: `$HOME` expanded (and the model
    /// directory override applied), the base URL's trailing slash dropped, the
    /// ceiling defaulted.
    ///
    /// The mode's field set is already a fact of the TYPE — the decode refused the
    /// other mode's node — so nothing here re-checks shapes. What is checked is what
    /// a string can still get wrong: a required value that is blank.
    pub fn engine(&self) -> Result<EmbeddingEngine, ConfigError> {
        match self {
            Self::Local(local) => Ok(EmbeddingEngine::Local {
                model_dir: local.resolved_model_dir()?,
            }),
            Self::Remote(remote) => Ok(EmbeddingEngine::Remote {
                base_url: stated("base-url", &remote.base_url)?
                    .trim_end_matches('/')
                    .to_string(),
                model: stated("model", &remote.model)?,
                api_key_env: match remote.api_key_env.as_deref().map(str::trim) {
                    // An empty variable NAME is a half-written file: absent means "no
                    // Authorization header", so it must be spelled by omission.
                    Some("") => {
                        return Err(ConfigError::Embedding {
                            detail: "`embedding` `api-key-env` is empty — omit the field for a \
                                     service that needs no key"
                                .to_string(),
                        })
                    }
                    Some(name) => Some(name.to_string()),
                    None => None,
                },
                timeout_ms: remote.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            }),
        }
    }

    /// The directory in force. Local mode only: a remote deployment has no artifact
    /// to name, and asking is an error rather than an empty path.
    pub fn resolved_model_dir(&self) -> Result<PathBuf, ConfigError> {
        match self {
            Self::Local(local) => local.resolved_model_dir(),
            Self::Remote(_) => Err(ConfigError::Embedding {
                detail: format!(
                    "`embedding` mode `{MODE_REMOTE}` has no model directory (that is mode \
                     {MODE_LOCAL}'s, and this file names neither a model directory nor an \
                     artifact)"
                ),
            }),
        }
    }
}

impl LocalEmbedding {
    /// The directory in force: `KRYSTALLIZER_MODEL_DIR` when set and non-empty, else
    /// the configured value — with `$HOME` expanded.
    pub fn resolved_model_dir(&self) -> Result<PathBuf, ConfigError> {
        let dir = std::env::var(MODEL_DIR_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| self.model_dir.clone());
        expand_home(&dir)
    }

    /// The architecture config (`config.json`) inside the directory.
    pub fn arch_path(&self) -> Result<PathBuf, ConfigError> {
        Ok(self.resolved_model_dir()?.join(ARCH_FILE))
    }

    /// The weights inside the directory.
    pub fn model_path(&self) -> Result<PathBuf, ConfigError> {
        Ok(self.resolved_model_dir()?.join(MODEL_FILE))
    }

    /// The tokenizer inside the directory.
    pub fn tokenizer_path(&self) -> Result<PathBuf, ConfigError> {
        Ok(self.resolved_model_dir()?.join(TOKENIZER_FILE))
    }
}

/// A required value that must SAY something. The decode already insisted the child is
/// present; `base-url ""` is still not a service, and naming the field is the whole
/// message.
fn stated(field: &str, value: &str) -> Result<String, ConfigError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ConfigError::Embedding {
            detail: format!("`embedding` `{field}` is empty"),
        });
    }
    Ok(value.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {report}")]
    Parse {
        path: std::path::PathBuf,
        /// Rendered via Display — miette::Report is a Diagnostic, not
        /// std::error::Error, so it cannot sit in `#[source]`.
        report: miette::Report,
    },
    /// A configured path carries `$HOME` and the environment has none: the
    /// service refuses to guess (a literal `$HOME/…` directory would be a
    /// second, invisible database).
    #[error("path `{raw}` uses {HOME_PLACEHOLDER}, but HOME is not set in this environment")]
    HomeUnset { raw: String },
    /// A node decoded but does not NAME a usable engine: an unknown mode, a mode's
    /// required field missing, or the other mode's field present. Caught before any
    /// store is opened, so a half-edited file never serves.
    #[error("config: {detail}")]
    Embedding { detail: String },
}

impl KrystallizerConfig {
    /// Load and decode `path`. The path is passed in (assembly-site
    /// decision), not defaulted here.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        knus::parse::<KrystallizerConfig>(path.to_string_lossy().as_ref(), &text).map_err(
            |report| ConfigError::Parse {
                path: path.to_path_buf(),
                report: report.into(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<KrystallizerConfig, miette::Report> {
        knus::parse::<KrystallizerConfig>("test.kdl", text).map_err(miette::Report::new)
    }

    /// Why the decode failed, not what the wrapper says. `knus::Error`'s own message is
    /// the flat "error parsing KDL"; the decoders' words are its RELATED diagnostics, so
    /// a test that asserts on a reason has to walk them.
    fn why(report: &miette::Report) -> String {
        use miette::Diagnostic;
        let mut text = report.to_string();
        if let Some(error) = report.downcast_ref::<knus::Error>() {
            for cause in error.related().into_iter().flatten() {
                text.push_str("\n  ");
                text.push_str(&cause.to_string());
                for nested in cause.related().into_iter().flatten() {
                    text.push_str("\n    ");
                    text.push_str(&nested.to_string());
                }
            }
        }
        text
    }

    const FULL: &str = r#"
storage {
    data-dir "/var/lib/krystallizer"
}
embedding {
    mode "local"
    model-dir "/models/bge-small-zh-v1.5"
    dimensions 512
    metric "cosine"
}
compression {
    context-bytes 131072
    budget-percent 50
    cache-ttl-ms 300000
    floor-bytes 2048
}
logs {
    max-rows 10000
}
"#;

    /// A config that differs from `FULL` only in its `embedding` node — the shape the
    /// engine-resolution tests need, since a node is only meaningful next to the rest
    /// of a decodable file.
    fn with_embedding(node: &str) -> String {
        format!(
            "storage {{\n    data-dir \"/var/lib/krystallizer\"\n}}\n{node}\ncompression {{\n    \
             context-bytes 131072\n    budget-percent 50\n    cache-ttl-ms 300000\n    floor-bytes \
             2048\n}}\nlogs {{\n    max-rows 10000\n}}\n"
        )
    }

    #[test]
    fn decodes_storage_node() {
        let cfg = parse(FULL).unwrap();
        assert_eq!(
            cfg.storage.data_dir().unwrap(),
            PathBuf::from("/var/lib/krystallizer")
        );
    }

    /// `$HOME` is the one placeholder a path value may carry. Only a LEADING
    /// `$HOME` that ends the value or a segment counts: `$HOMELESS` and `$FOO`
    /// stay literal, so a typo cannot silently point somewhere else.
    ///
    /// The home directory is STATED rather than set in the environment: the env
    /// is process-wide and shared with every other test in this file.
    #[test]
    fn a_leading_home_is_expanded_and_nothing_else_is() {
        let home = Some("/home/tester");
        assert_eq!(
            expand_home_with("$HOME/.local/share/krystallizer", home).unwrap(),
            PathBuf::from("/home/tester/.local/share/krystallizer")
        );
        assert_eq!(
            expand_home_with("$HOME", home).unwrap(),
            PathBuf::from("/home/tester"),
            "the placeholder alone is the home directory"
        );
        assert_eq!(
            expand_home_with("$HOMELESS/data", home).unwrap(),
            PathBuf::from("$HOMELESS/data"),
            "not a placeholder: a longer name"
        );
        assert_eq!(
            expand_home_with("/srv/$HOME/data", home).unwrap(),
            PathBuf::from("/srv/$HOME/data"),
            "not a placeholder: only a LEADING one expands"
        );
        assert_eq!(
            expand_home_with("/var/lib/krystallizer", home).unwrap(),
            PathBuf::from("/var/lib/krystallizer"),
            "a plain path is passed through untouched"
        );

        // An unset HOME is a boot error, not a directory called `$HOME`.
        assert!(matches!(
            expand_home_with("$HOME/data", None),
            Err(ConfigError::HomeUnset { .. })
        ));
        // ...but a path that does not use the placeholder never needs it.
        assert_eq!(
            expand_home_with("/var/lib/krystallizer", None).unwrap(),
            PathBuf::from("/var/lib/krystallizer")
        );
    }

    /// The tag picks the variant, and the variant carries the fields its mode has —
    /// which is why the local node has no `base-url` field to be left unset.
    #[test]
    fn decodes_embedding_node() {
        let cfg = parse(FULL).unwrap();
        assert_eq!(cfg.embedding.dimensions(), 512);
        assert_eq!(cfg.embedding.metric(), "cosine");
        match &cfg.embedding {
            EmbeddingConfig::Local(local) => {
                assert_eq!(local.model_dir, "/models/bge-small-zh-v1.5");
            }
            other => panic!("the file says mode local, got {other:?}"),
        }
    }

    /// The remote mode names a SERVICE, not an artifact: the URL (the `/embeddings`
    /// route is the client's), the model, the VARIABLE holding the key, and a timeout
    /// that defaults when the file omits it.
    #[test]
    fn the_remote_mode_resolves_the_service_and_defaults_the_timeout() {
        let node = r#"embedding {
    mode "remote"
    base-url "http://gpu-box:8000/v1/"
    model "bge-small-zh-v1.5"
    api-key-env "KRYSTALLIZER_EMBEDDING_KEY"
    dimensions 512
    metric "cosine"
}"#;
        let cfg = parse(&with_embedding(node)).unwrap();
        assert_eq!(
            cfg.embedding.engine().unwrap(),
            EmbeddingEngine::Remote {
                base_url: "http://gpu-box:8000/v1".to_string(),
                model: "bge-small-zh-v1.5".to_string(),
                api_key_env: Some("KRYSTALLIZER_EMBEDDING_KEY".to_string()),
                timeout_ms: DEFAULT_TIMEOUT_MS,
            }
        );

        // A service behind a private network needs no key; OMITTING the field is
        // how that is spelled.
        let open = node.replace("    api-key-env \"KRYSTALLIZER_EMBEDDING_KEY\"\n", "");
        let cfg = parse(&with_embedding(&open)).unwrap();
        assert!(
            matches!(
                cfg.embedding.engine().unwrap(),
                EmbeddingEngine::Remote {
                    api_key_env: None,
                    ..
                }
            ),
            "no key is the absence of the field"
        );

        // A stated ceiling is used as written.
        let timed = open.replace("    dimensions 512", "    timeout-ms 2500\n    dimensions 512");
        let cfg = parse(&with_embedding(&timed)).unwrap();
        assert!(matches!(
            cfg.embedding.engine().unwrap(),
            EmbeddingEngine::Remote {
                timeout_ms: 2500,
                ..
            }
        ));
    }

    /// The shape of a mode is its TYPE, so a field of the other mode is refused by the
    /// DECODE (an unknown child, at that line), and a required field that is missing is
    /// refused by name. No list of names is kept in step by hand.
    #[test]
    fn the_other_modes_fields_are_refused_at_the_decode() {
        let cases = [
            (
                r#"embedding {
    mode "remote"
    model "m"
    dimensions 512
    metric "cosine"
}"#,
                "base-url",
            ),
            (
                r#"embedding {
    mode "remote"
    base-url "http://x/v1"
    dimensions 512
    metric "cosine"
}"#,
                "model`",
            ),
            (
                r#"embedding {
    mode "remote"
    base-url "http://x/v1"
    model "m"
    model-dir "/models/m"
    dimensions 512
    metric "cosine"
}"#,
                "model-dir",
            ),
            (
                r#"embedding {
    mode "local"
    model-dir "/models/m"
    base-url "http://x/v1"
    dimensions 512
    metric "cosine"
}"#,
                "base-url",
            ),
            (
                r#"embedding {
    mode "local"
    dimensions 512
    metric "cosine"
}"#,
                "model-dir",
            ),
        ];
        for (node, want) in cases {
            let err = parse(&with_embedding(node)).expect_err("refused");
            let msg = why(&err);
            assert!(msg.contains(want), "expected `{want}` in: {msg}");
        }
    }

    /// A node without a tag is not a mode; a tag with a word the service does not know
    /// names both words it does.
    #[test]
    fn the_tag_must_be_present_and_be_one_of_the_two_words() {
        let untagged = parse(&with_embedding(
            r#"embedding {
    model-dir "/models/m"
    dimensions 512
    metric "cosine"
}"#,
        ))
        .expect_err("refused");
        let msg = why(&untagged);
        assert!(msg.contains("mode"), "names the missing tag: {msg}");

        let unknown = parse(&with_embedding(
            r#"embedding {
    mode "cloud"
    dimensions 512
    metric "cosine"
}"#,
        ))
        .expect_err("refused");
        let msg = why(&unknown);
        assert!(msg.contains("cloud"), "names the word it read: {msg}");
        assert!(
            msg.contains("not one of"),
            "names the words it knows: {msg}"
        );
    }

    /// What the TYPE cannot know: a required value that is present but blank. Those
    /// are still refused, before any store is opened.
    #[test]
    fn a_blank_value_is_refused_where_the_string_is_resolved() {
        let cases = [
            (
                r#"embedding {
    mode "remote"
    base-url ""
    model "m"
    dimensions 512
    metric "cosine"
}"#,
                "base-url",
            ),
            (
                r#"embedding {
    mode "remote"
    base-url "http://x/v1"
    model "  "
    dimensions 512
    metric "cosine"
}"#,
                "model",
            ),
            (
                r#"embedding {
    mode "remote"
    base-url "http://x/v1"
    model "m"
    api-key-env ""
    dimensions 512
    metric "cosine"
}"#,
                "api-key-env",
            ),
        ];
        for (node, want) in cases {
            let err = parse(&with_embedding(node))
                .expect("the node decodes")
                .embedding
                .engine()
                .expect_err("refused");
            let msg = err.to_string();
            assert!(msg.contains(want), "expected `{want}` in: {msg}");
        }
    }

    /// The policy numbers decode as parameters and reach the engine as
    /// one `Params` — the mid-band trigger is derived, not stored.
    #[test]
    fn decodes_compression_node() {
        let cfg = parse(FULL).unwrap().compression;
        assert_eq!(cfg.context_bytes, 131_072);
        assert_eq!(cfg.budget_percent, 50);
        assert_eq!(cfg.cache_ttl_ms, 300_000);
        assert_eq!(cfg.floor_bytes, 2_048);
        let params = cfg.params();
        assert_eq!(params.budget_bytes(), 65_536, "half the window");
    }

    /// ONE test owns `MODEL_DIR_ENV`: the environment is process-wide, so
    /// a second test touching the same variable would race this one (and
    /// a race here silently flips the resolved directory).
    #[test]
    fn the_directory_names_the_three_files_and_the_environment_overrides_it() {
        let cfg = parse(FULL).unwrap();
        let EmbeddingConfig::Local(local) = &cfg.embedding else {
            panic!("the file says mode local");
        };
        std::env::remove_var(MODEL_DIR_ENV);
        assert_eq!(
            local.model_path().unwrap(),
            PathBuf::from("/models/bge-small-zh-v1.5/model.safetensors")
        );
        assert_eq!(
            local.tokenizer_path().unwrap(),
            PathBuf::from("/models/bge-small-zh-v1.5/tokenizer.json")
        );
        assert_eq!(
            local.arch_path().unwrap(),
            PathBuf::from("/models/bge-small-zh-v1.5/config.json")
        );

        // The override is a deployment parameter: env wins over the file.
        std::env::set_var(MODEL_DIR_ENV, "/elsewhere/model");
        assert_eq!(
            cfg.embedding.resolved_model_dir().unwrap(),
            PathBuf::from("/elsewhere/model")
        );
        assert_eq!(
            cfg.embedding.engine().unwrap(),
            EmbeddingEngine::Local {
                model_dir: PathBuf::from("/elsewhere/model")
            },
            "the engine the file names goes through the same override"
        );
        assert_eq!(
            local.model_path().unwrap(),
            PathBuf::from("/elsewhere/model/model.safetensors")
        );

        // Empty counts as unset: the file's value comes back.
        std::env::set_var(MODEL_DIR_ENV, "");
        assert_eq!(
            cfg.embedding.resolved_model_dir().unwrap(),
            PathBuf::from("/models/bge-small-zh-v1.5")
        );
        std::env::remove_var(MODEL_DIR_ENV);
        assert_eq!(
            cfg.embedding.resolved_model_dir().unwrap(),
            PathBuf::from("/models/bge-small-zh-v1.5")
        );
    }

    #[test]
    fn rejects_unknown_nodes() {
        // A stray top-level node is a typo, not silent config — fail loud.
        assert!(parse(
            r#"
storage {
    data-dir "/var/lib/krystallizer"
}
embeddings {
    model-dir "x"
}
"#
        )
        .is_err());
        // Unknown child inside a known node fails too.
        assert!(parse(
            r#"
storage {
    data-dir "/var/lib/krystallizer"
}
embedding {
    model-dir "x"
    dimenshun 512
    metric "cosine"
}
"#
        )
        .is_err());
    }

    #[test]
    fn missing_child_fails() {
        assert!(parse(
            r#"
embedding {
    model-dir "x"
}
"#
        )
        .is_err());
        // The database's home is not optional: a service with no stated
        // storage directory must not fall back to the working directory.
        assert!(parse(
            r#"
embedding {
    model-dir "x"
    dimensions 512
    metric "cosine"
}
compression {
    context-bytes 131072
    budget-percent 50
    cache-ttl-ms 300000
    floor-bytes 2048
}
logs {
    max-rows 10000
}
"#
        )
        .is_err());
    }

    /// The repo's own config file must decode: it is the one the service
    /// loads at boot, and a typo there is a boot failure, not a test
    /// fixture problem.
    ///
    /// The path VALUES are asserted as written, not resolved: this file ships
    /// with `$HOME` (a machine's home is not a property of the software), and
    /// resolution reads the environment another test owns.
    #[test]
    fn the_repo_config_decodes() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/krystallizer.kdl");
        let cfg = KrystallizerConfig::load(&path).expect("repo config decodes");
        assert_eq!(cfg.embedding.metric(), "cosine");
        assert_eq!(cfg.embedding.dimensions(), 512);
        assert!(
            cfg.storage.data_dir.starts_with(HOME_PLACEHOLDER),
            "the shipped data dir is $HOME-relative, got {}",
            cfg.storage.data_dir
        );
        assert_eq!(
            cfg.logs.max_rows,
            crate::log::DEFAULT_MAX_ROWS,
            "the file's ceiling and the built-in default must not drift"
        );
        assert_eq!(
            cfg.compression.params(),
            crate::CompressionParams::default(),
            "the file's numbers and the built-in defaults must not drift"
        );
    }
}
