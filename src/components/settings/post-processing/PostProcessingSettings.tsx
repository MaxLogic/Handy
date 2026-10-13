import React, { useEffect, useLayoutEffect, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { RefreshCcw } from "lucide-react";
import { commands } from "@/bindings";

import { Alert } from "../../ui/Alert";
import { SettingContainer, SettingsGroup, Textarea } from "@/components/ui";
import { Button } from "../../ui/Button";
import { ResetButton } from "../../ui/ResetButton";
import { Input } from "../../ui/Input";

import { ProviderSelect } from "../PostProcessingSettingsApi/ProviderSelect";
import { BaseUrlField } from "../PostProcessingSettingsApi/BaseUrlField";
import { ApiKeyField } from "../PostProcessingSettingsApi/ApiKeyField";
import { ModelSelect } from "../PostProcessingSettingsApi/ModelSelect";
import { usePostProcessProviderState } from "../PostProcessingSettingsApi/usePostProcessProviderState";
import { profileDisplayName, BUILTIN_PROFILE_IDS } from "@/lib/profiles";
import { ProfilesSettings } from "./ProfilesSettings";
import { useSettings } from "../../../hooks/useSettings";
import { LlmRequestSettings } from "./LlmRequestSettings";

const PostProcessingSettingsApiComponent: React.FC = () => {
  const { t } = useTranslation();
  const state = usePostProcessProviderState();

  return (
    <>
      {state.error && (
        <p role="alert" className="px-4 text-sm text-red-500">
          {state.error}
        </p>
      )}
      <SettingContainer
        title={t("settings.postProcessing.api.provider.title")}
        description={t("settings.postProcessing.api.provider.description")}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped={true}
      >
        <div className="flex items-center gap-2">
          <ProviderSelect
            ariaLabel={t("settings.postProcessing.api.provider.title")}
            options={state.providerOptions}
            value={state.selectedProviderId}
            onChange={state.handleProviderSelect}
          />
        </div>
      </SettingContainer>

      {state.isAppleProvider ? (
        state.appleIntelligenceUnavailable ? (
          <Alert variant="error" contained>
            {t("settings.postProcessing.api.appleIntelligence.unavailable")}
          </Alert>
        ) : null
      ) : (
        <>
          {state.selectedProvider?.id === "custom" && (
            <SettingContainer
              title={t("settings.postProcessing.api.baseUrl.title")}
              description={t("settings.postProcessing.api.baseUrl.description")}
              descriptionMode="tooltip"
              layout="horizontal"
              grouped={true}
            >
              <div className="flex items-center gap-2">
                <BaseUrlField
                  ariaLabel={t("settings.postProcessing.api.baseUrl.title")}
                  value={state.baseUrl}
                  onBlur={state.handleBaseUrlChange}
                  placeholder={t(
                    "settings.postProcessing.api.baseUrl.placeholder",
                  )}
                  disabled={state.isBaseUrlUpdating}
                  disabledReason={t(
                    "settings.postProcessing.api.baseUrl.saving",
                  )}
                  className="min-w-[380px]"
                />
              </div>
            </SettingContainer>
          )}

          <SettingContainer
            title={t("settings.postProcessing.api.apiKey.title")}
            description={t("settings.postProcessing.api.apiKey.description")}
            descriptionMode="tooltip"
            layout="horizontal"
            grouped={true}
          >
            <div className="flex items-center gap-2">
              <ApiKeyField
                ariaLabel={t("settings.postProcessing.api.apiKey.title")}
                value={state.apiKey}
                onBlur={state.handleApiKeyChange}
                placeholder={t(
                  "settings.postProcessing.api.apiKey.placeholder",
                )}
                disabled={state.isApiKeyUpdating}
                className="min-w-[320px]"
              />
            </div>
          </SettingContainer>
        </>
      )}

      {!state.isAppleProvider && (
        <SettingContainer
          title={t("settings.postProcessing.api.model.title")}
          description={
            state.isCustomProvider
              ? t("settings.postProcessing.api.model.descriptionCustom")
              : t("settings.postProcessing.api.model.descriptionDefault")
          }
          descriptionMode="tooltip"
          layout="stacked"
          grouped={true}
        >
          <div className="flex items-center gap-2">
            <ModelSelect
              ariaLabel={t("settings.postProcessing.api.model.title")}
              value={state.model}
              options={state.modelOptions}
              disabled={state.isModelUpdating}
              isLoading={state.isFetchingModels}
              placeholder={
                state.modelOptions.length > 0
                  ? t(
                      "settings.postProcessing.api.model.placeholderWithOptions",
                    )
                  : t("settings.postProcessing.api.model.placeholderNoOptions")
              }
              onSelect={state.handleModelSelect}
              onCreate={state.handleModelCreate}
              onBlur={() => {}}
              className="flex-1 min-w-[380px]"
            />
            <ResetButton
              onClick={state.handleRefreshModels}
              disabled={state.isFetchingModels}
              ariaLabel={t("settings.postProcessing.api.model.refreshModels")}
              className="flex h-10 w-10 items-center justify-center"
            >
              <RefreshCcw
                className={`h-4 w-4 ${state.isFetchingModels ? "animate-spin" : ""}`}
              />
            </ResetButton>
          </div>
        </SettingContainer>
      )}
    </>
  );
};

const PostProcessingSettingsPromptsComponent: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, refreshSettings } = useSettings();
  const [isCreating, setIsCreating] = useState(false);
  const [draftName, setDraftName] = useState("");
  const [draftText, setDraftText] = useState("");
  const [promptError, setPromptError] = useState("");

  const prompts = getSetting("post_process_prompts") || [];
  const [selectedPromptId, setSelectedPromptId] = useState<string | null>(null);
  const effectivePromptId =
    selectedPromptId ??
    getSetting("post_process_selected_prompt_id") ??
    prompts[0]?.id ??
    "";
  const profiles = getSetting("processing_profiles") ?? [];
  const usedBy = profiles
    .filter(
      (profile) =>
        (profile.id === BUILTIN_PROFILE_IDS.cleanup
          ? getSetting("post_process_selected_prompt_id")
          : profile.id === BUILTIN_PROFILE_IDS.assistant
            ? getSetting("assistant_selected_prompt_id")
            : profile.prompt_id) === effectivePromptId,
    )
    .map((profile) => profileDisplayName(profile, t))
    .join(", ");
  const savingRef = useRef(false);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const createTriggerRef = useRef<HTMLButtonElement>(null);
  const creationRef = useRef<HTMLDivElement>(null);
  const returnCreateFocusRef = useRef(false);
  useLayoutEffect(() => {
    if (!isCreating && returnCreateFocusRef.current) {
      returnCreateFocusRef.current = false;
      createTriggerRef.current?.focus();
    }
  }, [isCreating]);
  const selectedPrompt =
    prompts.find((prompt) => prompt.id === effectivePromptId) || null;

  useEffect(() => {
    if (isCreating) return;

    if (selectedPrompt) {
      setDraftName(selectedPrompt.name);
      setDraftText(selectedPrompt.prompt);
    } else {
      setDraftName("");
      setDraftText("");
    }
  }, [
    isCreating,
    effectivePromptId,
    selectedPrompt?.name,
    selectedPrompt?.prompt,
  ]);

  const perform = async (action: () => Promise<void>, inUse = false) => {
    if (savingRef.current) return;
    savingRef.current = true;
    setSaving(true);
    setSaved(false);
    setPromptError("");
    try {
      await action();
      await refreshSettings();
      setSaved(true);
    } catch (error) {
      setPromptError(
        inUse && String(error) === "prompt_in_use"
          ? t("settings.postProcessing.profiles.promptInUse", {
              names:
                usedBy || t("settings.postProcessing.profiles.unknownUsers"),
            })
          : t("settings.postProcessing.assistant.saveError"),
      );
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  };
  const handleCreatePrompt = () => {
    if (!draftName.trim() || !draftText.trim()) return;
    return perform(async () => {
      const result = await commands.addPostProcessPrompt(
        draftName.trim(),
        draftText.trim(),
      );
      if (result.status === "error") throw result.error;
      setSelectedPromptId(result.data.id);
      returnCreateFocusRef.current = !!creationRef.current?.contains(
        document.activeElement,
      );
      setIsCreating(false);
    });
  };
  const handleUpdatePrompt = () => {
    if (!effectivePromptId || !draftName.trim() || !draftText.trim()) return;
    return perform(async () => {
      const result = await commands.updatePostProcessPrompt(
        effectivePromptId,
        draftName.trim(),
        draftText.trim(),
      );
      if (result.status === "error") throw result.error;
    });
  };
  const handleDeletePrompt = () =>
    perform(async () => {
      const result = await commands.deletePostProcessPrompt(effectivePromptId);
      if (result.status === "error") throw result.error;
      setSelectedPromptId(null);
    }, true);

  const handleStartCreate = () => {
    if (savingRef.current || isCreating) return;
    setPromptError("");
    setSaved(false);
    setIsCreating(true);
    setDraftName("");
    setDraftText("");
  };

  const handleCancelCreate = () => {
    if (savingRef.current) return;
    returnCreateFocusRef.current = true;
    setIsCreating(false);
    setDraftName(selectedPrompt?.name ?? "");
    setDraftText(selectedPrompt?.prompt ?? "");
  };

  const hasPrompts = prompts.length > 0;
  const isDirty =
    !!selectedPrompt &&
    (draftName.trim() !== selectedPrompt.name ||
      draftText.trim() !== selectedPrompt.prompt.trim());

  return (
    <SettingContainer
      title={t("settings.postProcessing.profiles.promptToEdit")}
      description={t(
        "settings.postProcessing.prompts.selectedPrompt.description",
      )}
      descriptionMode="tooltip"
      layout="stacked"
      grouped={true}
    >
      <div className="space-y-3">
        {promptError && (
          <p
            id="prompt-library-error"
            role="alert"
            className="text-sm text-red-500"
          >
            {promptError}
          </p>
        )}
        <div className="flex gap-2 min-w-0">
          <select
            aria-label={t("settings.postProcessing.profiles.promptToEdit")}
            aria-disabled={saving || isCreating}
            aria-describedby={
              promptError ? "prompt-library-error" : "prompt-used-by"
            }
            value={effectivePromptId}
            onChange={(e) => {
              if (!savingRef.current && !isCreating) {
                setSelectedPromptId(e.target.value);
                setPromptError("");
                setSaved(false);
              }
            }}
            className="flex-1 min-w-0 rounded-md border border-mid-gray/80 bg-mid-gray/10 px-3 py-2 text-sm focus-visible:ring-2 focus-visible:ring-logo-primary"
          >
            {prompts.map((prompt) => (
              <option key={prompt.id} value={prompt.id}>
                {prompt.name}
              </option>
            ))}
          </select>
          <Button
            ref={createTriggerRef}
            onClick={handleStartCreate}
            variant="primary"
            size="md"
            aria-disabled={isCreating || saving}
            className="shrink-0"
          >
            {t("settings.postProcessing.prompts.createNew")}
          </Button>
        </div>

        <p id="prompt-used-by" className="text-sm text-mid-gray">
          {usedBy
            ? t("settings.postProcessing.profiles.usedBy", { names: usedBy })
            : t("settings.postProcessing.profiles.notUsed")}
        </p>
        <p
          role="status"
          aria-live="polite"
          aria-atomic="true"
          className="text-sm"
        >
          {saved ? t("settings.postProcessing.requestOptions.saved") : ""}
        </p>

        {!isCreating && hasPrompts && selectedPrompt && (
          <div className="space-y-3">
            <div className="space-y-2 flex flex-col">
              <label
                htmlFor="cleanup-prompt-name"
                className="text-sm font-semibold"
              >
                {t("settings.postProcessing.prompts.promptLabel")}
              </label>
              <Input
                id="cleanup-prompt-name"
                type="text"
                value={draftName}
                readOnly={saving}
                aria-disabled={saving}
                aria-describedby={
                  promptError ? "prompt-library-error" : undefined
                }
                onChange={(e) => setDraftName(e.target.value)}
                placeholder={t(
                  "settings.postProcessing.prompts.promptLabelPlaceholder",
                )}
                variant="compact"
              />
            </div>

            <div className="space-y-2 flex flex-col">
              <label
                htmlFor="cleanup-prompt-text"
                className="text-sm font-semibold"
              >
                {t("settings.postProcessing.prompts.promptInstructions")}
              </label>
              <Textarea
                id="cleanup-prompt-text"
                value={draftText}
                readOnly={saving}
                aria-disabled={saving}
                aria-describedby={
                  promptError ? "prompt-library-error" : undefined
                }
                onChange={(e) => setDraftText(e.target.value)}
                placeholder={t(
                  "settings.postProcessing.prompts.promptInstructionsPlaceholder",
                )}
              />
              <p className="text-xs text-mid-gray/70">
                <Trans
                  i18nKey="settings.postProcessing.prompts.promptTip"
                  components={{ code: <code /> }}
                />
              </p>
            </div>

            <div className="flex gap-2 pt-2">
              <Button
                onClick={() => {
                  if (isDirty) void handleUpdatePrompt();
                }}
                variant="primary"
                size="md"
                aria-disabled={
                  saving || !draftName.trim() || !draftText.trim() || !isDirty
                }
              >
                {t("settings.postProcessing.prompts.updatePrompt")}
              </Button>
              <Button
                onClick={() => {
                  if (prompts.length > 1) void handleDeletePrompt();
                }}
                variant="secondary"
                size="md"
                aria-disabled={
                  saving || !effectivePromptId || prompts.length <= 1
                }
                aria-describedby={
                  promptError ? "prompt-library-error" : undefined
                }
              >
                {t("settings.postProcessing.prompts.deletePrompt")}
              </Button>
            </div>
          </div>
        )}

        {!isCreating && !selectedPrompt && (
          <div className="p-3 bg-mid-gray/5 rounded-md border border-mid-gray/20">
            <p className="text-sm text-mid-gray">
              {hasPrompts
                ? t("settings.postProcessing.prompts.selectToEdit")
                : t("settings.postProcessing.prompts.createFirst")}
            </p>
          </div>
        )}

        {isCreating && (
          <div ref={creationRef} className="space-y-3">
            <div className="space-y-2 block flex flex-col">
              <label
                htmlFor="cleanup-prompt-name"
                className="text-sm font-semibold text-text"
              >
                {t("settings.postProcessing.prompts.promptLabel")}
              </label>
              <Input
                id="cleanup-prompt-name"
                type="text"
                value={draftName}
                readOnly={saving}
                aria-disabled={saving}
                aria-describedby={
                  promptError ? "prompt-library-error" : undefined
                }
                onChange={(e) => setDraftName(e.target.value)}
                placeholder={t(
                  "settings.postProcessing.prompts.promptLabelPlaceholder",
                )}
                variant="compact"
              />
            </div>

            <div className="space-y-2 flex flex-col">
              <label
                htmlFor="cleanup-prompt-text"
                className="text-sm font-semibold"
              >
                {t("settings.postProcessing.prompts.promptInstructions")}
              </label>
              <Textarea
                id="cleanup-prompt-text"
                value={draftText}
                readOnly={saving}
                aria-disabled={saving}
                aria-describedby={
                  promptError ? "prompt-library-error" : undefined
                }
                onChange={(e) => setDraftText(e.target.value)}
                placeholder={t(
                  "settings.postProcessing.prompts.promptInstructionsPlaceholder",
                )}
              />
              <p className="text-xs text-mid-gray/70">
                <Trans
                  i18nKey="settings.postProcessing.prompts.promptTip"
                  components={{ code: <code /> }}
                />
              </p>
            </div>

            <div className="flex gap-2 pt-2">
              <Button
                onClick={handleCreatePrompt}
                variant="primary"
                size="md"
                aria-disabled={saving || !draftName.trim() || !draftText.trim()}
              >
                {t("settings.postProcessing.prompts.createPrompt")}
              </Button>
              <Button
                onClick={handleCancelCreate}
                variant="secondary"
                size="md"
              >
                {t("settings.postProcessing.prompts.cancel")}
              </Button>
            </div>
          </div>
        )}
      </div>
    </SettingContainer>
  );
};

export const PostProcessingSettingsApi = React.memo(
  PostProcessingSettingsApiComponent,
);
PostProcessingSettingsApi.displayName = "PostProcessingSettingsApi";

export const PostProcessingSettingsPrompts = React.memo(
  PostProcessingSettingsPromptsComponent,
);
PostProcessingSettingsPrompts.displayName = "PostProcessingSettingsPrompts";

export const PostProcessingSettings: React.FC = () => {
  const { t } = useTranslation();

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <ProfilesSettings />
      <SettingsGroup title={t("settings.postProcessing.api.title")}>
        <p className="px-4 py-3 text-sm text-mid-gray">
          {t("settings.postProcessing.profiles.defaultApiHelp")}
        </p>
        <PostProcessingSettingsApi />
      </SettingsGroup>

      <SettingsGroup
        title={t("settings.postProcessing.profiles.promptLibraryTitle")}
      >
        <PostProcessingSettingsPrompts />
      </SettingsGroup>
      <LlmRequestSettings />
    </div>
  );
};
