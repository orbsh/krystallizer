//! Configuration loading (ADR-0007): one unified KDL document,
//! `config/krystallizer.kdl`, one top-level node per subsystem. Each
//! node is a struct here; secrets are NEVER in the file — the config
//! names the environment variable that holds the value, and the
//! process resolves it at call time. Parsing goes through knus;
//! callers never see knus types.

use knus::Decode;

/// Root of `config/krystallizer.kdl`. New subsystems are new top-level
/// nodes here (and new fields here) — never new config files.
#[derive(Decode, Debug, Clone)]
pub struct KrystallizerConfig {
    /// `embedding { ... }` — embedding provider (ADR-0003/0007: the
    /// core's only network call, model locked with the data).
    #[knus(child)]
    pub embedding: EmbeddingConfig,
}

/// One OpenAI-compatible embeddings endpoint. `api_key_env` names the
/// environment variable holding the Bearer token; the key itself never
/// appears in the config or in this struct. Child-node form (knus maps
/// Rust field names to KDL node names as kebab-case):
///
/// ```kdl
/// embedding {
///     provider "dashscope"
///     base-url "https://..."
///     endpoint "/embeddings"
///     model "text-embedding-v4"
///     dimensions 1024
///     metric "cosine"
///     api-key-env "DASHSCOPE_API_KEY"
///     timeout-ms 10000
/// }
/// ```
#[derive(Decode, Debug, Clone)]
pub struct EmbeddingConfig {
    #[knus(child, unwrap(argument))]
    pub provider: String,
    #[knus(child, unwrap(argument))]
    pub base_url: String,
    #[knus(child, unwrap(argument))]
    pub endpoint: String,
    #[knus(child, unwrap(argument))]
    pub model: String,
    #[knus(child, unwrap(argument))]
    pub dimensions: u32,
    #[knus(child, unwrap(argument))]
    pub metric: String,
    /// Environment variable name holding the API key (not the key).
    #[knus(child, unwrap(argument))]
    pub api_key_env: String,
    #[knus(child, unwrap(argument))]
    pub timeout_ms: u64,
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
    /// Environment variable named by `api_key_env` is unset or empty.
    #[error("environment variable `{var}` (named by api_key_env) is unset or empty")]
    MissingSecret { var: String },
}

impl KrystallizerConfig {
    /// Load and decode `path`. The path is passed in (assembly-site
    /// decision), not defaulted here.
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        knus::parse::<KrystallizerConfig>(path.to_string_lossy().as_ref(), &text)
            .map_err(|report| ConfigError::Parse {
                path: path.to_path_buf(),
                report: report.into(),
            })
    }

    /// Resolve the embedding API key from the environment at call
    /// time. Errors name the variable, never a value.
    pub fn embedding_api_key(&self) -> Result<String, ConfigError> {
        let var = &self.embedding.api_key_env;
        std::env::var(var)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| ConfigError::MissingSecret { var: var.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<KrystallizerConfig, miette::Report> {
        knus::parse::<KrystallizerConfig>("test.kdl", text)
            .map_err(miette::Report::new)
    }

    const FULL: &str = r#"
embedding {
    provider "dashscope"
    base-url "https://dashscope.aliyuncs.com/compatible-mode/v1"
    endpoint "/embeddings"
    model "text-embedding-v4"
    dimensions 1024
    metric "cosine"
    api-key-env "DASHSCOPE_API_KEY"
    timeout-ms 10000
}
"#;

    #[test]
    fn decodes_embedding_node() {
        let cfg = parse(FULL).unwrap();
        assert_eq!(cfg.embedding.model, "text-embedding-v4");
        assert_eq!(cfg.embedding.dimensions, 1024);
        assert_eq!(cfg.embedding.api_key_env, "DASHSCOPE_API_KEY");
        assert_eq!(cfg.embedding.timeout_ms, 10000);
    }

    #[test]
    fn rejects_unknown_nodes() {
        // A stray top-level node is a typo, not silent config — fail loud.
        assert!(parse(
            r#"
embeddings {
    provider "x"
}
"#
        )
        .is_err());
        // Unknown child inside a known node fails too.
        assert!(parse(
            r#"
embedding {
    provider "x"
    dimenshun 1024
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
    provider "x"
}
"#
        )
        .is_err());
    }

    #[test]
    fn missing_secret_names_variable_not_value() {
        std::env::remove_var("KRYSTALLIZER_TEST_UNSET_KEY_VAR");
        let cfg = parse(FULL).unwrap();
        // Point the config at the unset var for this check.
        let cfg = KrystallizerConfig {
            embedding: EmbeddingConfig {
                api_key_env: "KRYSTALLIZER_TEST_UNSET_KEY_VAR".to_string(),
                ..cfg.embedding
            },
        };
        let err = cfg.embedding_api_key().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("KRYSTALLIZER_TEST_UNSET_KEY_VAR"), "names the var: {msg}");

        // Empty counts as missing too.
        std::env::set_var("KRYSTALLIZER_TEST_EMPTY_KEY_VAR", "");
        let cfg2 = KrystallizerConfig {
            embedding: EmbeddingConfig {
                api_key_env: "KRYSTALLIZER_TEST_EMPTY_KEY_VAR".to_string(),
                ..cfg.embedding
            },
        };
        assert!(cfg2.embedding_api_key().is_err());
    }
}
