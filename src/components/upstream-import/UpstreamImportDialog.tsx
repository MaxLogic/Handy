import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { relaunch } from "@tauri-apps/plugin-process";
import {
  commands,
  type UpstreamImportOffer,
  type UpstreamImportReport,
} from "@/bindings";
import { Button } from "../ui/Button";
import { Dialog } from "../ui/Dialog";

type Phase = "models" | "history" | "settings";

interface ImportProgress {
  phase: Phase;
  copied_bytes: number;
  total_bytes: number;
}

type Stage =
  | { kind: "offer" }
  | { kind: "importing"; cancelling: boolean }
  | { kind: "cancelled" }
  | { kind: "done"; report: UpstreamImportReport }
  | { kind: "error"; message: string };

interface UpstreamImportDialogProps {
  offer: UpstreamImportOffer;
  /**
   * Called after Not now or Don't import; the app continues its normal start
   * without offering the import again in this session.
   */
  onClose: () => void;
}

const UNITS = ["byte", "kilobyte", "megabyte", "gigabyte", "terabyte"];

export const formatBytes = (bytes: number, language: string) => {
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return new Intl.NumberFormat(language, {
    style: "unit",
    unit: UNITS[unit],
    unitDisplay: "short",
    maximumFractionDigits: unit === 0 ? 0 : 1,
  }).format(value);
};

const k = (key: string) => `upstreamImport.${key}`;

/**
 * Offer to copy upstream Handy data into Handy MaxLogic. The dialog cannot be
 * dismissed with Escape: the user makes an explicit choice. Don't import and a
 * clean import are recorded by the backend, so the question is not asked
 * again; Not now and an import with failures ask again next start. A choice
 * that would replace the fork's own settings or history starts unchecked, so a
 * repeated offer never overwrites them with a single Import press.
 */
export const UpstreamImportDialog: React.FC<UpstreamImportDialogProps> = ({
  offer,
  onClose,
}) => {
  const { t, i18n } = useTranslation();
  const size = (bytes: number) => formatBytes(bytes, i18n.language);
  const [choice, setChoice] = useState({
    settings: offer.settings_available && !offer.replaces_settings,
    models: offer.model_count > 0,
    history: false,
  });
  const [stage, setStage] = useState<Stage>({ kind: "offer" });
  const [progress, setProgress] = useState<ImportProgress | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const restartRef = useRef<HTMLButtonElement>(null);
  const retryRef = useRef<HTMLButtonElement>(null);

  const busy = stage.kind === "importing";
  // The Import / Not now / Don't import choice is on screen.
  const choosing = !busy && stage.kind !== "done";
  const selectedBytes =
    (choice.models ? offer.model_bytes : 0) +
    (choice.history ? offer.history_bytes : 0);
  const nothingSelected = !choice.settings && !choice.models && !choice.history;

  useEffect(() => {
    const unlisten = listen<ImportProgress>(
      "upstream-import-progress",
      (event) => setProgress(event.payload),
    );
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // Keep keyboard focus on the action that replaced the pressed button.
  useEffect(() => {
    const target =
      stage.kind === "importing"
        ? cancelRef
        : stage.kind === "done"
          ? restartRef
          : stage.kind === "cancelled" || stage.kind === "error"
            ? retryRef
            : null;
    target?.current?.focus();
  }, [stage.kind]);

  const startImport = async () => {
    if (busy || nothingSelected) return;
    setActionError(null);
    setProgress(null);
    setStage({ kind: "importing", cancelling: false });
    const result = await commands.runUpstreamImport(choice);
    if (result.status === "error") {
      setStage({ kind: "error", message: result.error });
    } else if (result.data.cancelled) {
      setStage({ kind: "cancelled" });
    } else {
      setStage({ kind: "done", report: result.data });
    }
  };

  const cancelImport = async () => {
    if (stage.kind !== "importing" || stage.cancelling) return;
    setStage({ kind: "importing", cancelling: true });
    await commands.cancelUpstreamImport();
  };

  const decline = async () => {
    if (busy) return;
    setActionError(null);
    const result = await commands.declineUpstreamImport();
    if (result.status === "error") {
      setActionError(t(k("declineFailed"), { error: result.error }));
      return;
    }
    onClose();
  };

  const notNow = () => {
    if (!busy) onClose();
  };

  const restart = async () => {
    setActionError(null);
    try {
      await relaunch();
    } catch (error) {
      setActionError(t(k("restartFailed"), { error: String(error) }));
    }
  };

  const toggle = (key: keyof typeof choice) => (checked: boolean) => {
    if (!busy) setChoice((current) => ({ ...current, [key]: checked }));
  };

  const checkbox = (
    key: keyof typeof choice,
    label: string,
    available: boolean,
    note?: string,
  ) =>
    available && (
      <div className="flex items-start gap-2">
        <input
          id={`upstream-import-${key}`}
          type="checkbox"
          checked={choice[key]}
          aria-disabled={busy || stage.kind === "done"}
          aria-describedby={note ? `upstream-import-${key}-note` : undefined}
          onClick={(event) => {
            if (busy || stage.kind === "done") event.preventDefault();
          }}
          onChange={(event) => toggle(key)(event.target.checked)}
          className="mt-1 accent-logo-primary focus-visible:ring-2 focus-visible:ring-logo-primary"
        />
        <div>
          <label htmlFor={`upstream-import-${key}`} className="text-sm">
            {label}
          </label>
          {/* A data-loss warning: full text color, not secondary gray. */}
          {note && (
            <p id={`upstream-import-${key}-note`} className="text-sm">
              {note}
            </p>
          )}
        </div>
      </div>
    );

  const report = stage.kind === "done" ? stage.report : null;
  const problems = report?.failures.length ?? 0;

  const phaseText = (() => {
    if (stage.kind === "importing") {
      if (stage.cancelling) return t(k("cancelling"));
      return t(k(`phase.${progress?.phase ?? "models"}`));
    }
    if (stage.kind === "done") {
      return problems > 0
        ? t(k("finishedWithProblems"), { count: problems })
        : t(k("finished"));
    }
    if (stage.kind === "cancelled") return t(k("cancelled"));
    return "";
  })();

  const importDescription =
    [
      // After a whole-run failure, focus lands on Try again: the error first.
      stage.kind === "error" ? "upstream-import-error" : null,
      nothingSelected
        ? "upstream-import-nothing"
        : selectedBytes > 0
          ? "upstream-import-total"
          : null,
    ]
      .filter(Boolean)
      .join(" ") || undefined;

  const footer = (() => {
    if (stage.kind === "importing") {
      return (
        <Button
          ref={cancelRef}
          variant="secondary"
          aria-disabled={stage.cancelling}
          onClick={() => void cancelImport()}
        >
          {t(k("cancel"))}
        </Button>
      );
    }
    if (stage.kind === "done") {
      return (
        <Button
          ref={restartRef}
          aria-describedby="upstream-import-result"
          onClick={() => void restart()}
        >
          {t(k("restart"))}
        </Button>
      );
    }
    const retry = stage.kind === "cancelled" || stage.kind === "error";
    // Tab order: the primary action, then the non-final Not now, and the
    // final Don't import last, so it is never the first button reached.
    return (
      <>
        <Button
          ref={retryRef}
          aria-disabled={nothingSelected}
          aria-describedby={importDescription}
          onClick={() => void startImport()}
        >
          {retry ? t(k("tryAgain")) : t(k("import"))}
        </Button>
        <Button
          variant="secondary"
          aria-describedby="upstream-import-not-now-note"
          onClick={notNow}
        >
          {t(k("notNow"))}
        </Button>
        <Button
          variant="secondary"
          aria-describedby="upstream-import-decline-note"
          onClick={() => void decline()}
        >
          {t(k("decline"))}
        </Button>
      </>
    );
  })();

  return (
    <Dialog
      open
      title={t(k("title"))}
      description={
        // The shared-shortcut warning is read once with the dialog.
        <>
          {t(k("description"))}{" "}
          <span className="mt-1 block">{t(k("shortcutsNote"))}</span>
        </>
      }
      onOpenChange={() => undefined}
      closeLabel={t(k("decline"))}
      dismissible={false}
      footer={footer}
    >
      <div className="space-y-3 text-sm">
        <fieldset className="space-y-2">
          <legend className="font-semibold">{t(k("choicesLegend"))}</legend>
          {checkbox(
            "settings",
            t(k("settings")),
            offer.settings_available,
            offer.replaces_settings ? t(k("replacesSettings")) : undefined,
          )}
          {checkbox(
            "models",
            t(k("models"), {
              count: offer.model_count,
              size: size(offer.model_bytes),
            }),
            offer.model_count > 0,
          )}
          {checkbox(
            "history",
            t(k("history"), { size: size(offer.history_bytes) }),
            offer.history_available,
            offer.replaces_history ? t(k("replacesHistory")) : undefined,
          )}
        </fieldset>
        {selectedBytes > 0 && (
          <p id="upstream-import-total">
            {t(k("total"), { size: size(selectedBytes) })}
          </p>
        )}
        <p className="break-all text-mid-gray">
          {t(k("source"), { path: offer.source_dir })}
        </p>
        <p className="break-all text-mid-gray">
          {t(k("destination"), { path: offer.destination_dir })}
        </p>
        {choosing && (
          <>
            <p id="upstream-import-not-now-note" className="text-mid-gray">
              {t(k("notNowNote"))}
            </p>
            <p
              id="upstream-import-decline-note"
              className="break-words text-mid-gray"
            >
              {t(k("declineNote"), { path: offer.source_dir })}
            </p>
          </>
        )}
        {nothingSelected && stage.kind !== "done" && (
          <p id="upstream-import-nothing" className="text-mid-gray">
            {t(k("nothingSelected"))}
          </p>
        )}
        {stage.kind === "importing" && (
          <div className="space-y-1">
            <progress
              className="w-full accent-logo-primary"
              aria-label={t(k("progressLabel"))}
              value={progress?.copied_bytes ?? 0}
              max={Math.max(progress?.total_bytes ?? 0, 1)}
            />
            <p className="tabular-nums text-mid-gray">
              {t(k("progressText"), {
                copied: size(progress?.copied_bytes ?? 0),
                total: size(progress?.total_bytes ?? selectedBytes),
              })}
            </p>
          </div>
        )}
        <p role="status" aria-live="polite" aria-atomic="true">
          {phaseText}
        </p>
        {report && (
          <div id="upstream-import-result" className="space-y-2">
            <ul className="list-disc ps-5">
              {report.settings_imported && <li>{t(k("resultSettings"))}</li>}
              {choice.models && (
                <li>
                  {t(k("resultModelsCopied"), { count: report.models_copied })}
                </li>
              )}
              {report.models_skipped > 0 && (
                <li>
                  {t(k("resultModelsKept"), { count: report.models_skipped })}
                </li>
              )}
              {report.history_imported && <li>{t(k("resultHistory"))}</li>}
              {report.skipped_links.length > 0 && (
                <li className="break-all">
                  {t(k("resultLinksSkipped"), {
                    count: report.skipped_links.length,
                    items: report.skipped_links.join(", "),
                  })}
                </li>
              )}
            </ul>
            {/* Not a live region: the status announces "finished with N
                problems", and Restart now, which gets focus, is described by
                this whole summary. */}
            {problems > 0 && (
              <div className="space-y-1 text-error">
                <p>{t(k("failuresTitle"), { count: problems })}</p>
                <ul className="list-disc ps-5 break-all">
                  {report.failures.map((failure) => (
                    <li key={failure.item}>
                      {t(k("failure"), {
                        item: failure.item,
                        error: failure.error,
                      })}
                    </li>
                  ))}
                </ul>
                <p className="break-words">
                  {t(k("recovery"), {
                    source: offer.source_dir,
                    destination: offer.destination_dir,
                  })}
                </p>
              </div>
            )}
            <p>{t(k("restartNote"))}</p>
          </div>
        )}
        {/* Not an alert: Try again, which gets focus, is described by it. */}
        {stage.kind === "error" && (
          <p id="upstream-import-error" className="text-error">
            {t(k("failed"), { error: stage.message })}
          </p>
        )}
        {actionError && (
          <p role="alert" className="text-error">
            {actionError}
          </p>
        )}
      </div>
    </Dialog>
  );
};
