import type { ProcessingProfile } from "@/bindings";
import type { TFunction } from "i18next";

// Persisted backend contracts, not labels. Specta does not export constants.
export const BUILTIN_PROFILE_IDS = {
  cleanup: "builtin_cleanup",
  assistant: "builtin_assistant",
} as const;
export const BUILTIN_PROFILE_BINDINGS = {
  builtin_cleanup: "transcribe_with_post_process",
  builtin_assistant: "assistant",
} as const;
export const MAX_CUSTOM_PROFILES = 16;

export function isBuiltinProfile(profile: ProcessingProfile): boolean {
  return (
    profile.id === BUILTIN_PROFILE_IDS.cleanup ||
    profile.id === BUILTIN_PROFILE_IDS.assistant
  );
}

export function profileBindingId(profile: ProcessingProfile): string {
  if (profile.id === BUILTIN_PROFILE_IDS.cleanup)
    return BUILTIN_PROFILE_BINDINGS.builtin_cleanup;
  if (profile.id === BUILTIN_PROFILE_IDS.assistant)
    return BUILTIN_PROFILE_BINDINGS.builtin_assistant;
  return `${profile.kind === "dictation" ? "dictation_profile_" : "command_profile_"}${profile.id}`;
}

export function profileDisplayName(
  profile: ProcessingProfile,
  t: TFunction,
): string {
  return (
    profile.name ||
    t(
      profile.id === BUILTIN_PROFILE_IDS.cleanup
        ? "settings.postProcessing.profiles.builtinName.cleanup"
        : "settings.postProcessing.assistant.title",
    )
  );
}
