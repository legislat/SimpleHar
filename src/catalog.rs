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

/// The outcome of a catalog lookup, as returned by [`resolve`].
///
/// The variants are exhaustive on purpose: a caller cannot reach an entry without
/// handling [`Resolution::Ambiguous`] first, so a colliding catalog can never be
/// answered by whichever yaml happened to sort first (FINDINGS-11 [4]). That is the
/// whole point of the type — an ambiguity is a catalog defect the operator must fix,
/// not a tie for the harness to break silently.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution<'a> {
    /// Exactly one entry matched the highest-ranked tier that matched anything.
    Found(&'a ModelEntry),
    /// No entry matched any tier.
    None,
    /// Several entries matched the *same* tier, listed in catalog order. The query
    /// addresses none of them, so picking one would serve a different model — and a
    /// write-back would edit the wrong file.
    Ambiguous(Vec<&'a ModelEntry>),
}

/// Every entry of `models` the tier predicate matches, in catalog order.
fn collect_matches<'a>(
    models: &'a [ModelEntry],
    predicate: impl Fn(&'a ModelEntry) -> bool,
) -> Vec<&'a ModelEntry> {
    models.iter().filter(|entry| predicate(entry)).collect()
}

/// Turns one tier's match list into a [`Resolution`].
fn classify<'a>(mut hits: Vec<&'a ModelEntry>) -> Resolution<'a> {
    if hits.len() > 1 {
        return Resolution::Ambiguous(hits);
    }
    match hits.pop() {
        Some(entry) => Resolution::Found(entry),
        None => Resolution::None,
    }
}

/// Resolves a client-supplied `model` string to a catalog entry.
///
/// Match order: exact harness name, then case-insensitive name, then GGUF file-name /
/// stem aliases from the `model:` reference. Only catalog identity is resolved here;
/// class filtering per listener happens at the router (Step 11).
///
/// Each tier is collected in full rather than won by the first match: two entries that
/// answer the same tier make the query ambiguous, and no entry is returned. A lower
/// tier is never consulted once a higher tier matched, so one exact name still beats a
/// crowd of alias claimants.
///
/// # Returns
///
/// [`Resolution::Found`] for the single match, [`Resolution::None`] when no entry
/// matches — the caller owes that a `404` listing that listener's ids (Step 11) — and
/// [`Resolution::Ambiguous`] with every colliding entry when one tier matched more
/// than one. The caller must refuse an ambiguity (the API answers `409` naming each
/// entry's yaml); treating it as `None` would misreport a real model as unknown, and
/// taking `Ambiguous`'s first entry is the defect this type exists to prevent.
///
/// # Examples
///
/// Two yamls that both claim the harness name `qwen3-0.6b` (the duplicate-name shape
/// the queue line names "duplicate names resolve scan-first"):
///
/// ```text
/// // catalog order: a/qwen3-0.6b.yaml (AUX), b/qwen3-copy.yaml (WORK)
/// match resolve("qwen3-0.6b", &models) {
///     Resolution::Found(entry)      => unreachable!(), // neither wins
///     Resolution::Ambiguous(both)   => assert_eq!(both.len(), 2),
///     Resolution::None              => unreachable!(),
/// }
/// // a unique query is unaffected, and a lower tier never overrides a higher one:
/// // resolve("Qwen3-0.6B", &models) is Ambiguous too (tier 2 claims both), while
/// // resolve("embeddinggemma-2.gguf", &models) stays Found(one).
/// ```
pub fn resolve<'a>(query: &str, models: &'a [ModelEntry]) -> Resolution<'a> {
    let trimmed = query.trim();
    let lower = trimmed.to_ascii_lowercase();
    let exact = collect_matches(models, |m| m.name == trimmed);
    if !exact.is_empty() {
        return classify(exact);
    }
    let folded = collect_matches(models, |m| m.name.to_ascii_lowercase() == lower);
    if !folded.is_empty() {
        return classify(folded);
    }
    classify(collect_matches(models, |m| {
        m.model_ref
            .as_deref()
            .is_some_and(|r| ref_aliases(r).contains(&lower))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn entry(name: &str, model_ref: &str, file: Option<&str>) -> ModelEntry {
        ModelEntry {
            name: name.to_string(),
            engine: "llama".to_string(),
            category: CategoryId::Aux,
            vram_gb: Some(2.0),
            gguf_size_gb: None,
            cost: crate::model::CostClass::Fixed,
            launch_floor_gb: None,
            load_timeout_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            model_ref: Some(model_ref.to_string()),
            serve_model: None,
            file: file.map(PathBuf::from),
            raw: BTreeMap::new(),
        }
    }

    fn catalog() -> Vec<ModelEntry> {
        vec![
            entry("qwen3-0.6b", "F:/MODELS/qwen3-0.6b.gguf", None),
            entry("embeddinggemma-2", "F:/MODELS/embeddinggemma-2.gguf", None),
        ]
    }

    fn names<'a>(resolution: &Resolution<'a>) -> Vec<&'a str> {
        match resolution {
            Resolution::Found(model) => vec![model.name.as_str()],
            Resolution::Ambiguous(models) => models.iter().map(|m| m.name.as_str()).collect(),
            Resolution::None => Vec::new(),
        }
    }

    #[test]
    fn resolves_exact_then_case_insensitive() {
        let models = catalog();
        assert_eq!(
            resolve("qwen3-0.6b", &models),
            Resolution::Found(&models[0]),
            "a unique catalog resolves to its one entry"
        );
        assert_eq!(
            resolve("Qwen3-0.6B", &models),
            Resolution::Found(&models[0]),
            "tier 2 folds case"
        );
    }

    #[test]
    fn resolves_gguf_file_name_and_stem_aliases() {
        let models = catalog();
        assert_eq!(
            resolve("embeddinggemma-2.gguf", &models),
            Resolution::Found(&models[1])
        );
        assert_eq!(
            resolve("EMBEDDINGGEMMA-2", &models),
            Resolution::Found(&models[1])
        );
    }

    #[test]
    fn unknown_model_resolves_to_none() {
        let models = catalog();
        assert!(matches!(resolve("not-a-model", &models), Resolution::None));
    }

    #[test]
    fn duplicate_names_are_ambiguous_and_name_both_yamls() {
        // The primary shape of FINDINGS-11 [4]: two files whose `name:` is byte-identical.
        // Both claim tier 1, so neither may win — the queue line's "duplicate names
        // resolve scan-first".
        let models = vec![
            entry(
                "qwen3-0.6b",
                "F:/MODELS_MID/qwen3-0.6b.gguf",
                Some("a.yaml"),
            ),
            entry(
                "qwen3-0.6b",
                "F:/MODELS_BIG/qwen3-0.6b.gguf",
                Some("b.yaml"),
            ),
        ];
        match resolve("qwen3-0.6b", &models) {
            Resolution::Ambiguous(hits) => {
                assert_eq!(hits.len(), 2, "both claimants are reported");
                assert_eq!(
                    hits.iter().map(|m| m.file.clone()).collect::<Vec<_>>(),
                    vec![Some(PathBuf::from("a.yaml")), Some(PathBuf::from("b.yaml"))],
                    "the operator needs every colliding file, in catalog order"
                );
            }
            other => panic!("a duplicate name must never resolve: {other:?}"),
        }
        // The case-folded tier is equally refused when two spellings fold together:
        // the query matches neither name exactly, so tier 2 is the only claimant.
        let folded = vec![
            entry("Qwen3-0.6B", "F:/MODELS_MID/q.gguf", Some("a.yaml")),
            entry("qwen3-0.6b", "F:/MODELS_BIG/q.gguf", Some("b.yaml")),
        ];
        assert_eq!(
            names(&resolve("QWEN3-0.6B", &folded)),
            vec!["Qwen3-0.6B", "qwen3-0.6b"]
        );
    }

    #[test]
    fn colliding_gguf_file_names_in_different_dirs_are_ambiguous() {
        // Reviewer's example: distinct harness names, one shared gguf file name, so the
        // alias tier alone is claimed twice.
        let models = vec![
            entry(
                "qwen3-mid",
                "F:\\MODELS_MID\\Qwen3-0.6B-Q8_0.gguf",
                Some("mid.yaml"),
            ),
            entry(
                "qwen3-big",
                "F:\\MODELS_BIG\\Qwen3-0.6B-Q8_0.gguf",
                Some("big.yaml"),
            ),
        ];
        for query in ["Qwen3-0.6B-Q8_0.gguf", "Qwen3-0.6B-Q8_0"] {
            match resolve(query, &models) {
                Resolution::Ambiguous(hits) => assert_eq!(
                    hits.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
                    vec!["qwen3-mid", "qwen3-big"],
                    "`{query}` addresses both quantizations"
                ),
                other => panic!("alias tier must refuse the tie for `{query}`: {other:?}"),
            }
        }
        // Each entry stays individually addressable by its own name.
        assert_eq!(resolve("qwen3-big", &models), Resolution::Found(&models[1]));
    }

    #[test]
    fn a_higher_tier_hit_beats_a_lower_tier_claimant_from_another_entry() {
        // `alpha` is A's exact name and B's gguf stem: the two tiers agree on neither
        // entry, so each query must still resolve to exactly one model.
        let models = vec![
            entry("alpha", "F:/MODELS/other.gguf", Some("a.yaml")),
            entry("beta", "F:/MODELS/alpha.gguf", Some("b.yaml")),
        ];
        assert_eq!(
            resolve("alpha", &models),
            Resolution::Found(&models[0]),
            "tier 1 for A wins over B's alias claim"
        );
        assert_eq!(
            resolve("alpha.gguf", &models),
            Resolution::Found(&models[1]),
            "tier 3 for B is the only claim on that query"
        );
        assert_eq!(
            resolve("other.gguf", &models),
            Resolution::Found(&models[0]),
            "tier 3 for A"
        );
    }

    #[test]
    fn ambiguity_is_a_tie_within_one_tier_not_across_tiers() {
        // One exact-name match plus a separate alias claimant is NOT ambiguous: the
        // tiers stay ordered, and only a tie inside a single tier is refused.
        let models = vec![
            entry("solo", "F:/MODELS/solo.gguf", Some("solo.yaml")),
            entry("shadow", "F:/MODELS/other.gguf", Some("shadow.yaml")),
        ];
        assert_eq!(collect_matches(&models, |m| m.name == "solo").len(), 1);
        assert_eq!(names(&resolve("solo", &models)), vec!["solo"]);
        assert_eq!(names(&resolve("other.gguf", &models)), vec!["shadow"]);
        assert!(matches!(resolve("nope", &models), Resolution::None));
    }
}
