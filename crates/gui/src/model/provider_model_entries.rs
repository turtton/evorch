use super::{ModelEntryConfig, ProviderSettingsModel};

impl ProviderSettingsModel {
    pub fn is_added(&self, id: &str) -> bool {
        self.models.iter().any(|model| model.id == id.trim())
    }

    pub fn add_model(&mut self, id: &str) -> Result<(), String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("Model ID must not be empty".into());
        }
        if self.is_added(id) {
            return Err("Already added".into());
        }
        self.models.push(ModelEntryConfig::enabled(id));
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

    fn reconcile_default(&mut self) {
        let first = self.models.iter().find(|model| model.enabled);
        self.validation_error = first
            .is_none()
            .then(|| "at least one enabled model required".into());
        if !self
            .models
            .iter()
            .any(|model| model.enabled && model.id == self.default_model)
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

    pub fn apply_fetched_selection(&mut self) {
        for id in std::mem::take(&mut self.fetch_selected) {
            if !self.is_added(&id) {
                self.models.push(ModelEntryConfig::enabled(id));
            }
        }
        self.reconcile_default();
    }
}
