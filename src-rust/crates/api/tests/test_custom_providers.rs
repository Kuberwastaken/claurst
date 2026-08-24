use claurst_core::Settings;
use claurst_api::ModelRegistry;

#[test]
fn custom_providers_appear_in_registry() {
    let settings = Settings::load_sync().unwrap_or_default();
    eprintln!("Custom providers: {}", settings.custom_providers.len());

    let mut registry = ModelRegistry::new();
    registry.apply_custom_providers(&settings.custom_providers);

    for id in settings.custom_providers.keys() {
        let models = registry.list_by_provider(id);
        eprintln!("Provider '{}': {} models", id, models.len());
        assert!(!models.is_empty(), "Provider '{}' has 0 models in registry!", id);
    }
}

#[test]
fn custom_provider_models_keyed_correctly() {
    let settings = Settings::load_sync().unwrap_or_default();
    if let Some(_llmg) = settings.custom_providers.get("llmg-coding") {
        let mut registry = ModelRegistry::new();
        registry.apply_custom_providers(&settings.custom_providers);

        let models = registry.list_by_provider("llmg-coding");
        eprintln!("list_by_provider('llmg-coding'): {} models", models.len());
        for m in &models {
            eprintln!("  id={}, provider_id={}", m.info.id, m.info.provider_id);
        }
    }
}
