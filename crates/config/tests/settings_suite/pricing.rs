use config::types::provider::{
    ModelEntryConfig, ModelPricing, ModelSpeed, PriceSchedule, PriceSet, PriceTier,
};
use config::{
    Config, ConfigError, LoadOptions, OpenAiCompatibleProviderInput, ProviderCredentialInput,
    save_openai_compatible_provider,
};

fn prices(input: f64, output: f64, cache_read: f64, cache_write: f64) -> ModelPricing {
    ModelPricing {
        input: Some(input),
        output: Some(output),
        cache_read: Some(cache_read),
        cache_write: Some(cache_write),
    }
}

fn tier(above_input_tokens: u64, pricing: ModelPricing) -> PriceTier {
    PriceTier {
        above_input_tokens,
        input: pricing.input,
        output: pricing.output,
        cache_read: pricing.cache_read,
        cache_write: pricing.cache_write,
    }
}

/// The gpt-6-astra schedule published by models.dev.
fn astra() -> PriceSchedule {
    PriceSchedule {
        standard: PriceSet {
            base: prices(10.0, 50.0, 1.0, 12.5),
            tiers: vec![tier(272_000, prices(20.0, 75.0, 2.0, 25.0))],
        },
        fast: Some(PriceSet {
            base: prices(20.0, 100.0, 2.0, 25.0),
            tiers: Vec::new(),
        }),
    }
}

fn load(content: &str, strict: bool) -> Result<Config, ConfigError> {
    let directory = tempfile::tempdir().expect("temporary directory");
    std::fs::create_dir_all(directory.path().join(".evorch")).expect("config directory");
    std::fs::write(directory.path().join(".evorch/config.toml"), content).expect("write config");
    let options = LoadOptions {
        user_config_dir: Some(directory.path().join(".evorch")),
        read_env: false,
        ..Default::default()
    };
    if strict {
        Config::load_strict(&options)
    } else {
        Config::load(&options)
    }
}

const NESTED_PRICES: &str = r#"
[[providers.my.models]]
id = "my-model"
enabled = true
metadata_source = "manual"
input_price = 10.0
output_price = 50.0
cache_read_price = 1.0

[providers.my.models.fast_price]
input = 20.0
output = 100.0
cache_read = 2.0

[[providers.my.models.price_tiers]]
above_input_tokens = 272000
input = 20.0
output = 75.0
cache_read = 2.0
"#;

#[test]
fn model_entry_cost_fields_roundtrip_via_save() {
    // Given: explicit standard, fast and tiered per-million-token prices in a model definition.
    let model: ModelEntryConfig = toml::from_str(
        "id = 'y'\ninput_price = 1.25\noutput_price = 5.0\ncache_read_price = 0.125\ncache_write_price = 1.5\nfast_price = { input = 2.5, cache_write = 3.0 }\nprice_tiers = [{ above_input_tokens = 200000, input = 2.5 }, { above_input_tokens = 500000, output = 7.5 }]",
    ).expect("model parses");
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("evorch.toml");
    let input = OpenAiCompatibleProviderInput {
        name: "x".into(),
        provider_type: config::ProviderTypeConfig::OpenAiCompatible,
        base_url: "https://example.com/v1".into(),
        credential: ProviderCredentialInput::Env {
            var: "API_KEY".into(),
        },
        models: vec![model.clone()],
        excluded_models: vec![],
        default_model: "y".into(),
    };
    // When: use the public save surface and deserialize its output.
    save_openai_compatible_provider(&path, &input).expect("save");
    let loaded: Config =
        toml::from_str(&std::fs::read_to_string(path).expect("read")).expect("load");
    // Then: every price survives, including enabled models usually saved as strings.
    assert_eq!(loaded.providers["x"].models[0], model);
    assert_eq!(
        model
            .pricing_for(None)
            .map(|schedule| schedule.standard.base),
        Some(prices(1.25, 5.0, 0.125, 1.5))
    );
}

#[test]
fn nested_price_tables_load_strictly_and_tolerantly() {
    for strict in [true, false] {
        // When: the nested table form is loaded.
        let config = load(NESTED_PRICES, strict).expect("nested prices load");
        let model = &config.providers["my"].models[0];

        // Then: fast and tiered prices are read; omitted prices stay unknown.
        assert_eq!(
            model.fast_price,
            Some(ModelPricing {
                cache_write: None,
                ..prices(20.0, 100.0, 2.0, 0.0)
            })
        );
        assert_eq!(
            model.price_tiers,
            Some(vec![PriceTier {
                cache_write: None,
                ..tier(272_000, prices(20.0, 75.0, 2.0, 0.0))
            }])
        );
    }
}

#[test]
fn unknown_nested_price_keys_are_rejected_with_path_or_ignored() {
    // Given: typos inside the fast price table and a tier table.
    let document = NESTED_PRICES
        .replace("cache_read = 2.0\n\n[[", "cache_reed = 2.0\n\n[[")
        .replace("above_input_tokens", "above_input_tokens = 1\nabove_tokens");

    // When: loaded strictly, the first typo is reported with its full path.
    let error = load(&document, true).expect_err("typo is rejected");
    assert!(
        matches!(&error, ConfigError::InvalidField { path, .. }
            if path == "providers.my.models[0].fast_price.cache_reed"),
        "{error}"
    );

    // When: loaded tolerantly, the unknown keys are dropped and the rest is kept.
    let config = load(&document, false).expect("typos are ignored");
    let model = &config.providers["my"].models[0];
    assert_eq!(model.fast_price.and_then(|fast| fast.cache_read), None);
    assert_eq!(model.price_tiers.as_ref().unwrap()[0].above_input_tokens, 1);
}

#[test]
fn static_toml_overrides_catalog_pricing() {
    // Given: static overrides differ from catalog prices; zero means free, not unknown.
    let model: ModelEntryConfig = toml::from_str(
        "id = 'y'\ninput_price = 0.0\ncache_write_price = 7.0\nfast_price = { output = 90.0 }\nprice_tiers = [{ above_input_tokens = 100000, input = 3.0 }]",
    )
    .expect("model parses");
    let catalog = PriceSchedule {
        fast: Some(PriceSet {
            base: prices(20.0, 100.0, 2.0, 25.0),
            tiers: vec![tier(272_000, prices(40.0, 150.0, 4.0, 50.0))],
        }),
        ..astra()
    };
    // When
    let resolved = model.pricing_for(Some(catalog)).expect("prices are known");
    // Then: absent static prices fall back per field, and static tiers replace the catalog's.
    assert_eq!(
        resolved.standard,
        PriceSet {
            base: ModelPricing {
                input: Some(0.0),
                cache_write: Some(7.0),
                ..prices(10.0, 50.0, 1.0, 12.5)
            },
            tiers: vec![PriceTier {
                input: Some(3.0),
                ..tier(100_000, ModelPricing::default())
            }],
        }
    );
    assert_eq!(
        resolved.fast,
        Some(PriceSet {
            base: ModelPricing {
                output: Some(90.0),
                ..prices(20.0, 100.0, 2.0, 25.0)
            },
            tiers: vec![tier(272_000, prices(40.0, 150.0, 4.0, 50.0))],
        })
    );
}

#[test]
fn input_above_a_tier_threshold_prices_the_whole_request_at_that_tier() {
    // Given: a schedule with two context tiers listed out of order.
    let schedule = PriceSchedule {
        standard: PriceSet {
            base: prices(10.0, 50.0, 1.0, 12.5),
            tiers: vec![
                tier(500_000, prices(30.0, 90.0, 3.0, 0.0)),
                tier(272_000, prices(20.0, 75.0, 2.0, 25.0)),
            ],
        },
        fast: None,
    };
    let standard = |input| schedule.rates_for(input, ModelSpeed::Standard);
    // When / Then: the threshold itself is not exceeded, and the highest exceeded tier wins.
    assert_eq!(standard(272_000), prices(10.0, 50.0, 1.0, 12.5));
    assert_eq!(standard(272_001), prices(20.0, 75.0, 2.0, 25.0));
    assert_eq!(standard(500_001), prices(30.0, 90.0, 3.0, 0.0));
}

#[test]
fn fast_requests_use_fast_prices_without_multiplying_standard_prices() {
    // Given
    let schedule = astra();
    // When / Then: fast prices apply as published, even above the standard tier threshold.
    assert_eq!(
        schedule.rates_for(1_000, ModelSpeed::Fast),
        prices(20.0, 100.0, 2.0, 25.0)
    );
    assert_eq!(
        schedule.rates_for(300_000, ModelSpeed::Fast),
        prices(20.0, 100.0, 2.0, 25.0)
    );
}

#[test]
fn fast_tiers_apply_to_fast_requests() {
    // Given: fast prices with their own context tier.
    let schedule = PriceSchedule {
        fast: Some(PriceSet {
            base: prices(20.0, 100.0, 2.0, 25.0),
            tiers: vec![tier(272_000, prices(40.0, 150.0, 4.0, 50.0))],
        }),
        ..astra()
    };
    // When / Then
    assert_eq!(
        schedule.rates_for(300_000, ModelSpeed::Fast),
        prices(40.0, 150.0, 4.0, 50.0)
    );
}

#[test]
fn fast_requests_without_fast_prices_fall_back_to_standard_prices() {
    // Given: no fast prices are known, and one fast price is missing in another schedule.
    let standard_only = PriceSchedule {
        fast: None,
        ..astra()
    };
    let partial_fast = PriceSchedule {
        fast: Some(PriceSet {
            base: ModelPricing {
                cache_write: None,
                ..prices(20.0, 100.0, 2.0, 0.0)
            },
            tiers: Vec::new(),
        }),
        ..astra()
    };
    // When / Then: standard (tiered) prices stand in, so the cost is a lower bound.
    assert_eq!(
        standard_only.rates_for(300_000, ModelSpeed::Fast),
        prices(20.0, 75.0, 2.0, 25.0)
    );
    assert_eq!(
        partial_fast.rates_for(1_000, ModelSpeed::Fast),
        prices(20.0, 100.0, 2.0, 12.5)
    );
}

#[test]
fn unknown_pricing_returns_none() {
    // Given
    let model = ModelEntryConfig::enabled("unknown");
    // When
    let resolved = model.pricing_for(None);
    // Then
    assert_eq!(resolved, None);
}

#[test]
fn catalog_only_pricing_preserves_unknown_components() {
    // Given
    let model = ModelEntryConfig::enabled("y");
    let catalog = PriceSchedule {
        standard: PriceSet {
            base: ModelPricing {
                output: Some(8.0),
                ..ModelPricing::default()
            },
            tiers: Vec::new(),
        },
        fast: None,
    };
    // When
    let resolved = model.pricing_for(Some(catalog.clone()));
    // Then
    assert_eq!(resolved, Some(catalog));
}

#[test]
fn empty_catalog_pricing_returns_none() {
    // Given
    let model = ModelEntryConfig::enabled("unknown");
    // When
    let resolved = model.pricing_for(Some(PriceSchedule::default()));
    // Then
    assert_eq!(resolved, None);
}
