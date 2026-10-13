import { test, expect, type Page } from "@playwright/test";
import type { ProcessingProfile, AppSettings } from "../src/bindings";
import {
  setup,
  defer,
  pending,
  release,
  emitKeys,
  screenshot,
  type HarnessWindow,
} from "./support/settings-harness";

const custom = (id: string, name = id): ProcessingProfile => ({
  id,
  name,
  kind: "dictation",
  prompt_id: "cleanup",
  model: null,
  speech_language: null,
  english_cleanup: null,
});
const builtins: ProcessingProfile[] = [
  {
    ...custom("builtin_cleanup", ""),
    english_cleanup: {
      styling: "casual",
      structure: "bullets",
      context: "technical",
    },
  },
  {
    ...custom("builtin_assistant", ""),
    kind: "selection_command",
    prompt_id: "assistant",
  },
];
async function saved(page: Page): Promise<AppSettings> {
  return page.evaluate(() =>
    JSON.parse(localStorage.getItem("test-settings")!),
  );
}
async function calls(page: Page, command: string) {
  return page.evaluate(
    (command) =>
      (window as unknown as HarnessWindow).commandCalls.filter(
        (c) => c === command,
      ).length,
    command,
  );
}
async function reload(page: Page) {
  await page.reload();
  await page.getByRole("button", { name: "Post Process", exact: true }).click();
}
async function add(page: Page, name: string) {
  await page
    .getByRole("button", { name: "Add dictation profile", exact: true })
    .press("Enter");
  await page.getByRole("textbox", { name: "Name", exact: true }).fill(name);
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(page.getByRole("dialog")).toHaveCount(0);
}

for (const backend of ["tauri", "handy_keys"]) {
  test(`${backend}: shortcut names stay stable and describe only current keys on Profiles and General`, async ({
    page,
  }) => {
    await setup(page, backend, {
      post_process_enabled: true,
      processing_profiles: [...builtins, custom("a", "Translator")],
      bindings: Object.fromEntries(
        [
          ["transcribe", "ctrl+space"],
          ["transcribe_with_post_process", "ctrl+shift+space"],
          ["assistant", "ctrl+alt+space"],
          ["cancel", "escape"],
        ].map(([id, current_binding]) => [
          id,
          {
            id,
            name: id,
            description: "",
            current_binding,
            default_binding: current_binding,
          },
        ]),
      ),
    });
    let captureIndex = 0;
    const check = async (name: string, idle: RegExp, label = "Hotkey ") => {
      const key = ["F8", "F9", "F10", "F11", "F12"][captureIndex++];
      const button = page.getByRole("button", { name: new RegExp(`^${name}`) });
      await expect(button).toHaveAccessibleName(name);
      await expect(button).toHaveAccessibleDescription(idle);
      await button.press("Enter");
      await expect(button).toHaveAttribute("aria-pressed", "true");
      await expect(button).toHaveAccessibleName(name);
      await expect(button).toHaveAccessibleDescription(`${label}Press keys...`);
      if (backend === "tauri") {
        await page.keyboard.down("Control");
        await page.keyboard.down(key);
      } else {
        await page.evaluate(
          (key) =>
            (window as unknown as HarnessWindow).emitTestEvent(
              "handy-keys-event",
              {
                modifiers: ["ctrl"],
                key: key.toLowerCase(),
                is_key_down: true,
                hotkey_string: `ctrl+${key.toLowerCase()}`,
              },
            ),
          key,
        );
      }
      await expect(button).toHaveAccessibleName(name);
      const captured = new RegExp(`^${label}Ctrl.*${key}$`);
      await expect(button).toHaveAccessibleDescription(captured);
      if (backend === "tauri") {
        await page.keyboard.up(key);
        await page.keyboard.up("Control");
      } else {
        await page.evaluate(
          (key) =>
            (window as unknown as HarnessWindow).emitTestEvent(
              "handy-keys-event",
              {
                modifiers: ["ctrl"],
                key: key.toLowerCase(),
                is_key_down: false,
                hotkey_string: `ctrl+${key.toLowerCase()}`,
              },
            ),
          key,
        );
      }
      await expect(button).toHaveAttribute("aria-pressed", "false");
      await expect(button).toHaveAccessibleName(name);
      await expect(button).toHaveAccessibleDescription(captured);
    };
    await check("Change Cleanup shortcut", /^Hotkey Ctrl.*Shift.*Space$/);
    await check(
      "Change Selected-text assistant shortcut",
      /^Hotkey Ctrl.*Alt.*Space$/,
    );
    await check("Change Translator shortcut", /^Hotkey Not set$/);
    await page
      .getByRole("button", { name: "General", exact: true })
      .press("Enter");
    await check("Change Transcribe shortcut", /^Ctrl.*Space$/, "");
    await check("Change Cancel shortcut", /^Escape$/, "");
    expect(await calls(page, "change_binding")).toBe(5);
  });

  test(`${backend}: General Change names say shortcut once and preserve other action names`, async ({
    page,
  }) => {
    await setup(page, backend, {
      processing_profiles: [...builtins, custom("a", "Night Shortcut")],
    });
    await expect(
      page.getByRole("button", {
        name: "Change Night Shortcut shortcut",
        exact: true,
      }),
    ).toHaveCount(1);
    await expect(
      page.getByRole("button", {
        name: "Change Cleanup shortcut",
        exact: true,
      }),
    ).toHaveCount(1);
    await page
      .getByRole("button", { name: "General", exact: true })
      .press("Enter");
    await expect(
      page.getByRole("button", { name: /^Change / }),
    ).toHaveAccessibleName("Change Transcribe shortcut");
    await expect(
      page.getByRole("button", {
        name: "Reset Transcribe Shortcut shortcut",
        exact: true,
      }),
    ).toHaveCount(1);
  });
}

test("profile rows expose name headings and plain Hotkey labels without empty help or duplicate Profiles heading", async ({
  page,
}) => {
  await setup(page, "tauri", {
    processing_profiles: [...builtins, custom("a", "Translator")],
  });
  await expect(
    page.getByRole("heading", { name: "Profiles", exact: true }),
  ).toHaveCount(1);
  const list = page.getByRole("list", { name: "Profiles", exact: true });
  const names = [
    "Cleanup, Dictation, built-in",
    "Selected-text assistant, Selection command, built-in",
    "Translator, Dictation",
  ];
  await expect(list.getByRole("heading", { level: 3 })).toHaveText(names);
  for (const name of names) {
    const row = list.getByRole("listitem", { name, exact: true });
    await expect(row.getByRole("heading", { level: 3 })).toHaveCount(1);
    await expect(row.getByRole("heading", { level: 3 })).toHaveAccessibleName(
      name,
    );
    await expect(row.getByText(name, { exact: true })).toHaveCount(1);
    await expect(row.getByText("Hotkey", { exact: true })).toHaveCount(1);
    await expect(
      row.getByRole("heading", { name: "Hotkey", exact: true }),
    ).toHaveCount(0);
    await expect(
      row.getByRole("button", { name: /^Change / }),
    ).toHaveAccessibleDescription(/^Hotkey /);
  }
  const row = list.getByRole("listitem", {
    name: "Translator, Dictation",
    exact: true,
  });
  await expect(
    row.getByRole("button", { name: "More information", exact: true }),
  ).toHaveCount(0);
});

test("Cleanup uses a neutral no-prompt option and names the Experimental path", async ({
  page,
}) => {
  await setup(page);
  await expect(
    page.getByText("This hotkey is off.", { exact: false }),
  ).toHaveText(
    "This hotkey is off. In Advanced settings, enable Experimental Features, then turn on Post Processing under Experimental to use it.",
  );
  const cleanup = page.getByRole("listitem", {
    name: "Cleanup, Dictation, built-in",
    exact: true,
  });
  await cleanup
    .getByRole("button", { name: "More information", exact: true })
    .press("Enter");
  await expect(
    page.getByText(
      "A dedicated Cleanup hotkey. Applies AI post-processing when Post Processing is enabled.",
      { exact: true },
    ),
  ).toBeVisible();
  await cleanup
    .getByRole("button", { name: "Edit Cleanup", exact: true })
    .press("Enter");
  await expect(
    page
      .getByRole("combobox", { name: "Profile prompt", exact: true })
      .getByRole("option", { name: "No prompt selected", exact: true }),
  ).toHaveCount(1);
});

test("Duplicate dialog names its source and prompt-library errors describe focused Delete", async ({
  page,
}) => {
  await setup(page);
  await page
    .getByRole("button", { name: "Duplicate Cleanup", exact: true })
    .press("Enter");
  await expect(page.getByRole("dialog")).toHaveAccessibleName(
    "Duplicate Cleanup",
  );
  await page.keyboard.press("Escape");
  const select = page.getByRole("combobox", {
    name: "Prompt to edit",
    exact: true,
  });
  await select.selectOption("assistant");
  const del = page.getByRole("button", { name: "Delete Prompt", exact: true });
  await del.press("Enter");
  await expect(del).toBeFocused();
  await expect(del).toHaveAccessibleDescription(
    "This prompt is used by Selected-text assistant. Choose another prompt for those profiles first.",
  );
});

test("prompt-library help describes editing only", async ({ page }) => {
  await setup(page);
  const group = page
    .getByRole("heading", { name: "Prompt to edit", exact: true })
    .locator("..");
  await group
    .getByRole("button", { name: "More information", exact: true })
    .press("Enter");
  await expect(
    page.getByText(
      "Choose which prompt to edit in the library. This does not change any profile's prompt.",
      { exact: true },
    ),
  ).toBeVisible();
});

test("built-in rows replace legacy groups, retain Reset, and individually name every action", async ({
  page,
}, testInfo) => {
  await setup(page);
  const list = page.getByRole("list", { name: "Profiles", exact: true });
  await expect(list.getByRole("listitem")).toHaveCount(2);
  await expect(list.getByRole("listitem").nth(0)).toHaveAccessibleName(
    "Cleanup, Dictation, built-in",
  );
  await expect(list.getByRole("listitem").nth(1)).toHaveAccessibleName(
    "Selected-text assistant, Selection command, built-in",
  );
  await expect(list.getByRole("button", { name: /Delete|Remove/ })).toHaveCount(
    0,
  );
  await expect(
    list.getByRole("button", { name: "Reset Cleanup shortcut", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Prompts", exact: true }),
  ).toBeVisible();
  await expect(list.getByRole("heading", { level: 3 })).toHaveText([
    "Cleanup, Dictation, built-in",
    "Selected-text assistant, Selection command, built-in",
  ]);
  await expect(
    page.getByRole("combobox", { name: "Assistant prompt", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", {
      name: "Add selection command profile",
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    page.getByRole("combobox", { name: "Profile model", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByText(
      "Default provider and model for every profile that has no model of its own.",
      { exact: true },
    ),
  ).toBeVisible();
  await expect(
    page.getByText(
      "Shared by all profiles. Changes apply to the next recording.",
      { exact: true },
    ),
  ).toBeVisible();
  if (process.env.HANDY_UX_SCREENS === "1")
    await screenshot(page, testInfo, "profiles");
});

test("built-in Cleanup accepts blank name, synchronizes prompt and renamed hotkey, and persists", async ({
  page,
}) => {
  await setup(page);
  await page
    .getByRole("button", { name: "Edit Cleanup", exact: true })
    .press("Enter");
  const name = page.getByRole("textbox", { name: "Name", exact: true });
  await expect(name).toBeFocused();
  await expect(name).toHaveAccessibleDescription(
    "Leave blank to use the default name.",
  );
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(
    page.getByRole("button", { name: "Edit Cleanup", exact: true }),
  ).toBeFocused();
  await page
    .getByRole("button", { name: "Edit Cleanup", exact: true })
    .press("Enter");
  await name.fill("My cleanup");
  await page
    .getByRole("combobox", { name: "Profile prompt", exact: true })
    .selectOption("assistant");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(
    page.getByRole("button", { name: "Edit My cleanup", exact: true }),
  ).toBeFocused();
  await expect(
    page.getByRole("button", { name: /Change My cleanup shortcut/ }),
  ).toHaveAccessibleDescription(/Ctrl.*Shift.*Space/);
  expect((await saved(page)).post_process_selected_prompt_id).toBe("assistant");
  await reload(page);
  await page
    .getByRole("button", { name: "Edit My cleanup", exact: true })
    .click();
  await expect(name).toHaveValue("My cleanup");
  await expect(
    page.getByRole("combobox", { name: "Profile prompt", exact: true }),
  ).toHaveValue("assistant");
});

test("built-in assistant permits No prompt selected and reloads null", async ({
  page,
}) => {
  await setup(page);
  await page
    .getByRole("button", { name: "Edit Selected-text assistant", exact: true })
    .press("Enter");
  await page
    .getByRole("combobox", { name: "Profile prompt", exact: true })
    .selectOption("");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .click();
  expect((await saved(page)).assistant_selected_prompt_id).toBeNull();
  await reload(page);
  await page
    .getByRole("button", { name: "Edit Selected-text assistant", exact: true })
    .click();
  await expect(
    page.getByRole("combobox", { name: "Profile prompt", exact: true }),
  ).toHaveValue("");
});

test("keyboard add returns focus to Add, announces Saved, and persists without a hotkey", async ({
  page,
}) => {
  await setup(page);
  await add(page, "Polish to English");
  await expect(
    page.getByRole("button", { name: "Add dictation profile", exact: true }),
  ).toBeFocused();
  await expect(
    page.getByRole("status").filter({ hasText: "Saved" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: /Change Polish to English shortcut/ }),
  ).toHaveAccessibleDescription(/Not set/);
  await reload(page);
  await expect(
    page.getByRole("button", { name: "Edit Polish to English", exact: true }),
  ).toBeVisible();
  expect(
    (await saved(page)).processing_profiles?.find(
      (p) => p.name === "Polish to English",
    )?.english_cleanup,
  ).toBeNull();
});

test("custom blank and duplicate default names reject before commands with per-field errors and help", async ({
  page,
}) => {
  await setup(page);
  await page
    .getByRole("button", { name: "Add dictation profile", exact: true })
    .press("Enter");
  const dialog = page.getByRole("dialog");
  const name = dialog.getByRole("textbox", { name: "Name", exact: true });
  const save = dialog.getByRole("button", { name: "Save", exact: true });
  await save.press("Enter");
  await expect(dialog.getByRole("alert")).toHaveText("Enter a profile name.");
  await expect(name).toHaveAttribute("aria-invalid", "true");
  await expect(name).toHaveAccessibleDescription("Enter a profile name.");
  await expect(save).toBeFocused();
  expect(await calls(page, "add_processing_profile")).toBe(0);
  await name.fill("cleanup");
  await save.press("Enter");
  await expect(dialog.getByRole("alert")).toContainText(
    "already uses that name",
  );
  expect(await calls(page, "add_processing_profile")).toBe(0);
  await name.fill("Selected-text assistant");
  await save.press("Enter");
  expect(await calls(page, "add_processing_profile")).toBe(0);
  await expect(page.getByRole("alert")).toContainText("already uses that name");
});

test("model, URL, language, fixed kind and English controls round-trip on edit and duplicate", async ({
  page,
}, testInfo) => {
  await setup(page, "tauri", { processing_profiles: builtins });
  await page
    .getByRole("button", { name: "Edit Cleanup", exact: true })
    .press("Enter");
  const dialog = page.getByRole("dialog");
  await expect(
    dialog.getByRole("combobox", { name: "Kind", exact: true }),
  ).toHaveCount(0);
  await expect(
    dialog.getByRole("checkbox", {
      name: "Use the English cleanup model for English speech",
      exact: true,
    }),
  ).toBeChecked();
  for (const [field, value] of Object.entries(builtins[0].english_cleanup!)) {
    await expect(
      dialog.getByRole("textbox", { name: `Profile ${field}`, exact: true }),
    ).toHaveValue(value);
  }
  await dialog
    .getByRole("combobox", { name: "Speech language", exact: true })
    .selectOption("pl");
  await dialog
    .getByRole("combobox", { name: "Profile model", exact: true })
    .selectOption("custom");
  await dialog
    .getByRole("textbox", { name: "Profile model name", exact: true })
    .fill("qwen3.5:4b");
  await dialog
    .getByRole("textbox", { name: "Profile base URL", exact: true })
    .fill("http://127.0.0.1:8081/v1");
  if (process.env.HANDY_UX_SCREENS === "1")
    await screenshot(page, testInfo, "profile-editor");
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  expect((await saved(page)).processing_profiles?.[0]).toMatchObject({
    kind: "dictation",
    speech_language: "pl",
    model: {
      provider_id: "custom",
      model: "qwen3.5:4b",
      base_url: "http://127.0.0.1:8081/v1",
    },
    english_cleanup: builtins[0].english_cleanup,
  });
  await reload(page);
  await page.getByRole("button", { name: "Edit Cleanup", exact: true }).click();
  await expect(
    dialog.getByRole("combobox", { name: "Speech language", exact: true }),
  ).toHaveValue("pl");
  await expect(
    dialog.getByRole("textbox", { name: "Profile base URL", exact: true }),
  ).toHaveValue("http://127.0.0.1:8081/v1");
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await page
    .getByRole("button", { name: "Duplicate Cleanup", exact: true })
    .press("Enter");
  await expect(
    dialog.getByRole("textbox", { name: "Name", exact: true }),
  ).toHaveValue("Cleanup (copy)");
  // Coordinator r2 decision overrides HANDY-008 D11 line 606: a copy must
  // explicitly opt in, so a translation prompt cannot inherit S1 routing.
  await expect(
    dialog.getByRole("checkbox", {
      name: "Use the English cleanup model for English speech",
      exact: true,
    }),
  ).not.toBeChecked();
  await dialog.getByRole("button", { name: "Save", exact: true }).click();
  const duplicate = (await saved(page)).processing_profiles?.find(
    (p) => p.name === "Cleanup (copy)",
  )!;
  expect(duplicate).toMatchObject({
    kind: "dictation",
    english_cleanup: null,
    speech_language: "pl",
  });
  expect(
    (await saved(page)).bindings?.["dictation_profile_" + duplicate.id],
  ).toBeUndefined();
});

test("delete traps focus, Escape returns, confirmation selects next or previous Edit without BODY", async ({
  page,
}, testInfo) => {
  await setup(page, "tauri", {
    processing_profiles: [
      ...builtins,
      custom("a", "First"),
      custom("b", "Last"),
    ],
  });
  const trigger = page.getByRole("button", {
    name: "Delete First",
    exact: true,
  });
  await trigger.press("Enter");
  const dialog = page.getByRole("dialog", { name: "Delete First?" });
  await expect(
    dialog.getByRole("button", { name: "Cancel", exact: true }),
  ).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(trigger).toBeFocused();
  await trigger.press("Enter");
  if (process.env.HANDY_UX_SCREENS === "1")
    await screenshot(page, testInfo, "delete-profile");
  await dialog
    .getByRole("button", { name: "Delete profile", exact: true })
    .press("Enter");
  await expect(
    page.getByRole("button", { name: "Edit Last", exact: true }),
  ).toBeFocused();
  await page
    .getByRole("button", { name: "Delete Last", exact: true })
    .press("Enter");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Delete profile", exact: true })
    .press("Enter");
  await expect(
    page.getByRole("button", {
      name: "Edit Selected-text assistant",
      exact: true,
    }),
  ).toBeFocused();
  expect(await page.evaluate(() => document.activeElement?.tagName)).not.toBe(
    "BODY",
  );
  expect((await saved(page)).post_process_prompts).toHaveLength(2);
  await expect(
    page.getByRole("status").filter({ hasText: "Profile deleted" }),
  ).toBeVisible();
});

for (const backend of ["tauri", "handy_keys"]) {
  test(`${backend}: absent profile hotkey saves exact id, removes, rolls conflict back without partial binding`, async ({
    page,
  }) => {
    await setup(page, backend, {
      processing_profiles: [...builtins, custom("a", "Translator")],
    });
    const shortcut = page.getByRole("button", {
      name: /Change Translator shortcut/,
    });
    await expect(shortcut).toHaveAccessibleDescription(/Not set/);
    await shortcut.press("Enter");
    if (backend === "tauri") await shortcut.press("Control+F9");
    else await emitKeys(page, "f9", "ctrl+f9", ["ctrl"]);
    await expect(shortcut).toHaveAttribute("aria-pressed", "false");
    expect(
      (await saved(page)).bindings?.dictation_profile_a.current_binding,
    ).toBe("ctrl+f9");
    const args = await page.evaluate(() =>
      (window as unknown as HarnessWindow).commandArgs.filter(
        (c) => c.command === "change_binding",
      ),
    );
    expect(args[0].args).toEqual({
      id: "dictation_profile_a",
      binding: "ctrl+f9",
    });
    await reload(page);
    await expect(shortcut).toContainText("F9");
    const remove = page.getByRole("button", {
      name: "Remove Translator hotkey",
      exact: true,
    });
    await remove.press("Enter");
    // Removed button should return focus to the retained change control.
    await expect(shortcut).toBeFocused();
    await expect(shortcut).toContainText("Not set");
    expect((await saved(page)).bindings?.dictation_profile_a).toBeUndefined();
    await shortcut.press("Enter");
    if (backend === "tauri") await shortcut.press("Control+Space");
    else await emitKeys(page, "space", "ctrl+space", ["ctrl"]);
    await expect(
      page
        .getByRole("alert")
        .filter({ hasText: /Transcribe/ })
        .first(),
    ).toBeVisible();
    await expect(shortcut).toHaveAccessibleDescription(/Transcribe/);
    await expect(shortcut).toContainText("Not set");
    expect((await saved(page)).bindings?.dictation_profile_a).toBeUndefined();
    expect(await calls(page, "change_binding")).toBe(1); // reload resets call history; conflict is one call, no restore
  });
}

test("Cleanup toggle note describes only its hotkey and Advanced explains the contract", async ({
  page,
}) => {
  await setup(page, "tauri", {
    experimental_enabled: true,
    processing_profiles: [...builtins, custom("a", "Translator")],
  });
  await expect(
    page.getByRole("button", { name: /Change Cleanup shortcut/ }),
  ).toHaveAccessibleDescription(/This hotkey is off/);
  await expect(
    page.getByRole("button", { name: /Change Translator shortcut/ }),
  ).not.toHaveAccessibleDescription(/This hotkey is off/);
  await page.getByRole("button", { name: "Advanced", exact: true }).click();
  await page
    .getByRole("heading", { name: "Post Processing", exact: true })
    .locator("..")
    .getByRole("button", { name: "More information" })
    .press("Enter");
  await expect(
    page.getByText(
      "Turns the Cleanup hotkey on or off. Other profile hotkeys and the --toggle-post-process command keep working.",
      { exact: true },
    ),
  ).toBeAttached();
});

test("prompt library names users, Create does not choose Cleanup prompt, in-use Delete explains refusal", async ({
  page,
}, testInfo) => {
  await setup(page);
  const select = page.getByRole("combobox", {
    name: "Prompt to edit",
    exact: true,
  });
  await expect(select).toHaveValue("cleanup");
  await expect(
    page.getByText("Used by: Cleanup", { exact: true }),
  ).toBeVisible();
  await select.selectOption("assistant");
  await expect(
    page.getByText("Used by: Selected-text assistant", { exact: true }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Delete Prompt", exact: true })
    .click();
  await expect(page.getByRole("alert")).toHaveText(
    "This prompt is used by Selected-text assistant. Choose another prompt for those profiles first.",
  );
  expect((await saved(page)).post_process_prompts).toHaveLength(2);
  if (process.env.HANDY_UX_SCREENS === "1")
    await screenshot(page, testInfo, "prompt-in-use");
  await page
    .getByRole("button", { name: "Create New Prompt", exact: true })
    .click();
  await page
    .getByRole("textbox", { name: "Prompt Label", exact: true })
    .fill("Translate to English");
  await page
    .getByRole("textbox", { name: "Prompt Instructions", exact: true })
    .fill("Translate the transcript to English.");
  await page
    .getByRole("button", { name: "Create Prompt", exact: true })
    .click();
  await expect(select).not.toHaveValue("cleanup");
  expect((await saved(page)).post_process_selected_prompt_id).toBe("cleanup");
  await expect(
    page.getByRole("button", { name: "Create New Prompt", exact: true }),
  ).toBeFocused();
  await expect(
    page.getByText("Not used by any profile.", { exact: true }),
  ).toBeVisible();
});

test("missing optional profile fields render without console errors", async ({
  page,
}) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  await setup(page, "tauri", { omitProfiles: true });
  await expect(
    page
      .getByRole("list", { name: "Profiles", exact: true })
      .getByRole("listitem"),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Add dictation profile", exact: true }),
  ).toBeVisible();
  expect(errors).toEqual([]);
});

test("custom limit leaves Add and Duplicate focusable, prevents re-entry, and explains 16 limit", async ({
  page,
}) => {
  await setup(page, "tauri", {
    processing_profiles: [
      ...builtins,
      ...Array.from({ length: 16 }, (_, i) => custom("p" + i)),
    ],
  });
  const add = page.getByRole("button", {
    name: "Add dictation profile",
    exact: true,
  });
  await add.press("Enter");
  await expect(add).toBeFocused();
  await expect(add).toHaveAttribute("aria-disabled", "true");
  await expect(add).toHaveAccessibleDescription(/at most 16 custom profiles/);
  await page
    .getByRole("button", { name: "Duplicate Cleanup", exact: true })
    .press("Enter");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect(await calls(page, "add_processing_profile")).toBe(0);
});

for (const code of [
  "name_taken",
  "profile_limit",
  "prompt_not_found",
  "provider_not_found",
  "model_required",
  "model_too_long",
  "base_url_invalid",
  "language_invalid",
  "kind_immutable",
]) {
  test(`backend ${code} exposes specific alert, keeps dialog focus and persisted profile`, async ({
    page,
  }) => {
    await setup(page);
    await page
      .getByRole("button", { name: "Edit Cleanup", exact: true })
      .press("Enter");
    await page
      .getByRole("textbox", { name: "Name", exact: true })
      .fill("Updated");
    if (
      ["model_required", "model_too_long", "base_url_invalid"].includes(code)
    ) {
      await page
        .getByRole("combobox", { name: "Profile model", exact: true })
        .selectOption("custom");
      await page
        .getByRole("textbox", { name: "Profile model name", exact: true })
        .fill("valid-model");
    }
    await page.evaluate((code) => {
      const w = window as unknown as HarnessWindow;
      w.failCommand = "update_processing_profile";
      w.failCode = code;
    }, code);
    const save = page
      .getByRole("dialog")
      .getByRole("button", { name: "Save", exact: true });
    await save.press("Enter");
    await expect(page.getByRole("dialog").getByRole("alert")).toHaveCount(1);
    await expect(page.getByRole("alert")).not.toContainText("Could not save");
    await expect(save).toBeFocused();
    if (code === "model_too_long") {
      const model = page.getByRole("textbox", {
        name: "Profile model name",
        exact: true,
      });
      await expect(model).toHaveAttribute("aria-invalid", "true");
      await expect(model).toHaveAccessibleDescription(/1 to 256 characters/);
    }
    if (["profile_limit", "kind_immutable"].includes(code)) {
      await expect(save).toHaveAccessibleDescription(
        await page.getByRole("alert").innerText(),
      );
    }
    expect((await saved(page)).processing_profiles?.[0].name ?? "").toBe("");
  });
}

test("deferred add and delete keep focus, ignore Escape, and send one command on re-entry", async ({
  page,
}) => {
  await setup(page);
  await page
    .getByRole("button", { name: "Add dictation profile", exact: true })
    .press("Enter");
  await page.getByRole("textbox", { name: "Name", exact: true }).fill("New");
  const save = page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true });
  await defer(page, "add_processing_profile");
  await save.press("Enter");
  await pending(page);
  await expect(save).toHaveAttribute("aria-disabled", "true");
  await expect(save).toBeFocused();
  await save.press("Enter");
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog")).toBeVisible();
  await release(page);
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect(await calls(page, "add_processing_profile")).toBe(1);
  await page
    .getByRole("button", { name: "Delete New", exact: true })
    .press("Enter");
  await defer(page, "delete_processing_profile", true);
  const confirm = page
    .getByRole("dialog")
    .getByRole("button", { name: "Delete profile", exact: true });
  await confirm.press("Enter");
  await pending(page);
  await confirm.press("Enter");
  await release(page);
  await expect(confirm).toBeFocused();
  await expect(page.getByRole("dialog").getByRole("alert")).toContainText(
    "Could not save",
  );
  expect(await calls(page, "delete_processing_profile")).toBe(1);
});

test("blank built-in rename still checks its localized default against custom names", async ({
  page,
}) => {
  await setup(page, "tauri", {
    processing_profiles: [
      { ...builtins[0], name: "My cleanup" },
      builtins[1],
      custom("a", "Cleanup"),
    ],
  });
  await page
    .getByRole("button", { name: "Edit My cleanup", exact: true })
    .press("Enter");
  const name = page.getByRole("textbox", { name: "Name", exact: true });
  await name.fill("");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(name).toHaveAccessibleDescription(
    /Leave blank.*already uses that name/,
  );
  expect(await calls(page, "update_processing_profile")).toBe(0);
});

test("custom profile without prompts rejects Save and keeps prompt help", async ({
  page,
}) => {
  await setup(page, "tauri", { post_process_prompts: [] });
  await page
    .getByRole("button", { name: "Add selection command profile", exact: true })
    .press("Enter");
  const dialog = page.getByRole("dialog");
  await dialog
    .getByRole("textbox", { name: "Name", exact: true })
    .fill("Command");
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(
    dialog.getByRole("combobox", { name: "Profile prompt", exact: true }),
  ).toHaveAccessibleDescription(/Prompts are edited.*Choose a prompt/);
  expect(await calls(page, "add_processing_profile")).toBe(0);
  await dialog
    .getByRole("button", { name: "Cancel", exact: true })
    .press("Enter");
});

test("new selection command uses command binding id, duplicate does not copy its hotkey", async ({
  page,
}) => {
  await setup(page);
  await page
    .getByRole("button", { name: "Add selection command profile", exact: true })
    .press("Enter");
  await page
    .getByRole("textbox", { name: "Name", exact: true })
    .fill("Rewrite");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  const profile = (await saved(page)).processing_profiles?.find(
    (p) => p.name === "Rewrite",
  )!;
  expect(profile.kind).toBe("selection_command");
  const shortcut = page.getByRole("button", {
    name: /Change Rewrite shortcut/,
  });
  await shortcut.press("Enter");
  await shortcut.press("Control+F9");
  await expect(shortcut).toHaveAttribute("aria-pressed", "false");
  expect(
    (await saved(page)).bindings?.["command_profile_" + profile.id]
      ?.current_binding,
  ).toBe("ctrl+f9");
  await page
    .getByRole("button", { name: "Duplicate Rewrite", exact: true })
    .press("Enter");
  await expect(
    page.getByRole("textbox", { name: "Name", exact: true }),
  ).toHaveValue("Rewrite (copy)");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  const duplicate = (await saved(page)).processing_profiles?.find(
    (p) => p.name === "Rewrite (copy)",
  )!;
  expect(duplicate.kind).toBe("selection_command");
  expect(
    (await saved(page)).bindings?.["command_profile_" + duplicate.id],
  ).toBeUndefined();
});

test("client name length and model/URL validation are specific, retain range help and send no command", async ({
  page,
}) => {
  await setup(page);
  await page
    .getByRole("button", { name: "Edit Cleanup", exact: true })
    .press("Enter");
  const dialog = page.getByRole("dialog");
  await dialog
    .getByRole("textbox", { name: "Name", exact: true })
    .fill("x".repeat(61));
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(dialog.getByRole("alert")).toContainText(
    "at most 60 characters",
  );
  await dialog
    .getByRole("textbox", { name: "Name", exact: true })
    .fill("Cleanup");
  await dialog
    .getByRole("combobox", { name: "Profile model", exact: true })
    .selectOption("custom");
  await dialog
    .getByRole("textbox", { name: "Profile base URL", exact: true })
    .fill("file:///private");
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(dialog.getByRole("alert")).toHaveCount(2);
  await expect(
    dialog.getByRole("textbox", { name: "Profile model name", exact: true }),
  ).toHaveAccessibleDescription(/1 to 256 characters/);
  await expect(
    dialog.getByRole("textbox", { name: "Profile base URL", exact: true }),
  ).toHaveAccessibleDescription(/Optional.*HTTP or HTTPS/);
  expect(await calls(page, "update_processing_profile")).toBe(0);
});

for (const backend of ["tauri", "handy_keys"]) {
  test(`${backend}: pending Remove/Reset remain focusable, guard re-entry and report failure`, async ({
    page,
  }) => {
    await setup(page, backend, {
      processing_profiles: [...builtins, custom("a", "Translator")],
      bindings: {
        assistant: {
          id: "assistant",
          name: "Assistant",
          description: "",
          current_binding: "ctrl+alt+space",
          default_binding: "ctrl+alt+space",
        },
        transcribe_with_post_process: {
          id: "transcribe_with_post_process",
          name: "Cleanup",
          description: "",
          current_binding: "ctrl+shift+f8",
          default_binding: "ctrl+shift+space",
        },
        dictation_profile_a: {
          id: "dictation_profile_a",
          name: "Translator",
          description: "",
          current_binding: "ctrl+f9",
          default_binding: "",
        },
      },
    });
    const remove = page.getByRole("button", {
      name: "Remove Translator hotkey",
      exact: true,
    });
    await defer(page, "clear_profile_hotkey", true);
    await remove.press("Enter");
    await pending(page);
    await expect(remove).toBeFocused();
    await expect(remove).toHaveAttribute("aria-disabled", "true");
    await remove.press("Enter");
    await release(page);
    await expect(remove).toBeFocused();
    await expect(page.getByRole("alert")).toContainText(
      "fixture rejected setting",
    );
    expect(await calls(page, "clear_profile_hotkey")).toBe(1);
    const reset = page.getByRole("button", {
      name: "Reset Cleanup shortcut",
      exact: true,
    });
    await defer(page, "reset_binding");
    await reset.press("Enter");
    await pending(page);
    await reset.press("Enter");
    await release(page);
    await expect(reset).toBeFocused();
    expect(await calls(page, "reset_binding")).toBe(1);
    await expect(
      page.getByRole("button", { name: /Change Cleanup shortcut/ }),
    ).toHaveAccessibleDescription(/Ctrl.*Shift.*Space/);
  });
}

test("duplicating a built-in with no prompt visibly requires a choice instead of implying the first prompt", async ({
  page,
}) => {
  await setup(page, "tauri", {
    assistant_selected_prompt_id: null,
    processing_profiles: [builtins[0], { ...builtins[1], prompt_id: null }],
  });
  await page
    .getByRole("button", {
      name: "Duplicate Selected-text assistant",
      exact: true,
    })
    .press("Enter");
  const select = page.getByRole("combobox", {
    name: "Profile prompt",
    exact: true,
  });
  await expect(select).toHaveValue("");
  await expect(select.locator("option:checked")).toHaveText("Select a prompt");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(page.getByRole("alert")).toHaveText(
    "Choose a prompt for this profile.",
  );
  expect(await calls(page, "add_processing_profile")).toBe(0);
  await select.selectOption("cleanup");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect(
    (await saved(page)).processing_profiles?.find(
      (p) => p.name === "Selected-text assistant (copy)",
    )?.prompt_id,
  ).toBe("cleanup");
});
