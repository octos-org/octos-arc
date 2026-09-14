use std::path::Path;
use std::sync::Arc;

use octos_llm::{
    ContextWindowOverride, LlmProvider, ModelCatalogEntry, ProviderChain, QosCatalog, RetryProvider,
};
use tracing::{info, warn};

use crate::commands::chat::create_provider_with_api_type;
use crate::config::Config;

/// #2142: wrap a freshly-created (probe-wrapped) provider in the operator's
/// `context_window` override when the config sets one.
///
/// The override sits just OUTSIDE the local-context probe (#2135) and INSIDE
/// `RetryProvider` / `ProviderChain` — both of which delegate
/// `context_window()` as of #2135 — so the operator's value resolves through
/// the entire runtime stack and beats BOTH the static catalog and the runtime
/// probe. Applied per provider (primary and each fallback independently) so a
/// primary pin never leaks onto a fallback's own window. `None` leaves the
/// provider untouched.
pub(crate) fn apply_context_window_override(
    provider: Arc<dyn LlmProvider>,
    window: Option<u32>,
    slot: &str,
) -> Arc<dyn LlmProvider> {
    match window {
        Some(w) => {
            info!(
                context_window = w,
                slot,
                "context window overridden by config.llm (operator override wins over probe/catalog)"
            );
            Arc::new(ContextWindowOverride::new(provider, w))
        }
        None => provider,
    }
}

/// The canonical model catalog (`model_catalog.json`), compiled in. This is the
/// single source of truth for model provisioning and the researched
/// context-window / pricing floor. It ships next to the binary at release time
/// (see `scripts/build-local-bundle.sh`), but is also embedded so a fresh
/// install — one with no per-profile data-dir catalog and no `~/.octos`
/// catalog yet — still seeds the context-window table and the pricing table
/// with researched values instead of cold-start zeros.
///
/// `crates/octos-cli/src/api/ui_protocol.rs` (onboarding) and
/// `crates/octos-cli/src/commands/init.rs` reference this same const so there is
/// exactly one embedded copy.
pub(crate) const EMBEDDED_MODEL_CATALOG: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../model_catalog.json"
));

/// Parse the compiled-in canonical catalog. `None` only if the committed file
/// is malformed (a build-time invariant, so effectively always `Some`).
pub(crate) fn embedded_qos_catalog() -> Option<QosCatalog> {
    serde_json::from_str(EMBEDDED_MODEL_CATALOG).ok()
}

/// Merge a live-scored `overlay` catalog onto a full canonical `base` catalog.
///
/// For a provider present in BOTH, the merged entry keeps the canonical
/// **static** metadata (cost, context window, max output, model type,
/// deep-search quality `ds_output`) from the base and takes only the
/// **dynamic** live QoS (score, stability, latency) from the overlay. This is
/// deliberate: a stale cost/context value written before an upgrade must not
/// win over the corrected canonical value and re-persist itself forever — the
/// on-disk catalog would never converge to the SSOT. `ds_output` (deep-search
/// quality) is also static/seed-only, but with one twist: the canonical catalog
/// uses `0` as a "not evaluated" sentinel, so it wins only when it carries a
/// positive evaluated value; when canonical `ds_output` is 0 the overlay's
/// value is preserved so an older on-disk benchmark is not erased. (Contrast
/// `cost`, where 0 is a real free-tier price and canonical 0 must win.)
/// Overlay-only providers (e.g. a configured custom-`base_url` model absent
/// from the canonical catalog) are kept verbatim; base-only providers are
/// preserved. Output is sorted by provider for deterministic diffs.
///
/// Strip an OpenAI-compatible `@host` tag from the family segment of a provider
/// key (`moonshot@api/kimi-k2.5` -> `moonshot/kimi-k2.5`). Returns `None` when
/// there is no tag, so callers only do the extra lookup when it can differ.
fn normalized_provider_key(provider: &str) -> Option<String> {
    let (family, model) = provider.split_once('/')?;
    let bare = family.split('@').next().unwrap_or(family);
    (bare != family).then(|| format!("{bare}/{model}"))
}

pub(crate) fn merge_qos_catalog(base: &QosCatalog, overlay: &QosCatalog) -> QosCatalog {
    // Immutable view of the canonical base, keyed by provider. Static-field
    // lookups below consult ONLY this map — never the accumulating `by_provider`
    // — so an overlay lane can never source its "canonical" static metadata from
    // a prior overlay lane that merely shares a host-tag-normalized key (e.g. an
    // overlay-only `openai/custom` followed by `openai@proxy/custom`: both are
    // custom lanes absent from the base and must be kept verbatim, not merged
    // into each other).
    let base_by_provider: std::collections::HashMap<&str, &ModelCatalogEntry> = base
        .models
        .iter()
        .map(|entry| (entry.provider.as_str(), entry))
        .collect();
    let mut by_provider: std::collections::BTreeMap<String, ModelCatalogEntry> = base
        .models
        .iter()
        .map(|entry| (entry.provider.clone(), entry.clone()))
        .collect();
    for entry in &overlay.models {
        // Find the canonical base entry by the exact key, then by the
        // host-tag-stripped key — a live OpenAI-compatible lane exports a
        // `moonshot@api/kimi-k2.5` key that must still reconcile with the
        // untagged canonical `moonshot/kimi-k2.5` so its corrected static
        // metadata wins. Copy the static fields out (all `Copy`) so the
        // immutable borrow ends before the insert below.
        let base_static = base_by_provider
            .get(entry.provider.as_str())
            .copied()
            .or_else(|| {
                normalized_provider_key(&entry.provider)
                    .and_then(|key| base_by_provider.get(key.as_str()).copied())
            })
            .map(|b| {
                (
                    b.model_type,
                    b.cost_in,
                    b.cost_out,
                    b.context_window,
                    b.max_output,
                    b.ds_output,
                    b.is_family_default,
                )
            });
        let merged = match base_static {
            Some((
                model_type,
                cost_in,
                cost_out,
                context_window,
                max_output,
                ds_output,
                is_family_default,
            )) => {
                ModelCatalogEntry {
                    provider: entry.provider.clone(),
                    // Static — canonical base wins (SSOT convergence). The
                    // family-default flag is canonical-only: a live QoS row must
                    // never be able to elect a different default model.
                    is_family_default,
                    model_type,
                    cost_in,
                    cost_out,
                    context_window,
                    max_output,
                    // `ds_output` (deep-search quality) is static/seed-only, BUT
                    // the canonical catalog leaves it `0` = "not evaluated". Taking
                    // a canonical 0 unconditionally would erase a positive value
                    // an older on-disk catalog had actually benchmarked. So the
                    // canonical wins only when it carries an evaluated value;
                    // otherwise the overlay's value is preserved. (`cost` differs:
                    // 0 there is a real free-tier price, so canonical 0 must win.)
                    ds_output: if ds_output != 0 {
                        ds_output
                    } else {
                        entry.ds_output
                    },
                    // Dynamic — live overlay wins.
                    stability: entry.stability,
                    tool_avg_ms: entry.tool_avg_ms,
                    p95_ms: entry.p95_ms,
                    score: entry.score,
                }
            }
            None => entry.clone(),
        };
        by_provider.insert(entry.provider.clone(), merged);
    }
    QosCatalog {
        // The overlay carries the fresh export timestamp; prefer it so the file
        // reflects when the live scores were last written.
        updated_at: overlay.updated_at.clone(),
        models: by_provider.into_values().collect(),
    }
}

/// Result of wiring up the LLM provider chain.
///
/// `llm` is the top-level provider that callers should pass to
/// `Agent`/`SessionManager`. `runtime_qos_catalog` is the catalog that was
/// derived from the seed (embedded canonical / on-disk catalog); it has
/// already been pushed into `octos_llm::context` and `octos_llm::pricing`
/// and persisted to `model_catalog.json` before this struct is returned.
pub(crate) struct ProviderBundle {
    pub llm: Arc<dyn LlmProvider>,
    pub runtime_qos_catalog: Option<QosCatalog>,
}

/// Build the LLM provider chain with retry + sequential failover.
///
/// 1. Wraps the primary `base_provider` in `RetryProvider` (unless
///    `no_retry`), layers in each `config.fallback_models` entry on
///    top, propagating each fallback's `cost_per_m` into the cost
///    vector and its `api_key_env` into the per-fallback config clone.
/// 2. When more than one provider exists, assembles a static
///    `ProviderChain` (bare `RetryProvider` when no fallbacks).
/// 3. Materializes the runtime QoS catalog from the seed and seeds
///    `octos_llm::context::seed_from_catalog` +
///    `octos_llm::pricing::seed_pricing_catalog`.
/// 4. Persists `model_catalog.json` next to `data_dir`.
pub(crate) fn build_provider_chain(
    base_provider: Arc<dyn LlmProvider>,
    config: &Config,
    data_dir: &Path,
    no_retry: bool,
) -> ProviderBundle {
    // #2142: operator override of the primary's effective context window,
    // applied before RetryProvider wraps it so it propagates through
    // the delegating stack and beats the probe/catalog.
    let base_provider =
        apply_context_window_override(base_provider, config.context_window, "primary");

    let llm: Arc<dyn LlmProvider> = if no_retry {
        base_provider
    } else if config.fallback_models.is_empty() {
        Arc::new(RetryProvider::new(base_provider))
    } else {
        let mut providers: Vec<Arc<dyn LlmProvider>> =
            vec![Arc::new(RetryProvider::new(base_provider))];
        for fb in &config.fallback_models {
            // Always swap in this fallback's own `api_key_env`. When the
            // fallback omits it (None), we clear the primary's value so
            // `Config::get_api_key` falls back to the provider registry
            // default for the fallback's family — otherwise a
            // cross-provider fallback (e.g. deepseek behind moonshot-coding)
            // would inherit the primary's AUTODL_API_KEY instead of
            // using DEEPSEEK_API_KEY.
            let mut fb_config = config.clone();
            fb_config.api_key_env = fb.api_key_env.clone();
            match create_provider_with_api_type(
                &fb.provider,
                &fb_config,
                fb.model.clone(),
                fb.base_url.clone(),
                fb.api_type.as_deref(),
            ) {
                Ok(p) => {
                    // #2142: per-fallback context-window override.
                    let p = apply_context_window_override(p, fb.context_window, "fallback");
                    providers.push(Arc::new(RetryProvider::new(p)));
                }
                Err(e) => {
                    warn!(provider = %fb.provider, error = %e, "skipping fallback provider");
                }
            }
        }
        Arc::new(ProviderChain::new(providers))
    };

    let catalog_path = data_dir.join("model_catalog.json");
    // Merge the canonical STATIC metadata onto the on-disk seed BEFORE it seeds
    // the runtime context/pricing tables. Otherwise a stale static value in an
    // on-disk `model_catalog.json` written by a pre-upgrade build (or
    // hand-edited) would drive THIS process's cost estimates and context
    // windows until the next restart: the persisted file is corrected below via
    // the same merge, but the already-seeded runtime tables would keep the
    // stale values. `embedded_base` is computed once and reused as the persist
    // base further down.
    let embedded_base = embedded_qos_catalog();
    let seed_catalog = load_seed_qos_catalog(data_dir).map(|on_disk| match &embedded_base {
        Some(base) => merge_qos_catalog(base, &on_disk),
        None => on_disk,
    });

    let runtime_qos_catalog: Option<QosCatalog> =
        materialize_runtime_qos_catalog(seed_catalog.as_ref());

    // The persisted `model_catalog.json` merges the runtime catalog ON TOP of
    // the full compiled-in canonical catalog, so it stays a complete superset
    // rather than shrinking to just the configured lanes. This also "seeds the
    // data-dir on first run": a fresh install writes the full catalog here
    // immediately. Reuse the base already loaded above for the seed merge
    // (single embed parse).
    let persist_base = embedded_base;

    if let Some(ref catalog) = runtime_qos_catalog {
        let ctx_entries: Vec<(String, u64, u64)> = catalog
            .models
            .iter()
            .map(|m| (m.provider.clone(), m.context_window, m.max_output))
            .collect();
        octos_llm::context::seed_from_catalog(&ctx_entries);
        let price_entries: Vec<(String, f64, f64)> = catalog
            .models
            .iter()
            .map(|m| (m.provider.clone(), m.cost_in, m.cost_out))
            .collect();
        octos_llm::pricing::seed_pricing_catalog(&price_entries);
        let to_persist = match &persist_base {
            Some(base) => merge_qos_catalog(base, catalog),
            None => catalog.clone(),
        };
        persist_qos_catalog(&catalog_path, &to_persist);
    }

    ProviderBundle {
        llm,
        runtime_qos_catalog,
    }
}

/// Derive a cold-start runtime catalog from static model metadata.
///
/// The heuristic model catalog is seed data, not a live score file. This
/// materializes an initial runtime catalog so downstream consumers can use
/// the same score semantics before any live traffic has been observed.
/// (Inlined from the retired adaptive-router cold-start path,
/// with its default scoring weights: latency 0.3, error rate 0.3,
/// priority 0.2, cost 0.2.)
pub(crate) fn derive_cold_start_qos_catalog(entries: &[ModelCatalogEntry]) -> QosCatalog {
    let weight_latency = 0.3;
    let weight_error_rate = 0.3;
    let weight_priority = 0.2;
    let weight_cost = 0.2;

    let max_quality = entries
        .iter()
        .map(|entry| entry.ds_output as f64 * entry.stability.clamp(0.0, 1.0))
        .fold(0.0_f64, f64::max);
    let max_cost = entries
        .iter()
        .map(|entry| entry.cost_out)
        .fold(0.0_f64, f64::max);
    let max_priority = entries.len().max(1) as f64;

    let models = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let baseline_stab = entry.stability.clamp(0.0, 1.0);
            let blended_err = 1.0 - baseline_stab;

            let quality = entry.ds_output as f64 * baseline_stab;
            let norm_quality = if max_quality > 0.0 {
                1.0 - (quality / max_quality)
            } else {
                0.5
            };

            // No live throughput at cold start, so keep the throughput term neutral.
            let norm_throughput = 0.5;
            let norm_priority = idx as f64 / max_priority;
            let norm_cost = if max_cost > 0.0 && entry.cost_out > 0.0 {
                entry.cost_out / max_cost
            } else {
                0.0
            };
            let ranking_component = 0.6 * norm_quality + 0.4 * norm_throughput;

            let mut model = entry.clone();
            model.score = weight_error_rate * blended_err
                + weight_latency * ranking_component
                + weight_priority * norm_priority
                + weight_cost * norm_cost;
            model
        })
        .collect();

    QosCatalog {
        updated_at: chrono::Utc::now().to_rfc3339(),
        models,
    }
}

pub(crate) fn load_seed_qos_catalog(data_dir: &Path) -> Option<QosCatalog> {
    let candidates = [
        data_dir.join("model_catalog.json"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".octos/model_catalog.json"),
    ];
    for path in &candidates {
        if let Ok(json) = std::fs::read_to_string(path) {
            if let Ok(catalog) = serde_json::from_str::<QosCatalog>(&json) {
                return Some(catalog);
            }
        }
    }
    // Fresh install: no runtime catalog on disk yet. Fall back to the compiled-in
    // canonical catalog so the context-window table / pricing table are
    // seeded with researched values instead of cold-start zeros. (On any machine
    // that already has a runtime catalog this branch is never reached.)
    embedded_qos_catalog()
}

pub(crate) fn persist_qos_catalog(path: &Path, catalog: &QosCatalog) {
    match serde_json::to_string_pretty(catalog) {
        Ok(json) => {
            if let Err(error) = std::fs::write(path, json) {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "failed to persist runtime model catalog"
                );
            }
        }
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "failed to serialize runtime model catalog"
        ),
    }
}

pub(crate) fn materialize_runtime_qos_catalog(
    seed_catalog: Option<&QosCatalog>,
) -> Option<QosCatalog> {
    seed_catalog.map(|catalog| derive_cold_start_qos_catalog(&catalog.models))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_llm::ModelType;
    use tempfile::tempdir;

    fn sample_catalog(scores: [f64; 2]) -> QosCatalog {
        QosCatalog {
            updated_at: "2026-04-11T00:00:00Z".to_string(),
            models: vec![
                ModelCatalogEntry {
                    provider: "zai/glm-5-turbo".to_string(),
                    model_type: ModelType::Fast,
                    is_family_default: false,
                    stability: 0.97,
                    tool_avg_ms: 900,
                    p95_ms: 1500,
                    score: scores[0],
                    cost_in: 0.5,
                    cost_out: 2.0,
                    ds_output: 1200,
                    context_window: 128_000,
                    max_output: 8_192,
                },
                ModelCatalogEntry {
                    provider: "deepseek/deepseek-v4-pro".to_string(),
                    model_type: ModelType::Strong,
                    is_family_default: false,
                    stability: 0.92,
                    tool_avg_ms: 1400,
                    p95_ms: 2400,
                    score: scores[1],
                    cost_in: 0.8,
                    cost_out: 3.2,
                    ds_output: 800,
                    context_window: 128_000,
                    max_output: 16_384,
                },
            ],
        }
    }

    #[test]
    fn load_seed_qos_catalog_reads_profile_local_catalog() {
        let temp = tempdir().unwrap();
        let data_dir = temp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let path = data_dir.join("model_catalog.json");
        let catalog = sample_catalog([0.0, 0.0]);
        std::fs::write(&path, serde_json::to_string_pretty(&catalog).unwrap()).unwrap();

        let loaded = load_seed_qos_catalog(&data_dir).expect("catalog should load");
        assert_eq!(loaded.models.len(), 2);
        assert_eq!(loaded.models[0].provider, "zai/glm-5-turbo");
        assert_eq!(loaded.models[1].provider, "deepseek/deepseek-v4-pro");
    }

    fn scored_entry(provider: &str, score: f64, ctx: u64) -> ModelCatalogEntry {
        ModelCatalogEntry {
            provider: provider.to_string(),
            model_type: ModelType::Strong,
            is_family_default: false,
            stability: 1.0,
            tool_avg_ms: 0,
            p95_ms: 0,
            score,
            cost_in: 0.0,
            cost_out: 0.0,
            ds_output: 0,
            context_window: ctx,
            max_output: 0,
        }
    }

    /// The exporter persists `merge(embedded_base, live_export)`: every base
    /// entry survives; for an overlapping provider the canonical STATIC metadata
    /// (context window etc.) wins while the live DYNAMIC score is taken from the
    /// overlay; a live-only lane is appended. This keeps the on-disk catalog a
    /// full superset that converges to the SSOT instead of re-persisting stale
    /// static values written before an upgrade.
    #[test]
    fn merge_qos_catalog_preserves_base_and_overlays_live() {
        // Base deepseek carries an EVALUATED canonical deep-search quality (>0);
        // the overlay's is stale, so the canonical value wins. Base glm-5.3 leaves
        // ds_output at the `0` "not evaluated" sentinel (via `scored_entry`), so an
        // evaluated overlay value there must be PRESERVED, not clobbered by 0.
        let mut base_deepseek = scored_entry("deepseek/deepseek-v4-pro", 0.0, 1_048_576);
        base_deepseek.ds_output = 1500;
        let base = QosCatalog {
            updated_at: "SEED".to_string(),
            models: vec![scored_entry("zai/glm-5.3", 0.0, 1_000_000), base_deepseek],
        };
        let mut overlay_deepseek = scored_entry("deepseek/deepseek-v4-pro", 0.87, 999);
        overlay_deepseek.ds_output = 1; // stale seed re-exported by the router
        // Host-tagged lane whose bare base (`zai/glm-5.3`) is unevaluated (0);
        // this lane carries a real benchmarked ds_output the merge must keep.
        let mut overlay_glm = scored_entry("zai@api/glm-5.3", 0.9, 111);
        overlay_glm.ds_output = 2222;
        let overlay = QosCatalog {
            updated_at: "2026-07-12T00:00:00Z".to_string(),
            models: vec![
                // Live score for a configured lane already in the base, but with
                // a STALE context window + ds_output (as if seeded from a
                // pre-upgrade catalog) — the canonical values must win.
                overlay_deepseek,
                // A host-tagged OpenAI-compatible lane whose bare form IS in the
                // base — it must reconcile with `zai/glm-5.3` (stale ctx wins from
                // canonical, live score from overlay), not be treated as new.
                overlay_glm,
                // … plus a lane not in the base (custom base_url model).
                scored_entry("stub/stub-model", 0.5, 0),
                // Two overlay-only lanes that share a host-tag-normalized key but
                // are BOTH absent from the base. Ordered so the untagged lane is
                // processed first: a lookup against the accumulating merge map
                // (rather than the immutable base) would let the proxy lane copy
                // this lane's static fields. Both must be kept verbatim instead.
                scored_entry("openai/custom-model", 0.3, 40_000),
                scored_entry("openai@proxy/custom-model", 0.4, 50_000),
            ],
        };

        let merged = merge_qos_catalog(&base, &overlay);
        // Fresh export timestamp is carried onto the merged catalog.
        assert_eq!(merged.updated_at, "2026-07-12T00:00:00Z");
        // base-only (zai/glm-5.3) + overlaid (deepseek) + host-tagged
        // (zai@api/glm-5.3) + overlay-only (stub) + two custom lanes = 6.
        assert_eq!(merged.models.len(), 6);
        let by = |p: &str| merged.models.iter().find(|m| m.provider == p).unwrap();
        // Base-only entry preserved (researched context window intact).
        assert_eq!(by("zai/glm-5.3").context_window, 1_000_000);
        assert_eq!(by("zai/glm-5.3").score, 0.0);
        // Overlapping provider: live score wins (DYNAMIC) …
        assert_eq!(by("deepseek/deepseek-v4-pro").score, 0.87);
        // … but the canonical static context window wins over the stale overlay.
        assert_eq!(
            by("deepseek/deepseek-v4-pro").context_window,
            1_048_576,
            "canonical static field must win over a stale overlay value"
        );
        // ds_output: the canonical base has an EVALUATED value (>0), so it wins
        // over the stale overlay.
        assert_eq!(
            by("deepseek/deepseek-v4-pro").ds_output,
            1500,
            "evaluated canonical ds_output must win over a stale overlay value"
        );
        // Host-tagged lane reconciled with the bare canonical base: canonical
        // static context window, live overlay score.
        assert_eq!(
            by("zai@api/glm-5.3").context_window,
            1_000_000,
            "host-tagged lane converges to canonical static via key normalization"
        );
        assert_eq!(by("zai@api/glm-5.3").score, 0.9);
        // …but the canonical ds_output for glm-5.3 is the `0` "not evaluated"
        // sentinel, so the overlay's benchmarked value must be PRESERVED, not
        // erased to 0.
        assert_eq!(
            by("zai@api/glm-5.3").ds_output,
            2222,
            "an evaluated overlay ds_output survives when the canonical value is the 0 sentinel"
        );
        // Overlay-only lane appended.
        assert_eq!(by("stub/stub-model").score, 0.5);
        // Both overlay-only custom lanes are kept verbatim — the proxy lane does
        // NOT source its static metadata from the untagged lane processed before
        // it (that would happen if lookups consulted the accumulating merge map).
        assert_eq!(by("openai/custom-model").context_window, 40_000);
        assert_eq!(
            by("openai@proxy/custom-model").context_window,
            50_000,
            "overlay-only proxy lane must keep its own static, not copy a sibling overlay lane"
        );
        // Deterministic (sorted-by-provider) output.
        let providers: Vec<&str> = merged.models.iter().map(|m| m.provider.as_str()).collect();
        let mut sorted = providers.clone();
        sorted.sort_unstable();
        assert_eq!(providers, sorted);
    }

    /// The compiled-in canonical catalog (the seed floor for fresh installs) is
    /// well-formed and reflects curation: glm-5.3 + the moonshot-coding k3
    /// default present, deepseek-chat removed.
    #[test]
    fn embedded_qos_catalog_is_curated_ssot() {
        let catalog = embedded_qos_catalog().expect("embedded canonical catalog must parse");
        let has = |p: &str| catalog.models.iter().any(|m| m.provider == p);
        assert!(has("zai/glm-5.3"), "glm-5.3 present");
        assert!(
            has("moonshot-coding/k3"),
            "moonshot-coding k3 default present"
        );
        assert!(
            !has("deepseek/deepseek-chat"),
            "deepseek-chat curated out of the embedded catalog"
        );
        // Researched context window survives the round-trip through the embed.
        let glm53 = catalog
            .models
            .iter()
            .find(|m| m.provider == "zai/glm-5.3")
            .unwrap();
        assert_eq!(glm53.context_window, 1_000_000);
        // k3 researched values: 1M window, 131072 (default max completion).
        let k3 = catalog
            .models
            .iter()
            .find(|m| m.provider == "moonshot-coding/k3")
            .unwrap();
        assert_eq!(k3.context_window, 1_048_576);
        assert_eq!(k3.max_output, 131_072);
        assert!(
            k3.is_family_default,
            "k3 is the moonshot-coding family default"
        );
    }

    #[test]
    fn persist_qos_catalog_round_trips_runtime_scores() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("model_catalog.json");
        let catalog = sample_catalog([0.21857142857142858, 0.4]);

        persist_qos_catalog(&path, &catalog);

        let json = std::fs::read_to_string(&path).unwrap();
        let loaded: QosCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.models.len(), 2);
        assert!((loaded.models[0].score - 0.21857142857142858).abs() < 1e-12);
        assert!((loaded.models[1].score - 0.4).abs() < 1e-12);
    }

    #[test]
    fn materialize_runtime_qos_catalog_derives_non_zero_scores_from_seed() {
        let seed = sample_catalog([0.0, 0.0]);

        let materialized =
            materialize_runtime_qos_catalog(Some(&seed)).expect("catalog should materialize");

        assert_eq!(materialized.models.len(), seed.models.len());
        assert!(materialized.models.iter().all(|entry| entry.score > 0.0));
    }

    /// #2142: an operator `context_window` override must resolve through the
    /// WHOLE assembled stack (RetryProvider here), beating what the underlying
    /// provider reports — the acceptance criterion "a profile pinning
    /// context_window: 16384 on a 262K server reports 16384 through the full
    /// runtime stack".
    #[test]
    fn context_window_override_wins_through_the_assembled_stack() {
        use crate::config::Config;
        use octos_core::Message;
        use octos_llm::{ChatConfig, ChatResponse, ToolSpec};
        use std::sync::Arc;

        // A backend that advertises a large window (stands in for the probed
        // 262K llama-server).
        struct WideProvider;
        #[async_trait::async_trait]
        impl LlmProvider for WideProvider {
            async fn chat(
                &self,
                _messages: &[Message],
                _tools: &[ToolSpec],
                _config: &ChatConfig,
            ) -> eyre::Result<ChatResponse> {
                Err(eyre::eyre!("stub not callable in tests"))
            }
            fn model_id(&self) -> &str {
                "wide-model"
            }
            fn provider_name(&self) -> &str {
                "wide"
            }
            fn context_window(&self) -> u32 {
                262_144
            }
        }

        let temp = tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();

        // Control: no override → the backend's own window survives the
        // RetryProvider wrap (delegation, per #2135).
        let control =
            build_provider_chain(Arc::new(WideProvider), &Config::default(), &data_dir, false);
        assert_eq!(
            control.llm.context_window(),
            262_144,
            "without an override the probed/backend window must pass through the stack"
        );

        // Override: 16384 must win through RetryProvider all the way out.
        let config = Config {
            context_window: Some(16_384),
            ..Default::default()
        };
        let overridden = build_provider_chain(Arc::new(WideProvider), &config, &data_dir, false);
        assert_eq!(
            overridden.llm.context_window(),
            16_384,
            "config.context_window must override the 262K backend through the full stack"
        );

        // And in the no_retry path (bare provider) the override still holds.
        let bare = build_provider_chain(Arc::new(WideProvider), &config, &data_dir, true);
        assert_eq!(
            bare.llm.context_window(),
            16_384,
            "override must hold even on the no_retry (unwrapped) path"
        );
    }
}
