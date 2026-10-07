//! run 境界で skill 発見結果と system prompt catalog を更新する供給元。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use event_bus::{Event, EventBus, FaultEvent, SkillDiagnosticKind};

use crate::prompt::{
    AvailableAgent, CatalogBuildInput, PromptCompositionError, SystemPromptCatalog, build_catalog,
};
use crate::skill::{
    SkillDiagnostic, SkillRegistry, SkillScope, discover_with_builtin, repo_skill_dirs,
};

const fn is_repo_scope(scope: SkillScope) -> bool {
    matches!(scope, SkillScope::Repo | SkillScope::RepoAgents)
}

fn replace_repo_dirs(dirs: &mut Vec<(SkillScope, PathBuf)>, repo_root: &Path) {
    dirs.retain(|(scope, _)| !is_repo_scope(*scope));
    dirs.splice(0..0, repo_skill_dirs(repo_root));
}

/// 1 run に渡す metadata と catalog の組。実行中の System 履歴は再構成しない。
#[derive(Clone)]
pub struct SourceSnapshot {
    /// 今回の発見結果。filesystem 本文は load 時に読む。
    pub registry: Arc<SkillRegistry>,
    /// 今回または直前に正常構築できた catalog。初期構築失敗なら None。
    pub catalog: Option<Arc<SystemPromptCatalog>>,
}

struct SourceState {
    registry: Arc<SkillRegistry>,
    catalog: Option<Arc<SystemPromptCatalog>>,
    diagnostics: Vec<SkillDiagnostic>,
}

/// skill と prompt の設定を所有し、run ごとにディスクを再走査する。
///
/// catalog 構築失敗時も registry は更新し、直前の正常な catalog を維持する。
/// filesystem skill の本文は registry の既存契約どおり load 時に読む。
pub struct SkillCatalogSource {
    config: config::Config,
    // 名前と異なり user config dir。resolver が内部で `presets` を付加する。
    user_presets_dir: Option<PathBuf>,
    available_agents: Vec<AvailableAgent>,
    skill_dirs: Mutex<Vec<(SkillScope, PathBuf)>>,
    bus: Arc<EventBus>,
    state: Mutex<SourceState>,
}

impl SkillCatalogSource {
    /// 初期 snapshot を同期構築し、発見・構築診断を発行する (fail-soft)。
    pub fn new(
        config: config::Config,
        user_presets_dir: Option<PathBuf>,
        available_agents: Vec<AvailableAgent>,
        skill_dirs: Vec<(SkillScope, PathBuf)>,
        bus: Arc<EventBus>,
    ) -> Self {
        let registry = Arc::new(discover_with_builtin(&skill_dirs));
        let catalog = build_catalog(&CatalogBuildInput {
            config: &config,
            user_presets_dir: user_presets_dir.as_deref(),
            available_agents: &available_agents,
            available_skills: &registry.available_skills(),
        });
        let (catalog, error) = match catalog {
            Ok(catalog) => (Some(Arc::new(catalog)), None),
            Err(error) => (None, Some(error)),
        };
        let diagnostics = registry.diagnostics.clone();
        let source = Self {
            config,
            user_presets_dir,
            available_agents,
            skill_dirs: Mutex::new(skill_dirs),
            bus,
            state: Mutex::new(SourceState {
                registry,
                catalog,
                diagnostics: diagnostics.clone(),
            }),
        };
        source.emit_diagnostics(diagnostics, error);
        source
    }

    /// preset 上書きの解決に使う user config dir を返す。
    pub fn user_config_dir(&self) -> Option<&std::path::Path> {
        self.user_presets_dir.as_deref()
    }

    /// リポジトリスコープの探索先を `repo_root` に差し替える。次の snapshot から反映される。
    pub fn set_repo_root(&self, repo_root: &Path) {
        replace_repo_dirs(
            &mut self
                .skill_dirs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            repo_root,
        );
    }

    /// run 開始時の snapshot を取得する。更新を直列化し、通知はロック解放後に行う。
    ///
    /// 発見診断は直前の集合 (順序を含む) と異なる場合にのみ全件発行する。
    /// catalog 構築失敗は試行ごとに通知し、正常な catalog がまだ無ければ None を返す。
    pub fn snapshot(&self) -> SourceSnapshot {
        self.snapshot_for(None, true)
    }

    /// [`Self::snapshot`] with repository skills discovered under `repo_root`
    /// instead of the configured repository, for runs bound to another project.
    pub fn snapshot_for(&self, repo_root: Option<&Path>, repo_skills: bool) -> SourceSnapshot {
        let (snapshot, diagnostics, error) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut skill_dirs = self
                .skill_dirs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if !repo_skills {
                skill_dirs.retain(|(scope, _)| !is_repo_scope(*scope));
            } else if let Some(repo_root) = repo_root {
                replace_repo_dirs(&mut skill_dirs, repo_root);
            }
            let registry = Arc::new(discover_with_builtin(&skill_dirs));
            let catalog = build_catalog(&CatalogBuildInput {
                config: &self.config,
                user_presets_dir: self.user_presets_dir.as_deref(),
                available_agents: &self.available_agents,
                available_skills: &registry.available_skills(),
            });
            let error = match catalog {
                Ok(catalog) => {
                    state.catalog = Some(Arc::new(catalog));
                    None
                }
                Err(error) => Some(error),
            };
            let diagnostics = if state.diagnostics == registry.diagnostics {
                Vec::new()
            } else {
                state.diagnostics = registry.diagnostics.clone();
                state.diagnostics.clone()
            };
            state.registry = registry;
            (
                SourceSnapshot {
                    registry: Arc::clone(&state.registry),
                    catalog: state.catalog.clone(),
                },
                diagnostics,
                error,
            )
        };
        self.emit_diagnostics(diagnostics, error);
        snapshot
    }

    // bus の購読側が snapshot を要求しても state のロックと競合させない。
    fn emit_diagnostics(
        &self,
        diagnostics: Vec<SkillDiagnostic>,
        error: Option<PromptCompositionError>,
    ) {
        for diagnostic in diagnostics {
            self.bus.emit(Event::new(FaultEvent::SkillDiagnostic {
                kind: diagnostic.kind,
                skill: diagnostic.skill,
                scope: diagnostic.scope.as_str().to_owned(),
                detail: diagnostic.detail,
            }));
        }
        if let Some(error) = error {
            // config のエラーにはパスや不正入力を含むものもあるため Display は使わない。
            let detail = match error {
                PromptCompositionError::PresetResolution(_) => "preset-resolution-failed",
                PromptCompositionError::Catalog(_) => "catalog-validation-failed",
            };
            tracing::error!(detail, "system prompt catalog rebuild failed");
            self.bus.emit(Event::new(FaultEvent::SkillDiagnostic {
                kind: SkillDiagnosticKind::DiscoveryError,
                skill: "<system-prompts>".to_owned(),
                scope: "runtime".to_owned(),
                detail: detail.to_owned(),
            }));
        }
    }
}
