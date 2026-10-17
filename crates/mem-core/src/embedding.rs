//! The built-in embedding engine (ADR-0003): a small BERT runs INSIDE
//! this process.
//!
//! No endpoint, no key, no transport hop. The artifact is a local model
//! DIRECTORY (the three files a sentence-transformer ships: the
//! architecture JSON, the safetensors weights, the tokenizer) named by
//! `config/krystallizer.kdl`'s `embedding` node and overridable through
//! `KRYSTALLIZER_MODEL_DIR`; the artifact is NOT in this repo. The
//! vector space is locked with the data — two models do not share a
//! space — so the memory system owns embedding consistency, never a
//! caller.
//!
//! Shape of one embedding: WordPiece tokenize (truncated at the model's
//! own position ceiling) → BERT → CLS pooling → L2 normalize. CLS is the
//! model's declared pooling (`1_Pooling/config.json:
//! pooling_mode_cls_token`), and the normalization is what makes the
//! contract's `cosine` metric a plain inner product downstream.

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use std::path::{Path, PathBuf};
use tokenizers::{Tokenizer, TruncationParams};

use crate::config::{ARCH_FILE, MODEL_FILE, TOKENIZER_FILE};
use crate::EmbeddingConfig;

/// Everything that can go wrong loading or running the local model. A
/// file error names the FILE (a mis-named artifact directory is the
/// likeliest cause, and "No such file or directory" alone does not say
/// which one).
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embedding artifact {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("embedding model config {path} is not valid JSON: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("embedding tokenizer: {0}")]
    Tokenizer(String),
    #[error("embedding model: {0}")]
    Model(#[from] candle_core::Error),
}

/// Read one artifact file, carrying its path into the error.
fn read_artifact(path: &Path) -> Result<Vec<u8>, EmbedError> {
    std::fs::read(path).map_err(|source| EmbedError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// The in-process embedder: one loaded model, one tokenizer, one width.
pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    dims: usize,
}

impl Embedder {
    /// Load a model DIRECTORY: the three artifact files are found by
    /// name inside it, so a model swap is a directory swap.
    pub fn load(dir: &str) -> Result<Self, EmbedError> {
        let dir = Path::new(dir);
        let arch = dir.join(ARCH_FILE);
        let cfg: BertConfig = serde_json::from_slice(&read_artifact(&arch)?)
            .map_err(|source| EmbedError::Json { path: arch, source })?;
        let device = Device::Cpu;
        // The artifact is mmapped read-only: the weights never enter the
        // heap, and loading is a page-in rather than a copy.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[dir.join(MODEL_FILE)], DType::F32, &device)?
        };
        let model = BertModel::load(vb, &cfg)?;
        let tokenizer_path = dir.join(TOKENIZER_FILE);
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| EmbedError::Tokenizer(format!("{}: {e}", tokenizer_path.display())))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: cfg.max_position_embeddings,
                ..Default::default()
            }))
            .map_err(|e| EmbedError::Tokenizer(e.to_string()))?;
        Ok(Self {
            model,
            tokenizer,
            device,
            dims: cfg.hidden_size,
        })
    }

    /// Load from the `embedding` node — the one place that knows the
    /// config's field names and the environment override.
    pub fn from_config(cfg: &EmbeddingConfig) -> Result<Self, EmbedError> {
        Self::load(&cfg.resolved_model_dir())
    }

    /// The width of every vector this model produces. The store writes
    /// it into the row's `Vector<f32>` frame, so the two must agree.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Embed one text: CLS pooling + L2 normalization.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| EmbedError::Tokenizer(e.to_string()))?;
        let ids = Tensor::new(enc.get_ids(), &self.device)?.unsqueeze(0)?;
        let type_ids = Tensor::new(enc.get_type_ids(), &self.device)?.unsqueeze(0)?;
        let mask = Tensor::new(enc.get_attention_mask(), &self.device)?.unsqueeze(0)?;
        let hidden = self.model.forward(&ids, &type_ids, Some(&mask))?;
        // CLS = position 0 of the last hidden state.
        let cls = hidden.narrow(1, 0, 1)?.squeeze(1)?.squeeze(0)?;
        let v: Vec<f32> = cls.to_vec1()?;
        Ok(l2_normalize(v))
    }
}

/// L2 normalization: the contract's metric is cosine, so the stored and
/// queried vectors are unit-length and ranking is an inner product.
fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The artifact is ~96 MB and deliberately NOT in the repo, so the
    /// tests that need it read the repo config's own directory and skip
    /// when the machine has no model there — the default gate stays
    /// runnable anywhere, and a machine that HAS the model exercises the
    /// real inference path. The configured value is used directly (not
    /// the env-resolved one) so an environment override in another test
    /// cannot turn this into a silent skip.
    fn local_embedder() -> Option<Embedder> {
        let cfg_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/krystallizer.kdl");
        let cfg = crate::KrystallizerConfig::load(&cfg_path).ok()?;
        if !Path::new(&cfg.embedding.model_dir)
            .join(MODEL_FILE)
            .exists()
        {
            eprintln!(
                "skipping: no embedding model in {}",
                cfg.embedding.model_dir
            );
            return None;
        }
        Some(Embedder::load(&cfg.embedding.model_dir).expect("model loads"))
    }

    #[test]
    fn a_missing_artifact_names_the_file_it_could_not_read() {
        let err = Embedder::load("/nonexistent/model-dir")
            .err()
            .expect("load fails");
        let msg = err.to_string();
        assert!(msg.contains(ARCH_FILE), "names the missing file: {msg}");
        assert!(
            msg.contains("/nonexistent/model-dir"),
            "names the directory too: {msg}"
        );
    }

    #[test]
    fn embeds_one_chinese_sentence_normalized_and_at_the_model_width() {
        let Some(emb) = local_embedder() else { return };
        let v = emb.embed("krystallizer 用 fjall 做本地存储引擎").unwrap();
        assert_eq!(v.len(), emb.dims(), "width agrees with the declared field");
        assert_eq!(v.len(), 512, "bge-small-zh-v1.5 is 512-dim");
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "unit length, got {norm}");
    }

    #[test]
    fn related_texts_rank_above_unrelated() {
        let Some(emb) = local_embedder() else { return };
        let dot = |a: &str, b: &str| -> f32 {
            let (x, y) = (emb.embed(a).unwrap(), emb.embed(b).unwrap());
            x.iter().zip(y.iter()).map(|(a, b)| a * b).sum()
        };
        let near = dot("LSM 存储引擎的写放大", "fjall 是一个 LSM 存储引擎");
        let far = dot("LSM 存储引擎的写放大", "今天中午吃什么比较好");
        assert!(near > far, "related {near} should beat unrelated {far}");
    }
}
