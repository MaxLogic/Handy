import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands, type LlmRequestOptions } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { useOsType } from "@/hooks/useOsType";
import { Dialog, SettingsGroup } from "@/components/ui";
import { Button } from "@/components/ui/Button";
import { Input } from "@/components/ui/Input";
import { ShortcutInput } from "../ShortcutInput";

const fieldClass =
  "w-full rounded-md border border-mid-gray/80 bg-mid-gray/10 px-3 py-2 text-sm focus:outline-none focus-visible:ring-2 focus-visible:ring-logo-primary";

export const AssistantSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, refreshSettings, isUpdating } =
    useSettings();
  const os = useOsType();
  const prompts = getSetting("post_process_prompts") ?? [];
  const selectedId = getSetting("assistant_selected_prompt_id") ?? "";
  const selected = prompts.find((p) => p.id === selectedId);
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [text, setText] = useState("");
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);
  const savingRef = useRef(false);
  const selectingRef = useRef(false);

  const edit = () => {
    if (!selected || savingRef.current) return;
    setName(selected.name);
    setText(selected.prompt);
    setError("");
    setOpen(true);
  };
  const save = async () => {
    if (!selected || savingRef.current) return;
    if (!name.trim() || !text.trim()) {
      setError(t("settings.postProcessing.assistant.promptRequired"));
      return;
    }
    savingRef.current = true;
    setSaving(true);
    try {
      const result = await commands.updatePostProcessPrompt(
        selected.id,
        name.trim(),
        text.trim(),
      );
      if (result.status === "error") throw new Error(result.error);
      await refreshSettings();
      setOpen(false);
    } catch {
      setError(t("settings.postProcessing.assistant.saveError"));
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  };
  return (
    <SettingsGroup title={t("settings.postProcessing.assistant.title")}>
      {os === "windows" && <ShortcutInput shortcutId="assistant" grouped />}
      <div className="space-y-3 px-4 py-3">
        <p className="text-sm text-mid-gray">
          {t("settings.postProcessing.assistant.description")}
        </p>
        <label
          className="block text-sm font-semibold"
          htmlFor="assistant-prompt"
        >
          {t("settings.postProcessing.assistant.prompt")}
        </label>
        <div className="flex gap-2">
          <select
            id="assistant-prompt"
            className={fieldClass}
            value={selectedId}
            aria-disabled={isUpdating("assistant_selected_prompt_id")}
            onKeyDown={(event) => {
              if (selectingRef.current && event.key !== "Tab")
                event.preventDefault();
            }}
            onMouseDown={(event) => {
              if (selectingRef.current) event.preventDefault();
            }}
            onChange={async (event) => {
              if (selectingRef.current) {
                event.currentTarget.value = selectedId;
                return;
              }
              selectingRef.current = true;
              setError("");
              try {
                await updateSetting(
                  "assistant_selected_prompt_id",
                  event.target.value || null,
                );
              } catch {
                setError(t("settings.postProcessing.assistant.saveError"));
              } finally {
                selectingRef.current = false;
              }
            }}
          >
            <option value="">
              {t("settings.postProcessing.assistant.noPrompt")}
            </option>
            {prompts.map((prompt) => (
              <option key={prompt.id} value={prompt.id}>
                {prompt.name}
              </option>
            ))}
          </select>
          <Button variant="secondary" onClick={edit} disabled={!selected}>
            {t("settings.postProcessing.assistant.editPrompt")}
          </Button>
        </div>
        {error && !open && (
          <p role="alert" className="text-sm text-red-500">
            {error}
          </p>
        )}
      </div>
      <Dialog
        open={open}
        title={t("settings.postProcessing.assistant.editPrompt")}
        closeLabel={t("common.close")}
        onOpenChange={setOpen}
        initialFocusRef={nameRef}
        dismissible={!saving}
        footer={
          <>
            <Button
              variant="secondary"
              aria-disabled={saving}
              onClick={() => {
                if (!savingRef.current) setOpen(false);
              }}
            >
              {t("settings.postProcessing.prompts.cancel")}
            </Button>
            <Button onClick={save} aria-disabled={saving}>
              {t("settings.postProcessing.assistant.save")}
            </Button>
          </>
        }
      >
        <div className="space-y-3">
          <label
            htmlFor="assistant-prompt-name"
            className="block text-sm font-semibold"
          >
            {t("settings.postProcessing.prompts.promptLabel")}
          </label>
          <input
            ref={nameRef}
            id="assistant-prompt-name"
            className={fieldClass}
            value={name}
            onChange={(e) => setName(e.target.value)}
            aria-invalid={!!error}
            aria-describedby={error ? "assistant-prompt-error" : undefined}
          />
          <label
            htmlFor="assistant-prompt-text"
            className="block text-sm font-semibold"
          >
            {t("settings.postProcessing.prompts.promptInstructions")}
          </label>
          <textarea
            id="assistant-prompt-text"
            className={`${fieldClass} min-h-48`}
            value={text}
            onChange={(e) => setText(e.target.value)}
            aria-invalid={!!error}
            aria-describedby={error ? "assistant-prompt-error" : undefined}
          />
          <p className="text-xs text-mid-gray">
            {t("settings.postProcessing.assistant.sharedPrompt")}
          </p>
          {error && (
            <p
              id="assistant-prompt-error"
              role="alert"
              className="text-sm text-red-500"
            >
              {error}
            </p>
          )}
        </div>
      </Dialog>
    </SettingsGroup>
  );
};

export const LlmRequestSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const options = getSetting("post_process_request_options");
  const [draft, setDraft] = useState({
    temperature: "",
    max_output_tokens: "",
    timeout_secs: "60",
    connect_timeout_secs: "5",
  });
  const [error, setError] = useState("");
  const [invalid, setInvalid] = useState<Record<string, string>>({});
  const [saved, setSaved] = useState(false);
  const savingRef = useRef(false);
  useEffect(() => {
    if (!options) return;
    setDraft({
      temperature: options.temperature?.toString() ?? "",
      max_output_tokens: options.max_output_tokens?.toString() ?? "",
      timeout_secs: String(options.timeout_secs),
      connect_timeout_secs: String(options.connect_timeout_secs),
    });
  }, [options]);
  const save = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (savingRef.current) return;
    setSaved(false);
    const form = event.currentTarget;
    const badInput = (key: keyof typeof draft) =>
      (form.elements.namedItem(`llm-${key}`) as HTMLInputElement).validity
        .badInput;
    const value: LlmRequestOptions = {
      temperature:
        draft.temperature.trim() === "" ? null : Number(draft.temperature),
      max_output_tokens:
        draft.max_output_tokens.trim() === ""
          ? null
          : Number(draft.max_output_tokens),
      timeout_secs: Number(draft.timeout_secs),
      connect_timeout_secs: Number(draft.connect_timeout_secs),
    };
    const bad: Record<string, string> = {};
    if (
      badInput("temperature") ||
      (value.temperature !== null &&
        (!Number.isFinite(value.temperature) ||
          value.temperature < 0 ||
          value.temperature > 2))
    )
      bad.temperature = t(
        "settings.postProcessing.requestOptions.invalidTemperature",
      );
    if (
      badInput("max_output_tokens") ||
      (value.max_output_tokens !== null &&
        (!Number.isInteger(value.max_output_tokens) ||
          value.max_output_tokens < 1 ||
          value.max_output_tokens > 131072))
    )
      bad.max_output_tokens = t(
        "settings.postProcessing.requestOptions.invalidMaxOutputTokens",
      );
    if (
      badInput("timeout_secs") ||
      !Number.isInteger(value.timeout_secs) ||
      value.timeout_secs < 1 ||
      value.timeout_secs > 300
    )
      bad.timeout_secs = t(
        "settings.postProcessing.requestOptions.invalidTimeout",
      );
    if (
      badInput("connect_timeout_secs") ||
      !Number.isInteger(value.connect_timeout_secs) ||
      value.connect_timeout_secs < 1 ||
      value.connect_timeout_secs > 30
    )
      bad.connect_timeout_secs = t(
        "settings.postProcessing.requestOptions.invalidConnectTimeout",
      );
    else if (
      !bad.timeout_secs &&
      value.connect_timeout_secs > value.timeout_secs
    )
      bad.connect_timeout_secs = t(
        "settings.postProcessing.requestOptions.connectExceedsOverall",
      );
    setInvalid(bad);
    setError("");
    if (Object.keys(bad).length) {
      return;
    }
    savingRef.current = true;
    try {
      await updateSetting("post_process_request_options", value);
      setSaved(true);
    } catch {
      setError(t("settings.postProcessing.assistant.saveError"));
    } finally {
      savingRef.current = false;
    }
  };
  return (
    <SettingsGroup title={t("settings.postProcessing.requestOptions.title")}>
      <form onSubmit={save} noValidate className="space-y-3 px-4 py-3">
        <p className="text-sm text-mid-gray">
          {t("settings.postProcessing.requestOptions.description")}
        </p>
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          {(Object.keys(draft) as (keyof typeof draft)[]).map((key) => (
            <div key={key} className="space-y-2">
              <label
                htmlFor={`llm-${key}`}
                className="block text-sm font-semibold"
              >
                {t(`settings.postProcessing.requestOptions.${key}`)}
              </label>
              <Input
                id={`llm-${key}`}
                type="number"
                step={key === "temperature" ? "0.1" : "1"}
                value={draft[key]}
                onChange={(e) => {
                  setSaved(false);
                  setDraft((old) => ({ ...old, [key]: e.target.value }));
                }}
                aria-invalid={!!invalid[key]}
                aria-describedby={`llm-options-help${invalid[key] ? ` llm-${key}-error` : ""}${error ? " llm-options-error" : ""}`}
                className="w-full"
              />
              {invalid[key] && (
                <p
                  id={`llm-${key}-error`}
                  role="alert"
                  className="text-sm text-red-500"
                >
                  {invalid[key]}
                </p>
              )}
            </div>
          ))}
        </div>
        <p id="llm-options-help" className="text-xs text-mid-gray">
          {t("settings.postProcessing.requestOptions.help")}
        </p>
        {error && (
          <p
            id="llm-options-error"
            role="alert"
            className="text-sm text-red-500"
          >
            {error}
          </p>
        )}
        <Button
          type="submit"
          aria-disabled={isUpdating("post_process_request_options")}
        >
          {t("settings.postProcessing.requestOptions.save")}
        </Button>
        <p
          role="status"
          aria-live="polite"
          aria-atomic="true"
          className="text-sm"
        >
          {saved ? t("settings.postProcessing.requestOptions.saved") : ""}
        </p>
      </form>
    </SettingsGroup>
  );
};
