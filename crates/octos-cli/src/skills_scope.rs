use std::path::{Path, PathBuf};

use eyre::Result;
use octos_agent::{SkillFilter, SkillsLoader};

use crate::profiles::{ProfileStore, SkillSelectionMode, UserProfile};

/// Outcome of resolving a profile's inherited skill selection against a set of
/// discovered candidate skills (skill layering v1).
///
/// Callers pass the output of
/// [`ProfileStore::resolve_runtime_profile`](crate::profiles::ProfileStore::resolve_runtime_profile)
/// (an already parent- and defaults-merged profile) plus the discovered
/// candidate skill ids; the resolver partitions the candidates into the ones
/// that load and the ones an inherited rule excludes, and produces the
/// crate-agnostic [`SkillFilter`] handed to both the plugin loader and the
/// [`SkillsLoader`].
#[derive(Debug, Clone)]
pub struct ResolvedSkillCatalog {
    /// The effective selection mode that was applied.
    pub mode: SkillSelectionMode,
    /// Candidate skill ids that will load (deduped, precedence preserved).
    pub enabled: Vec<String>,
    /// Candidate skill ids excluded by the active selection (debug logging).
    pub disabled: Vec<String>,
    /// Filter to hand to the loaders. `None` ⇒ the profile has no skills layer
    /// ⇒ load everything (byte-identical to pre-skill-layering behavior).
    pub filter: Option<SkillFilter>,
}

impl ResolvedSkillCatalog {
    /// Whether any candidate was excluded (for a one-line debug log).
    pub fn has_disabled(&self) -> bool {
        !self.disabled.is_empty()
    }
}

/// Resolve a profile's inherited skill selection over `discovered` candidate
/// skill ids.
///
/// `effective_profile` MUST be the resolved runtime profile (parent + global
/// `profile-defaults.json` already merged via
/// [`ProfileStore::resolve_runtime_profile`](crate::profiles::ProfileStore::resolve_runtime_profile))
/// so its `config.skills` carries the fully-merged selection layer.
///
/// Candidates are deduped by id keeping first-occurrence precedence (mirrors
/// the loader's "first dir wins" discovery). Rule matching is last-wins per id:
/// in `AllDiscovered` a skill loads unless a rule disables it; in `AllowList`
/// only skills with an enabling rule load.
///
/// When the profile has no skills layer at all the returned `filter` is `None`
/// and every candidate is reported enabled — callers pass `None` to the loaders
/// and nothing is filtered.
pub fn resolve_profile_skills(
    effective_profile: &UserProfile,
    discovered: &[String],
) -> ResolvedSkillCatalog {
    // Dedupe candidates, preserving first-occurrence precedence.
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<String> = discovered
        .iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect();

    match effective_profile.config.skills.as_ref() {
        None => ResolvedSkillCatalog {
            mode: SkillSelectionMode::AllDiscovered,
            enabled: unique,
            disabled: Vec::new(),
            filter: None,
        },
        Some(skills) => {
            let (mut enabled, mut disabled) = (Vec::new(), Vec::new());
            for id in unique {
                if skills.allows(&id) {
                    enabled.push(id);
                } else {
                    disabled.push(id);
                }
            }
            ResolvedSkillCatalog {
                mode: skills.effective_mode(),
                enabled,
                disabled,
                filter: Some(skills.to_agent_filter()),
            }
        }
    }
}

/// Resolve the installed skills directory for exactly the requested account.
///
/// This is intentionally strict: sub-accounts do not inherit their parent
/// profile's installed customer skills.
pub fn resolve_account_skills_dir(store: &ProfileStore, profile_id: &str) -> Result<PathBuf> {
    let profile = store
        .get(profile_id)?
        .ok_or_else(|| eyre::eyre!("profile '{profile_id}' not found"))?;
    let data_dir = store.resolve_data_dir(&profile);
    Ok(data_dir.join("skills"))
}

/// Build a skills loader scoped to the current account only.
pub fn build_account_skills_loader(data_dir: &Path) -> SkillsLoader {
    SkillsLoader::new(data_dir)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::profiles::{
        GatewaySettings, ProfileConfig, ProfileSkillsConfig, SkillRule, UserProfile,
    };

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn profile_with_skills(skills: Option<ProfileSkillsConfig>) -> UserProfile {
        UserProfile {
            id: "p".into(),
            name: "p".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                skills,
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn rule(id: &str, enabled: bool) -> SkillRule {
        SkillRule {
            id: id.to_string(),
            enabled,
        }
    }

    #[test]
    fn resolve_none_layer_loads_everything_and_filters_nothing() {
        let profile = profile_with_skills(None);
        let discovered = ids(&["news", "weather", "time"]);
        let catalog = resolve_profile_skills(&profile, &discovered);
        assert_eq!(catalog.mode, SkillSelectionMode::AllDiscovered);
        assert_eq!(catalog.enabled, discovered);
        assert!(catalog.disabled.is_empty());
        // No filter ⇒ loaders do zero filtering (backwards-compatible).
        assert!(catalog.filter.is_none());
    }

    #[test]
    fn resolve_all_discovered_excludes_only_disabled_rule() {
        let profile = profile_with_skills(Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllDiscovered),
            rules: vec![rule("weather", false)],
        }));
        let catalog = resolve_profile_skills(&profile, &ids(&["news", "weather", "time"]));
        assert_eq!(catalog.enabled, ids(&["news", "time"]));
        assert_eq!(catalog.disabled, ids(&["weather"]));
        let filter = catalog.filter.expect("filter present");
        assert!(filter.allows("news"));
        assert!(!filter.allows("weather"));
        // An id with no rule still loads under AllDiscovered.
        assert!(filter.allows("brand-new-skill"));
    }

    #[test]
    fn resolve_allow_list_loads_only_enabled_rules() {
        let profile = profile_with_skills(Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllowList),
            rules: vec![rule("news", true), rule("time", false)],
        }));
        let catalog = resolve_profile_skills(&profile, &ids(&["news", "weather", "time"]));
        assert_eq!(catalog.enabled, ids(&["news"]));
        assert_eq!(catalog.disabled, ids(&["weather", "time"]));
        let filter = catalog.filter.expect("filter present");
        assert!(filter.allows("news"));
        // Not allow-listed ⇒ excluded.
        assert!(!filter.allows("weather"));
        // Explicitly disabled ⇒ excluded even though a rule exists.
        assert!(!filter.allows("time"));
    }

    #[test]
    fn resolve_profile_may_reenable_inherited_disabled_rule() {
        // Simulates the post-merge config where an inherited `enabled: false`
        // was replaced by the profile's `enabled: true` (last-wins per id).
        let profile = profile_with_skills(Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllDiscovered),
            rules: vec![rule("news", false), rule("news", true)],
        }));
        let catalog = resolve_profile_skills(&profile, &ids(&["news"]));
        assert_eq!(catalog.enabled, ids(&["news"]));
        assert!(catalog.disabled.is_empty());
    }

    #[test]
    fn resolve_dedupes_candidates_preserving_precedence() {
        let profile = profile_with_skills(None);
        let catalog = resolve_profile_skills(&profile, &ids(&["news", "news", "time"]));
        assert_eq!(catalog.enabled, ids(&["news", "time"]));
    }

    #[test]
    fn resolve_account_skills_dir_keeps_sub_account_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let parent = UserProfile {
            id: "dspfac".into(),
            name: "DSPFAC".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let child = UserProfile {
            id: "dspfac--newsbot".into(),
            name: "Newsbot".into(),
            enabled: true,
            data_dir: None,
            parent_id: Some("dspfac".into()),
            public_subdomain: Some("newsbot".into()),
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.save(&parent).unwrap();
        store.save(&child).unwrap();

        let parent_dir = resolve_account_skills_dir(&store, "dspfac").unwrap();
        let child_dir = resolve_account_skills_dir(&store, "dspfac--newsbot").unwrap();

        assert_ne!(parent_dir, child_dir);
        assert!(child_dir.to_string_lossy().contains("dspfac--newsbot"));
    }
}
