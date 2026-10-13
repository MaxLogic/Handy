import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  commands,
  type EnglishCleanupSettings as EnglishSettings,
} from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { SettingsGroup } from "@/components/ui";
import { Button } from "@/components/ui/Button";
import { Input } from "@/components/ui/Input";

export const EnglishCleanupSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, refreshSettings } = useSettings();
  const persisted = getSetting("english_cleanup");
  const [draft, setDraft] = useState<EnglishSettings>({
    enabled: false,
    base_url: "",
    model: "",
  });
  const [invalid, setInvalid] = useState<Record<string, string>>({});
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const [saving, setSaving] = useState(false);
  const busy = useRef(false);
  const k = (key: string) => `settings.postProcessing.englishCleanup.${key}`;

  useEffect(() => {
    if (persisted) setDraft(persisted);
  }, [persisted]);

  const change = (patch: Partial<EnglishSettings>) => {
    if (busy.current) return;
    setDraft((current) => ({ ...current, ...patch }));
    setInvalid({});
    setError("");
    setSaved(false);
  };
  const save = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (busy.current) return;
    const value = {
      ...draft,
      base_url: draft.base_url.trim(),
      model: draft.model.trim(),
    };
    const errors: Record<string, string> = {};
    if (
      (value.enabled && !value.base_url) ||
      (value.base_url &&
        (!/^https?:\/\//.test(value.base_url) ||
          [...value.base_url].length > 2048))
    )
      errors.base_url = t(k("invalidBaseUrl"));
    if ((value.enabled && !value.model) || [...value.model].length > 256)
      errors.model = t(k("invalidModel"));
    setSaved(false);
    setError("");
    setInvalid(errors);
    if (Object.keys(errors).length) return;
    busy.current = true;
    setSaving(true);
    try {
      const result = await commands.changeEnglishCleanupSettings(value);
      if (result.status === "error") throw result.error;
      await refreshSettings();
      setDraft(value);
      setSaved(true);
    } catch (caught) {
      const code = String(caught);
      if (code === "base_url_invalid")
        setInvalid({ base_url: t(k("invalidBaseUrl")) });
      else if (code === "model_required" || code === "model_too_long")
        setInvalid({ model: t(k("invalidModel")) });
      else setError(t("settings.postProcessing.assistant.saveError"));
    } finally {
      busy.current = false;
      setSaving(false);
    }
  };
  const described = (field: string) =>
    [
      invalid[field] ? `english-${field}-error` : "",
      error ? "english-save-error" : "",
    ]
      .filter(Boolean)
      .join(" ") || undefined;

  return (
    <SettingsGroup title={t(k("title"))}>
      <form
        aria-label={t(k("title"))}
        onSubmit={save}
        noValidate
        className="space-y-3 px-4 py-3"
      >
        <div className="flex items-start gap-2">
          <input
            id="english-enabled"
            type="checkbox"
            checked={draft.enabled}
            aria-disabled={saving}
            aria-describedby={[
              "english-prompt-note",
              "english-language-note",
              "english-failure-note",
              error ? "english-save-error" : "",
            ]
              .filter(Boolean)
              .join(" ")}
            onClick={(event) => {
              if (busy.current) event.preventDefault();
            }}
            onChange={(event) => change({ enabled: event.target.checked })}
            className="mt-1 accent-logo-primary focus-visible:ring-2 focus-visible:ring-logo-primary"
          />
          <label htmlFor="english-enabled" className="text-sm font-semibold">
            {t(k("enabled"))}
          </label>
        </div>
        <p id="english-prompt-note" className="text-xs text-mid-gray">
          {t(k("promptNote"))}
        </p>
        <p id="english-language-note" className="text-xs text-mid-gray">
          {t(k("languageNote"))}
        </p>
        <p id="english-failure-note" className="text-xs text-mid-gray">
          {t(k("failureNote"))}
        </p>
        {(["base_url", "model"] as const).map((field) => (
          <div key={field} className="space-y-2">
            <label
              htmlFor={`english-${field}`}
              className="block text-sm font-semibold"
            >
              {t(k(field === "base_url" ? "baseUrl" : "model"))}
            </label>
            <Input
              id={`english-${field}`}
              value={draft[field]}
              placeholder={
                field === "base_url" ? "http://127.0.0.1:8082/v1" : "S1-mini"
              }
              readOnly={saving}
              aria-disabled={saving}
              aria-invalid={!!invalid[field]}
              aria-describedby={described(field)}
              onChange={(event) => change({ [field]: event.target.value })}
              className="w-full"
            />
            {invalid[field] && (
              <p
                id={`english-${field}-error`}
                role="alert"
                className="text-sm text-red-500"
              >
                {invalid[field]}
              </p>
            )}
          </div>
        ))}
        {error && (
          <p
            id="english-save-error"
            role="alert"
            className="text-sm text-red-500"
          >
            {error}
          </p>
        )}
        <Button
          type="submit"
          aria-disabled={saving}
          aria-describedby={error ? "english-save-error" : undefined}
        >
          {t("settings.postProcessing.profiles.save")}
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
