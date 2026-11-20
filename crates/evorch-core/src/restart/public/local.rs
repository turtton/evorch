//! Local-file allowlist and #710's three-stage target checks. No wire paths.
use super::*;
use core_launcher::handoff::{
    ProbeLimits,
    targets::{RestartTarget, TargetRequirements, TargetSelection},
};
use std::path::PathBuf;
pub struct Requirements {
    pub handoff: u32,
    pub snapshot: u32,
    pub storage_schema: u32,
    pub protocol: u32,
    pub platform: String,
}
struct Catalog {
    settings: Option<config::RestartConfig>,
    revision: Revision,
}
/// Listing reads local config only: neither ELF hash nor probe is performed.
pub struct LocalTargets {
    directory: PathBuf,
    pub(super) current: Arc<core_launcher::handoff::CurrentElf>,
    requirements: Requirements,
    pub(super) limits: ProbeLimits,
    catalog: Mutex<Catalog>,
}
impl LocalTargets {
    pub fn new(
        directory: PathBuf,
        current: Arc<core_launcher::handoff::CurrentElf>,
        requirements: Requirements,
        limits: ProbeLimits,
    ) -> Self {
        Self {
            directory,
            current,
            requirements,
            limits,
            catalog: Mutex::new(Catalog {
                settings: None,
                revision: Revision(0),
            }),
        }
    }
    async fn reload(&self) -> Result<(config::RestartConfig, Revision), AdminFailure> {
        let directory = self.directory.clone();
        let settings =
            tokio::task::spawn_blocking(move || config::Config::load_restart_settings(&directory))
                .await
                .map_err(|_| failed())?
                .map_err(|_| invalid())?;
        let mut catalog = self.catalog.lock().unwrap();
        if catalog.settings.as_ref() != Some(&settings) {
            catalog.revision = Revision(catalog.revision.get().checked_add(1).ok_or_else(failed)?);
            catalog.settings = Some(settings.clone());
        }
        Ok((settings, catalog.revision))
    }
    pub(super) async fn targets(
        &self,
        epoch: InstanceEpoch,
    ) -> Result<RestartTargets, AdminFailure> {
        let (settings, revision) = self.reload().await?;
        let targets = std::iter::once("current")
            .chain(settings.targets.keys().map(String::as_str))
            .map(|name| {
                Ok(RestartTargetView {
                    id: admin_id(name)?,
                    revision,
                })
            })
            .collect::<Result<Vec<_>, AdminFailure>>()?;
        Ok(RestartTargets {
            core_epoch: epoch,
            revision,
            targets,
            truncated: false,
        })
    }
    pub(super) async fn prepare(
        &self,
        binding: &RestartBinding,
    ) -> Result<RestartTarget, AdminFailure> {
        let (settings, revision) = self.reload().await?;
        if revision != binding.target_revision {
            return Err(conflict());
        }
        let path = settings
            .target_path(binding.target_id.as_str())
            .map_err(|_| conflict())?;
        // ELF/build/catalog/format comparisons belong exclusively to #710.
        RestartTarget::preflight(
            TargetSelection {
                id: binding.target_id.as_str(),
                configured_path: path,
                timeout: std::time::Duration::from_secs(settings.timeout_secs),
            },
            &self.current,
            TargetRequirements {
                handoff: self.requirements.handoff,
                snapshot: self.requirements.snapshot,
                storage_schema: self.requirements.storage_schema,
                protocol: self.requirements.protocol,
                platform: &self.requirements.platform,
            },
            self.limits,
            std::future::pending(),
        )
        .await
        .map_err(|_| invalid())
    }
    pub(super) fn matches(&self, binding: &RestartBinding) -> bool {
        let catalog = self.catalog.lock().unwrap();
        catalog.revision == binding.target_revision
            && catalog
                .settings
                .as_ref()
                .is_some_and(|settings| settings.target_path(binding.target_id.as_str()).is_ok())
    }
}
