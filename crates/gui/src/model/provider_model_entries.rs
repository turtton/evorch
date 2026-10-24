use super::{ModelEntryConfig, ProviderSettingsModel};

impl ProviderSettingsModel {
    pub fn is_added(&self, id: &str) -> bool {
        self.models.iter().any(|model| model.id == id.trim())
    }

    /// Adds a manually entered model, writing the kind inferred from its ID (ADR 0038).
    pub fn add_model(&mut self, id: &str) -> Result<(), String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("Model ID must not be empty".into());
        }
        if self.is_added(id) {
            return Err("Already added".into());
        }
        self.models
            .push(ModelEntryConfig::enabled_with_inferred_kind(id));
        self.reconcile_default();
        Ok(())
    }

    pub fn remove_model(&mut self, index: usize) -> Result<(), String> {
        if index >= self.models.len() {
            return Err("Model not found".into());
        }
        self.models.remove(index);
        self.reconcile_default();
        Ok(())
    }

    pub fn set_model_enabled(&mut self, index: usize, enabled: bool) -> Result<(), String> {
        let model = self.models.get_mut(index).ok_or("Model not found")?;
        model.enabled = enabled;
        self.reconcile_default();
        Ok(())
    }

    /// Sets the model's kind explicitly; an image model stops being the default.
    pub fn set_model_kind(&mut self, index: usize, kind: config::ModelKind) -> Result<(), String> {
        let model = self.models.get_mut(index).ok_or("Model not found")?;
        model.kind = Some(kind);
        self.reconcile_default();
        Ok(())
    }

    pub fn rename_model(&mut self, index: usize, id: &str) -> Result<(), String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("Model ID must not be empty".into());
        }
        if self
            .models
            .iter()
            .enumerate()
            .any(|(i, model)| i != index && model.id == id)
        {
            return Err("Already added".into());
        }
        let model = self.models.get_mut(index).ok_or("Model not found")?;
        if self.default_model == model.id {
            self.default_model = id.into();
        }
        model.id = id.into();
        self.reconcile_default();
        Ok(())
    }

    /// Keeps the default on an enabled chat model; image models never serve as the
    /// default because route candidates without a model send conversations to it.
    fn reconcile_default(&mut self) {
        let is_default_candidate = |model: &&ModelEntryConfig| {
            model.enabled && model.effective_kind() == config::ModelKind::Chat
        };
        let first = self.models.iter().find(is_default_candidate);
        self.validation_error = if !self.models.iter().any(|model| model.enabled) {
            Some("at least one enabled model required".into())
        } else if first.is_none() {
            Some("at least one enabled chat model required for the default model".into())
        } else {
            None
        };
        if !self
            .models
            .iter()
            .filter(is_default_candidate)
            .any(|model| model.id == self.default_model)
        {
            self.default_model = first.map(|model| model.id.clone()).unwrap_or_default();
        }
    }

    pub fn selection_toggle(&mut self, id: &str) {
        let id = id.trim();
        if !id.is_empty() && !self.fetch_selected.remove(id) {
            self.fetch_selected.insert(id.into());
        }
    }

    /// Adds the selected fetched models, each with its inferred kind written explicitly.
    pub fn apply_fetched_selection(&mut self) {
        for id in std::mem::take(&mut self.fetch_selected) {
            if !self.is_added(&id) {
                self.models
                    .push(ModelEntryConfig::enabled_with_inferred_kind(id));
            }
        }
        self.reconcile_default();
    }
}
