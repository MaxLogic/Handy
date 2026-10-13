import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { LlmRequestOptions } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { SettingsGroup } from "@/components/ui";
import { Button } from "@/components/ui/Button";
import { Input } from "@/components/ui/Input";
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
          {t("settings.postProcessing.requestOptions.descriptionProfiles")}
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
