//! Explicit validation scopes for every configuration write.

use std::path::Path;

use crate::{Config, ConfigError, LoadOptions};

/// A save must be validated in the layers used by its caller, not guessed from a path.
#[derive(Clone, Copy)]
pub struct LayeredSaveContext<'a> {
    options: &'a LoadOptions,
}

impl<'a> LayeredSaveContext<'a> {
    pub const fn new(options: &'a LoadOptions) -> Self {
        Self { options }
    }

    /// Refresh the shared save checks after an approval wait without rebuilding
    /// the approved text. Recompose it at its original position in today's layers.
    ///
    /// # Errors
    /// Returns structure, layer-loading, or model-kind errors before any write.
    pub fn revalidate(
        &self,
        prepared: crate::save::ValidatedSave,
    ) -> Result<crate::save::ValidatedSave, ConfigError> {
        prepared.revalidate(self.scope())
    }

    const fn scope(&self) -> SaveScope<'_> {
        SaveScope::Layered(self.options)
    }
}

/// Contract: this file is the only user layer, plus built-in defaults.
/// No home directory, adjacent drop-ins, environment or CLI values are read.
#[derive(Clone, Copy, Default)]
pub struct StandaloneSaveContext;

impl StandaloneSaveContext {
    const fn scope(&self) -> SaveScope<'_> {
        SaveScope::Standalone
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SaveScope<'a> {
    Layered(&'a LoadOptions),
    Standalone,
}

impl SaveScope<'_> {
    pub(crate) fn compose(self, path: &Path, text: &str) -> Result<Config, ConfigError> {
        match self {
            Self::Layered(options) => Config::load_unresolved_with_file_text(options, path, text),
            Self::Standalone => Config::load_standalone_text(text),
        }
    }
}

macro_rules! save_methods {
    () => {
        /// # Errors
        /// Rejects invalid input, final model kinds, or I/O failures before writing.
        pub fn save_openai_compatible_provider_edit(
            &self,
            path: &Path,
            input: &crate::OpenAiCompatibleProviderInput,
            original_name: Option<&str>,
        ) -> Result<(), ConfigError> {
            crate::save::save_openai_compatible_provider_edit_inner(
                path,
                input,
                original_name,
                self.scope(),
            )
        }

        /// # Errors
        /// Rejects invalid input, final model kinds, or I/O failures before writing.
        pub fn save_codex_provider_edit(
            &self,
            path: &Path,
            input: &crate::CodexProviderInput,
            original_name: Option<&str>,
            intent: crate::CodexModelIntent,
        ) -> Result<(), ConfigError> {
            crate::save_codex::save_codex_provider_edit_inner(
                path,
                input,
                original_name,
                self.scope(),
                intent,
            )
        }

        /// # Errors
        /// Rejects invalid input, final model kinds, or I/O failures before writing.
        pub fn save_subscription_provider_edit(
            &self,
            path: &Path,
            input: &crate::SubscriptionProviderInput,
            original_name: Option<&str>,
        ) -> Result<(), ConfigError> {
            crate::save_subscription::save_subscription_provider_edit_inner(
                path,
                input,
                original_name,
                self.scope(),
            )
        }

        /// # Errors
        /// Rejects invalid final model kinds or I/O failures before writing.
        pub fn delete_provider(&self, path: &Path, name: &str) -> Result<(), ConfigError> {
            crate::save_codex::delete_provider_in_scope(path, name, self.scope())
        }

        /// # Errors
        /// Rejects invalid input, final model kinds, or I/O failures before writing.
        pub fn save_role_profile_bindings(
            &self,
            path: &Path,
            profile: Option<&str>,
            routing: Option<&crate::RoutingConfig>,
            agents: Option<&crate::AgentsConfig>,
        ) -> Result<(), ConfigError> {
            self.save_role_profiles_bindings(
                path,
                &[crate::RoleProfileBindings {
                    profile,
                    routing,
                    agents,
                }],
            )
        }

        /// # Errors
        /// Rejects invalid input, final model kinds, or I/O failures before writing.
        pub fn save_role_profiles_bindings(
            &self,
            path: &Path,
            changes: &[crate::RoleProfileBindings<'_>],
        ) -> Result<(), ConfigError> {
            self.prepare_role_profiles_bindings(path, changes)?.write()
        }

        /// Prepare once, inspect the effective result, then write the same checked bytes.
        /// # Errors
        /// Rejects invalid input and final model kinds before opening any output file.
        pub fn prepare_role_profiles_bindings(
            &self,
            path: &Path,
            changes: &[crate::RoleProfileBindings<'_>],
        ) -> Result<crate::save::ValidatedSave, ConfigError> {
            crate::save::checked_text_for_save(
                path,
                &crate::save_role_profiles::bindings_document(path, changes)?,
                self.scope(),
            )
        }

        /// # Errors
        /// Uses exactly the same validation boundary as a write, without opening a temporary file.
        pub fn preview_role_profiles_bindings(
            &self,
            path: &Path,
            changes: &[crate::RoleProfileBindings<'_>],
        ) -> Result<String, ConfigError> {
            Ok(self
                .prepare_role_profiles_bindings(path, changes)?
                .into_text())
        }

        /// # Errors
        /// Rejects invalid names, final model kinds, or I/O failures before writing.
        pub fn save_role_profile(
            &self,
            path: &Path,
            name: &str,
            profile: &crate::RoleProfileConfig,
        ) -> Result<(), ConfigError> {
            crate::save_role_profiles::save_role_profile_in_scope(path, name, profile, self.scope())
        }

        /// # Errors
        /// Rejects invalid names, final model kinds, or I/O failures before writing.
        pub fn save_project_role_profile(
            &self,
            project_dir: &Path,
            profile: Option<&str>,
        ) -> Result<(), ConfigError> {
            crate::save_role_profiles::save_project_role_profile_in_scope(
                project_dir,
                profile,
                self.scope(),
            )
        }

        /// # Errors
        /// Rejects invalid names, final model kinds, or I/O failures before writing.
        pub fn delete_role_profile(&self, path: &Path, name: &str) -> Result<(), ConfigError> {
            crate::save_role_profiles::delete_role_profile_in_scope(path, name, self.scope())
        }

        /// # Errors
        /// Rejects invalid final model kinds or I/O failures before writing.
        pub fn save_routing(
            &self,
            path: &Path,
            routing: &crate::RoutingConfig,
        ) -> Result<(), ConfigError> {
            self.save_role_profile_bindings(path, None, Some(routing), None)
        }

        /// # Errors
        /// Rejects invalid final model kinds or I/O failures before writing.
        pub fn save_agent_bindings(
            &self,
            path: &Path,
            agents: &crate::AgentsConfig,
        ) -> Result<(), ConfigError> {
            self.save_role_profile_bindings(path, None, None, Some(agents))
        }

        /// # Errors
        /// Rejects invalid final model kinds or I/O failures before writing.
        pub fn save_routing_and_agents(
            &self,
            path: &Path,
            routing: &crate::RoutingConfig,
            agents: Option<&crate::AgentsConfig>,
        ) -> Result<(), ConfigError> {
            self.save_role_profile_bindings(path, None, Some(routing), agents)
        }

        /// # Errors
        /// Rejects invalid final model kinds or I/O failures before writing.
        pub fn save_sandbox(
            &self,
            path: &Path,
            sandbox: crate::SandboxConfig,
        ) -> Result<(), ConfigError> {
            crate::save_sandbox::save_sandbox_in_scope(path, sandbox, self.scope())
        }

        /// # Errors
        /// Rejects invalid input, final model kinds, or I/O failures before writing.
        pub fn save_self_improvement(
            &self,
            path: &Path,
            settings: &crate::SelfImprovementConfig,
        ) -> Result<(), ConfigError> {
            crate::save_self_improvement::save_self_improvement_in_scope(
                path,
                settings,
                self.scope(),
            )
        }
    };
}

impl LayeredSaveContext<'_> {
    save_methods!();
}

impl StandaloneSaveContext {
    save_methods!();
}
