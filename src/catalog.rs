//! Model identity and alias resolution (DESIGN Step 10).
//!
//! The harness name (`name:`) is canonical. Accepted aliases, kept from v3.3 (G5):
//! the GGUF file name with or without extension, the full path, all case-insensitive.

use crate::model::ModelEntry;
use std::path::Path;

/// Lower-cased file name and stem of a `model:` reference, used for alias matching.
fn ref_aliases(model_ref: &str) -> Vec<String> {
    let path = Path::new(model_ref);
    let mut aliases = Vec::new();
    if let Some(file_name) = path.file_name().and_then(|s| s.to_str()) {
        let lower = file_name.to_ascii_lowercase();
        aliases.push(lower.clone());
        if let Some(stem) = lower.strip_suffix(".gguf") {
            aliases.push(stem.to_string());
        }
    }
    aliases
}

/// Resolves a client-supplied `model` string to a catalog entry.
///
/// Match order: exact harness name, then case-insensitive name, then GGUF file-name /
/// stem aliases from the `model:` reference. Only catalog identity is resolved here;
/// class filtering per listener happens at the router (Step 11).
///
/// # Returns
///
/// `Some(&ModelEntry)` for the first match, `None` when no entry matches — the caller
/// owes the caller a `404` listing that listener's ids (Step 11).
pub fn resolve<'a>(query: &str, models: &'a [ModelEntry]) -> Option<&'a ModelEntry> {
    let lower = query.trim().to_ascii_lowercase();
    models
        .iter()
        .find(|m| m.name == query.trim())
        .or_else(|| models.iter().find(|m| m.name.to_ascii_lowercase() == lower))
        .or_else(|| {
            models.iter().find(|m| {
                m.model_ref
                    .as_deref()
                    .is_some_and(|r| ref_aliases(r).contains(&lower))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;

    fn catalog() -> Vec<ModelEntry> {
        vec![
            ModelEntry {
                name: "qwen3-0.6b".to_string(),
                engine: "llama".to_string(),
                category: CategoryId::Aux,
                vram_gb: Some(2.0),
                model_ref: Some("F:/MODELS/qwen3-0.6b.gguf".to_string()),
                file: None,
            },
            ModelEntry {
                name: "embeddinggemma-2".to_string(),
                engine: "llama".to_string(),
                category: CategoryId::Emb,
                vram_gb: Some(2.0),
                model_ref: Some("F:/MODELS/embeddinggemma-2.gguf".to_string()),
                file: None,
            },
        ]
    }

    #[test]
    fn resolves_exact_then_case_insensitive() {
        let models = catalog();
        assert_eq!(
            resolve("qwen3-0.6b", &models).map(|m| m.name.as_str()),
            Some("qwen3-0.6b")
        );
        assert_eq!(
            resolve("Qwen3-0.6B", &models).map(|m| m.name.as_str()),
            Some("qwen3-0.6b")
        );
    }

    #[test]
    fn resolves_gguf_file_name_and_stem_aliases() {
        let models = catalog();
        assert_eq!(
            resolve("embeddinggemma-2.gguf", &models).map(|m| m.name.as_str()),
            Some("embeddinggemma-2")
        );
        assert_eq!(
            resolve("EMBEDDINGGEMMA-2", &models).map(|m| m.name.as_str()),
            Some("embeddinggemma-2")
        );
    }

    #[test]
    fn unknown_model_resolves_to_none() {
        let models = catalog();
        assert!(resolve("not-a-model", &models).is_none());
    }
}
