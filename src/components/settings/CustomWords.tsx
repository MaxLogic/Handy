import React, { useId, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands } from "@/bindings";
import { useSettings } from "../../hooks/useSettings";
import { Input } from "../ui/Input";
import { Button } from "../ui/Button";
import { SettingContainer } from "../ui/SettingContainer";

interface CustomWordsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

const normalizeCustomWord = (word: string) =>
  word
    .replace(/[<>"']/g, "")
    .replace(/\s+/g, " ")
    .trim();

type CustomWordsError = { source: "add" | "open"; message: string };

export const CustomWords: React.FC<CustomWordsProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, refreshSettings } = useSettings();
    const [newWord, setNewWord] = useState("");
    const [error, setError] = useState<CustomWordsError | null>(null);
    const busy = useRef(false);
    const inputRef = useRef<HTMLInputElement>(null);
    const wordRefs = useRef(new Map<string, HTMLButtonElement>());
    const focusedWord = useRef<string | null>(null);
    const customWords = getSetting("custom_words") || [];
    const previousWords = useRef(customWords);
    const normalizedWord = normalizeCustomWord(newWord);
    const helpId = useId();
    const errorId = useId();

    // A word can disappear while its button has focus: removed here or in
    // custom_words.txt. Move focus to a neighbouring word (or the input) so
    // it never falls to the page body.
    useLayoutEffect(() => {
      const previous = previousWords.current;
      previousWords.current = customWords;
      const focused = focusedWord.current;
      if (!focused || customWords.includes(focused)) return;
      focusedWord.current = null;
      const active = document.activeElement;
      if (active && active !== document.body) return;
      const index = previous.indexOf(focused);
      const target =
        previous.slice(index + 1).find((w) => customWords.includes(w)) ??
        previous
          .slice(0, Math.max(index, 0))
          .reverse()
          .find((w) => customWords.includes(w));
      (target ? wordRefs.current.get(target) : inputRef.current)?.focus();
    }, [customWords]);

    const errorMessage = (code: string) => {
      if (code === "comment_prefix")
        return t("settings.advanced.customWords.commentPrefix");
      if (code === "file_unreadable")
        return t("settings.advanced.customWords.fileUnreadable");
      return t("settings.advanced.customWords.saveFailed", { error: code });
    };

    const run = async (
      action: () => Promise<
        { status: "ok" } | { status: "error"; error: string }
      >,
    ) => {
      if (busy.current) return false;
      busy.current = true;
      try {
        const result = await action();
        if (result.status === "error") {
          setError({ source: "add", message: errorMessage(result.error) });
          return false;
        }
        setError(null);
        await refreshSettings();
        return true;
      } catch (e) {
        setError({ source: "add", message: errorMessage(String(e)) });
        return false;
      } finally {
        busy.current = false;
      }
    };

    const handleAddWord = async () => {
      if (!normalizedWord || normalizedWord.length > 50) return;
      if (customWords.includes(normalizedWord)) {
        setError({
          source: "add",
          message: t("settings.advanced.customWords.duplicate", {
            word: normalizedWord,
          }),
        });
        return;
      }
      if (await run(() => commands.addCustomWord(normalizedWord))) {
        setNewWord("");
      }
      // The Add button disables once the field is empty; keep focus in the
      // field so it never falls to the page body.
      inputRef.current?.focus();
    };

    const handleRemoveWord = (wordToRemove: string) =>
      run(() => commands.removeCustomWord(wordToRemove));

    const handleOpenFile = async () => {
      try {
        const result = await commands.openCustomWordsFile();
        if (result.status === "error") {
          setError({
            source: "open",
            message: t("settings.advanced.customWords.openFailed", {
              error: result.error,
            }),
          });
        } else if (error?.source === "open") {
          setError(null);
        }
      } catch (e) {
        setError({
          source: "open",
          message: t("settings.advanced.customWords.openFailed", {
            error: String(e),
          }),
        });
      }
    };

    const handleKeyPress = (e: React.KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        void handleAddWord();
      }
    };

    return (
      <>
        <SettingContainer
          title={t("settings.advanced.customWords.title")}
          description={t("settings.advanced.customWords.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        >
          <div className="flex items-center gap-2">
            <Input
              ref={inputRef}
              type="text"
              className="max-w-40"
              value={newWord}
              onChange={(e) => {
                setNewWord(e.target.value);
                if (error?.source === "add") setError(null);
              }}
              onKeyDown={handleKeyPress}
              placeholder={t("settings.advanced.customWords.placeholder")}
              aria-label={t("settings.advanced.customWords.inputLabel")}
              aria-invalid={error?.source === "add" ? true : undefined}
              aria-describedby={error?.source === "add" ? errorId : undefined}
              variant="compact"
            />
            <Button
              onClick={() => void handleAddWord()}
              disabled={!normalizedWord || normalizedWord.length > 50}
              variant="primary"
              size="md"
            >
              {t("settings.advanced.customWords.add")}
            </Button>
          </div>
        </SettingContainer>
        <div>
          <p
            id={errorId}
            role="alert"
            className={error ? "px-4 pb-2 text-sm text-red-600" : "sr-only"}
          >
            {error?.message}
          </p>
          {customWords.length > 0 && (
            <div
              className={`px-4 p-2 ${grouped ? "" : "rounded-lg border border-mid-gray/20"} flex flex-wrap gap-1`}
            >
              {customWords.map((word) => (
                <Button
                  key={word}
                  ref={(element) => {
                    if (element) wordRefs.current.set(word, element);
                    else wordRefs.current.delete(word);
                  }}
                  onClick={() => void handleRemoveWord(word)}
                  onFocus={() => {
                    focusedWord.current = word;
                  }}
                  onBlur={(e) => {
                    if (e.relatedTarget) focusedWord.current = null;
                  }}
                  variant="secondary"
                  size="sm"
                  className="inline-flex items-center gap-1 cursor-pointer"
                  aria-label={t("settings.advanced.customWords.remove", {
                    word,
                  })}
                >
                  <span>{word}</span>
                  <svg
                    className="w-3 h-3"
                    fill="none"
                    stroke="currentColor"
                    viewBox="0 0 24 24"
                    aria-hidden="true"
                  >
                    <path
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      strokeWidth={2}
                      d="M6 18L18 6M6 6l12 12"
                    />
                  </svg>
                </Button>
              ))}
            </div>
          )}
          <div className="px-4 pb-2 flex items-start gap-2">
            <p id={helpId} className="text-xs text-mid-gray flex-1">
              {t("settings.advanced.customWords.fileHelp")}
            </p>
            <Button
              onClick={() => void handleOpenFile()}
              variant="secondary"
              size="sm"
              aria-describedby={
                error?.source === "open" ? `${helpId} ${errorId}` : helpId
              }
            >
              {t("settings.advanced.customWords.openFile")}
            </Button>
          </div>
        </div>
      </>
    );
  },
);
