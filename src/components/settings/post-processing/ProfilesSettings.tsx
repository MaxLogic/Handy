import React, { useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  commands,
  type ProcessingProfile,
  type ProcessingProfileInput,
  type ProfileKind,
  type S1Controls,
} from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { useOsType } from "@/hooks/useOsType";
import { Dialog, SettingsGroup } from "@/components/ui";
import { Button } from "@/components/ui/Button";
import { LANGUAGES } from "@/lib/constants/languages";
import {
  BUILTIN_PROFILE_IDS,
  MAX_CUSTOM_PROFILES,
  isBuiltinProfile,
  profileBindingId,
  profileDisplayName,
} from "@/lib/profiles";
import { ShortcutInput } from "../ShortcutInput";

const fieldClass =
  "w-full rounded-md border border-mid-gray/80 bg-mid-gray/10 px-3 py-2 text-sm focus:outline-none focus-visible:ring-2 focus-visible:ring-logo-primary";
// Backend S1Controls::default(); bindings export the type, not the constants.
const defaultControls: S1Controls = {
  styling: "semi-formal",
  structure: "prose",
  context: "general",
};
const knownErrors = [
  "not_found",
  "name_required",
  "name_too_long",
  "name_taken",
  "profile_limit",
  "prompt_required",
  "prompt_not_found",
  "prompt_in_use",
  "kind_immutable",
  "kind_unsupported",
  "builtin_profile",
  "provider_not_found",
  "provider_unsupported",
  "base_url_not_allowed",
  "base_url_invalid",
  "model_required",
  "model_too_long",
  "language_invalid",
  "controls_invalid",
  "english_cleanup_command",
];
const errorField: Record<string, string> = {
  name_required: "name",
  name_too_long: "name",
  name_taken: "name",
  prompt_required: "prompt_id",
  prompt_not_found: "prompt_id",
  provider_not_found: "provider",
  provider_unsupported: "provider",
  base_url_not_allowed: "base_url",
  base_url_invalid: "base_url",
  model_required: "model",
  model_too_long: "model",
  language_invalid: "speech_language",
};
type Editor = {
  source?: ProcessingProfile;
  duplicate?: boolean;
  input: ProcessingProfileInput;
};

export const ProfilesSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, refreshSettings, clearProfileHotkey } = useSettings();
  const os = useOsType();
  const profiles = getSetting("processing_profiles") ?? [];
  const visibleProfiles = [...profiles]
    .sort((a, b) => {
      const rank = (p: ProcessingProfile) =>
        p.id === BUILTIN_PROFILE_IDS.cleanup
          ? 0
          : p.id === BUILTIN_PROFILE_IDS.assistant
            ? 1
            : 2;
      return rank(a) - rank(b);
    })
    .filter((p) => p.kind !== "selection_command" || os === "windows");
  const prompts = getSetting("post_process_prompts") ?? [];
  const providers = getSetting("post_process_providers") ?? [];
  const [editor, setEditor] = useState<Editor | null>(null);
  const [deleting, setDeleting] = useState<ProcessingProfile | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [invalid, setInvalid] = useState<Record<string, string>>({});
  const [status, setStatus] = useState("");
  const busyRef = useRef(false);
  const nameRef = useRef<HTMLInputElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const headingRef = useRef<HTMLParagraphElement>(null);
  const editRefs = useRef(new Map<string, HTMLButtonElement>());
  const focusAfterDelete = useRef<string | null>(null);
  const k = (key: string) => `settings.postProcessing.profiles.${key}`;
  const name = (profile: ProcessingProfile) => profileDisplayName(profile, t);
  const builtin =
    !!editor?.source && !editor.duplicate && isBuiltinProfile(editor.source);
  const englishOverride =
    !!editor?.input.english_cleanup &&
    !!editor.input.model &&
    !editor.input.model.model.toLowerCase().includes("s1-mini");
  const englishDisabled = !getSetting("english_cleanup")?.enabled;

  useLayoutEffect(() => {
    if (deleting || focusAfterDelete.current === null) return;
    const id = focusAfterDelete.current;
    focusAfterDelete.current = null;
    (editRefs.current.get(id) ?? headingRef.current)?.focus();
  }, [deleting, profiles]);

  const openEditor = (
    kind: ProfileKind,
    source?: ProcessingProfile,
    duplicate = false,
  ) => {
    if (busyRef.current) return;
    setError("");
    setInvalid({});
    setStatus("");
    const promptId =
      source?.id === BUILTIN_PROFILE_IDS.cleanup
        ? getSetting("post_process_selected_prompt_id")
        : source?.id === BUILTIN_PROFILE_IDS.assistant
          ? getSetting("assistant_selected_prompt_id")
          : source?.prompt_id;
    setEditor({
      source,
      duplicate,
      input: {
        name: source
          ? duplicate
            ? t(k("copyName"), { name: name(source) })
            : source.name
          : "",
        kind,
        prompt_id: source ? (promptId ?? null) : (prompts[0]?.id ?? null),
        model: source?.model ? { ...source.model } : null,
        speech_language: source?.speech_language ?? null,
        english_cleanup:
          !duplicate && source?.english_cleanup
            ? { ...source.english_cleanup }
            : null,
      },
    });
  };
  const update = (patch: Partial<ProcessingProfileInput>) => {
    if (busyRef.current) return;
    setEditor((current) =>
      current ? { ...current, input: { ...current.input, ...patch } } : null,
    );
    setInvalid({});
    setError("");
  };
  const message = (code: string) =>
    t(
      knownErrors.includes(code)
        ? k(`errors.${code}`)
        : "settings.postProcessing.assistant.saveError",
    );
  const save = async () => {
    if (!editor || busyRef.current) return;
    const input = {
      ...editor.input,
      name: editor.input.name.trim(),
      model: editor.input.model
        ? {
            ...editor.input.model,
            model: editor.input.model.model.trim(),
            base_url: editor.input.model.base_url?.trim() || null,
          }
        : null,
      english_cleanup: editor.input.english_cleanup
        ? {
            styling: editor.input.english_cleanup.styling.trim(),
            structure: editor.input.english_cleanup.structure.trim(),
            context: editor.input.english_cleanup.context.trim(),
          }
        : null,
    };
    const errors: Record<string, string> = {};
    const candidateName =
      input.name ||
      (builtin && editor.source ? name({ ...editor.source, name: "" }) : "");
    if (!builtin && !input.name) errors.name = message("name_required");
    else if ([...input.name].length > 60)
      errors.name = message("name_too_long");
    else if (
      candidateName &&
      profiles.some(
        (p) =>
          p.id !== (!editor.duplicate ? editor.source?.id : undefined) &&
          name(p).trim().toLocaleLowerCase() ===
            candidateName.toLocaleLowerCase(),
      )
    )
      errors.name = message("name_taken");
    if (!builtin && !input.prompt_id)
      errors.prompt_id = message("prompt_required");
    if (input.model) {
      if (!input.model.model || [...input.model.model].length > 256)
        errors.model = message("model_required");
      if (
        input.model.base_url &&
        (!/^https?:\/\//i.test(input.model.base_url) ||
          input.model.base_url.length > 2048)
      )
        errors.base_url = message("base_url_invalid");
      if (
        input.kind === "selection_command" &&
        input.model.provider_id === "apple_intelligence"
      )
        errors.provider = message("provider_unsupported");
    }
    if (input.english_cleanup) {
      for (const field of ["styling", "structure", "context"] as const) {
        const value = input.english_cleanup[field];
        if (
          ![...value].length ||
          [...value].length > 40 ||
          /[\[\]\p{Cc}]/u.test(value)
        )
          errors[field] = t(k(`englishControlErrors.${field}`));
      }
    }
    if (Object.keys(errors).length) {
      setInvalid(errors);
      return;
    }
    busyRef.current = true;
    setSaving(true);
    setError("");
    setInvalid({});
    try {
      const result =
        editor.source && !editor.duplicate
          ? await commands.updateProcessingProfile(editor.source.id, input)
          : await commands.addProcessingProfile(input);
      if (result.status === "error") throw result.error;
      await refreshSettings();
      setEditor(null);
      setStatus(t("settings.postProcessing.requestOptions.saved"));
    } catch (caught) {
      const code = String(caught);
      const field = errorField[code];
      if (field) setInvalid({ [field]: message(code) });
      else if (code === "controls_invalid" && input.english_cleanup)
        setInvalid(
          Object.fromEntries(
            ["styling", "structure", "context"].map((field) => [
              field,
              message(code),
            ]),
          ),
        );
      else setError(message(code));
    } finally {
      busyRef.current = false;
      setSaving(false);
    }
  };
  const remove = async () => {
    if (!deleting || busyRef.current) return;
    busyRef.current = true;
    setSaving(true);
    setError("");
    try {
      const index = visibleProfiles.findIndex((p) => p.id === deleting.id);
      const target = visibleProfiles[index + 1] ?? visibleProfiles[index - 1];
      const result = await commands.deleteProcessingProfile(deleting.id);
      if (result.status === "error") throw result.error;
      await refreshSettings();
      focusAfterDelete.current = target?.id ?? "";
      setDeleting(null);
      setStatus(t(k("deleted")));
    } catch (caught) {
      setError(message(String(caught)));
    } finally {
      busyRef.current = false;
      setSaving(false);
    }
  };
  const described = (field: string, help?: string) =>
    [
      help,
      invalid[field] ? `profile-${field}-error` : "",
      error ? "profile-error" : "",
    ]
      .filter(Boolean)
      .join(" ") || undefined;
  const fieldError = (field: string) =>
    invalid[field] && (
      <p
        id={`profile-${field}-error`}
        role="alert"
        className="text-sm text-red-500"
      >
        {invalid[field]}
      </p>
    );
  const customCount = profiles.filter((p) => !isBuiltinProfile(p)).length;
  return (
    <SettingsGroup title={t(k("title"))}>
      <div className="space-y-4 px-4 py-3">
        <p ref={headingRef} tabIndex={-1} className="sr-only">
          {t(k("title"))}
        </p>
        <p className="text-sm text-mid-gray">{t(k("intro"))}</p>
        <ul aria-label={t(k("title"))} className="space-y-4">
          {visibleProfiles.map((profile) => (
            <li
              key={profile.id}
              aria-label={t(
                k(isBuiltinProfile(profile) ? "builtinRow" : "row"),
                { name: name(profile), kind: t(k(`kind.${profile.kind}`)) },
              )}
              className="rounded-md border border-mid-gray/20"
            >
              <h3 className="px-4 pt-3 text-sm font-semibold">
                {name(profile)}, {t(k(`kind.${profile.kind}`))}
                {isBuiltinProfile(profile) ? `, ${t(k("builtin"))}` : ""}
              </h3>
              {profile.id === BUILTIN_PROFILE_IDS.cleanup &&
                !getSetting("post_process_enabled") && (
                  <p
                    id="cleanup-hotkey-off"
                    className="px-4 pt-2 text-sm text-mid-gray"
                  >
                    {t(k("cleanupOff"))}
                  </p>
                )}
              <ShortcutInput
                shortcutId={profileBindingId(profile)}
                label={name(profile)}
                title={t(k("hotkey"))}
                unassignedLabel={t(k("notSet"))}
                describedBy={
                  profile.id === BUILTIN_PROFILE_IDS.cleanup &&
                  !getSetting("post_process_enabled")
                    ? "cleanup-hotkey-off"
                    : undefined
                }
                onRemove={
                  isBuiltinProfile(profile)
                    ? undefined
                    : () => clearProfileHotkey(profile.id)
                }
                removeLabel={t(k("removeHotkey"), { name: name(profile) })}
                grouped
              />
              <div className="flex flex-wrap gap-2 px-4 pb-3">
                <Button
                  ref={(element) => {
                    if (element) editRefs.current.set(profile.id, element);
                    else editRefs.current.delete(profile.id);
                  }}
                  variant="secondary"
                  onClick={() => openEditor(profile.kind, profile)}
                >
                  {t(k("edit"), { name: name(profile) })}
                </Button>
                <Button
                  variant="secondary"
                  aria-disabled={customCount >= MAX_CUSTOM_PROFILES}
                  aria-describedby={
                    customCount >= MAX_CUSTOM_PROFILES
                      ? "profile-limit"
                      : undefined
                  }
                  onClick={() => {
                    if (customCount < MAX_CUSTOM_PROFILES)
                      openEditor(profile.kind, profile, true);
                  }}
                >
                  {t(k("duplicate"), { name: name(profile) })}
                </Button>
                {!isBuiltinProfile(profile) && (
                  <Button
                    variant="secondary"
                    onClick={() => {
                      setError("");
                      setStatus("");
                      setDeleting(profile);
                    }}
                  >
                    {t(k("delete"), { name: name(profile) })}
                  </Button>
                )}
              </div>
            </li>
          ))}
        </ul>
        <div className="flex flex-wrap gap-2">
          {(
            [
              "dictation",
              ...(os === "windows" ? ["selection_command"] : []),
            ] as ProfileKind[]
          ).map((kind) => (
            <Button
              key={kind}
              aria-disabled={customCount >= MAX_CUSTOM_PROFILES}
              aria-describedby={
                customCount >= MAX_CUSTOM_PROFILES ? "profile-limit" : undefined
              }
              onClick={() => {
                if (customCount < MAX_CUSTOM_PROFILES) openEditor(kind);
              }}
            >
              {t(k(`add.${kind}`))}
            </Button>
          ))}
        </div>
        {customCount >= MAX_CUSTOM_PROFILES && (
          <p id="profile-limit" className="text-sm">
            {message("profile_limit")}
          </p>
        )}
        <p
          role="status"
          aria-live="polite"
          aria-atomic="true"
          className="text-sm"
        >
          {status}
        </p>
      </div>
      <Dialog
        open={!!editor}
        title={
          editor?.source
            ? t(k(editor.duplicate ? "duplicate" : "edit"), {
                name: name(editor.source),
              })
            : t(k(`add.${editor?.input.kind ?? "dictation"}`))
        }
        closeLabel={t("common.close")}
        initialFocusRef={nameRef}
        dismissible={!saving}
        onOpenChange={(open) => {
          if (!open && !busyRef.current) setEditor(null);
        }}
        footer={
          <>
            <Button
              variant="secondary"
              aria-disabled={saving}
              onClick={() => {
                if (!busyRef.current) setEditor(null);
              }}
            >
              {t("settings.postProcessing.prompts.cancel")}
            </Button>
            <Button
              aria-disabled={saving}
              aria-describedby={error ? "profile-error" : undefined}
              onClick={() => void save()}
            >
              {t(k("save"))}
            </Button>
          </>
        }
      >
        {editor && (
          <div className="space-y-3">
            <label
              htmlFor="profile-name"
              className="block text-sm font-semibold"
            >
              {t(k("name"))}
            </label>
            <input
              id="profile-name"
              ref={nameRef}
              className={fieldClass}
              value={editor.input.name}
              placeholder={
                builtin && editor.source
                  ? name({ ...editor.source, name: "" })
                  : undefined
              }
              readOnly={saving}
              aria-disabled={saving}
              aria-required={!builtin}
              aria-invalid={!!invalid.name}
              aria-describedby={described(
                "name",
                builtin ? "profile-name-help" : undefined,
              )}
              onChange={(e) => update({ name: e.target.value })}
            />
            {builtin && (
              <p id="profile-name-help" className="text-xs text-mid-gray">
                {t(k("nameHelp"))}
              </p>
            )}
            {fieldError("name")}
            <p className="text-sm">
              {t(k("kindLabel"))}: {t(k(`kind.${editor.input.kind}`))}
            </p>
            <label
              htmlFor="profile-prompt"
              className="block text-sm font-semibold"
            >
              {t(k("prompt"))}
            </label>
            <select
              id="profile-prompt"
              onKeyDown={(e) => {
                if (busyRef.current && e.key !== "Tab") e.preventDefault();
              }}
              onMouseDown={(e) => {
                if (busyRef.current) e.preventDefault();
              }}
              className={fieldClass}
              value={editor.input.prompt_id ?? ""}
              aria-disabled={saving}
              aria-invalid={!!invalid.prompt_id}
              aria-describedby={described("prompt_id", "profile-prompt-help")}
              onChange={(e) => update({ prompt_id: e.target.value || null })}
            >
              {builtin && <option value="">{t(k("noPrompt"))}</option>}
              {!builtin && prompts.length > 0 && !editor.input.prompt_id && (
                <option value="" disabled>
                  {t("settings.postProcessing.prompts.selectPrompt")}
                </option>
              )}
              {!builtin && !prompts.length && (
                <option value="">
                  {t("settings.postProcessing.prompts.noPrompts")}
                </option>
              )}
              {prompts.map((prompt) => (
                <option key={prompt.id} value={prompt.id}>
                  {prompt.name}
                </option>
              ))}
            </select>
            <p id="profile-prompt-help" className="text-xs text-mid-gray">
              {t(k("promptHelp"))}
            </p>
            {fieldError("prompt_id")}
            <label
              htmlFor="profile-provider"
              className="block text-sm font-semibold"
            >
              {t(k("model"))}
            </label>
            <select
              id="profile-provider"
              onKeyDown={(e) => {
                if (busyRef.current && e.key !== "Tab") e.preventDefault();
              }}
              onMouseDown={(e) => {
                if (busyRef.current) e.preventDefault();
              }}
              className={fieldClass}
              value={editor.input.model?.provider_id ?? ""}
              aria-disabled={saving}
              aria-invalid={!!invalid.provider}
              aria-describedby={described(
                "provider",
                englishOverride ? "profile-english-override" : undefined,
              )}
              onChange={(e) =>
                update({
                  model: e.target.value
                    ? { provider_id: e.target.value, base_url: null, model: "" }
                    : null,
                })
              }
            >
              <option value="">{t(k("defaultModel"))}</option>
              {providers.map((provider) => (
                <option key={provider.id} value={provider.id}>
                  {provider.label}
                </option>
              ))}
            </select>
            {fieldError("provider")}
            {editor.input.model && (
              <>
                <label
                  htmlFor="profile-model"
                  className="block text-sm font-semibold"
                >
                  {t(k("modelName"))}
                </label>
                <input
                  id="profile-model"
                  className={fieldClass}
                  value={editor.input.model.model}
                  readOnly={saving}
                  aria-disabled={saving}
                  aria-invalid={!!invalid.model}
                  aria-describedby={described("model")}
                  onChange={(e) =>
                    update({
                      model: { ...editor.input.model!, model: e.target.value },
                    })
                  }
                />
                {fieldError("model")}
                {editor.input.model.provider_id === "custom" && (
                  <>
                    <label
                      htmlFor="profile-base-url"
                      className="block text-sm font-semibold"
                    >
                      {t(k("baseUrl"))}
                    </label>
                    <input
                      id="profile-base-url"
                      className={fieldClass}
                      value={editor.input.model.base_url ?? ""}
                      placeholder={
                        providers.find((p) => p.id === "custom")?.base_url
                      }
                      readOnly={saving}
                      aria-disabled={saving}
                      aria-invalid={!!invalid.base_url}
                      aria-describedby={described(
                        "base_url",
                        "profile-url-help",
                      )}
                      onChange={(e) =>
                        update({
                          model: {
                            ...editor.input.model!,
                            base_url: e.target.value || null,
                          },
                        })
                      }
                    />
                    <p id="profile-url-help" className="text-xs text-mid-gray">
                      {t(k("baseUrlHelp"))}
                    </p>
                    {fieldError("base_url")}
                  </>
                )}
              </>
            )}
            <label
              htmlFor="profile-language"
              className="block text-sm font-semibold"
            >
              {t(k("speechLanguage"))}
            </label>
            <select
              id="profile-language"
              onKeyDown={(e) => {
                if (busyRef.current && e.key !== "Tab") e.preventDefault();
              }}
              onMouseDown={(e) => {
                if (busyRef.current) e.preventDefault();
              }}
              className={fieldClass}
              value={editor.input.speech_language ?? ""}
              aria-disabled={saving}
              aria-invalid={!!invalid.speech_language}
              aria-describedby={described(
                "speech_language",
                "profile-language-help",
              )}
              onChange={(e) =>
                update({ speech_language: e.target.value || null })
              }
            >
              <option value="">{t(k("globalLanguage"))}</option>
              {LANGUAGES.map((language) => (
                <option key={language.value} value={language.value}>
                  {language.label}
                </option>
              ))}
            </select>
            <p id="profile-language-help" className="text-xs text-mid-gray">
              {t(k("languageHelp"))}
            </p>
            {fieldError("speech_language")}
            {editor.input.kind === "dictation" && (
              <div id="profile-english-controls" className="space-y-3">
                <div className="flex items-start gap-2">
                  <input
                    id="profile-english-enabled"
                    type="checkbox"
                    checked={!!editor.input.english_cleanup}
                    aria-disabled={saving}
                    aria-describedby={described(
                      "english_cleanup",
                      [
                        "profile-english-note",
                        englishDisabled ? "profile-english-disabled" : "",
                        englishOverride ? "profile-english-override" : "",
                      ]
                        .filter(Boolean)
                        .join(" "),
                    )}
                    onClick={(event) => {
                      if (busyRef.current) event.preventDefault();
                    }}
                    onChange={(event) =>
                      update({
                        english_cleanup: event.target.checked
                          ? { ...defaultControls }
                          : null,
                      })
                    }
                    className="mt-1 accent-logo-primary focus-visible:ring-2 focus-visible:ring-logo-primary"
                  />
                  <label
                    htmlFor="profile-english-enabled"
                    className="text-sm font-semibold"
                  >
                    {t("settings.postProcessing.englishCleanup.enabled")}
                  </label>
                </div>
                <p id="profile-english-note" className="text-xs text-mid-gray">
                  {t(k("englishNote"))}
                </p>
                {englishDisabled && (
                  <p
                    id="profile-english-disabled"
                    className="text-xs text-mid-gray"
                  >
                    {t(k("englishDisabled"))}
                  </p>
                )}
                {englishOverride && (
                  <p
                    id="profile-english-override"
                    className="text-xs text-mid-gray"
                  >
                    {t(k("englishOverride"))}
                  </p>
                )}
                {editor.input.english_cleanup && (
                  <>
                    {(["styling", "structure", "context"] as const).map(
                      (field) => (
                        <div key={field} className="space-y-2">
                          <label
                            htmlFor={`profile-${field}`}
                            className="block text-sm font-semibold"
                          >
                            {t(k(`englishControls.${field}`))}
                          </label>
                          <input
                            id={`profile-${field}`}
                            className={fieldClass}
                            value={editor.input.english_cleanup![field]}
                            readOnly={saving}
                            aria-disabled={saving}
                            aria-invalid={!!invalid[field]}
                            aria-describedby={described(
                              field,
                              [
                                "profile-english-help",
                                englishOverride
                                  ? "profile-english-override"
                                  : "",
                              ]
                                .filter(Boolean)
                                .join(" "),
                            )}
                            onChange={(event) =>
                              update({
                                english_cleanup: {
                                  ...editor.input.english_cleanup!,
                                  [field]: event.target.value,
                                },
                              })
                            }
                          />
                          {fieldError(field)}
                        </div>
                      ),
                    )}
                    <p
                      id="profile-english-help"
                      className="text-xs text-mid-gray"
                    >
                      {t(k("englishControlsHelp"))}
                    </p>
                  </>
                )}
              </div>
            )}
            {error && (
              <p
                id="profile-error"
                role="alert"
                className="text-sm text-red-500"
              >
                {error}
              </p>
            )}
          </div>
        )}
      </Dialog>
      <Dialog
        open={!!deleting}
        title={t(k("deleteTitle"), { name: deleting ? name(deleting) : "" })}
        closeLabel={t("common.close")}
        initialFocusRef={cancelRef}
        dismissible={!saving}
        onOpenChange={(open) => {
          if (!open && !busyRef.current) setDeleting(null);
        }}
        footer={
          <>
            <Button
              ref={cancelRef}
              variant="secondary"
              aria-disabled={saving}
              onClick={() => {
                if (!busyRef.current) setDeleting(null);
              }}
            >
              {t("settings.postProcessing.prompts.cancel")}
            </Button>
            <Button
              aria-disabled={saving}
              aria-describedby={error ? "profile-delete-error" : undefined}
              onClick={() => void remove()}
            >
              {t(k("confirmDelete"))}
            </Button>
          </>
        }
      >
        <p className="text-sm">{t(k("deleteHelp"))}</p>
        {error && (
          <p
            id="profile-delete-error"
            role="alert"
            className="text-sm text-red-500"
          >
            {error}
          </p>
        )}
      </Dialog>
    </SettingsGroup>
  );
};
