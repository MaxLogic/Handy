import React, { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { events } from "@/bindings";
import { Button } from "../ui/Button";

export const WorkflowNotice: React.FC<{
  onHistory: () => void;
  onDismiss: () => void;
}> = ({ onHistory, onDismiss }) => {
  const { t } = useTranslation();
  const [message, setMessage] = useState<string | null>(null);
  const [hasHistory, setHasHistory] = useState(true);
  useEffect(() => {
    const subscriptions = [
      listen<{ history_available: boolean; raw_delivered: boolean }>(
        "cleanup-fallback",
        ({ payload }) => {
          // Null is accepted for notices retained from the previous event shape.
          const history = payload?.history_available ?? true;
          setHasHistory(history);
          setMessage(
            t(
              payload?.raw_delivered === false
                ? history
                  ? "errors.cleanupFallbackHistory"
                  : "errors.cleanupFallbackNotDelivered"
                : history
                  ? "errors.cleanupFallback"
                  : "errors.cleanupFallbackNoHistory",
            ),
          );
        },
      ),
      listen<{ cleanup_failed: boolean; raw_history_available: boolean }>(
        "paste-error",
        ({ payload }) => {
          if (payload?.cleanup_failed || payload?.raw_history_available) {
            setHasHistory(payload.raw_history_available);
            setMessage(
              t(
                payload.raw_history_available
                  ? "errors.cleanupPasteFailedHistory"
                  : "errors.cleanupPasteFailedNoHistory",
              ),
            );
          }
        },
      ),
      events.assistantErrorEvent.listen(({ payload }) => {
        setHasHistory(true);
        const remedies: Record<string, string> = {
          llm_context_exceeded: "errors.assistantTextTooLong",
          llm_config: "errors.assistantConfiguration",
          recovery_full: "errors.assistantRecoveryFull",
          selection_too_large: "errors.assistantSelectionTooLarge",
          password_field: "errors.assistantPasswordField",
          read_only: "errors.assistantReadOnly",
          busy: "errors.assistantBusy",
        };
        setMessage(t(remedies[payload.code] ?? "errors.assistantFailed"));
      }),
      events.assistantRecoveryUpdate.listen(({ payload }) => {
        if (payload.action === "added") {
          setHasHistory(true);
          setMessage(t("errors.assistantRecovery"));
        }
      }),
    ];
    return () => {
      subscriptions.forEach((subscription) => {
        void subscription.then((unlisten) => unlisten());
      });
    };
  }, [t]);
  if (!message) return null;
  return (
    <div className="flex flex-wrap items-center gap-3 rounded-md border border-warning/40 bg-warning/10 p-3 text-sm">
      <p role="alert" className="min-w-0 flex-1">
        {message}
      </p>
      {hasHistory && (
        <Button variant="secondary" onClick={onHistory}>
          {t("settings.history.title")}
        </Button>
      )}
      <Button
        variant="secondary"
        onClick={() => {
          onDismiss();
          setMessage(null);
        }}
      >
        {t("common.dismiss")}
      </Button>
    </div>
  );
};
