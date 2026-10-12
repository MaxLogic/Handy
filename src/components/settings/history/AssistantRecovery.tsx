import React, {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import {
  commands,
  events,
  type AssistantRecoveryItem,
  type RecoveryPart,
} from "@/bindings";
import { Button } from "../../ui/Button";

export const AssistantRecovery: React.FC = () => {
  const { t } = useTranslation();
  const [items, setItems] = useState<AssistantRecoveryItem[]>([]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState<number | null>(null);
  const busyRef = useRef(false);
  const focusRef = useRef<number | "heading" | null>(null);
  const [visible, setVisible] = useState(false);
  const [copied, setCopied] = useState<{
    id: number;
    part: RecoveryPart;
  } | null>(null);
  const refresh = useCallback(async () => {
    try {
      const state = await commands.getAssistantRecoveryState();
      if (state.items.length) setVisible(true);
      setItems(state.items);
      return true;
    } catch {
      setError(t("settings.history.assistant.actionError"));
      return false;
    }
  }, [t]);
  useEffect(() => {
    void refresh();
    const subscription = events.assistantRecoveryUpdate.listen(() => {
      void refresh();
    });
    return () => {
      void subscription.then((unlisten) => unlisten());
    };
  }, [refresh]);
  useLayoutEffect(() => {
    if (focusRef.current === null) return;
    const target =
      focusRef.current === "heading"
        ? "assistant-recovery-title"
        : `assistant-recovery-${focusRef.current}`;
    document.getElementById(target)?.focus();
    focusRef.current = null;
  }, [items]);
  const act = async (id: number, part?: RecoveryPart) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(id);
    setCopied(null);
    setError("");
    try {
      const result = part
        ? await commands.copyAssistantRecoveryText(id, part)
        : await commands.dismissAssistantRecoveryItem(id);
      if (result.status === "error") throw new Error(result.error);
      if (!part) {
        const next = items[items.findIndex((item) => item.id === id) + 1];
        focusRef.current = next?.id ?? "heading";
        if (!(await refresh())) focusRef.current = null;
      } else {
        setCopied({ id, part });
      }
    } catch {
      setError(t("settings.history.assistant.actionError"));
    } finally {
      busyRef.current = false;
      setBusy(null);
    }
  };
  if (!visible && !error) return null;
  return (
    <section
      className="space-y-3 rounded-lg border border-mid-gray/20 p-4"
      aria-label={t("settings.history.assistant.title")}
    >
      <h2
        id="assistant-recovery-title"
        tabIndex={-1}
        className="text-sm font-semibold focus-visible:ring-2 focus-visible:ring-logo-primary"
      >
        {t("settings.history.assistant.title")}
      </h2>
      <p className="text-sm text-mid-gray">
        {t("settings.history.assistant.description")}
      </p>
      {error && (
        <p role="alert" className="text-sm text-red-500">
          {error}
        </p>
      )}
      {items.map((item) => (
        <article
          key={item.id}
          id={`assistant-recovery-${item.id}`}
          tabIndex={-1}
          aria-labelledby={`recovery-outcome-${item.id} recovery-name-${item.id}`}
          className="space-y-3 border-t border-mid-gray/20 pt-3"
        >
          <p id={`recovery-outcome-${item.id}`} className="text-sm">
            {t(
              item.outcome === "uncertain"
                ? "settings.history.assistant.uncertain"
                : "settings.history.assistant.refused",
            )}
          </p>
          <p id={`recovery-name-${item.id}`} className="sr-only">
            {item.instruction}
          </p>
          <label
            htmlFor={`recovery-generated-${item.id}`}
            className="block text-sm font-semibold"
          >
            {t("settings.history.assistant.generated")}
          </label>
          <textarea
            id={`recovery-generated-${item.id}`}
            readOnly
            value={item.generated}
            className="w-full min-h-24 select-text rounded border border-mid-gray/40 p-2 text-sm focus-visible:ring-2 focus-visible:ring-logo-primary"
          />
          <label
            htmlFor={`recovery-instruction-${item.id}`}
            className="block text-sm font-semibold"
          >
            {t("settings.history.assistant.instruction")}
          </label>
          <textarea
            id={`recovery-instruction-${item.id}`}
            readOnly
            value={item.instruction}
            className="w-full select-text rounded border border-mid-gray/40 p-2 text-sm focus-visible:ring-2 focus-visible:ring-logo-primary"
          />
          <div className="flex flex-wrap gap-2">
            <Button
              aria-disabled={busy !== null}
              aria-describedby={`recovery-name-${item.id}`}
              onClick={() => act(item.id, "generated")}
            >
              {t("settings.history.assistant.copyGenerated")}
            </Button>
            <Button
              variant="secondary"
              aria-disabled={busy !== null}
              aria-describedby={`recovery-name-${item.id}`}
              onClick={() => act(item.id, "instruction")}
            >
              {t("settings.history.assistant.copyInstruction")}
            </Button>
            <Button
              variant="secondary"
              aria-disabled={busy !== null}
              aria-describedby={`recovery-name-${item.id}`}
              onClick={() => act(item.id)}
            >
              {t("common.dismiss")}
            </Button>
          </div>
          <p
            role="status"
            aria-live="polite"
            aria-atomic="true"
            className="text-sm"
          >
            {copied?.id === item.id
              ? t(
                  copied.part === "generated"
                    ? "settings.history.assistant.generatedCopied"
                    : "settings.history.assistant.instructionCopied",
                )
              : ""}
          </p>
        </article>
      ))}
    </section>
  );
};
