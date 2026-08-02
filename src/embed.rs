// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Local static embeddings for semantic (hybrid) retrieval — feature `vector`.
//!
//! The embedding engine is Model2Vec (via `model2vec-rs`): static,
//! non-contextual token embeddings mean-pooled per text, which keeps
//! inference CPU-cheap and dependency-light. The model is loaded from a
//! **local directory only** — model2vec-rs is compiled with its `local-only`
//! feature, so nothing is ever fetched from a network (Standard §9,
//! privacy-first). Install a model (e.g. `minishlab/potion-base-8M`:
//! `model.safetensors` + `tokenizer.json` + `config.json`) by hand at one of
//! the cascade locations documented on [`resolve_model_path`].
//!
//! Similarity is brute-force cosine over every stored vector
//! ([`cosine`], [`crate::store::Store::vector_candidates`]). That is the
//! deliberate no-index choice: at engram's scale (well under 100k rows) a
//! scalar scan is sub-millisecond, and an ANN index (sqlite-vec, HNSW)
//! would add a dependency and a consistency surface for no measurable win.
//! Revisit only if a real corpus outgrows the scan.
//!
//! This module is compiled in every build so that [`cosine`] and the
//! [`HybridUnavailable`] gate are shared across feature sets; the
//! model-touching items ([`Embedder`], [`resolve_model_path`]) exist only
//! under `--features vector`.

#[cfg(feature = "vector")]
use std::path::{Path, PathBuf};

// The vector-bundled feature name is reserved in Cargo.toml but must not
// ship half-done: no model blob is vendored in this repository yet, so a
// build that selects it is refused at compile time rather than producing a
// binary that silently has nothing bundled. Ships at release time once a
// model blob is vendored; until then build with `--features vector` and a
// local model path.
#[cfg(feature = "vector-bundled")]
compile_error!(
    "vector-bundled ships at release time once a model blob is vendored; \
     build with --features vector and a local model path instead"
);

/// Cosine similarity between two vectors, by plain scalar loop.
///
/// Returns `0.0` on a length mismatch or when either vector has zero norm —
/// a degenerate comparison scores as "no similarity" rather than erroring,
/// so a stray row with the wrong dimensionality can never poison a query.
///
/// Deliberately no SIMD, no ANN index: at < 100k rows brute force is
/// sub-millisecond, and keeping this a ten-line pure function keeps it
/// testable and identical across feature sets (see the module docs).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Why hybrid retrieval is not available right now.
///
/// One shared gate for every surface: the CLI maps these to a structured
/// exit-2 error, HTTP to a 400, and the auto-mode callers simply fall back
/// to FTS5-only on any of them. The enum exists in every build so the
/// surfaces can match on it uniformly; which variants are constructed
/// depends on the feature set, hence the `allow(dead_code)` (an `expect`
/// would itself warn in whichever build does construct a given variant).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum HybridUnavailable {
    /// The binary was built without `--features vector`.
    FeatureDisabled,
    /// No model directory exists at any cascade location.
    NoModel,
    /// A model directory was found but could not be loaded.
    LoadFailed(String),
    /// The model loaded, but no memory has been embedded with it yet.
    NoVectors {
        /// Name of the resolved model, so the hint can name it.
        model: String,
    },
    /// The vector-count probe hit a storage error.
    Storage(String),
}

impl HybridUnavailable {
    /// Human/agent-facing explanation of what is missing.
    pub fn message(&self) -> String {
        match self {
            Self::FeatureDisabled => "engram was built without the vector feature".to_string(),
            Self::NoModel => "no embedding model found".to_string(),
            Self::LoadFailed(e) => format!("failed to load the embedding model: {e}"),
            Self::NoVectors { model } => {
                format!("no vectors indexed for model '{model}'")
            }
            Self::Storage(e) => format!("could not probe the vector index: {e}"),
        }
    }

    /// The next step that would make hybrid retrieval available.
    pub fn hint(&self) -> String {
        match self {
            Self::FeatureDisabled => {
                "rebuild with `cargo build --release --features vector`".to_string()
            }
            Self::NoModel => "the model path cascade is: --model-path, then ENGRAM_MODEL, then \
                              $XDG_DATA_HOME/engram/model (default ~/.local/share/engram/model); \
                              none exists — place a Model2Vec model there yourself (engram never \
                              downloads anything)"
                .to_string(),
            Self::LoadFailed(_) => "a Model2Vec model directory contains model.safetensors, \
                                    tokenizer.json, and config.json"
                .to_string(),
            Self::NoVectors { .. } => {
                "run `engram index` to embed the stored memories first".to_string()
            }
            Self::Storage(_) => "check that the engram database file is readable".to_string(),
        }
    }
}

/// A query ready for hybrid retrieval: the resolved model name plus the
/// embedded query vector.
#[derive(Debug, Clone)]
pub struct HybridReady {
    /// Model name, as stored in `memory_vectors.model`.
    pub model: String,
    /// The embedded query.
    pub query_vec: Vec<f32>,
}

/// The auto-hybrid gate, shared by all three surfaces.
///
/// Hybrid retrieval is ready iff: the `vector` feature is compiled in, AND a
/// model directory resolves ([`resolve_model_path`]) and loads, AND at least
/// one memory has been embedded with that model (`vector_count > 0` — a
/// database that was never indexed gains nothing from an empty vector
/// channel). Callers in auto mode call `.ok()` and fall back to FTS5-only;
/// callers honoring an explicit `hybrid` request surface the error.
///
/// The embedder is loaded once per process and cached ([`cached_embedder`]),
/// so long-running servers do not reload the model per request.
#[cfg_attr(
    not(feature = "vector"),
    expect(
        unused_variables,
        reason = "without the vector feature every argument is ignored; renaming them _-style would desync the documented signature between builds"
    )
)]
pub fn try_hybrid(
    store: &crate::store::Store,
    model_path: Option<&std::path::Path>,
    query: &str,
) -> Result<HybridReady, HybridUnavailable> {
    #[cfg(feature = "vector")]
    {
        let embedder = cached_embedder(model_path)?;
        let count = store
            .vector_count(embedder.model_name())
            .map_err(|e| HybridUnavailable::Storage(e.to_string()))?;
        if count == 0 {
            return Err(HybridUnavailable::NoVectors {
                model: embedder.model_name().to_string(),
            });
        }
        Ok(HybridReady {
            model: embedder.model_name().to_string(),
            query_vec: embedder.embed(query),
        })
    }
    #[cfg(not(feature = "vector"))]
    {
        Err(HybridUnavailable::FeatureDisabled)
    }
}

/// Resolves the local model directory, first hit wins:
///
/// 1. the explicit CLI value (`--model-path`);
/// 2. the `ENGRAM_MODEL` environment variable;
/// 3. `$XDG_DATA_HOME/engram/model`, defaulting to
///    `~/.local/share/engram/model` when `XDG_DATA_HOME` is unset.
///
/// Only paths that actually **exist on disk** are returned; a candidate that
/// does not exist falls through to the next. `None` means no candidate
/// exists — and per the §9 privacy-first posture engram will never fetch
/// one: installing a model is always a deliberate, manual act.
#[cfg(feature = "vector")]
pub fn resolve_model_path(cli: Option<&Path>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = cli {
        candidates.push(p.to_path_buf());
    }
    if let Some(env) = std::env::var_os("ENGRAM_MODEL").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(env));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(base) = data_home {
        candidates.push(base.join("engram/model"));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// A loaded Model2Vec static-embedding model.
#[cfg(feature = "vector")]
#[derive(Debug)]
pub struct Embedder {
    model: model2vec_rs::model::StaticModel,
    dim: usize,
    name: String,
}

#[cfg(feature = "vector")]
impl Embedder {
    /// Loads a Model2Vec model from a local directory
    /// (`model.safetensors` + `tokenizer.json` + `config.json`).
    ///
    /// model2vec-rs is compiled `local-only`, so a path that does not exist
    /// on disk is an error here, never a hub download.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory is missing any model file, the
    /// files fail to parse, or the model embeds to zero dimensions.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let model = model2vec_rs::model::StaticModel::from_pretrained(path, None, None, None)?;
        // Probe the output dimensionality once; every later embed call is
        // checked against nothing (Model2Vec output width is fixed by the
        // embedding matrix), so a zero here means a broken model.
        let dim = model.encode_single("dimension probe").len();
        anyhow::ensure!(
            dim > 0,
            "model at {} embeds to zero dimensions",
            path.display()
        );
        // The directory basename identifies the model in `memory_vectors.model`;
        // vectors from different models must never be compared against each
        // other, and the name is the discriminator.
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "model".to_string());
        Ok(Self { model, dim, name })
    }

    /// Embeds one text into a `dim()`-wide vector.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        self.model.encode_single(text)
    }

    /// Output dimensionality of this model.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Model name (the model directory's basename) — the value stored in
    /// `memory_vectors.model`.
    pub fn model_name(&self) -> &str {
        &self.name
    }
}

/// Process-wide embedder cache: resolve + load happens once, and long-running
/// servers (`engram serve`, `engram mcp`) reuse the loaded model across
/// requests. The first caller's `model_path` wins — `main` warms the cache
/// with the CLI flag before any request handler runs.
#[cfg(feature = "vector")]
fn cached_embedder(model_path: Option<&Path>) -> Result<&'static Embedder, HybridUnavailable> {
    static CACHE: std::sync::OnceLock<Result<Embedder, HybridUnavailable>> =
        std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let Some(path) = resolve_model_path(model_path) else {
                return Err(HybridUnavailable::NoModel);
            };
            Embedder::load(&path).map_err(|e| HybridUnavailable::LoadFailed(e.to_string()))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Warms the process-wide embedder cache with the CLI `--model-path` before
/// a server starts handling requests, so request-time callers (which pass no
/// path) resolve against the flag rather than only the environment. A miss
/// is fine — hybrid simply stays unavailable.
#[cfg(feature = "vector")]
pub fn warm(model_path: Option<&Path>) {
    let _ = cached_embedder(model_path);
}

/// The cached embedder when it resolved and loaded, else `None` — the
/// best-effort accessor for the post-`remember` write path, which must never
/// surface an error.
#[cfg(feature = "vector")]
pub fn try_embedder(model_path: Option<&Path>) -> Option<&'static Embedder> {
    cached_embedder(model_path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- cosine algebra (runs in every feature set) ----------------------

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        let v = [0.5f32, -1.0, 2.0];
        let got = cosine(&v, &v);
        assert!((got - 1.0).abs() < 1e-6, "expected 1.0, got {got}");
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn cosine_of_opposite_vectors_is_minus_one() {
        let got = cosine(&[1.0, 2.0], &[-1.0, -2.0]);
        assert!((got + 1.0).abs() < 1e-6, "expected -1.0, got {got}");
    }

    #[test]
    fn cosine_is_zero_on_length_mismatch_and_zero_norm() {
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0, "length mismatch");
        assert_eq!(cosine(&[], &[]), 0.0, "empty");
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 2.0]), 0.0, "zero norm left");
        assert_eq!(cosine(&[1.0, 2.0], &[0.0, 0.0]), 0.0, "zero norm right");
    }

    #[test]
    fn cosine_is_scale_invariant() {
        let a = [1.0f32, 2.0, 3.0];
        let b = [2.0f32, 4.0, 6.0];
        let got = cosine(&a, &b);
        assert!((got - 1.0).abs() < 1e-6, "expected 1.0, got {got}");
    }

    // ---- resolve_model_path cascade (vector feature only) ----------------

    /// Env-var mutation is process-global; every test that touches
    /// ENGRAM_MODEL / XDG_DATA_HOME / HOME serializes on this lock and
    /// restores the prior values before releasing it.
    #[cfg(feature = "vector")]
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `body` with the three cascade variables set (or removed, on
    /// `None`), restoring the previous environment afterwards.
    #[cfg(feature = "vector")]
    fn with_env(
        engram_model: Option<&std::ffi::OsStr>,
        xdg_data_home: Option<&std::ffi::OsStr>,
        home: Option<&std::ffi::OsStr>,
        body: impl FnOnce(),
    ) {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let vars = [
            ("ENGRAM_MODEL", engram_model),
            ("XDG_DATA_HOME", xdg_data_home),
            ("HOME", home),
        ];
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = vars
            .iter()
            .map(|(name, value)| {
                let prior = std::env::var_os(name);
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
                (*name, prior)
            })
            .collect();
        body();
        for (name, prior) in saved {
            match prior {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }

    #[cfg(feature = "vector")]
    #[test]
    fn resolve_prefers_the_explicit_cli_path_when_it_exists() {
        let cli_dir = tempfile::tempdir().expect("tempdir");
        let env_dir = tempfile::tempdir().expect("tempdir");
        with_env(Some(env_dir.path().as_os_str()), None, None, || {
            let got = resolve_model_path(Some(cli_dir.path()));
            assert_eq!(got.as_deref(), Some(cli_dir.path()), "CLI wins over env");
        });
    }

    #[cfg(feature = "vector")]
    #[test]
    fn resolve_falls_through_a_nonexistent_cli_path_to_the_env() {
        let env_dir = tempfile::tempdir().expect("tempdir");
        with_env(Some(env_dir.path().as_os_str()), None, None, || {
            let ghost = env_dir.path().join("does-not-exist");
            let got = resolve_model_path(Some(&ghost));
            assert_eq!(
                got.as_deref(),
                Some(env_dir.path()),
                "only paths that exist are returned; a missing CLI path falls through"
            );
        });
    }

    #[cfg(feature = "vector")]
    #[test]
    fn resolve_uses_xdg_data_home_then_home_fallback() {
        let xdg = tempfile::tempdir().expect("tempdir");
        let model_dir = xdg.path().join("engram/model");
        std::fs::create_dir_all(&model_dir).expect("mkdir");
        with_env(None, Some(xdg.path().as_os_str()), None, || {
            assert_eq!(
                resolve_model_path(None).as_deref(),
                Some(model_dir.as_path())
            );
        });

        // With XDG_DATA_HOME unset, ~/.local/share/engram/model is probed.
        let home = tempfile::tempdir().expect("tempdir");
        let fallback = home.path().join(".local/share/engram/model");
        std::fs::create_dir_all(&fallback).expect("mkdir");
        with_env(None, None, Some(home.path().as_os_str()), || {
            assert_eq!(
                resolve_model_path(None).as_deref(),
                Some(fallback.as_path())
            );
        });
    }

    #[cfg(feature = "vector")]
    #[test]
    fn resolve_returns_none_when_no_candidate_exists() {
        let empty = tempfile::tempdir().expect("tempdir");
        // XDG_DATA_HOME points somewhere real but engram/model was never
        // created there; HOME points at the same modelless place.
        with_env(
            None,
            Some(empty.path().as_os_str()),
            Some(empty.path().as_os_str()),
            || {
                assert_eq!(resolve_model_path(None), None);
            },
        );
    }
}

// Rust guideline compliant 2026-08-02
