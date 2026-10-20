//! Configuration loading (ADR-0007): one unified KDL document,
//! `config/krystallizer.kdl`, one top-level node per subsystem. Each
//! node is a struct here; secrets are NEVER in the file — a node that
//! needs one names the environment variable that holds it, and the
//! process resolves it at call time (the embedding node needs none: the
//! model is local, ADR-0003). Parsing goes through knus; callers never
//! see knus types.

use knus::Decode;
use std::path::PathBuf;

/// Root of `config/krystallizer.kdl`. New subsystems are new top-level
/// nodes here (and new fields here) — never new config files.
#[derive(Decode, Debug, Clone)]
pub struct KrystallizerConfig {
    /// `embedding { ... }` — the built-in local embedding model
    /// (ADR-0003: in-process, no endpoint, no key).
    #[knus(child)]
    pub embedding: EmbeddingConfig,
    /// `compression { ... }` — the Phase 2.6 policy engine's numbers
    /// (ADR-0008 §4: every threshold is a parameter, never a constant).
    #[knus(child)]
    pub compression: CompressionConfig,
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

/// The local embedding artifact's DIRECTORY. The three files inside are
/// found by name (`config.json` = architecture, `model.safetensors` =
/// weights, `tokenizer.json`), so one path is the whole configuration
/// and a model swap is a directory swap. `dimensions` is the width the
/// store declares for a row's vector frame; `metric` is the ranking
/// metric downstream. Child-node form (knus maps Rust field names to
/// KDL node names as kebab-case):
///
/// ```kdl
/// embedding {
///     model-dir "/…/models/bge-small-zh-v1.5"
///     dimensions 512
///     metric "cosine"
/// }
/// ```
#[derive(Decode, Debug, Clone)]
pub struct EmbeddingConfig {
    /// Directory holding the artifact. A deployment parameter, so the
    /// environment overrides it — see `resolved_model_dir`.
    #[knus(child, unwrap(argument))]
    pub model_dir: String,
    /// Vector width this model produces (512 for bge-small-zh-v1.5).
    #[knus(child, unwrap(argument))]
    pub dimensions: u32,
    /// Ranking metric downstream; the engine emits unit-length vectors,
    /// so `cosine` is an inner product.
    #[knus(child, unwrap(argument))]
    pub metric: String,
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
    /// The directory in force: `KRYSTALLIZER_MODEL_DIR` when set and
    /// non-empty, else the configured value.
    pub fn resolved_model_dir(&self) -> String {
        std::env::var(MODEL_DIR_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.model_dir.clone())
    }

    /// The architecture config (`config.json`) inside the directory.
    pub fn arch_path(&self) -> PathBuf {
        PathBuf::from(self.resolved_model_dir()).join(ARCH_FILE)
    }

    /// The weights inside the directory.
    pub fn model_path(&self) -> PathBuf {
        PathBuf::from(self.resolved_model_dir()).join(MODEL_FILE)
    }

    /// The tokenizer inside the directory.
    pub fn tokenizer_path(&self) -> PathBuf {
        PathBuf::from(self.resolved_model_dir()).join(TOKENIZER_FILE)
    }
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
}

impl KrystallizerConfig {
    /// Load and decode `path`. The path is passed in (assembly-site
    /// decision), not defaulted here.
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
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

    const FULL: &str = r#"
embedding {
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
"#;

    #[test]
    fn decodes_embedding_node() {
        let cfg = parse(FULL).unwrap();
        assert_eq!(cfg.embedding.model_dir, "/models/bge-small-zh-v1.5");
        assert_eq!(cfg.embedding.dimensions, 512);
        assert_eq!(cfg.embedding.metric, "cosine");
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
        std::env::remove_var(MODEL_DIR_ENV);
        assert_eq!(
            cfg.embedding.model_path(),
            PathBuf::from("/models/bge-small-zh-v1.5/model.safetensors")
        );
        assert_eq!(
            cfg.embedding.tokenizer_path(),
            PathBuf::from("/models/bge-small-zh-v1.5/tokenizer.json")
        );
        assert_eq!(
            cfg.embedding.arch_path(),
            PathBuf::from("/models/bge-small-zh-v1.5/config.json")
        );

        // The override is a deployment parameter: env wins over the file.
        std::env::set_var(MODEL_DIR_ENV, "/elsewhere/model");
        assert_eq!(cfg.embedding.resolved_model_dir(), "/elsewhere/model");
        assert_eq!(
            cfg.embedding.model_path(),
            PathBuf::from("/elsewhere/model/model.safetensors")
        );

        // Empty counts as unset: the file's value comes back.
        std::env::set_var(MODEL_DIR_ENV, "");
        assert_eq!(
            cfg.embedding.resolved_model_dir(),
            "/models/bge-small-zh-v1.5"
        );
        std::env::remove_var(MODEL_DIR_ENV);
        assert_eq!(
            cfg.embedding.resolved_model_dir(),
            "/models/bge-small-zh-v1.5"
        );
    }

    #[test]
    fn rejects_unknown_nodes() {
        // A stray top-level node is a typo, not silent config — fail loud.
        assert!(parse(
            r#"
embeddings {
    model-dir "x"
}
"#
        )
        .is_err());
        // Unknown child inside a known node fails too.
        assert!(parse(
            r#"
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
    }

    /// The repo's own config file must decode: it is the one the service
    /// loads at boot, and a typo there is a boot failure, not a test
    /// fixture problem.
    #[test]
    fn the_repo_config_decodes() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/krystallizer.kdl");
        let cfg = KrystallizerConfig::load(&path).expect("repo config decodes");
        assert_eq!(cfg.embedding.metric, "cosine");
        assert_eq!(cfg.embedding.dimensions, 512);
        assert_eq!(
            cfg.compression.params(),
            crate::CompressionParams::default(),
            "the file's numbers and the built-in defaults must not drift"
        );
    }
}
