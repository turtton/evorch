use std::{io::Write, path::Path};

use crate::{ArenaError, ArenaReport, Attribution, Confirmation, Topology};

pub struct ActiveConfig<'a> {
    pub config: &'a mut config::Config,
    pub user_config_dir: &'a Path,
}

impl ArenaReport {
    /// Updates the configuration used by the next runtime composition, not an existing run.
    /// Prompt bodies use the normal user preset resolver; callers own saving/reloading Config.
    pub fn activate(
        &self,
        id: &str,
        confirmation: Confirmation,
        target: ActiveConfig<'_>,
    ) -> Result<(), ArenaError> {
        let winner = self.promote_config(id, confirmation)?;
        if winner.variant.topology != Topology::Single {
            return Err(ArenaError::InvalidSpec(
                "active runtime requires single-role promotion",
            ));
        }
        let (role, binding) = match winner.attribution {
            Attribution::Orchestrator => ("orchestrator", &mut target.config.agents.orchestrator),
            Attribution::Explorer => ("explorer", &mut target.config.agents.explorer),
            Attribution::Worker => ("worker", &mut target.config.agents.worker),
            Attribution::Reviewer => ("reviewer", &mut target.config.agents.reviewer),
            Attribution::Planner => ("planner", &mut target.config.agents.roles.planner),
            Attribution::Oracle => ("oracle", &mut target.config.agents.roles.oracle),
            Attribution::MultimodalLooker => (
                "multimodal_looker",
                &mut target.config.agents.roles.multimodal_looker,
            ),
            Attribution::Qa
            | Attribution::ToolUse
            | Attribution::Librarian
            | Attribution::Synthesizer => {
                return Err(ArenaError::InvalidSpec(
                    "evaluation role has no active runtime binding",
                ));
            }
        };
        let preset = match &winner.variant.prompt {
            None => None,
            Some(prompt) => {
                if prompt.system.len() > 65_536 {
                    return Err(ArenaError::InvalidSpec("prompt exceeds preset size limit"));
                }
                let directory = target.user_config_dir.join("presets");
                std::fs::create_dir_all(&directory)?;
                let mut file = tempfile::Builder::new()
                    .prefix("arena-")
                    .tempfile_in(&directory)?;
                file.write_all(prompt.system.as_bytes())?;
                file.as_file().sync_all()?;
                let suffix = file
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(ArenaError::InvalidSpec("preset filename"))?;
                let name = format!(
                    "arena-{}",
                    suffix
                        .bytes()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                );
                file.persist_noclobber(directory.join(format!("{name}.md")))
                    .map_err(|error| error.error)?;
                Some(name)
            }
        };
        let route = format!("arena-{role}");
        target.config.routing.routes.insert(
            route.clone(),
            vec![config::RouteCandidateConfig {
                profile: winner.profile.clone(),
                model: Some(winner.model_for(winner.attribution).to_owned()),
            }],
        );
        binding.logical_model = Some(route);
        binding.preset = preset;
        Ok(())
    }
}
