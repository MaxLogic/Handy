import React, { useEffect, useState, useRef, useCallback } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { formatKeyCombination } from "../../lib/utils/keyboard";
import { ResetButton } from "../ui/ResetButton";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";
import { commands } from "@/bindings";
import { toast } from "sonner";
import { openUrl } from "@tauri-apps/plugin-opener";
import { SECURE_INPUT_HELP_URL } from "../SecureInputWarning";

interface HandyKeysShortcutInputProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  shortcutId: string;
  disabled?: boolean;
}

interface HandyKeysEvent {
  modifiers: string[];
  key: string | null;
  is_key_down: boolean;
  hotkey_string: string;
}

export const HandyKeysShortcutInput: React.FC<HandyKeysShortcutInputProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
  shortcutId,
  disabled = false,
}) => {
  const { t } = useTranslation();
  const translateRef = useRef(t);
  translateRef.current = t;
  const { getSetting, updateBinding, resetBinding, isUpdating, isLoading } =
    useSettings();
  const [isRecording, setIsRecording] = useState(false);
  const [currentKeys, setCurrentKeys] = useState<string>("");
  const [originalBinding, setOriginalBinding] = useState<string>("");
  const shortcutRef = useRef<HTMLButtonElement | null>(null);
  const captureRef = useRef<HTMLDivElement>(null);
  const stoppingRef = useRef(false);
  const navigationRef = useRef(false);
  const recordedKeysRef = useRef(new Set<string>());
  const unlistenRef = useRef<(() => void) | null>(null);
  // Use a ref to track currentKeys for the event handler (avoids stale closure)
  const currentKeysRef = useRef<string>("");
  // Track keyed vs modifier-only captures separately so a combo commits only
  // on its key's release and a modifier-only shortcut only once every
  // modifier is released. Committing on the *first* release (the old
  // behavior) silently saved just the modifier whenever the key event never
  // arrived — e.g. while macOS Secure Input is active (issue #1578).
  const keyedShortcutRef = useRef<string>("");
  const modifierOnlyShortcutRef = useRef<string>("");
  const osType = useOsType();
  const [shortcutError, setShortcutError] = useState("");

  const bindings = getSetting("bindings") || {};

  // The session owns its one matching release, including a start still in flight.
  const captureSessionRef = useRef<{
    start: Promise<boolean>;
    released: boolean;
  } | null>(null);
  const mountedRef = useRef(true);
  const releaseCapture = useCallback(async () => {
    const session = captureSessionRef.current;
    if (!session || session.released) return;
    session.released = true;
    unlistenRef.current?.();
    unlistenRef.current = null;
    try {
      const started = await session.start;
      if (!started) return;
      const result = await commands.stopHandyKeysRecording();
      if (result.status === "error") throw new Error(result.error);
    } catch (error) {
      console.error("Failed to restore shortcut bindings:", error);
      const message = translateRef.current(
        "settings.general.shortcut.errors.set",
        {
          error: String(error),
        },
      );
      if (mountedRef.current) setShortcutError(message);
      else toast.error(message);
    } finally {
      if (captureSessionRef.current === session)
        captureSessionRef.current = null;
    }
  }, []);

  // Handle cancellation
  const cancelRecording = useCallback(
    async (restoreFocus = true) => {
      if (!captureSessionRef.current) return;
      stoppingRef.current = true;
      setIsRecording(false);
      if (restoreFocus) shortcutRef.current?.focus();

      // Stop listening for backend events
      if (unlistenRef.current) {
        unlistenRef.current();
        unlistenRef.current = null;
      }

      // Stop backend recording
      await releaseCapture();

      setIsRecording(false);
      setCurrentKeys("");
      currentKeysRef.current = "";
      keyedShortcutRef.current = "";
      modifierOnlyShortcutRef.current = "";
      recordedKeysRef.current.clear();
      setOriginalBinding("");
    },
    [releaseCapture],
  );

  // Set up event listener for handy-keys events
  useEffect(() => {
    mountedRef.current = true;
    const handleWindowBlur = () => {
      void cancelRecording(false);
    };
    window.addEventListener("blur", handleWindowBlur);
    return () => {
      mountedRef.current = false;
      window.removeEventListener("blur", handleWindowBlur);
      void releaseCapture();
    };
  }, [releaseCapture, cancelRecording]);

  useEffect(() => {
    if (!isRecording) return;

    let cleanup = false;

    const setupListener = async () => {
      // Listen for key events from backend
      const commitAndStop = async (keysToCommit: string) => {
        if (stoppingRef.current) return;
        stoppingRef.current = true;
        try {
          if (
            ["enter", "return", "escape", "esc", "tab"].includes(
              keysToCommit.toLowerCase(),
            )
          ) {
            setShortcutError(
              t("settings.general.shortcut.errors.navigationKey"),
            );
          } else {
            await updateBinding(shortcutId, keysToCommit);
          }
        } catch (error) {
          console.error("Failed to change binding:", error);
          setShortcutError(
            t("settings.general.shortcut.errors.set", { error: String(error) }),
          );
          toast.error(
            t("settings.general.shortcut.errors.set", {
              error: String(error),
            }),
          );

          // Reset to original binding on error
          if (originalBinding) {
            try {
              await updateBinding(shortcutId, originalBinding);
            } catch (resetError) {
              console.error("Failed to reset binding:", resetError);
              toast.error(t("settings.general.shortcut.errors.reset"));
            }
          }
        }

        // Stop recording
        if (unlistenRef.current) {
          unlistenRef.current();
          unlistenRef.current = null;
        }
        await releaseCapture();
        setIsRecording(false);
        setCurrentKeys("");
        currentKeysRef.current = "";
        keyedShortcutRef.current = "";
        modifierOnlyShortcutRef.current = "";
        recordedKeysRef.current.clear();
        setOriginalBinding("");
        requestAnimationFrame(() => {
          if (
            mountedRef.current &&
            captureRef.current?.contains(document.activeElement)
          )
            shortcutRef.current?.focus();
        });
      };

      const unlisten = await listen<HandyKeysEvent>(
        "handy-keys-event",
        async (event) => {
          if (cleanup || stoppingRef.current) return;

          const { hotkey_string, is_key_down, key, modifiers } = event.payload;

          if (
            is_key_down &&
            ["escape", "esc"].includes(key?.toLowerCase() ?? "") &&
            modifiers.length === 0
          ) {
            await cancelRecording();
            return;
          }
          if (
            is_key_down &&
            key?.toLowerCase() === "tab" &&
            modifiers.every((modifier) =>
              ["shift", "shift_left", "shift_right"].includes(
                modifier.toLowerCase(),
              ),
            )
          ) {
            navigationRef.current = true;
            keyedShortcutRef.current = "";
            modifierOnlyShortcutRef.current = "";
            currentKeysRef.current = "";
            setCurrentKeys("");
            return;
          }
          if (document.activeElement !== shortcutRef.current) return;
          if (navigationRef.current && !is_key_down) return;
          if (is_key_down) navigationRef.current = false;

          // The native hook can deliver the Change activation's release even
          // though its press preceded this session. Only accept matching presses.
          if (key) {
            const recordedKey = key.toLowerCase();
            if (is_key_down) recordedKeysRef.current.add(recordedKey);
            else if (!recordedKeysRef.current.delete(recordedKey)) return;
          }

          if (is_key_down && hotkey_string) {
            // Update both state (for display) and refs (for release handler)
            if (key) {
              keyedShortcutRef.current = hotkey_string;
            } else {
              modifierOnlyShortcutRef.current = hotkey_string;
            }
            currentKeysRef.current = hotkey_string;
            setCurrentKeys(hotkey_string);
          } else if (!is_key_down && key) {
            // The recorded main key was released — commit the keyed combo.
            // Never fall back to a
            // modifier-only capture here: that's how bindings used to get
            // silently overwritten with just the modifier (issue #1578).
            const keysToCommit = keyedShortcutRef.current || hotkey_string;
            if (keysToCommit) {
              await commitAndStop(keysToCommit);
            }
          } else if (
            !is_key_down &&
            !key &&
            modifiers.length === 0 &&
            !keyedShortcutRef.current &&
            modifierOnlyShortcutRef.current
          ) {
            // Every modifier released without a main key ever going down —
            // commit as a modifier-only shortcut
            await commitAndStop(modifierOnlyShortcutRef.current);
          }
        },
      );

      if (cleanup || stoppingRef.current) unlisten();
      else unlistenRef.current = unlisten;
    };

    setupListener();

    return () => {
      cleanup = true;
      if (unlistenRef.current) {
        unlistenRef.current();
        unlistenRef.current = null;
      }
      // The session lifetime cleanup owns the matching backend stop.
    };
  }, [
    isRecording,
    shortcutId,
    originalBinding,
    updateBinding,
    cancelRecording,
    t,
    releaseCapture,
  ]);

  // Handle click outside
  useEffect(() => {
    if (!isRecording) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if (
        event.key === "Escape" &&
        !event.ctrlKey &&
        !event.altKey &&
        !event.metaKey &&
        !event.shiftKey
      ) {
        event.preventDefault();
        void cancelRecording();
      } else if (
        event.key === "Tab" &&
        !event.ctrlKey &&
        !event.altKey &&
        !event.metaKey
      ) {
        navigationRef.current = true;
        keyedShortcutRef.current = "";
        modifierOnlyShortcutRef.current = "";
        currentKeysRef.current = "";
        setCurrentKeys("");
      }
    };

    const handleClickOutside = (e: MouseEvent) => {
      if (
        captureRef.current &&
        !captureRef.current.contains(e.target as Node)
      ) {
        void cancelRecording(false);
      }
    };

    window.addEventListener("click", handleClickOutside);
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.removeEventListener("click", handleClickOutside);
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, [isRecording, cancelRecording]);

  // Start recording a new shortcut
  const startRecording = async () => {
    setShortcutError("");
    if (
      captureSessionRef.current ||
      isRecording ||
      disabled ||
      isUpdating(`binding_${shortcutId}`)
    )
      return;

    // Store the original binding to restore if canceled
    setOriginalBinding(bindings[shortcutId]?.current_binding || "");

    // Start backend recording. The backend refuses while macOS Secure Input
    // is active (the recorder's listener would receive no key events and
    // capture just the modifier) — it also flips the warning banner on, so
    // the toast points at a visible explanation.
    const start = commands.startHandyKeysRecording(shortcutId);
    const session = {
      start: start.then(
        (result) => result.status === "ok",
        () => false,
      ),
      released: false,
    };
    captureSessionRef.current = session;
    try {
      const result = await start;
      if (result.status === "error") {
        await releaseCapture();
        if (String(result.error).includes("secure-input-active")) {
          toast.error(t("secureInput.recorderBlocked"), {
            action: {
              label: t("secureInput.learnMore"),
              onClick: () => openUrl(SECURE_INPUT_HELP_URL),
            },
          });
        } else {
          toast.error(
            t("settings.general.shortcut.errors.set", {
              error: String(result.error),
            }),
          );
        }
        return;
      }
      if (
        !mountedRef.current ||
        session.released ||
        !captureRef.current?.contains(document.activeElement)
      ) {
        await releaseCapture();
        return;
      }
      setIsRecording(true);
      stoppingRef.current = false;
      navigationRef.current = false;
      setCurrentKeys("");
      currentKeysRef.current = "";
      keyedShortcutRef.current = "";
      modifierOnlyShortcutRef.current = "";
      recordedKeysRef.current.clear();
    } catch (error) {
      await releaseCapture();
      console.error("Failed to start recording:", error);
      toast.error(
        t("settings.general.shortcut.errors.set", { error: String(error) }),
      );
    }
  };

  // Format the current shortcut keys being recorded
  const formatCurrentKeys = (): string => {
    if (!currentKeys) return t("settings.general.shortcut.pressKeys");
    return formatKeyCombination(currentKeys, osType);
  };

  // If still loading, show loading state
  if (isLoading) {
    return (
      <SettingContainer
        title={t("settings.general.shortcut.title")}
        description={t("settings.general.shortcut.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="text-sm text-mid-gray">
          {t("settings.general.shortcut.loading")}
        </div>
      </SettingContainer>
    );
  }

  // If no bindings are loaded, show empty state
  if (Object.keys(bindings).length === 0) {
    return (
      <SettingContainer
        title={t("settings.general.shortcut.title")}
        description={t("settings.general.shortcut.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="text-sm text-mid-gray">
          {t("settings.general.shortcut.none")}
        </div>
      </SettingContainer>
    );
  }

  const binding = bindings[shortcutId];
  if (!binding) {
    return (
      <SettingContainer
        title={t("settings.general.shortcut.title")}
        description={t("settings.general.shortcut.notFound")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="text-sm text-mid-gray">
          {t("settings.general.shortcut.none")}
        </div>
      </SettingContainer>
    );
  }

  // Get translated name and description for the binding
  const translatedName = t(
    `settings.general.shortcut.bindings.${shortcutId}.name`,
    binding.name,
  );
  const translatedDescription = t(
    `settings.general.shortcut.bindings.${shortcutId}.description`,
    binding.description,
  );

  return (
    <SettingContainer
      title={translatedName}
      description={translatedDescription}
      descriptionMode={descriptionMode}
      grouped={grouped}
      disabled={disabled}
      layout="horizontal"
    >
      <div
        ref={captureRef}
        className="flex items-center space-x-1"
        onBlur={(event) => {
          if (!event.currentTarget.contains(event.relatedTarget as Node | null))
            void cancelRecording(false);
        }}
      >
        <button
          type="button"
          ref={shortcutRef}
          aria-label={t("settings.general.shortcut.change", {
            name: translatedName,
          })}
          aria-pressed={isRecording}
          aria-describedby={`shortcut-${shortcutId}-value`}
          className="px-2 py-1 text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 rounded-md cursor-pointer hover:border-logo-primary focus-visible:ring-2 focus-visible:ring-logo-primary"
          disabled={disabled}
          aria-disabled={isUpdating(`binding_${shortcutId}`)}
          onClick={startRecording}
        >
          <span
            id={`shortcut-${shortcutId}-value`}
            role="status"
            aria-live="polite"
            aria-atomic="true"
          >
            {isRecording
              ? formatCurrentKeys()
              : formatKeyCombination(binding.current_binding, osType)}
          </span>
        </button>
        {isRecording && (
          <button
            type="button"
            className="rounded px-2 py-1 focus-visible:ring-2 focus-visible:ring-logo-primary"
            onClick={() => cancelRecording()}
          >
            {t("settings.postProcessing.prompts.cancel")}
          </button>
        )}
        <ResetButton
          onClick={() => resetBinding(shortcutId)}
          disabled={disabled || isUpdating(`binding_${shortcutId}`)}
          ariaLabel={t("settings.general.shortcut.reset", {
            name: translatedName,
          })}
        />
      </div>
      {shortcutError && (
        <p role="alert" className="text-sm text-red-500">
          {shortcutError}
        </p>
      )}
    </SettingContainer>
  );
};
