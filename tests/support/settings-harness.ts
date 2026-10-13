import { expect, type Page, type TestInfo } from "@playwright/test";
import type {
  AppSettings,
  AssistantRecoveryItem,
  EnglishCleanupSettings,
  LlmRequestOptions,
  ProcessingProfile,
  ProcessingProfileInput,
} from "../../src/bindings";

export type HarnessWindow = Window & {
  __TAURI_INTERNALS__: Record<string, unknown>;
  __TAURI_OS_PLUGIN_INTERNALS__: Record<string, unknown>;
  __TAURI_EVENT_PLUGIN_INTERNALS__: Record<string, unknown>;
  emitTestEvent: (event: string, payload: unknown) => void;
  failCommand?: string;
  failCode?: string;
  commandArgs: { command: string; args: Record<string, unknown> }[];
  pendingCommand?: string;
  releaseCommand?: () => void;
  commandCalls: string[];
  /** Optional per-spec responder installed by an earlier init script. */
  commandHook?: (
    command: string,
    args: Record<string, unknown>,
  ) => { value: unknown } | { error: unknown } | undefined;
};

export async function screenshot(
  page: Page,
  _testInfo: TestInfo,
  name: string,
) {
  if (process.env.HANDY_UX_SCREENS !== "1") return;
  const directory =
    process.env.HANDY_UX_SCREENS_DIR ?? ".agents/slices/HANDY-010/screens";
  await page.screenshot({
    path: `${directory}/${name}.png`,
    fullPage: true,
  });
}

export async function setup(
  page: Page,
  keyboard = "tauri",
  overrides: AppSettings & { omitProfiles?: boolean } = {},
  openSection: string | null = "Post Process",
) {
  await page.addInitScript(
    ({ keyboard, overrides }) => {
      const w = window as unknown as HarnessWindow;
      w.commandCalls = [];
      w.commandArgs = [];
      const initial: AppSettings = {
        processing_profiles: [
          {
            id: "builtin_cleanup",
            name: "",
            kind: "dictation",
            prompt_id: "cleanup",
            model: null,
            speech_language: null,
            english_cleanup: {
              styling: "semi-formal",
              structure: "prose",
              context: "general",
            },
          },
          {
            id: "builtin_assistant",
            name: "",
            kind: "selection_command",
            prompt_id: "assistant",
            model: null,
            speech_language: null,
            english_cleanup: null,
          },
        ],
        onboarding_completed: true,
        app_language: "en",
        post_process_enabled: false,
        english_cleanup: { enabled: false, base_url: "", model: "" },
        keyboard_implementation: keyboard as "tauri" | "handy_keys",
        audio_feedback: true,
        theme: "light",
        overlay_style: "none",
        selected_model: "turbo",
        whats_new_last_seen_version: "0.9.8",
        bindings: {
          assistant: {
            id: "assistant",
            name: "Selected Text Assistant",
            description: "Assistant",
            current_binding: "ctrl+alt+space",
            default_binding: "ctrl+alt+space",
          },
          transcribe: {
            id: "transcribe",
            name: "Transcribe",
            description: "Raw",
            current_binding: "ctrl+space",
            default_binding: "ctrl+space",
          },
          transcribe_with_post_process: {
            id: "transcribe_with_post_process",
            name: "Cleanup",
            description: "Cleanup",
            current_binding: "ctrl+shift+space",
            default_binding: "ctrl+shift+space",
          },
        },
        post_process_provider_id: "custom",
        post_process_providers: [
          {
            id: "custom",
            label: "Custom",
            base_url: "http://localhost:11434/v1",
          },
        ],
        post_process_api_keys: { custom: "" },
        post_process_models: { custom: "old-model" },
        post_process_prompts: [
          {
            id: "cleanup",
            name: "Conservative Cleanup",
            prompt: "Clean the text.",
          },
          {
            id: "assistant",
            name: "Assistant",
            prompt: "Apply the instruction.",
          },
        ],
        assistant_selected_prompt_id: "assistant",
        post_process_selected_prompt_id: "cleanup",
        post_process_request_options: {
          temperature: 0,
          max_output_tokens: null,
          timeout_secs: 60,
          connect_timeout_secs: 5,
        },
      };
      const merged = { ...initial, ...overrides };
      if (overrides.omitProfiles) {
        delete merged.processing_profiles;
        delete merged.english_cleanup;
      }
      let settings: AppSettings = JSON.parse(
        localStorage.getItem("test-settings") ?? JSON.stringify(merged),
      );
      const callbacks: Record<number, (event: unknown) => void> = {};
      const listeners: Record<string, number[]> = {};
      let next = 1;
      const persist = () =>
        localStorage.setItem("test-settings", JSON.stringify(settings));
      w.emitTestEvent = (event, payload) => {
        (listeners[event] ?? []).forEach((id) =>
          callbacks[id]?.({ event, id, payload }),
        );
      };
      w.__TAURI_OS_PLUGIN_INTERNALS__ = {
        platform: "windows",
        os_type: "windows",
        family: "windows",
        arch: "x86_64",
        version: "11",
        eol: "\r\n",
      };
      w.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
        unregisterListener: (event: string, id: number) => {
          listeners[event] = (listeners[event] ?? []).filter(
            (value) => value !== id,
          );
        },
      };
      w.__TAURI_INTERNALS__ = {
        metadata: {
          currentWindow: { label: "main" },
          currentWebview: { label: "main" },
        },
        transformCallback: (callback: (event: unknown) => void) => {
          const id = next++;
          callbacks[id] = callback;
          return id;
        },
        unregisterCallback: (id: number) => {
          delete callbacks[id];
        },
        convertFileSrc: (path: string) => path,
        invoke: async (command: string, args: Record<string, unknown> = {}) => {
          w.commandCalls.push(command);
          w.commandArgs.push({ command, args });
          if (command === w.pendingCommand)
            await new Promise<void>((resolve) => {
              w.releaseCommand = resolve;
            });
          if (command === w.failCommand)
            throw w.failCode ?? "fixture rejected setting";
          const hooked = w.commandHook?.(command, args);
          if (hooked && "error" in hooked) throw hooked.error;
          if (hooked) return hooked.value;
          if (command === "plugin:event|listen") {
            const event = String(args.event);
            (listeners[event] ??= []).push(Number(args.handler));
            return args.handler;
          }
          if (command === "plugin:os|locale") return "en-US";
          if (command === "plugin:app|version") return "0.9.8";
          if (command === "get_app_settings") return settings;
          if (command === "get_default_settings") return initial;
          if (command === "get_windows_microphone_permission_status")
            return { supported: false, overall_access: "allowed" };
          if (command === "get_available_models") return [];
          if (command === "check_custom_sounds")
            return { start: false, stop: false };
          if (command === "is_update_checks_locked") return false;
          if (
            command === "get_available_microphones" ||
            command === "get_available_output_devices"
          )
            return [];
          if (command === "get_history_entries")
            return {
              entries: JSON.parse(localStorage.getItem("test-history") ?? "[]"),
              has_more: false,
            };
          if (command === "get_assistant_recovery_state")
            return {
              items: JSON.parse(localStorage.getItem("test-recovery") ?? "[]"),
              capacity: 8,
              free_slots: 8,
            };
          if (command === "dismiss_assistant_recovery_item") {
            const items: AssistantRecoveryItem[] = JSON.parse(
              localStorage.getItem("test-recovery") ?? "[]",
            );
            localStorage.setItem(
              "test-recovery",
              JSON.stringify(items.filter((item) => item.id !== args.id)),
            );
            return null;
          }
          if (command === "copy_assistant_recovery_text") {
            localStorage.setItem("test-copy", String(args.part));
            return null;
          }
          if (command === "fetch_post_process_models") return ["qwen3.5:9b"];
          if (command === "set_assistant_selected_prompt")
            settings.assistant_selected_prompt_id = args.id as string | null;
          if (command === "change_post_process_request_options")
            settings.post_process_request_options =
              args.options as LlmRequestOptions;
          if (command === "change_english_cleanup_settings")
            settings.english_cleanup = args.value as EnglishCleanupSettings;
          if (command === "update_post_process_prompt") {
            const prompt = settings.post_process_prompts?.find(
              (p) => p.id === args.id,
            );
            if (prompt) {
              prompt.name = String(args.name);
              prompt.prompt = String(args.prompt);
            }
          }
          if (command === "change_post_process_base_url_setting")
            settings.post_process_providers![0].base_url = String(args.baseUrl);
          if (command === "change_post_process_model_setting")
            settings.post_process_models!.custom = String(args.model);
          if (command === "change_post_process_api_key_setting")
            settings.post_process_api_keys!.custom = String(args.apiKey);
          const profiles = settings.processing_profiles ?? [];
          if (
            command === "add_processing_profile" ||
            command === "update_processing_profile"
          ) {
            const input = args.input as ProcessingProfileInput;
            const current = profiles.find((p) => p.id === args.id);
            if (
              profiles.some(
                (p) =>
                  p.id !== current?.id &&
                  p.name &&
                  p.name.toLowerCase() === input.name.toLowerCase(),
              )
            )
              throw "name_taken";
            if (command === "add_processing_profile") {
              if (
                profiles.filter((p) => !p.id.startsWith("builtin_")).length >=
                16
              )
                throw "profile_limit";
              const profile: ProcessingProfile = { ...input, id: "p" + next++ };
              profiles.push(profile);
              settings.processing_profiles = profiles;
              persist();
              return profile;
            }
            if (!current) throw "not_found";
            Object.assign(current, input);
            if (current.id === "builtin_cleanup")
              settings.post_process_selected_prompt_id = input.prompt_id;
            if (current.id === "builtin_assistant")
              settings.assistant_selected_prompt_id = input.prompt_id;
            const bindingId =
              current.id === "builtin_cleanup"
                ? "transcribe_with_post_process"
                : current.id === "builtin_assistant"
                  ? "assistant"
                  : (current.kind === "dictation"
                      ? "dictation_profile_"
                      : "command_profile_") + current.id;
            if (settings.bindings?.[bindingId])
              settings.bindings[bindingId].name =
                input.name || initial.bindings![bindingId]?.name;
          }
          if (
            command === "delete_processing_profile" ||
            command === "clear_profile_hotkey"
          ) {
            const profile = profiles.find((p) => p.id === args.id);
            if (profile?.id.startsWith("builtin_")) throw "builtin_profile";
            if (!profile) throw "not_found";
            const bindingId =
              (profile.kind === "dictation"
                ? "dictation_profile_"
                : "command_profile_") + profile.id;
            delete settings.bindings?.[bindingId];
            if (command === "delete_processing_profile")
              settings.processing_profiles = profiles.filter(
                (p) => p.id !== args.id,
              );
          }
          if (command === "add_post_process_prompt") {
            const prompt = {
              id: "prompt" + next++,
              name: String(args.name),
              prompt: String(args.prompt),
            };
            settings.post_process_prompts!.push(prompt);
            persist();
            return prompt;
          }
          if (command === "delete_post_process_prompt") {
            if (
              profiles.some(
                (p) =>
                  (p.id === "builtin_cleanup"
                    ? settings.post_process_selected_prompt_id
                    : p.id === "builtin_assistant"
                      ? settings.assistant_selected_prompt_id
                      : p.prompt_id) === args.id,
              )
            )
              throw "prompt_in_use";
            settings.post_process_prompts =
              settings.post_process_prompts!.filter((p) => p.id !== args.id);
          }
          if (command === "change_binding") {
            if (args.binding === "ctrl+space")
              return {
                success: false,
                error: "Shortcut already used by Transcribe",
              };
            const id = String(args.id);
            settings.bindings ??= {};
            const profile = profiles.find((p) => id.endsWith("_" + p.id));
            settings.bindings[id] ??= {
              id,
              name: profile?.name ?? id,
              description: "",
              default_binding: "",
              current_binding: "",
            };
            settings.bindings[id].current_binding = String(args.binding);
            persist();
            return { success: true, error: null };
          }
          if (command === "reset_binding")
            settings.bindings![String(args.id)].current_binding =
              settings.bindings![String(args.id)].default_binding;
          persist();
          return null;
        },
      };
    },
    { keyboard, overrides },
  );
  await page.goto("/");
  if (openSection)
    await page.getByRole("button", { name: openSection, exact: true }).click();
}

export async function emitKeys(
  page: Page,
  key: string,
  hotkey: string,
  modifiers: string[] = [],
) {
  await page.evaluate(
    ({ key, hotkey, modifiers }) => {
      const w = window as unknown as HarnessWindow;
      w.emitTestEvent("handy-keys-event", {
        modifiers,
        key,
        is_key_down: true,
        hotkey_string: hotkey,
      });
      w.emitTestEvent("handy-keys-event", {
        modifiers,
        key,
        is_key_down: false,
        hotkey_string: hotkey,
      });
    },
    { key, hotkey, modifiers },
  );
}

export async function defer(page: Page, command: string, fail = false) {
  await page.evaluate(
    ({ command, fail }) => {
      const w = window as unknown as HarnessWindow;
      w.pendingCommand = command;
      w.failCommand = fail ? command : undefined;
      w.releaseCommand = undefined;
    },
    { command, fail },
  );
}

export async function pending(page: Page) {
  await expect
    .poll(() =>
      page.evaluate(
        () => !!(window as unknown as HarnessWindow).releaseCommand,
      ),
    )
    .toBe(true);
  expect(await page.evaluate(() => document.activeElement?.tagName)).not.toBe(
    "BODY",
  );
}

export async function release(page: Page) {
  await page.evaluate(() => {
    const w = window as unknown as HarnessWindow;
    w.pendingCommand = undefined;
    w.releaseCommand?.();
  });
}
