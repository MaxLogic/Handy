import React, { useEffect, useState, useRef, useCallback } from "react";
import { useTranslation } from "react-i18next";
import {
  getKeyName,
  formatKeyCombination,
  normalizeKey,
} from "../../lib/utils/keyboard";
import { ResetButton } from "../ui/ResetButton";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";
import { commands } from "@/bindings";
import { toast } from "sonner";

interface GlobalShortcutInputProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  shortcutId: string;
  disabled?: boolean;
}

export const GlobalShortcutInput: React.FC<GlobalShortcutInputProps> = ({
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
  const [keyPressed, setKeyPressed] = useState<string[]>([]);
  const [recordedKeys, setRecordedKeys] = useState<string[]>([]);
  const [editingShortcutId, setEditingShortcutId] = useState<string | null>(
    null,
  );
  const [originalBinding, setOriginalBinding] = useState<string>("");
  const shortcutRefs = useRef<Map<string, HTMLButtonElement | null>>(new Map());
  const committingRef = useRef(false);
  const captureRef = useRef<HTMLDivElement>(null);
  const navigationRef = useRef(false);
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
    try {
      const started = await session.start;
      // Even an interrupted suspend may need its matching resume.
      void started;
      const result = await commands.resumeAllBindings();
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

  const cancelRecording = useCallback(
    async (restoreFocus = true) => {
      if (!captureSessionRef.current) return;
      committingRef.current = true;
      setEditingShortcutId(null);
      setKeyPressed([]);
      setRecordedKeys([]);
      setOriginalBinding("");
      if (restoreFocus) shortcutRefs.current.get(shortcutId)?.focus();
      await releaseCapture();
    },
    [shortcutId, releaseCapture],
  );

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
    // Only add event listeners when we're in editing mode
    if (editingShortcutId === null) return;

    let cleanup = false;

    // Keyboard event listeners
    const handleKeyDown = async (e: KeyboardEvent) => {
      if (cleanup || committingRef.current) return;
      if (e.repeat) return; // ignore auto-repeat
      if (
        e.key === "Escape" &&
        !e.ctrlKey &&
        !e.altKey &&
        !e.metaKey &&
        !e.shiftKey
      ) {
        e.preventDefault();
        await cancelRecording();
        return;
      }
      if (e.key === "Tab" && !e.ctrlKey && !e.altKey && !e.metaKey) {
        // Let Tab reach Cancel. Clear Shift pressed before Shift+Tab so its
        // eventual release cannot save a modifier-only binding.
        navigationRef.current = true;
        setKeyPressed([]);
        setRecordedKeys([]);
        return;
      }
      if (
        document.activeElement !== shortcutRefs.current.get(editingShortcutId)
      )
        return;
      navigationRef.current = false;
      e.preventDefault();

      // Get the key with OS-specific naming and normalize it
      const rawKey = getKeyName(e, osType);
      const key = normalizeKey(rawKey);

      if (!keyPressed.includes(key)) {
        setKeyPressed((prev) => [...prev, key]);
        // Also add to recorded keys if not already there
        if (!recordedKeys.includes(key)) {
          setRecordedKeys((prev) => [...prev, key]);
        }
      }
    };

    const handleKeyUp = async (e: KeyboardEvent) => {
      // Navigation is decided on key-down. Modifiers may already be released
      // when the recorded Tab/Escape key comes up.
      if (cleanup || committingRef.current || navigationRef.current) return;
      if (
        document.activeElement !== shortcutRefs.current.get(editingShortcutId)
      )
        return;
      e.preventDefault();

      // Get the key with OS-specific naming and normalize it
      const rawKey = getKeyName(e, osType);
      const key = normalizeKey(rawKey);

      // Remove from currently pressed keys
      if (!recordedKeys.includes(key)) return;
      setKeyPressed((prev) => prev.filter((k) => k !== key));

      // If no keys are pressed anymore, commit the shortcut
      const updatedKeyPressed = keyPressed.filter((k) => k !== key);
      if (updatedKeyPressed.length === 0 && recordedKeys.length > 0) {
        if (committingRef.current) return;
        committingRef.current = true;
        // Create the shortcut string from all recorded keys
        // Sort keys so modifiers come first, then the main key
        const modifiers = [
          "ctrl",
          "control",
          "shift",
          "alt",
          "option",
          "meta",
          "command",
          "cmd",
          "super",
          "win",
          "windows",
        ];
        const sortedKeys = [...recordedKeys].sort((a, b) => {
          const aIsModifier = modifiers.includes(a.toLowerCase());
          const bIsModifier = modifiers.includes(b.toLowerCase());
          if (aIsModifier && !bIsModifier) return -1;
          if (!aIsModifier && bIsModifier) return 1;
          return 0;
        });
        const newShortcut = sortedKeys.join("+");

        if (editingShortcutId && bindings[editingShortcutId]) {
          try {
            if (
              ["enter", "escape", "tab"].includes(newShortcut.toLowerCase())
            ) {
              setShortcutError(
                t("settings.general.shortcut.errors.navigationKey"),
              );
            } else {
              await updateBinding(editingShortcutId, newShortcut);
            }
          } catch (error) {
            console.error("Failed to change binding:", error);
            setShortcutError(
              t("settings.general.shortcut.errors.set", {
                error: String(error),
              }),
            );
            toast.error(
              t("settings.general.shortcut.errors.set", {
                error: String(error),
              }),
            );

            // Reset to original binding on error
            if (originalBinding) {
              try {
                await updateBinding(editingShortcutId, originalBinding);
              } catch (resetError) {
                console.error("Failed to reset binding:", resetError);
                toast.error(t("settings.general.shortcut.errors.reset"));
              }
            }
          }

          // Re-register all bindings (the one just committed is already
          // registered; re-registering it fails cleanly and is ignored)
          await releaseCapture();

          // Exit editing mode and reset states
          setEditingShortcutId(null);
          setKeyPressed([]);
          setRecordedKeys([]);
          setOriginalBinding("");
          requestAnimationFrame(
            () =>
              mountedRef.current &&
              captureRef.current?.contains(document.activeElement) &&
              shortcutRefs.current.get(shortcutId)?.focus(),
          );
        }
      }
    };

    // Add click outside handler
    const handleClickOutside = async (e: MouseEvent) => {
      if (cleanup || committingRef.current) return;
      if (
        captureRef.current &&
        !captureRef.current.contains(e.target as Node)
      ) {
        await cancelRecording(false);
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    window.addEventListener("keyup", handleKeyUp);
    window.addEventListener("click", handleClickOutside);

    return () => {
      cleanup = true;
      window.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("keyup", handleKeyUp);
      window.removeEventListener("click", handleClickOutside);
    };
  }, [
    keyPressed,
    recordedKeys,
    editingShortcutId,
    bindings,
    originalBinding,
    updateBinding,
    osType,
    cancelRecording,
    t,
    shortcutId,
    releaseCapture,
  ]);

  // Start recording a new shortcut
  const startRecording = async (id: string) => {
    setShortcutError("");
    if (
      captureSessionRef.current ||
      editingShortcutId === id ||
      disabled ||
      isUpdating(`binding_${id}`)
    )
      return;

    // Suspend all bindings so no shortcut fires (or swallows the
    // keystrokes) while keys are being recorded
    const start = commands.suspendAllBindings();
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
      if (result.status === "error") throw new Error(result.error);
      if (
        !mountedRef.current ||
        session.released ||
        !captureRef.current?.contains(document.activeElement)
      ) {
        await releaseCapture();
        return;
      }
    } catch (error) {
      await releaseCapture();
      setShortcutError(
        t("settings.general.shortcut.errors.set", { error: String(error) }),
      );
      return;
    }

    // Store the original binding to restore if canceled
    setOriginalBinding(bindings[id]?.current_binding || "");
    committingRef.current = false;
    navigationRef.current = false;
    setEditingShortcutId(id);
    setKeyPressed([]);
    setRecordedKeys([]);
  };

  // Format the current shortcut keys being recorded
  const formatCurrentKeys = (): string => {
    if (recordedKeys.length === 0)
      return t("settings.general.shortcut.pressKeys");

    // Use the same formatting as the display to ensure consistency
    return formatKeyCombination(recordedKeys.join("+"), osType);
  };

  // Store references to shortcut elements
  const setShortcutRef = (id: string, ref: HTMLButtonElement | null) => {
    shortcutRefs.current.set(id, ref);
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
          ref={(ref) => setShortcutRef(shortcutId, ref)}
          aria-label={t("settings.general.shortcut.change", {
            name: translatedName,
          })}
          aria-pressed={editingShortcutId === shortcutId}
          aria-describedby={`shortcut-${shortcutId}-value`}
          className="px-2 py-1 text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 rounded-md cursor-pointer hover:border-logo-primary focus-visible:ring-2 focus-visible:ring-logo-primary"
          disabled={disabled}
          aria-disabled={isUpdating(`binding_${shortcutId}`)}
          onClick={() => startRecording(shortcutId)}
        >
          <span
            id={`shortcut-${shortcutId}-value`}
            role="status"
            aria-live="polite"
            aria-atomic="true"
          >
            {editingShortcutId === shortcutId
              ? formatCurrentKeys()
              : formatKeyCombination(binding.current_binding, osType)}
          </span>
        </button>
        {editingShortcutId === shortcutId && (
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
