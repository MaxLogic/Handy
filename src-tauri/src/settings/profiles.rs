//! Named processing profiles: the built-in Cleanup and selected-text assistant
//! records plus user-defined profiles, each optionally bound to its own hotkey.
//!
//! The legacy prompt selectors (`post_process_selected_prompt_id`,
//! `assistant_selected_prompt_id`) remain the source of truth for the built-in
//! prompt choice; the built-in records carry a synced copy for the UI.
use super::{AppSettings, ShortcutBinding};
use log::warn;
use serde::{Deserialize, Deserializer, Serialize};
use specta::Type;

/// Built-ins are not counted.
pub const MAX_CUSTOM_PROFILES: usize = 16;
pub const BUILTIN_CLEANUP_PROFILE_ID: &str = "builtin_cleanup";
pub const BUILTIN_ASSISTANT_PROFILE_ID: &str = "builtin_assistant";
pub const DICTATION_PROFILE_PREFIX: &str = "dictation_profile_";
pub const COMMAND_PROFILE_PREFIX: &str = "command_profile_";

pub(crate) const BUILTIN_CLEANUP_BINDING_ID: &str = "transcribe_with_post_process";
pub(crate) const BUILTIN_ASSISTANT_BINDING_ID: &str = "assistant";

const MAX_NAME_CHARS: usize = 60;
const MAX_MODEL_CHARS: usize = 256;
const MAX_BASE_URL_CHARS: usize = 2048;
const MAX_LANGUAGE_CHARS: usize = 16;
const MAX_CONTROL_CHARS: usize = 40;
const MAX_ID_CHARS: usize = 40;

/// Immutable after creation; custom profiles encode it in their binding id.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Dictation,
    SelectionCommand,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Type)]
pub struct LlmEndpoint {
    /// Must exist in `post_process_providers`.
    pub provider_id: String,
    /// Only for provider "custom"; None uses the provider's URL.
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
}

/// S1-mini control line values.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Type)]
#[serde(default)]
pub struct S1Controls {
    pub styling: String,
    pub structure: String,
    pub context: String,
}

impl Default for S1Controls {
    fn default() -> Self {
        Self {
            styling: "semi-formal".into(),
            structure: "prose".into(),
            context: "general".into(),
        }
    }
}

impl S1Controls {
    /// Keeps the control line well-formed; it never checks model quality.
    pub fn validate(&self) -> Result<(), String> {
        let ok = |value: &str| {
            let count = value.chars().count();
            (1..=MAX_CONTROL_CHARS).contains(&count)
                && !value
                    .chars()
                    .any(|c| c == '[' || c == ']' || c.is_control())
        };
        if ok(&self.styling) && ok(&self.structure) && ok(&self.context) {
            Ok(())
        } else {
            Err("controls_invalid".into())
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Type, Default)]
#[serde(default)]
pub struct EnglishCleanupSettings {
    pub enabled: bool,
    /// For example `http://127.0.0.1:8082/v1`; uses the Custom provider's key.
    pub base_url: String,
    /// For example `S1-mini`.
    pub model: String,
}

impl EnglishCleanupSettings {
    /// URL and model are required only when enabled; non-empty values are
    /// always validated.
    pub fn validate(&self) -> Result<(), String> {
        let url = self.base_url.trim();
        if url.is_empty() {
            if self.enabled {
                return Err("base_url_invalid".into());
            }
        } else {
            validate_base_url(url)?;
        }
        let model = self.model.trim();
        if model.is_empty() {
            if self.enabled {
                return Err("model_required".into());
            }
        } else if model.chars().count() > MAX_MODEL_CHARS {
            return Err("model_too_long".into());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Type)]
pub struct ProcessingProfile {
    /// A reserved built-in id, or a server-assigned `[a-z0-9_]{1,40}`.
    pub id: String,
    /// Trimmed; `""` only on built-ins, meaning the localized default name.
    pub name: String,
    pub kind: ProfileKind,
    /// Built-ins: a synced copy of the legacy selector. None only on built-ins.
    pub prompt_id: Option<String>,
    /// Override of the default model; None uses the default model and routing.
    #[serde(default)]
    pub model: Option<LlmEndpoint>,
    /// None uses the global `selected_language`.
    #[serde(default)]
    pub speech_language: Option<String>,
    /// Dictation only. Some means English speech may use the English model.
    #[serde(default)]
    pub english_cleanup: Option<S1Controls>,
}

/// Add and update payload; ids are never chosen by the client.
#[derive(Deserialize, Debug, Clone, Type)]
pub struct ProcessingProfileInput {
    pub name: String,
    pub kind: ProfileKind,
    pub prompt_id: Option<String>,
    pub model: Option<LlmEndpoint>,
    pub speech_language: Option<String>,
    pub english_cleanup: Option<S1Controls>,
}

/// What one binding runs, resolved on the coordinator thread at admission.
#[derive(Clone, Debug)]
pub(crate) struct ProfileSpec {
    pub profile: ProfileRef,
    pub kind: ProfileKind,
    /// Built-ins: read from the legacy selector.
    pub prompt_id: Option<String>,
    pub model: Option<LlmEndpoint>,
    pub speech_language: Option<String>,
    pub english_cleanup: Option<S1Controls>,
}

/// `name` may be `""` for an unnamed built-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProfileRef {
    pub id: String,
    pub name: String,
    pub builtin: bool,
}

pub(crate) fn is_builtin_profile_id(id: &str) -> bool {
    id == BUILTIN_CLEANUP_PROFILE_ID || id == BUILTIN_ASSISTANT_PROFILE_ID
}

fn builtin_kind(id: &str) -> Option<ProfileKind> {
    match id {
        BUILTIN_CLEANUP_PROFILE_ID => Some(ProfileKind::Dictation),
        BUILTIN_ASSISTANT_PROFILE_ID => Some(ProfileKind::SelectionCommand),
        _ => None,
    }
}

fn prefix(kind: ProfileKind) -> &'static str {
    match kind {
        ProfileKind::Dictation => DICTATION_PROFILE_PREFIX,
        ProfileKind::SelectionCommand => COMMAND_PROFILE_PREFIX,
    }
}

/// Splits a custom profile binding id into its kind and profile id.
pub(crate) fn parse_profile_binding_id(binding_id: &str) -> Option<(ProfileKind, &str)> {
    if let Some(id) = binding_id.strip_prefix(DICTATION_PROFILE_PREFIX) {
        Some((ProfileKind::Dictation, id))
    } else {
        binding_id
            .strip_prefix(COMMAND_PROFILE_PREFIX)
            .map(|id| (ProfileKind::SelectionCommand, id))
    }
}

fn valid_profile_id(id: &str) -> bool {
    (1..=MAX_ID_CHARS).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl ProcessingProfile {
    /// True for the two reserved ids.
    pub fn is_builtin(&self) -> bool {
        is_builtin_profile_id(&self.id)
    }

    /// Built-ins keep the legacy binding ids; custom profiles use prefix(kind) + id.
    pub fn binding_id(&self) -> String {
        match self.id.as_str() {
            BUILTIN_CLEANUP_PROFILE_ID => BUILTIN_CLEANUP_BINDING_ID.into(),
            BUILTIN_ASSISTANT_PROFILE_ID => BUILTIN_ASSISTANT_BINDING_ID.into(),
            id => format!("{}{id}", prefix(self.kind)),
        }
    }
}

/// The custom profile that owns a prefixed binding id, if it still exists.
pub(crate) fn custom_profile_for_binding<'a>(
    settings: &'a AppSettings,
    binding_id: &str,
) -> Option<&'a ProcessingProfile> {
    let (kind, id) = parse_profile_binding_id(binding_id)?;
    settings
        .processing_profiles
        .iter()
        .find(|p| !p.is_builtin() && p.id == id && p.kind == kind)
}

fn legacy_selector(settings: &AppSettings, profile_id: &str) -> Option<String> {
    match profile_id {
        BUILTIN_CLEANUP_PROFILE_ID => settings.post_process_selected_prompt_id.clone(),
        BUILTIN_ASSISTANT_PROFILE_ID => settings.assistant_selected_prompt_id.clone(),
        _ => None,
    }
}

/// The record a built-in has before the user edits it.
pub(crate) fn builtin_seed(settings: &AppSettings, profile_id: &str) -> Option<ProcessingProfile> {
    let kind = builtin_kind(profile_id)?;
    Some(ProcessingProfile {
        id: profile_id.into(),
        name: String::new(),
        kind,
        prompt_id: legacy_selector(settings, profile_id),
        model: None,
        speech_language: None,
        english_cleanup: (kind == ProfileKind::Dictation).then(S1Controls::default),
    })
}

/// The only Rust reader of a profile's prompt choice. Built-ins always
/// resolve (from the stored record, or the seed on un-normalized settings) and
/// read their prompt from the legacy selector.
pub(crate) fn spec_for_profile(settings: &AppSettings, profile_id: &str) -> Option<ProfileSpec> {
    if let Some(kind) = builtin_kind(profile_id) {
        let record = settings
            .processing_profiles
            .iter()
            .find(|p| p.id == profile_id)
            .cloned()
            .or_else(|| builtin_seed(settings, profile_id))?;
        return Some(ProfileSpec {
            profile: ProfileRef {
                id: profile_id.into(),
                name: record.name,
                builtin: true,
            },
            kind,
            prompt_id: legacy_selector(settings, profile_id),
            model: record.model,
            speech_language: record.speech_language,
            english_cleanup: if kind == ProfileKind::Dictation {
                record.english_cleanup
            } else {
                None
            },
        });
    }
    let record = settings
        .processing_profiles
        .iter()
        .find(|p| p.id == profile_id)?;
    Some(ProfileSpec {
        profile: ProfileRef {
            id: record.id.clone(),
            name: record.name.clone(),
            builtin: false,
        },
        kind: record.kind,
        prompt_id: record.prompt_id.clone(),
        model: record.model.clone(),
        speech_language: record.speech_language.clone(),
        english_cleanup: record.english_cleanup.clone(),
    })
}

/// What a binding runs. Raw dictation (`transcribe`) and unknown ids give None.
pub(crate) fn spec_for_binding(settings: &AppSettings, binding_id: &str) -> Option<ProfileSpec> {
    match binding_id {
        BUILTIN_CLEANUP_BINDING_ID => spec_for_profile(settings, BUILTIN_CLEANUP_PROFILE_ID),
        BUILTIN_ASSISTANT_BINDING_ID => spec_for_profile(settings, BUILTIN_ASSISTANT_PROFILE_ID),
        _ => {
            let profile = custom_profile_for_binding(settings, binding_id)?;
            spec_for_profile(settings, &profile.id)
        }
    }
}

/// Keeps every entry that parses; a non-array value still fails the field.
pub(super) fn deserialize_profiles<'de, D>(
    deserializer: D,
) -> Result<Vec<ProcessingProfile>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(profile) => Some(profile),
            Err(_) => {
                // Log no values: profiles may carry endpoint URLs.
                warn!("Dropping invalid processing profile");
                None
            }
        })
        .collect())
}

/// Idempotent load normalization; a second call returns false.
pub(crate) fn normalize_profiles(settings: &mut AppSettings) -> bool {
    let before = settings.processing_profiles.clone();
    let mut seen = std::collections::HashSet::new();
    let mut builtins: Vec<ProcessingProfile> = Vec::new();
    let mut custom: Vec<ProcessingProfile> = Vec::new();
    for profile in std::mem::take(&mut settings.processing_profiles) {
        if !valid_profile_id(&profile.id) || !seen.insert(profile.id.clone()) {
            continue;
        }
        if profile.is_builtin() {
            builtins.push(profile);
        } else {
            custom.push(profile);
        }
    }
    let mut profiles = Vec::new();
    for id in [BUILTIN_CLEANUP_PROFILE_ID, BUILTIN_ASSISTANT_PROFILE_ID] {
        let mut record = match builtins.iter().position(|p| p.id == id) {
            Some(index) => builtins.swap_remove(index),
            None => builtin_seed(settings, id).expect("reserved id has a seed"),
        };
        let kind = builtin_kind(id).expect("reserved id has a kind");
        if record.kind != kind {
            warn!("Repairing the kind of built-in profile '{id}'");
            record.kind = kind;
            if kind == ProfileKind::SelectionCommand {
                record.english_cleanup = None;
            }
        }
        record.prompt_id = legacy_selector(settings, id);
        profiles.push(record);
    }
    custom.truncate(MAX_CUSTOM_PROFILES);
    profiles.extend(custom);
    settings.processing_profiles = profiles;

    let orphans: Vec<String> = settings
        .bindings
        .keys()
        .filter(|id| {
            parse_profile_binding_id(id).is_some()
                && custom_profile_for_binding(settings, id).is_none()
        })
        .cloned()
        .collect();
    for id in &orphans {
        settings.bindings.remove(id);
    }
    settings.processing_profiles != before || !orphans.is_empty()
}

fn validate_base_url(url: &str) -> Result<(), String> {
    if url.chars().count() > MAX_BASE_URL_CHARS
        || !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err("base_url_invalid".into());
    }
    Ok(())
}

fn valid_language(language: &str) -> bool {
    if language == "auto" {
        return true;
    }
    if language.len() > MAX_LANGUAGE_CHARS {
        return false;
    }
    let mut parts = language.split('-');
    let primary = parts.next().unwrap_or_default();
    (2..=3).contains(&primary.len())
        && primary.bytes().all(|b| b.is_ascii_alphabetic())
        && parts.all(|part| {
            (2..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Validates an input against the stored settings and returns the normalized
/// record fields. `existing` is the stored profile on update.
fn validated_profile(
    settings: &AppSettings,
    input: ProcessingProfileInput,
    id: &str,
    builtin: bool,
) -> Result<ProcessingProfile, String> {
    let name = input.name.trim().to_string();
    if name.is_empty() && !builtin {
        return Err("name_required".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err("name_too_long".into());
    }
    if !name.is_empty() {
        let folded = name.to_lowercase();
        if settings
            .processing_profiles
            .iter()
            .any(|p| p.id != id && !p.name.is_empty() && p.name.to_lowercase() == folded)
        {
            return Err("name_taken".into());
        }
    }
    match &input.prompt_id {
        None if !builtin => return Err("prompt_required".into()),
        Some(prompt)
            if !settings
                .post_process_prompts
                .iter()
                .any(|p| &p.id == prompt) =>
        {
            return Err("prompt_not_found".into())
        }
        _ => {}
    }
    let model = match input.model {
        None => None,
        Some(endpoint) => {
            let provider = settings
                .post_process_provider(&endpoint.provider_id)
                .ok_or("provider_not_found")?;
            if input.kind == ProfileKind::SelectionCommand
                && provider.id == super::APPLE_INTELLIGENCE_PROVIDER_ID
            {
                return Err("provider_unsupported".into());
            }
            let base_url = non_blank(endpoint.base_url);
            if let Some(url) = &base_url {
                if provider.id != "custom" {
                    return Err("base_url_not_allowed".into());
                }
                validate_base_url(url)?;
            }
            let model = endpoint.model.trim().to_string();
            if model.is_empty() {
                return Err("model_required".into());
            }
            if model.chars().count() > MAX_MODEL_CHARS {
                return Err("model_too_long".into());
            }
            Some(LlmEndpoint {
                provider_id: endpoint.provider_id,
                base_url,
                model,
            })
        }
    };
    let speech_language = non_blank(input.speech_language);
    if speech_language
        .as_deref()
        .is_some_and(|l| !valid_language(l))
    {
        return Err("language_invalid".into());
    }
    if let Some(controls) = &input.english_cleanup {
        if input.kind == ProfileKind::SelectionCommand {
            return Err("english_cleanup_command".into());
        }
        controls.validate()?;
    }
    Ok(ProcessingProfile {
        id: id.into(),
        name,
        kind: input.kind,
        prompt_id: input.prompt_id,
        model,
        speech_language,
        english_cleanup: input.english_cleanup,
    })
}

fn custom_count(settings: &AppSettings) -> usize {
    settings
        .processing_profiles
        .iter()
        .filter(|p| !p.is_builtin())
        .count()
}

/// Creates a custom profile with a server-assigned id.
pub(crate) fn add_profile(
    settings: &mut AppSettings,
    input: ProcessingProfileInput,
    now_ms: i64,
) -> Result<ProcessingProfile, String> {
    if input.kind == ProfileKind::SelectionCommand && !cfg!(target_os = "windows") {
        return Err("kind_unsupported".into());
    }
    if custom_count(settings) >= MAX_CUSTOM_PROFILES {
        return Err("profile_limit".into());
    }
    let base = format!("p{}", now_ms.max(0));
    let mut id = base.clone();
    let mut n = 1;
    while settings.processing_profiles.iter().any(|p| p.id == id) {
        id = format!("{base}_{n}");
        n += 1;
    }
    let profile = validated_profile(settings, input, &id, false)?;
    settings.processing_profiles.push(profile.clone());
    Ok(profile)
}

/// Updates any profile. Built-ins write their legacy selector in the same
/// change. Registration is not touched.
pub(crate) fn update_profile(
    settings: &mut AppSettings,
    id: &str,
    input: ProcessingProfileInput,
) -> Result<(), String> {
    let index = settings
        .processing_profiles
        .iter()
        .position(|p| p.id == id)
        .ok_or("not_found")?;
    let stored = &settings.processing_profiles[index];
    if stored.kind != input.kind {
        return Err("kind_immutable".into());
    }
    let builtin = stored.is_builtin();
    let profile = validated_profile(settings, input, id, builtin)?;
    let binding_id = profile.binding_id();
    let binding_name = if profile.name.is_empty() {
        super::get_default_settings()
            .bindings
            .get(&binding_id)
            .map(|b| b.name.clone())
    } else {
        Some(profile.name.clone())
    };
    if let (Some(name), Some(binding)) = (binding_name, settings.bindings.get_mut(&binding_id)) {
        binding.name = name;
    }
    match id {
        BUILTIN_CLEANUP_PROFILE_ID => {
            settings.post_process_selected_prompt_id = profile.prompt_id.clone()
        }
        BUILTIN_ASSISTANT_PROFILE_ID => {
            settings.assistant_selected_prompt_id = profile.prompt_id.clone()
        }
        _ => {}
    }
    settings.processing_profiles[index] = profile;
    Ok(())
}

/// The stored binding of a custom profile. Built-ins are refused.
pub(crate) fn custom_profile_binding(
    settings: &AppSettings,
    id: &str,
) -> Result<Option<ShortcutBinding>, String> {
    let profile = settings
        .processing_profiles
        .iter()
        .find(|p| p.id == id)
        .ok_or("not_found")?;
    if profile.is_builtin() {
        return Err("builtin_profile".into());
    }
    Ok(settings.bindings.get(&profile.binding_id()).cloned())
}

/// Removes a custom profile and its binding.
pub(crate) fn delete_profile(settings: &mut AppSettings, id: &str) -> Result<(), String> {
    custom_profile_binding(settings, id)?;
    if let Some(profile) = settings.processing_profiles.iter().find(|p| p.id == id) {
        let binding_id = profile.binding_id();
        settings.bindings.remove(&binding_id);
    }
    settings.processing_profiles.retain(|p| p.id != id);
    Ok(())
}

/// Removes a custom profile's hotkey.
pub(crate) fn clear_profile_hotkey(settings: &mut AppSettings, id: &str) -> Result<(), String> {
    custom_profile_binding(settings, id)?;
    if let Some(profile) = settings.processing_profiles.iter().find(|p| p.id == id) {
        let binding_id = profile.binding_id();
        settings.bindings.remove(&binding_id);
    }
    Ok(())
}

/// True when any profile uses the prompt: built-ins through their legacy
/// selectors, custom profiles through their record.
pub(crate) fn prompt_in_use(settings: &AppSettings, prompt_id: &str) -> bool {
    settings.post_process_selected_prompt_id.as_deref() == Some(prompt_id)
        || settings.assistant_selected_prompt_id.as_deref() == Some(prompt_id)
        || settings
            .processing_profiles
            .iter()
            .any(|p| !p.is_builtin() && p.prompt_id.as_deref() == Some(prompt_id))
}

#[cfg(test)]
mod tests;
