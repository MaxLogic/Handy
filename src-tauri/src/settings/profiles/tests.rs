use super::super::{
    get_default_settings, load_settings_value, remove_prompt, salvage_settings, AppSettings,
    LLMPrompt, ShortcutBinding, BUILTIN_ASSISTANT_ID, BUILTIN_CLEANUP_ID, CLEANUP_PROMPT,
    CURRENT_SETTINGS_SCHEMA_VERSION, PREVIOUS_CLEANUP_PROMPT,
};
use super::*;
use serde_json::{json, Value};

const CUSTOM_PROMPT: &str = "prompt_1700000000000";

fn input(name: &str, kind: ProfileKind, prompt: Option<&str>) -> ProcessingProfileInput {
    ProcessingProfileInput {
        name: name.into(),
        kind,
        prompt_id: prompt.map(Into::into),
        model: None,
        speech_language: None,
        english_cleanup: None,
    }
}

fn dictation(name: &str) -> ProcessingProfileInput {
    input(
        name,
        ProfileKind::Dictation,
        Some("default_improve_transcriptions"),
    )
}

fn settings() -> AppSettings {
    let mut settings = get_default_settings();
    settings.post_process_prompts.push(LLMPrompt {
        id: CUSTOM_PROMPT.into(),
        name: "Translate".into(),
        prompt: "Translate into German.".into(),
    });
    settings
}

fn binding(id: &str, accelerator: &str) -> ShortcutBinding {
    ShortcutBinding {
        id: id.into(),
        name: "Profile".into(),
        description: String::new(),
        default_binding: String::new(),
        current_binding: accelerator.into(),
    }
}

fn custom(id: &str, kind: ProfileKind) -> ProcessingProfile {
    ProcessingProfile {
        id: id.into(),
        name: format!("Profile {id}"),
        kind,
        prompt_id: Some("default_improve_transcriptions".into()),
        model: None,
        speech_language: None,
        english_cleanup: None,
    }
}

fn record<'a>(settings: &'a AppSettings, id: &str) -> &'a ProcessingProfile {
    settings
        .processing_profiles
        .iter()
        .find(|p| p.id == id)
        .unwrap()
}

fn without_new_keys(mut value: Value) -> Value {
    let map = value.as_object_mut().unwrap();
    map.remove("processing_profiles");
    map.remove("english_cleanup");
    value
}

fn builtin_cleanup_seed(prompt: Option<&str>) -> ProcessingProfile {
    ProcessingProfile {
        id: BUILTIN_CLEANUP_PROFILE_ID.into(),
        name: String::new(),
        kind: ProfileKind::Dictation,
        prompt_id: prompt.map(Into::into),
        model: None,
        speech_language: None,
        english_cleanup: Some(S1Controls::default()),
    }
}

fn builtin_assistant_seed(prompt: Option<&str>) -> ProcessingProfile {
    ProcessingProfile {
        id: BUILTIN_ASSISTANT_PROFILE_ID.into(),
        name: String::new(),
        kind: ProfileKind::SelectionCommand,
        prompt_id: prompt.map(Into::into),
        model: None,
        speech_language: None,
        english_cleanup: None,
    }
}

// ---------------------------------------------------------------------------
// Validation and custom profiles
// ---------------------------------------------------------------------------

#[test]
fn handy008_add_validation_table_covers_input_codes() {
    let long_name = "x".repeat(61);
    let cases: Vec<(ProcessingProfileInput, &str)> = vec![
        (dictation("  "), "name_required"),
        (dictation(&long_name), "name_too_long"),
        (
            input("Cleanup copy", ProfileKind::Dictation, None),
            "prompt_required",
        ),
        (
            input("P", ProfileKind::Dictation, Some("missing")),
            "prompt_not_found",
        ),
        (
            ProcessingProfileInput {
                model: Some(LlmEndpoint {
                    provider_id: "missing".into(),
                    base_url: None,
                    model: "m".into(),
                }),
                ..dictation("P")
            },
            "provider_not_found",
        ),
        (
            ProcessingProfileInput {
                model: Some(LlmEndpoint {
                    provider_id: "openai".into(),
                    base_url: Some("http://127.0.0.1:8081/v1".into()),
                    model: "m".into(),
                }),
                ..dictation("P")
            },
            "base_url_not_allowed",
        ),
        (
            ProcessingProfileInput {
                model: Some(LlmEndpoint {
                    provider_id: "custom".into(),
                    base_url: Some("ftp://127.0.0.1".into()),
                    model: "m".into(),
                }),
                ..dictation("P")
            },
            "base_url_invalid",
        ),
        (
            ProcessingProfileInput {
                model: Some(LlmEndpoint {
                    provider_id: "custom".into(),
                    base_url: Some(format!("http://{}", "a".repeat(2048))),
                    model: "m".into(),
                }),
                ..dictation("P")
            },
            "base_url_invalid",
        ),
        (
            ProcessingProfileInput {
                model: Some(LlmEndpoint {
                    provider_id: "custom".into(),
                    base_url: None,
                    model: "  ".into(),
                }),
                ..dictation("P")
            },
            "model_required",
        ),
        (
            ProcessingProfileInput {
                speech_language: Some("polish".into()),
                ..dictation("P")
            },
            "language_invalid",
        ),
        (
            ProcessingProfileInput {
                speech_language: Some("pl-".into()),
                ..dictation("P")
            },
            "language_invalid",
        ),
        (
            ProcessingProfileInput {
                english_cleanup: Some(S1Controls {
                    styling: "[x]".into(),
                    ..S1Controls::default()
                }),
                ..dictation("P")
            },
            "controls_invalid",
        ),
    ];
    for (input, code) in cases {
        let mut settings = settings();
        let before = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            add_profile(&mut settings, input, 1).unwrap_err(),
            code,
            "{code}"
        );
        assert_eq!(serde_json::to_value(&settings).unwrap(), before, "{code}");
    }
}

#[test]
fn handy008_name_taken_is_case_insensitive_and_includes_builtins() {
    let mut settings = settings();
    add_profile(&mut settings, dictation("Polish to English"), 1).unwrap();
    assert_eq!(
        add_profile(&mut settings, dictation(" polish TO english "), 2).unwrap_err(),
        "name_taken"
    );
    update_profile(
        &mut settings,
        BUILTIN_CLEANUP_PROFILE_ID,
        input(
            "My cleanup",
            ProfileKind::Dictation,
            Some(BUILTIN_CLEANUP_ID),
        ),
    )
    .unwrap();
    assert_eq!(
        add_profile(&mut settings, dictation("MY CLEANUP"), 3).unwrap_err(),
        "name_taken"
    );
    // Renaming a profile to its own name is not a conflict.
    update_profile(&mut settings, "p1", dictation("POLISH to English")).unwrap();
    assert_eq!(record(&settings, "p1").name, "POLISH to English");
}

#[test]
fn handy008_selection_command_codes_on_builtin_assistant() {
    let mut settings = settings();
    settings
        .post_process_providers
        .push(crate::llm_client::test_http::provider(
            "apple-intelligence://local",
        ));
    settings.post_process_providers.last_mut().unwrap().id =
        super::super::APPLE_INTELLIGENCE_PROVIDER_ID.into();
    let apple = ProcessingProfileInput {
        model: Some(LlmEndpoint {
            provider_id: super::super::APPLE_INTELLIGENCE_PROVIDER_ID.into(),
            base_url: None,
            model: "Apple Intelligence".into(),
        }),
        ..input("", ProfileKind::SelectionCommand, None)
    };
    assert_eq!(
        update_profile(&mut settings, BUILTIN_ASSISTANT_PROFILE_ID, apple).unwrap_err(),
        "provider_unsupported"
    );
    let english = ProcessingProfileInput {
        english_cleanup: Some(S1Controls::default()),
        ..input("", ProfileKind::SelectionCommand, None)
    };
    assert_eq!(
        update_profile(&mut settings, BUILTIN_ASSISTANT_PROFILE_ID, english).unwrap_err(),
        "english_cleanup_command"
    );
    assert_eq!(
        update_profile(&mut settings, "missing", dictation("P")).unwrap_err(),
        "not_found"
    );
}

#[cfg(not(target_os = "windows"))]
#[test]
fn handy008_selection_command_profiles_are_windows_only() {
    let mut settings = settings();
    assert_eq!(
        add_profile(
            &mut settings,
            input(
                "Fix",
                ProfileKind::SelectionCommand,
                Some(BUILTIN_ASSISTANT_ID)
            ),
            1
        )
        .unwrap_err(),
        "kind_unsupported"
    );
}

#[test]
fn handy008_add_assigns_server_ids_with_collision_suffix() {
    let mut settings = settings();
    let first = add_profile(&mut settings, dictation("A"), 1700000000000).unwrap();
    let second = add_profile(&mut settings, dictation("B"), 1700000000000).unwrap();
    let third = add_profile(&mut settings, dictation("C"), 1700000000000).unwrap();
    assert_eq!(
        [first.id.as_str(), second.id.as_str(), third.id.as_str()],
        ["p1700000000000", "p1700000000000_1", "p1700000000000_2"]
    );
    assert_eq!(first.binding_id(), "dictation_profile_p1700000000000");
    assert!(!first.is_builtin());
    // Stored trimmed, server values only.
    let trimmed = add_profile(
        &mut settings,
        ProcessingProfileInput {
            speech_language: Some(" pl ".into()),
            model: Some(LlmEndpoint {
                provider_id: "custom".into(),
                base_url: Some("  ".into()),
                model: " qwen3.5:4b ".into(),
            }),
            ..dictation("  Spaced  ")
        },
        5,
    )
    .unwrap();
    assert_eq!(trimmed.name, "Spaced");
    assert_eq!(trimmed.speech_language.as_deref(), Some("pl"));
    let model = trimmed.model.unwrap();
    assert_eq!(model.model, "qwen3.5:4b");
    assert_eq!(model.base_url, None);
}

#[test]
fn handy008_profile_limit_does_not_count_builtins() {
    let mut settings = settings();
    for n in 0..MAX_CUSTOM_PROFILES {
        add_profile(&mut settings, dictation(&format!("P{n}")), n as i64).unwrap();
    }
    assert_eq!(settings.processing_profiles.len(), MAX_CUSTOM_PROFILES + 2);
    assert_eq!(
        add_profile(&mut settings, dictation("One too many"), 99).unwrap_err(),
        "profile_limit"
    );
}

#[test]
fn handy008_kind_is_immutable_for_custom_and_builtins() {
    let mut settings = settings();
    let profile = add_profile(&mut settings, dictation("A"), 1).unwrap();
    for id in [
        profile.id.as_str(),
        BUILTIN_CLEANUP_PROFILE_ID,
        BUILTIN_ASSISTANT_PROFILE_ID,
    ] {
        let kind = match record(&settings, id).kind {
            ProfileKind::Dictation => ProfileKind::SelectionCommand,
            ProfileKind::SelectionCommand => ProfileKind::Dictation,
        };
        assert_eq!(
            update_profile(
                &mut settings,
                id,
                input("Renamed", kind, Some(CUSTOM_PROMPT))
            )
            .unwrap_err(),
            "kind_immutable",
            "{id}"
        );
    }
}

#[test]
fn handy008_custom_rename_updates_binding_name() {
    let mut settings = settings();
    let profile = add_profile(&mut settings, dictation("Old"), 1).unwrap();
    settings.bindings.insert(
        profile.binding_id(),
        binding(&profile.binding_id(), "ctrl+f9"),
    );
    update_profile(&mut settings, &profile.id, dictation("  New name ")).unwrap();
    assert_eq!(settings.bindings[&profile.binding_id()].name, "New name");
    assert_eq!(
        settings.bindings[&profile.binding_id()].current_binding,
        "ctrl+f9"
    );
}

#[test]
fn handy008_spec_for_binding_maps_builtins_prefixes_and_rejects_others() {
    let mut settings = settings();
    settings
        .processing_profiles
        .push(custom("pd", ProfileKind::Dictation));
    settings
        .processing_profiles
        .push(custom("pc", ProfileKind::SelectionCommand));
    let cleanup = spec_for_binding(&settings, "transcribe_with_post_process").unwrap();
    assert_eq!(cleanup.profile.id, BUILTIN_CLEANUP_PROFILE_ID);
    assert!(cleanup.profile.builtin);
    assert_eq!(cleanup.kind, ProfileKind::Dictation);
    let assistant = spec_for_binding(&settings, "assistant").unwrap();
    assert_eq!(assistant.profile.id, BUILTIN_ASSISTANT_PROFILE_ID);
    assert_eq!(assistant.kind, ProfileKind::SelectionCommand);
    let dictation = spec_for_binding(&settings, "dictation_profile_pd").unwrap();
    assert_eq!(
        dictation.profile,
        ProfileRef {
            id: "pd".into(),
            name: "Profile pd".into(),
            builtin: false
        }
    );
    assert_eq!(
        spec_for_binding(&settings, "command_profile_pc")
            .unwrap()
            .kind,
        ProfileKind::SelectionCommand
    );
    for missing in [
        "dictation_profile_pc",
        "command_profile_pd",
        "dictation_profile_missing",
        "dictation_profile_builtin_cleanup",
        "transcribe",
        "cancel",
        "unknown",
    ] {
        assert!(spec_for_binding(&settings, missing).is_none(), "{missing}");
    }
}

#[test]
fn handy008_tolerant_deserializer_drops_one_bad_entry_without_salvage() {
    let mut stored = serde_json::to_value(settings()).unwrap();
    stored["processing_profiles"] = json!([
        {"id":"pa","name":"A","kind":"dictation","prompt_id":"default_improve_transcriptions"},
        {"id":"pb","name":"B","kind":"teleport","prompt_id":null},
        {"id":"pc","name":"C","kind":"dictation","prompt_id":"default_improve_transcriptions","speech_language":"pl"}
    ]);
    stored["selected_model"] = json!("kept-model");
    let parsed: AppSettings =
        serde_json::from_value(stored.clone()).expect("one bad profile must not fail the load");
    assert_eq!(
        parsed
            .processing_profiles
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        ["pa", "pc"]
    );
    let (loaded, changed) = load_settings_value(&stored);
    assert!(changed);
    assert_eq!(loaded.selected_model, "kept-model");
    assert_eq!(
        loaded
            .processing_profiles
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        [
            BUILTIN_CLEANUP_PROFILE_ID,
            BUILTIN_ASSISTANT_PROFILE_ID,
            "pa",
            "pc"
        ]
    );
    // A non-array value fails the field; salvage then drops only that field.
    stored["processing_profiles"] = json!("not a list");
    assert!(serde_json::from_value::<AppSettings>(stored.clone()).is_err());
    let (salvaged, _) = load_settings_value(&stored);
    assert_eq!(salvaged.selected_model, "kept-model");
    assert_eq!(salvaged.processing_profiles.len(), 2);
}

#[test]
fn handy008_normalize_prunes_orphan_and_mismatched_profile_bindings() {
    let mut settings = settings();
    settings
        .processing_profiles
        .push(custom("pd", ProfileKind::Dictation));
    for (id, accelerator) in [
        ("dictation_profile_pd", "ctrl+f9"),
        ("command_profile_pd", "ctrl+f10"),
        ("dictation_profile_gone", "ctrl+f11"),
        ("dictation_profile_builtin_cleanup", "ctrl+f12"),
    ] {
        settings
            .bindings
            .insert(id.into(), binding(id, accelerator));
    }
    assert!(normalize_profiles(&mut settings));
    let mut ids: Vec<_> = settings.bindings.keys().cloned().collect();
    ids.sort();
    let mut expected: Vec<String> = get_default_settings().bindings.into_keys().collect();
    expected.push("dictation_profile_pd".into());
    expected.sort();
    assert_eq!(ids, expected, "default bindings are never pruned");
    assert!(!normalize_profiles(&mut settings));
}

#[test]
fn handy008_normalize_dedupes_ids_limits_custom_and_is_idempotent() {
    let mut settings = settings();
    settings.processing_profiles.clear();
    settings
        .processing_profiles
        .push(custom("dup", ProfileKind::Dictation));
    let mut second = custom("dup", ProfileKind::Dictation);
    second.name = "Second".into();
    settings.processing_profiles.push(second);
    settings
        .processing_profiles
        .push(custom("Bad-Id", ProfileKind::Dictation));
    settings
        .processing_profiles
        .push(custom(&"x".repeat(41), ProfileKind::Dictation));
    for n in 0..20 {
        settings
            .processing_profiles
            .push(custom(&format!("p{n}"), ProfileKind::Dictation));
    }
    assert!(normalize_profiles(&mut settings));
    let ids: Vec<_> = settings
        .processing_profiles
        .iter()
        .map(|p| p.id.clone())
        .collect();
    assert_eq!(ids.len(), 2 + MAX_CUSTOM_PROFILES);
    assert_eq!(
        ids[..3],
        [
            BUILTIN_CLEANUP_PROFILE_ID,
            BUILTIN_ASSISTANT_PROFILE_ID,
            "dup"
        ]
    );
    assert_eq!(record(&settings, "dup").name, "Profile dup", "first wins");
    assert_eq!(ids.last().unwrap(), "p14");
    let normalized = serde_json::to_value(&settings).unwrap();
    assert!(!normalize_profiles(&mut settings));
    assert_eq!(serde_json::to_value(&settings).unwrap(), normalized);
}

#[test]
fn handy008_english_settings_validate_cases() {
    let valid = EnglishCleanupSettings {
        enabled: true,
        base_url: "http://127.0.0.1:8082/v1".into(),
        model: "S1-mini".into(),
    };
    assert_eq!(valid.validate(), Ok(()));
    assert_eq!(EnglishCleanupSettings::default().validate(), Ok(()));
    for (value, code) in [
        (
            EnglishCleanupSettings {
                base_url: String::new(),
                ..valid.clone()
            },
            "base_url_invalid",
        ),
        (
            EnglishCleanupSettings {
                model: " ".into(),
                ..valid.clone()
            },
            "model_required",
        ),
        (
            EnglishCleanupSettings {
                enabled: false,
                base_url: "localhost:8082".into(),
                model: String::new(),
            },
            "base_url_invalid",
        ),
        (
            EnglishCleanupSettings {
                base_url: format!("https://{}", "a".repeat(2048)),
                ..valid.clone()
            },
            "base_url_invalid",
        ),
    ] {
        assert_eq!(value.validate().unwrap_err(), code, "{value:?}");
    }
    // Disabled with blank values is a valid "off" state.
    assert_eq!(
        EnglishCleanupSettings {
            enabled: false,
            ..EnglishCleanupSettings::default()
        }
        .validate(),
        Ok(())
    );
}

#[test]
fn handy008_s1_controls_and_language_validation() {
    assert_eq!(S1Controls::default().validate(), Ok(()));
    for bad in ["", "[x", "x]", "a\nb", &"y".repeat(41)] {
        let controls = S1Controls {
            context: bad.to_string(),
            ..S1Controls::default()
        };
        assert_eq!(
            controls.validate().unwrap_err(),
            "controls_invalid",
            "{bad:?}"
        );
    }
    for good in ["auto", "pl", "en", "yue", "pt-BR", "zh-Hant", "sr-Latn-RS"] {
        assert!(valid_language(good), "{good}");
    }
    for bad in [
        "p",
        "polish",
        "pl_PL",
        "pl-",
        "-pl",
        "pl-x",
        "en-abcdefghi",
        "a1",
    ] {
        assert!(!valid_language(bad), "{bad}");
    }
    assert!(!valid_language(&format!("en-{}", "abcdefgh-".repeat(2))));
}

#[test]
fn handy008_english_opt_in_with_model_override_is_accepted() {
    let mut settings = settings();
    let with_model = ProcessingProfileInput {
        model: Some(LlmEndpoint {
            provider_id: "custom".into(),
            base_url: Some("http://127.0.0.1:8081/v1".into()),
            model: "qwen3.5:4b".into(),
        }),
        english_cleanup: Some(S1Controls::default()),
        ..dictation("Both")
    };
    let profile = add_profile(&mut settings, with_model, 1).unwrap();
    assert!(profile.english_cleanup.is_some() && profile.model.is_some());
    // Seeded built-in Cleanup keeps its opt-in when it gets its first override.
    let builtin = ProcessingProfileInput {
        model: Some(LlmEndpoint {
            provider_id: "custom".into(),
            base_url: None,
            model: "qwen3.5:9b".into(),
        }),
        english_cleanup: Some(S1Controls::default()),
        ..input("", ProfileKind::Dictation, Some(BUILTIN_CLEANUP_ID))
    };
    update_profile(&mut settings, BUILTIN_CLEANUP_PROFILE_ID, builtin).unwrap();
}

// ---------------------------------------------------------------------------
// Migration and rollback (all through load_settings_value)
// ---------------------------------------------------------------------------

fn schema4_fixture() -> Value {
    let mut defaults = get_default_settings();
    for prompt in &mut defaults.post_process_prompts {
        prompt.name = format!("Renamed {}", prompt.name);
    }
    defaults.post_process_prompts.push(LLMPrompt {
        id: CUSTOM_PROMPT.into(),
        name: "Mine".into(),
        prompt: "My cleanup".into(),
    });
    defaults.post_process_selected_prompt_id = Some(CUSTOM_PROMPT.into());
    defaults.assistant_selected_prompt_id = None;
    defaults
        .bindings
        .get_mut("transcribe_with_post_process")
        .unwrap()
        .current_binding = "ctrl+alt+f9".into();
    defaults
        .bindings
        .get_mut("transcribe")
        .unwrap()
        .current_binding = "enter".into();
    defaults.post_process_provider_id = "custom".into();
    defaults
        .post_process_models
        .insert("custom".into(), "S1-mini".into());
    defaults.post_process_enabled = false;
    let mut value = without_new_keys(serde_json::to_value(defaults).unwrap());
    value["settings_schema_version"] = json!(4);
    value
}

fn assert_only_new_keys_and_version_changed(before: &Value, after: &Value) {
    let mut before = without_new_keys(before.clone());
    let mut after = without_new_keys(after.clone());
    before
        .as_object_mut()
        .unwrap()
        .remove("settings_schema_version");
    after
        .as_object_mut()
        .unwrap()
        .remove("settings_schema_version");
    assert_eq!(after, before);
}

#[test]
fn handy008_schema4_store_migrates_to_builtin_profiles() {
    let stored = schema4_fixture();
    let (settings, changed) = load_settings_value(&stored);
    assert!(changed);
    assert_eq!(settings.settings_schema_version, 5);
    assert_eq!(CURRENT_SETTINGS_SCHEMA_VERSION, 5);
    assert_eq!(
        settings.processing_profiles,
        vec![
            builtin_cleanup_seed(Some(CUSTOM_PROMPT)),
            builtin_assistant_seed(None)
        ]
    );
    assert_eq!(settings.english_cleanup, EnglishCleanupSettings::default());
    let migrated = serde_json::to_value(&settings).unwrap();
    assert_only_new_keys_and_version_changed(&stored, &migrated);
    assert_eq!(
        migrated["bindings"]["transcribe_with_post_process"]["current_binding"],
        "ctrl+alt+f9"
    );
    assert_eq!(
        migrated["bindings"]["transcribe"]["current_binding"],
        "enter"
    );
    let (again, changed) = load_settings_value(&migrated);
    assert!(!changed, "a second load reports no change");
    assert_eq!(serde_json::to_value(again).unwrap(), migrated);
}

#[test]
fn handy008_schema3_store_migrates_to_builtin_profiles() {
    let mut defaults = get_default_settings();
    defaults
        .post_process_prompts
        .iter_mut()
        .find(|p| p.id == BUILTIN_CLEANUP_ID)
        .unwrap()
        .prompt = PREVIOUS_CLEANUP_PROMPT.into();
    let mut stored = without_new_keys(serde_json::to_value(defaults).unwrap());
    stored["settings_schema_version"] = json!(3);
    let (settings, changed) = load_settings_value(&stored);
    assert!(changed);
    assert_eq!(settings.settings_schema_version, 5);
    assert_eq!(
        settings
            .post_process_prompts
            .iter()
            .find(|p| p.id == BUILTIN_CLEANUP_ID)
            .unwrap()
            .prompt,
        CLEANUP_PROMPT,
        "the schema-4 rule still upgrades untouched text"
    );
    assert_eq!(
        settings.processing_profiles,
        vec![
            builtin_cleanup_seed(Some(BUILTIN_CLEANUP_ID)),
            builtin_assistant_seed(Some(BUILTIN_ASSISTANT_ID))
        ]
    );
    let migrated = serde_json::to_value(&settings).unwrap();
    assert!(!load_settings_value(&migrated).1);
}

#[test]
fn handy008_rollback_round_trip_reseeds_builtins_from_legacy() {
    let mut settings = settings();
    let profile = add_profile(&mut settings, dictation("Polish to English"), 1).unwrap();
    settings.bindings.insert(
        profile.binding_id(),
        binding(&profile.binding_id(), "ctrl+shift+f9"),
    );
    update_profile(
        &mut settings,
        BUILTIN_CLEANUP_PROFILE_ID,
        ProcessingProfileInput {
            model: Some(LlmEndpoint {
                provider_id: "custom".into(),
                base_url: None,
                model: "qwen".into(),
            }),
            speech_language: Some("pl".into()),
            ..input("My cleanup", ProfileKind::Dictation, Some(CUSTOM_PROMPT))
        },
    )
    .unwrap();
    settings.english_cleanup.enabled = false;
    settings.english_cleanup.model = "S1-mini".into();
    let mut stored = serde_json::to_value(&settings).unwrap();
    assert_eq!(stored["settings_schema_version"], 5);
    // An older build writes its own struct: the new keys disappear, and the
    // user changed the Cleanup prompt there.
    stored = without_new_keys(stored);
    stored["post_process_selected_prompt_id"] = json!("default_improve_transcriptions");
    let (loaded, changed) = load_settings_value(&stored);
    assert!(changed);
    assert_eq!(loaded.settings_schema_version, 5);
    assert_eq!(
        loaded.processing_profiles,
        vec![
            builtin_cleanup_seed(Some("default_improve_transcriptions")),
            builtin_assistant_seed(Some(BUILTIN_ASSISTANT_ID))
        ]
    );
    assert!(!loaded.bindings.contains_key(&profile.binding_id()));
    assert_eq!(loaded.english_cleanup, EnglishCleanupSettings::default());
    let (_, again) = load_settings_value(&serde_json::to_value(&loaded).unwrap());
    assert!(!again);
}

#[test]
fn handy008_schema5_store_without_new_keys_parses_strictly() {
    let (normalized, _) = load_settings_value(&serde_json::to_value(settings()).unwrap());
    let older_view = without_new_keys(serde_json::to_value(&normalized).unwrap());
    let parsed: AppSettings =
        serde_json::from_value(older_view).expect("older builds parse a schema-5 store strictly");
    assert_eq!(
        parsed.post_process_selected_prompt_id.as_deref(),
        Some(BUILTIN_CLEANUP_ID)
    );
    assert_eq!(
        parsed.assistant_selected_prompt_id.as_deref(),
        Some(BUILTIN_ASSISTANT_ID)
    );
    assert!(parsed.processing_profiles.is_empty());
}

#[test]
fn handy008_missing_profiles_key_loads_empty_then_seeds_on_every_load() {
    let parsed: AppSettings = serde_json::from_value(json!({})).unwrap();
    assert!(
        parsed.processing_profiles.is_empty(),
        "the field default must not reuse get_default_settings()"
    );
    // A schema-5 store that lost its records is re-seeded (not gated on < 5).
    let mut stored = without_new_keys(serde_json::to_value(settings()).unwrap());
    stored["settings_schema_version"] = json!(5);
    let (loaded, changed) = load_settings_value(&stored);
    assert!(changed);
    assert_eq!(loaded.processing_profiles.len(), 2);
}

#[test]
fn handy008_legacy_selector_wins_over_record() {
    let mut settings = settings();
    settings.processing_profiles[0].prompt_id = Some(CUSTOM_PROMPT.into());
    settings.processing_profiles[1].prompt_id = Some(CUSTOM_PROMPT.into());
    settings.post_process_selected_prompt_id = Some("default_improve_transcriptions".into());
    settings.assistant_selected_prompt_id = None;
    // The spec never reads a stale record, even before normalization.
    assert_eq!(
        spec_for_profile(&settings, BUILTIN_CLEANUP_PROFILE_ID)
            .unwrap()
            .prompt_id
            .as_deref(),
        Some("default_improve_transcriptions")
    );
    assert_eq!(
        spec_for_profile(&settings, BUILTIN_ASSISTANT_PROFILE_ID)
            .unwrap()
            .prompt_id,
        None
    );
    let (loaded, changed) = load_settings_value(&serde_json::to_value(&settings).unwrap());
    assert!(changed);
    assert_eq!(
        loaded.processing_profiles[0].prompt_id.as_deref(),
        Some("default_improve_transcriptions")
    );
    assert_eq!(loaded.processing_profiles[1].prompt_id, None);
}

#[test]
fn handy008_fresh_defaults_contain_builtin_profiles() {
    let settings = get_default_settings();
    assert_eq!(
        settings.processing_profiles,
        vec![
            builtin_cleanup_seed(Some(BUILTIN_CLEANUP_ID)),
            builtin_assistant_seed(Some(BUILTIN_ASSISTANT_ID))
        ]
    );
    assert!(!settings.english_cleanup.enabled);
    let mut copy = settings.clone();
    assert!(!normalize_profiles(&mut copy), "defaults are normalized");
}

#[test]
fn handy008_salvaged_store_seeds_builtins_from_salvaged_selectors() {
    let stored = json!({
        "sound_theme": "theremin",
        "post_process_prompts": [{"id": CUSTOM_PROMPT, "name": "Mine", "prompt": "Text"}],
        "post_process_selected_prompt_id": CUSTOM_PROMPT
    });
    assert!(serde_json::from_value::<AppSettings>(stored.clone()).is_err());
    let salvaged = salvage_settings(&stored);
    assert_eq!(
        salvaged.post_process_selected_prompt_id.as_deref(),
        Some(CUSTOM_PROMPT)
    );
    let (loaded, changed) = load_settings_value(&stored);
    assert!(changed);
    assert_eq!(
        loaded.processing_profiles[0].prompt_id.as_deref(),
        Some(CUSTOM_PROMPT)
    );
    assert_eq!(loaded.processing_profiles[1].prompt_id, None);
}

#[test]
fn handy008_builtin_record_repairs() {
    let mut settings = settings();
    let mut wrong_kind = builtin_cleanup_seed(Some(BUILTIN_CLEANUP_ID));
    wrong_kind.kind = ProfileKind::SelectionCommand;
    wrong_kind.name = "Kept name".into();
    let mut wrong_assistant = builtin_assistant_seed(None);
    wrong_assistant.kind = ProfileKind::Dictation;
    wrong_assistant.english_cleanup = Some(S1Controls::default());
    settings.processing_profiles = vec![custom("first", ProfileKind::Dictation), wrong_kind];
    settings
        .processing_profiles
        .push(builtin_cleanup_seed(Some("duplicate")));
    settings.processing_profiles.push(wrong_assistant);
    for n in 0..MAX_CUSTOM_PROFILES {
        settings
            .processing_profiles
            .push(custom(&format!("c{n}"), ProfileKind::Dictation));
    }
    assert!(normalize_profiles(&mut settings));
    let cleanup = &settings.processing_profiles[0];
    assert_eq!(cleanup.id, BUILTIN_CLEANUP_PROFILE_ID);
    assert_eq!(cleanup.kind, ProfileKind::Dictation);
    assert_eq!(cleanup.name, "Kept name", "first duplicate wins");
    let assistant = &settings.processing_profiles[1];
    assert_eq!(assistant.kind, ProfileKind::SelectionCommand);
    assert_eq!(assistant.english_cleanup, None);
    assert_eq!(settings.processing_profiles[2].id, "first");
    assert_eq!(settings.processing_profiles.len(), 2 + MAX_CUSTOM_PROFILES);
    assert!(!settings.processing_profiles.iter().any(|p| p.id == "c15"));
    assert!(!normalize_profiles(&mut settings));
}

#[test]
fn handy008_spec_for_builtin_without_record_uses_seed() {
    let settings: AppSettings = serde_json::from_value(json!({})).unwrap();
    assert!(settings.processing_profiles.is_empty());
    let cleanup = spec_for_profile(&settings, BUILTIN_CLEANUP_PROFILE_ID).unwrap();
    assert_eq!(cleanup.kind, ProfileKind::Dictation);
    assert_eq!(cleanup.english_cleanup, Some(S1Controls::default()));
    assert_eq!(cleanup.prompt_id, settings.post_process_selected_prompt_id);
    assert_eq!(cleanup.profile.name, "");
    let assistant = spec_for_binding(&settings, "assistant").unwrap();
    assert_eq!(assistant.kind, ProfileKind::SelectionCommand);
    assert_eq!(assistant.prompt_id, settings.assistant_selected_prompt_id);
}

#[test]
fn handy008_spec_reads_legacy_selector_not_profile_id() {
    // The profile id `builtin_assistant` and the prompt id `builtin_assistant`
    // live in two namespaces and must never be compared.
    assert_eq!(BUILTIN_ASSISTANT_PROFILE_ID, BUILTIN_ASSISTANT_ID);
    let mut settings = settings();
    settings.assistant_selected_prompt_id = Some(CUSTOM_PROMPT.into());
    let spec = spec_for_profile(&settings, BUILTIN_ASSISTANT_PROFILE_ID).unwrap();
    assert_eq!(spec.prompt_id.as_deref(), Some(CUSTOM_PROMPT));
    assert_eq!(spec.profile.id, BUILTIN_ASSISTANT_PROFILE_ID);
    settings.assistant_selected_prompt_id = None;
    assert_eq!(
        spec_for_profile(&settings, BUILTIN_ASSISTANT_PROFILE_ID)
            .unwrap()
            .prompt_id,
        None
    );
}

// ---------------------------------------------------------------------------
// Built-in command rules
// ---------------------------------------------------------------------------

#[test]
fn handy008_builtin_delete_and_clear_hotkey_are_refused() {
    let mut settings = settings();
    let before = serde_json::to_value(&settings).unwrap();
    for id in [BUILTIN_CLEANUP_PROFILE_ID, BUILTIN_ASSISTANT_PROFILE_ID] {
        assert_eq!(
            delete_profile(&mut settings, id).unwrap_err(),
            "builtin_profile"
        );
        assert_eq!(
            clear_profile_hotkey(&mut settings, id).unwrap_err(),
            "builtin_profile"
        );
    }
    assert_eq!(serde_json::to_value(&settings).unwrap(), before);
    assert_eq!(
        delete_profile(&mut settings, "missing").unwrap_err(),
        "not_found"
    );
}

#[test]
fn handy008_builtin_update_writes_legacy_selector_with_record() {
    let mut settings = settings();
    update_profile(
        &mut settings,
        BUILTIN_CLEANUP_PROFILE_ID,
        input("", ProfileKind::Dictation, Some(CUSTOM_PROMPT)),
    )
    .unwrap();
    assert_eq!(
        settings.post_process_selected_prompt_id.as_deref(),
        Some(CUSTOM_PROMPT)
    );
    assert_eq!(
        record(&settings, BUILTIN_CLEANUP_PROFILE_ID)
            .prompt_id
            .as_deref(),
        Some(CUSTOM_PROMPT)
    );
    update_profile(
        &mut settings,
        BUILTIN_ASSISTANT_PROFILE_ID,
        input("", ProfileKind::SelectionCommand, None),
    )
    .unwrap();
    assert_eq!(settings.assistant_selected_prompt_id, None);
    assert_eq!(
        record(&settings, BUILTIN_ASSISTANT_PROFILE_ID).prompt_id,
        None
    );
    assert!(
        !normalize_profiles(&mut settings),
        "record and selector agree"
    );
}

#[test]
fn handy008_empty_name_and_no_prompt_only_on_builtins() {
    let mut settings = settings();
    update_profile(
        &mut settings,
        BUILTIN_CLEANUP_PROFILE_ID,
        input("  ", ProfileKind::Dictation, None),
    )
    .unwrap();
    assert_eq!(record(&settings, BUILTIN_CLEANUP_PROFILE_ID).name, "");
    assert_eq!(settings.post_process_selected_prompt_id, None);
    let profile = add_profile(&mut settings, dictation("Custom"), 1).unwrap();
    assert_eq!(
        update_profile(&mut settings, &profile.id, dictation("")).unwrap_err(),
        "name_required"
    );
    assert_eq!(
        update_profile(
            &mut settings,
            &profile.id,
            input("Custom", ProfileKind::Dictation, None)
        )
        .unwrap_err(),
        "prompt_required"
    );
}

#[test]
fn handy008_builtin_rename_syncs_binding_name_and_blank_restores_default() {
    let mut settings = settings();
    let default_name = settings.bindings["transcribe_with_post_process"]
        .name
        .clone();
    update_profile(
        &mut settings,
        BUILTIN_CLEANUP_PROFILE_ID,
        input(
            "My cleanup",
            ProfileKind::Dictation,
            Some(BUILTIN_CLEANUP_ID),
        ),
    )
    .unwrap();
    assert_eq!(
        settings.bindings["transcribe_with_post_process"].name,
        "My cleanup"
    );
    update_profile(
        &mut settings,
        BUILTIN_CLEANUP_PROFILE_ID,
        input("", ProfileKind::Dictation, Some(BUILTIN_CLEANUP_ID)),
    )
    .unwrap();
    assert_eq!(
        settings.bindings["transcribe_with_post_process"].name,
        default_name
    );
}

#[test]
fn handy008_delete_custom_profile_removes_profile_and_binding() {
    let mut settings = settings();
    let profile = add_profile(&mut settings, dictation("Gone"), 1).unwrap();
    settings.bindings.insert(
        profile.binding_id(),
        binding(&profile.binding_id(), "ctrl+f9"),
    );
    assert!(custom_profile_binding(&settings, &profile.id)
        .unwrap()
        .is_some());
    delete_profile(&mut settings, &profile.id).unwrap();
    assert!(!settings
        .processing_profiles
        .iter()
        .any(|p| p.id == profile.id));
    assert!(!settings.bindings.contains_key(&profile.binding_id()));
}

#[test]
fn handy008_clear_hotkey_removes_only_the_binding() {
    let mut settings = settings();
    let profile = add_profile(&mut settings, dictation("Keep"), 1).unwrap();
    settings.bindings.insert(
        profile.binding_id(),
        binding(&profile.binding_id(), "ctrl+f9"),
    );
    clear_profile_hotkey(&mut settings, &profile.id).unwrap();
    assert!(settings
        .processing_profiles
        .iter()
        .any(|p| p.id == profile.id));
    assert!(!settings.bindings.contains_key(&profile.binding_id()));
    assert!(custom_profile_binding(&settings, &profile.id)
        .unwrap()
        .is_none());
}

// ---------------------------------------------------------------------------
// remove_prompt
// ---------------------------------------------------------------------------

#[test]
fn handy008_remove_prompt_refuses_in_use_prompts() {
    let mut settings = settings();
    // Through the built-in Cleanup selector.
    settings.post_process_selected_prompt_id = Some(CUSTOM_PROMPT.into());
    assert_eq!(
        remove_prompt(&mut settings, CUSTOM_PROMPT).unwrap_err(),
        "prompt_in_use"
    );
    // Through the built-in assistant selector.
    settings.post_process_selected_prompt_id = Some(BUILTIN_CLEANUP_ID.into());
    settings.assistant_selected_prompt_id = Some(CUSTOM_PROMPT.into());
    assert_eq!(
        remove_prompt(&mut settings, CUSTOM_PROMPT).unwrap_err(),
        "prompt_in_use"
    );
    // Through a custom profile.
    settings.assistant_selected_prompt_id = None;
    add_profile(
        &mut settings,
        input("Translate", ProfileKind::Dictation, Some(CUSTOM_PROMPT)),
        1,
    )
    .unwrap();
    let before = serde_json::to_value(&settings).unwrap();
    assert_eq!(
        remove_prompt(&mut settings, CUSTOM_PROMPT).unwrap_err(),
        "prompt_in_use"
    );
    assert_eq!(serde_json::to_value(&settings).unwrap(), before);
    // Ordering: last prompt, then not found, then in use.
    assert_eq!(
        remove_prompt(&mut settings, "missing").unwrap_err(),
        "Prompt not found"
    );
    delete_profile(&mut settings, "p1").unwrap();
    remove_prompt(&mut settings, CUSTOM_PROMPT).unwrap();
    // A stale record copy on a built-in is not a user of the prompt.
    let mut stale = self::settings();
    stale.processing_profiles[1].prompt_id = Some(CUSTOM_PROMPT.into());
    remove_prompt(&mut stale, CUSTOM_PROMPT).unwrap();
}
