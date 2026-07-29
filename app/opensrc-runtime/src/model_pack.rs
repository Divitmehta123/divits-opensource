use crate::ProviderRouter;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelPackStrategy {
    CostOptimized,
    Balanced,
    QualityFirst,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelPackStage {
    Plan,
    Execute,
    Review,
    Validate,
    Synthesize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPackMember {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub stages: Vec<ModelPackStage>,
    #[serde(default = "default_cost_tier")]
    pub cost_tier: u8,
    #[serde(default = "default_quality_tier")]
    pub quality_tier: u8,
    #[serde(default)]
    pub reasoning_level: Option<String>,
}

const fn default_cost_tier() -> u8 {
    2
}

const fn default_quality_tier() -> u8 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPack {
    pub id: String,
    pub name: String,
    pub description: String,
    pub strategy: ModelPackStrategy,
    pub members: Vec<ModelPackMember>,
    #[serde(default)]
    pub generated: bool,
}

impl ModelPack {
    #[must_use]
    pub fn select(&self, stage: ModelPackStage, role: &str) -> Option<ModelPackMember> {
        let role = role.to_ascii_lowercase();
        let mut candidates = self.members.iter().collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            let left_score = member_specialization(left, stage, &role);
            let right_score = member_specialization(right, stage, &role);
            right_score
                .cmp(&left_score)
                .then_with(|| match self.strategy {
                    ModelPackStrategy::CostOptimized => left
                        .cost_tier
                        .cmp(&right.cost_tier)
                        .then_with(|| right.quality_tier.cmp(&left.quality_tier)),
                    ModelPackStrategy::Balanced => {
                        let left_balance = i16::from(left.quality_tier) - i16::from(left.cost_tier);
                        let right_balance =
                            i16::from(right.quality_tier) - i16::from(right.cost_tier);
                        right_balance.cmp(&left_balance)
                    }
                    ModelPackStrategy::QualityFirst => right
                        .quality_tier
                        .cmp(&left.quality_tier)
                        .then_with(|| left.cost_tier.cmp(&right.cost_tier)),
                })
                .then_with(|| left.provider.cmp(&right.provider))
                .then_with(|| left.model.cmp(&right.model))
        });
        candidates.first().map(|member| (*member).clone())
    }

    #[must_use]
    pub fn fallback_chain(&self, selected: &ModelPackMember) -> Vec<String> {
        let mut values = self
            .members
            .iter()
            .filter(|member| member.provider != selected.provider || member.model != selected.model)
            .map(|member| {
                (
                    member.cost_tier,
                    std::cmp::Reverse(member.quality_tier),
                    format!("{}/{}", member.provider, member.model),
                )
            })
            .collect::<Vec<_>>();
        values.sort();
        values.into_iter().map(|(_, _, value)| value).collect()
    }
}

fn member_specialization(member: &ModelPackMember, stage: ModelPackStage, role: &str) -> u16 {
    let stage_score = u16::from(member.stages.contains(&stage)) * 100;
    let role_score = u16::from(member.roles.iter().any(|candidate| {
        candidate == "*"
            || candidate.eq_ignore_ascii_case(role)
            || role.starts_with(&candidate.to_ascii_lowercase())
    })) * 50;
    stage_score + role_score
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPackDescriptor {
    #[serde(flatten)]
    pub pack: ModelPack,
    pub available: bool,
    #[serde(default)]
    pub missing_providers: Vec<String>,
    #[serde(default)]
    pub missing_models: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModelPackFile {
    #[serde(default)]
    packs: Vec<ModelPack>,
}

#[derive(Debug, Error)]
pub enum ModelPackError {
    #[error("failed to read or write model packs at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid model pack file {path}: {source}")]
    InvalidFile {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid model pack: {0}")]
    Invalid(String),
    #[error("unknown model pack `{0}`")]
    Unknown(String),
}

#[derive(Clone, Default)]
pub struct ModelPackRegistry {
    path: Option<Arc<PathBuf>>,
    custom: Arc<RwLock<Vec<ModelPack>>>,
}

impl ModelPackRegistry {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ModelPackError> {
        let path = path.as_ref().to_path_buf();
        let packs = if path.is_file() {
            let bytes = std::fs::read(&path).map_err(|source| ModelPackError::Io {
                path: path.clone(),
                source,
            })?;
            serde_json::from_slice::<ModelPackFile>(&bytes)
                .map_err(|source| ModelPackError::InvalidFile {
                    path: path.clone(),
                    source,
                })?
                .packs
        } else {
            Vec::new()
        };
        for pack in &packs {
            validate_pack(pack)?;
        }
        Ok(Self {
            path: Some(Arc::new(path)),
            custom: Arc::new(RwLock::new(packs)),
        })
    }

    #[must_use]
    pub fn list(&self, providers: &ProviderRouter) -> Vec<ModelPackDescriptor> {
        let mut packs = recommended_packs(&providers.model_catalog());
        packs.extend(
            self.custom
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned(),
        );
        let mut unique = Vec::new();
        let mut ids = HashSet::new();
        for pack in packs.into_iter().rev() {
            if ids.insert(pack.id.clone()) {
                unique.push(pack);
            }
        }
        unique.reverse();
        let registered = providers.provider_ids().into_iter().collect::<HashSet<_>>();
        unique
            .into_iter()
            .map(|pack| {
                let mut missing_providers = pack
                    .members
                    .iter()
                    .filter(|member| !registered.contains(&member.provider))
                    .map(|member| member.provider.clone())
                    .collect::<Vec<_>>();
                missing_providers.sort();
                missing_providers.dedup();
                let mut missing_models = pack
                    .members
                    .iter()
                    .filter(|member| registered.contains(&member.provider))
                    .filter(|member| {
                        let known = providers.known_models(&member.provider);
                        !known.is_empty() && !known.contains(&member.model)
                    })
                    .map(member_identity)
                    .collect::<Vec<_>>();
                missing_models.sort();
                missing_models.dedup();
                ModelPackDescriptor {
                    available: missing_providers.is_empty() && missing_models.is_empty(),
                    missing_providers,
                    missing_models,
                    pack,
                }
            })
            .collect()
    }

    pub fn get(&self, id: &str, providers: &ProviderRouter) -> Result<ModelPack, ModelPackError> {
        self.list(providers)
            .into_iter()
            .find(|descriptor| descriptor.pack.id == id)
            .map(|descriptor| descriptor.pack)
            .ok_or_else(|| ModelPackError::Unknown(id.to_string()))
    }

    pub fn upsert(&self, mut pack: ModelPack) -> Result<ModelPack, ModelPackError> {
        pack.generated = false;
        validate_pack(&pack)?;
        {
            let mut packs = self
                .custom
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = packs.iter_mut().find(|candidate| candidate.id == pack.id) {
                *existing = pack.clone();
            } else {
                packs.push(pack.clone());
            }
            packs.sort_by(|left, right| left.id.cmp(&right.id));
        }
        self.persist()?;
        Ok(pack)
    }

    pub fn remove(&self, id: &str) -> Result<bool, ModelPackError> {
        let removed = {
            let mut packs = self
                .custom
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let before = packs.len();
            packs.retain(|pack| pack.id != id);
            packs.len() != before
        };
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    fn persist(&self) -> Result<(), ModelPackError> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ModelPackError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let document = ModelPackFile {
            packs: self
                .custom
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        };
        let bytes =
            serde_json::to_vec_pretty(&document).map_err(|source| ModelPackError::InvalidFile {
                path: path.clone(),
                source,
            })?;
        let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
        std::fs::write(&temporary, bytes).map_err(|source| ModelPackError::Io {
            path: temporary.clone(),
            source,
        })?;
        if path.exists() {
            std::fs::remove_file(path).map_err(|source| ModelPackError::Io {
                path: path.clone(),
                source,
            })?;
        }
        std::fs::rename(&temporary, path).map_err(|source| ModelPackError::Io {
            path: path.clone(),
            source,
        })
    }
}

fn validate_pack(pack: &ModelPack) -> Result<(), ModelPackError> {
    if pack.id.is_empty()
        || !pack
            .id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(ModelPackError::Invalid(format!(
            "pack id `{}` is unsafe",
            pack.id
        )));
    }
    if pack.name.trim().is_empty() || pack.description.trim().is_empty() {
        return Err(ModelPackError::Invalid(
            "pack name and description are required".to_string(),
        ));
    }
    if pack.members.len() != 3 {
        return Err(ModelPackError::Invalid(
            "a model pack must contain exactly three distinct provider/model members".to_string(),
        ));
    }
    let mut identities = BTreeSet::new();
    for member in &pack.members {
        if member.provider.trim().is_empty() || member.model.trim().is_empty() {
            return Err(ModelPackError::Invalid(
                "every pack member needs a provider and model".to_string(),
            ));
        }
        if !identities.insert((member.provider.clone(), member.model.clone())) {
            return Err(ModelPackError::Invalid(format!(
                "duplicate member `{}/{}`",
                member.provider, member.model
            )));
        }
        if member.cost_tier > 5 || member.quality_tier > 5 {
            return Err(ModelPackError::Invalid(
                "cost_tier and quality_tier must be between zero and five".to_string(),
            ));
        }
    }
    Ok(())
}

fn recommended_packs(catalog: &[(String, String)]) -> Vec<ModelPack> {
    let mut catalog = catalog.to_vec();
    catalog.sort();
    catalog.dedup();
    if catalog.len() < 3 {
        return Vec::new();
    }
    let efficient = pick_trio(
        &catalog,
        &[
            (&["deepseek"][..], &["v4-pro", "pro", "v4", "flash"][..]),
            (&["kimi", "moonshot"][..], &["k2.7", "code", "k3"][..]),
            (&["glm", "zai"][..], &["4.5", "5.2", "flash"][..]),
        ],
    );
    let quality = pick_trio(
        &catalog,
        &[
            (&["deepseek"][..], &["v4-pro", "pro", "v4"][..]),
            (&["kimi", "moonshot"][..], &["k2.7", "code", "k3"][..]),
            (&["glm", "zai"][..], &["4.5", "5.2", "pro"][..]),
        ],
    );
    let mut packs = Vec::new();
    if let Some(members) = efficient {
        packs.push(build_recommended_pack(
            "efficient-trio",
            "Efficient Trio",
            "Three specialists route planning, implementation, and verification to low-cost models.",
            ModelPackStrategy::CostOptimized,
            members,
            [3, 2, 1],
            [5, 4, 3],
        ));
    }
    if let Some(members) = quality
        && packs.first().is_none_or(|pack| {
            pack.members.iter().map(member_identity).collect::<Vec<_>>()
                != members.iter().map(pair_identity).collect::<Vec<_>>()
        })
    {
        packs.push(build_recommended_pack(
            "quality-trio",
            "Quality Trio",
            "Three stronger specialists favor architecture, implementation quality, and review depth.",
            ModelPackStrategy::QualityFirst,
            members,
            [3, 4, 3],
            [5, 5, 5],
        ));
    }
    packs
}

type CatalogPair = (String, String);

fn member_identity(member: &ModelPackMember) -> String {
    format!("{}/{}", member.provider, member.model)
}

fn pair_identity(pair: &CatalogPair) -> String {
    format!("{}/{}", pair.0, pair.1)
}

fn pick_trio(
    catalog: &[CatalogPair],
    selectors: &[(&[&str], &[&str]); 3],
) -> Option<[CatalogPair; 3]> {
    let mut selected = Vec::new();
    for (families, bonuses) in selectors {
        let candidate =
            best_catalog_match(catalog, families, bonuses, &selected).or_else(|| {
                catalog
                    .iter()
                    .find(|pair| !selected.contains(pair))
                    .cloned()
            })?;
        selected.push(candidate);
    }
    selected.try_into().ok()
}

fn best_catalog_match(
    catalog: &[CatalogPair],
    families: &[&str],
    bonuses: &[&str],
    selected: &[CatalogPair],
) -> Option<CatalogPair> {
    let mut candidates = catalog
        .iter()
        .filter(|pair| !selected.contains(pair))
        .filter_map(|pair| {
            let value = format!("{}/{}", pair.0, pair.1).to_ascii_lowercase();
            let family_score = families
                .iter()
                .enumerate()
                .filter(|(_, family)| value.contains(**family))
                .map(|(index, _)| 100_i32 - i32::try_from(index).unwrap_or(100) * 5)
                .max()?;
            let bonus_score = bonuses
                .iter()
                .enumerate()
                .filter(|(_, bonus)| value.contains(**bonus))
                .map(|(index, _)| 30_i32 - i32::try_from(index).unwrap_or(30) * 3)
                .max()
                .unwrap_or_default();
            Some((family_score + bonus_score, pair.clone()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.1.cmp(&left.1.1)));
    candidates.first().map(|(_, pair)| pair.clone())
}

fn build_recommended_pack(
    id: &str,
    name: &str,
    description: &str,
    strategy: ModelPackStrategy,
    members: [CatalogPair; 3],
    costs: [u8; 3],
    qualities: [u8; 3],
) -> ModelPack {
    let [deepseek, kimi, glm] = members;
    ModelPack {
        id: id.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        strategy,
        members: vec![
            pack_member(
                deepseek,
                &[
                    "architect",
                    "investigator",
                    "code-reviewer",
                    "security-reviewer",
                    "dependency-specialist",
                    "performance-specialist",
                    "database-specialist",
                ],
                &[
                    ModelPackStage::Plan,
                    ModelPackStage::Review,
                    ModelPackStage::Validate,
                ],
                costs[0],
                qualities[0],
            ),
            pack_member(
                kimi,
                &[
                    "implementer",
                    "backend-specialist",
                    "refactoring-specialist",
                    "integration-specialist",
                    "test-debugging-specialist",
                    "media-specialist",
                    "browser-validation-specialist",
                ],
                &[ModelPackStage::Execute, ModelPackStage::Validate],
                costs[1],
                qualities[1],
            ),
            pack_member(
                glm,
                &[
                    "generalist",
                    "frontend-specialist",
                    "documentation-specialist",
                    "accessibility-specialist",
                ],
                &[ModelPackStage::Execute, ModelPackStage::Synthesize],
                costs[2],
                qualities[2],
            ),
        ],
        generated: true,
    }
}

fn pack_member(
    (provider, model): CatalogPair,
    roles: &[&str],
    stages: &[ModelPackStage],
    cost_tier: u8,
    quality_tier: u8,
) -> ModelPackMember {
    ModelPackMember {
        provider,
        model,
        roles: roles.iter().map(|value| (*value).to_string()).collect(),
        stages: stages.to_vec(),
        cost_tier,
        quality_tier,
        reasoning_level: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ModelPack, ModelPackMember, ModelPackRegistry, ModelPackStage, ModelPackStrategy,
        recommended_packs,
    };
    use uuid::Uuid;

    #[test]
    fn recommends_a_role_specialized_trio_from_the_live_style_catalog() {
        let catalog = [
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "glm-5.2",
            "kimi-k2.7-code",
            "kimi-k3",
            "qwen3.7-max",
        ]
        .into_iter()
        .map(|model| ("openrouter".to_string(), model.to_string()))
        .collect::<Vec<_>>();
        let packs = recommended_packs(&catalog);
        let efficient = packs
            .iter()
            .find(|pack| pack.id == "efficient-trio")
            .expect("efficient pack");
        assert_eq!(efficient.members.len(), 3);
        assert_eq!(efficient.strategy, ModelPackStrategy::CostOptimized);
        assert!(
            efficient
                .select(ModelPackStage::Execute, "implementer")
                .expect("builder")
                .model
                .contains("kimi")
        );
        assert!(
            efficient
                .select(ModelPackStage::Validate, "test-debugging-specialist")
                .expect("validator")
                .model
                .contains("kimi")
        );
        assert!(
            efficient
                .select(ModelPackStage::Plan, "architect")
                .expect("architect")
                .model
                .contains("deepseek")
        );
        assert!(
            efficient
                .select(ModelPackStage::Execute, "frontend-specialist")
                .expect("frontend")
                .model
                .contains("glm")
        );
    }

    #[test]
    fn custom_three_model_pack_survives_restart() {
        let directory =
            std::env::temp_dir().join(format!("opensrc-model-packs-{}", Uuid::new_v4()));
        let path = directory.join("model-packs.json");
        let registry = ModelPackRegistry::open(&path).expect("registry");
        let pack = ModelPack {
            id: "custom-trio".to_string(),
            name: "Custom Trio".to_string(),
            description: "A persisted planner, builder, and verifier group.".to_string(),
            strategy: ModelPackStrategy::Balanced,
            members: ["planner", "builder", "verifier"]
                .into_iter()
                .enumerate()
                .map(|(index, model)| ModelPackMember {
                    provider: "provider".to_string(),
                    model: model.to_string(),
                    roles: Vec::new(),
                    stages: vec![match index {
                        0 => ModelPackStage::Plan,
                        1 => ModelPackStage::Execute,
                        _ => ModelPackStage::Validate,
                    }],
                    cost_tier: 2,
                    quality_tier: 4,
                    reasoning_level: None,
                })
                .collect(),
            generated: false,
        };
        registry.upsert(pack.clone()).expect("persist pack");

        let reopened = ModelPackRegistry::open(&path).expect("reopen registry");
        assert_eq!(
            reopened.custom.read().expect("custom packs").as_slice(),
            &[pack]
        );
        std::fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn rejects_non_trio_custom_packs() {
        let registry = ModelPackRegistry::default();
        let error = registry
            .upsert(ModelPack {
                id: "pair".to_string(),
                name: "Pair".to_string(),
                description: "Too few members.".to_string(),
                strategy: ModelPackStrategy::Balanced,
                members: ["one", "two"]
                    .into_iter()
                    .map(|model| ModelPackMember {
                        provider: "provider".to_string(),
                        model: model.to_string(),
                        roles: Vec::new(),
                        stages: Vec::new(),
                        cost_tier: 1,
                        quality_tier: 1,
                        reasoning_level: None,
                    })
                    .collect(),
                generated: false,
            })
            .expect_err("pair must be rejected");
        assert!(error.to_string().contains("exactly three"));
    }
}
