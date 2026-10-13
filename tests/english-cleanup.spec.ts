import { test, expect, type Page } from "@playwright/test";
import type { AppSettings, ProcessingProfile } from "../src/bindings";
import {
  setup,
  defer,
  pending,
  release,
  screenshot,
  type HarnessWindow,
} from "./support/settings-harness";

const checkboxName = "Use the English cleanup model for English speech";
const form = (page: Page) =>
  page.getByRole("form", { name: "English cleanup model", exact: true });
const checkbox = (page: Page) =>
  form(page).getByRole("checkbox", { name: checkboxName, exact: true });
const save = (page: Page) =>
  form(page).getByRole("button", { name: "Save", exact: true });
const englishCommand = "change_english_cleanup_settings";
const defaults = {
  styling: "semi-formal",
  structure: "prose",
  context: "general",
};
async function stored(page: Page): Promise<AppSettings> {
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
async function configure(page: Page) {
  await checkbox(page).check();
  await form(page)
    .getByRole("textbox", { name: "English model base URL", exact: true })
    .fill("  http://127.0.0.1:8082/v1  ");
  await form(page)
    .getByRole("textbox", { name: "English model name", exact: true })
    .fill("  S1-mini  ");
}
async function editCleanup(page: Page) {
  await page
    .getByRole("button", { name: "Edit Cleanup", exact: true })
    .press("Enter");
  return page.getByRole("dialog", { name: "Edit Cleanup", exact: true });
}

test("English group saves trimmed values with linked routing notes and survives reload", async ({
  page,
}, info) => {
  await setup(page);
  const headings = await page
    .getByRole("heading", { level: 2 })
    .allTextContents();
  expect(headings.indexOf("English cleanup model")).toBe(
    headings.findIndex((heading) => heading.startsWith("API")) + 1,
  );
  await expect(form(page).getByRole("textbox")).toHaveCount(2);
  await expect(checkbox(page)).toHaveAccessibleName(checkboxName);
  await expect(checkbox(page)).toHaveAccessibleDescription(
    /S1-mini ignores the cleanup prompt and custom words.*styling, structure and context.*Handy knows the speech is English.*reports that it detected English.*Auto speech language.*no detected language.*never used.*raw text.*failure sound.*notice appears in Handy's window/,
  );
  for (const id of [
    "english-prompt-note",
    "english-language-note",
    "english-failure-note",
  ]) {
    await expect(page.locator(`#${id}`)).toBeVisible();
  }
  await configure(page);
  await save(page).press("Enter");
  await expect(form(page).getByRole("status")).toHaveText("Saved");
  await expect(form(page).getByRole("status")).toHaveAttribute(
    "aria-live",
    "polite",
  );
  await expect(form(page).getByRole("status")).toHaveAttribute(
    "aria-atomic",
    "true",
  );
  await expect(save(page)).toBeFocused();
  expect((await stored(page)).english_cleanup).toEqual({
    enabled: true,
    base_url: "http://127.0.0.1:8082/v1",
    model: "S1-mini",
  });
  await reload(page);
  await expect(checkbox(page)).toBeChecked();
  await expect(
    form(page).getByRole("textbox", {
      name: "English model base URL",
      exact: true,
    }),
  ).toHaveValue("http://127.0.0.1:8082/v1");
  await expect(
    form(page).getByRole("textbox", {
      name: "English model name",
      exact: true,
    }),
  ).toHaveValue("S1-mini");
  const englishGroup = page.locator("div.space-y-2").filter({
    has: page.getByRole("heading", {
      name: "English cleanup model",
      exact: true,
    }),
  });
  await englishGroup.scrollIntoViewIfNeeded();
  await expect(englishGroup).toBeInViewport({ ratio: 1 });
  await screenshot(page, info, "english-cleanup-settings");
});

test("enabled English settings require URL and model with per-field accessible alerts", async ({
  page,
}) => {
  await setup(page);
  await checkbox(page).check();
  await save(page).press("Enter");
  const url = form(page).getByRole("textbox", {
    name: "English model base URL",
    exact: true,
  });
  const model = form(page).getByRole("textbox", {
    name: "English model name",
    exact: true,
  });
  await expect(url).toHaveAttribute("aria-invalid", "true");
  await expect(url).toHaveAccessibleDescription(
    "Enter an HTTP or HTTPS base URL of at most 2048 characters.",
  );
  await expect(model).toHaveAttribute("aria-invalid", "true");
  await expect(model).toHaveAccessibleDescription(
    "Enter an English model name of at most 256 characters.",
  );
  await expect(form(page).getByRole("alert")).toHaveCount(2);
  expect(await calls(page, englishCommand)).toBe(0);
  await expect(save(page)).toBeFocused();
  await configure(page);
  await save(page).press("Enter");
  await expect(form(page).getByRole("status")).toHaveText("Saved");
});

test("disabled routing accepts empty settings but still validates non-empty values", async ({
  page,
}) => {
  await setup(page);
  await save(page).press("Enter");
  await expect(form(page).getByRole("status")).toHaveText("Saved");
  const url = form(page).getByRole("textbox", {
    name: "English model base URL",
    exact: true,
  });
  const model = form(page).getByRole("textbox", {
    name: "English model name",
    exact: true,
  });
  for (const value of ["ftp://localhost/v1", `http://${"a".repeat(2042)}`]) {
    await url.fill(value);
    await save(page).press("Enter");
    await expect(url).toHaveAttribute("aria-invalid", "true");
  }
  await url.fill("");
  await model.fill("m".repeat(257));
  await save(page).press("Enter");
  await expect(model).toHaveAttribute("aria-invalid", "true");
  expect(await calls(page, englishCommand)).toBe(1);
});

for (const fail of [false, true]) {
  test(`English pending save keeps focus, freezes controls and guards re-entry (${fail ? "failure" : "success"})`, async ({
    page,
  }, info) => {
    await setup(page);
    await configure(page);
    const before = (await stored(page)).english_cleanup;
    await defer(page, englishCommand, fail);
    await save(page).press("Enter");
    await pending(page);
    await expect(save(page)).toBeFocused();
    await expect(save(page)).toHaveAttribute("aria-disabled", "true");
    await expect(checkbox(page)).toHaveAttribute("aria-disabled", "true");
    await save(page).press("Enter");
    await checkbox(page).focus();
    await checkbox(page).press("Space");
    await expect(checkbox(page)).toBeChecked();
    await expect(form(page).getByRole("textbox").first()).toHaveAttribute(
      "readonly",
      "",
    );
    expect(await calls(page, englishCommand)).toBe(1);
    await save(page).focus();
    await release(page);
    await expect(save(page)).toHaveAttribute("aria-disabled", "false");
    await expect(save(page)).toBeFocused();
    if (fail) {
      await expect(form(page).getByRole("alert")).toHaveText(
        "Could not save these settings. Check your settings and try again.",
      );
      await expect(save(page)).toHaveAccessibleDescription(
        "Could not save these settings. Check your settings and try again.",
      );
      expect((await stored(page)).english_cleanup).toEqual(before);
      await screenshot(page, info, "english-save-error");
      await page.evaluate(() => {
        (window as unknown as HarnessWindow).failCommand = undefined;
      });
      await save(page).press("Enter");
    }
    await expect(form(page).getByRole("status")).toHaveText("Saved");
    await expect(form(page).getByRole("alert")).toHaveCount(0);
  });
}

for (const code of ["base_url_invalid", "model_required", "model_too_long"]) {
  test(`English backend ${code} error describes the affected field`, async ({
    page,
  }) => {
    await setup(page);
    await configure(page);
    await page.evaluate(
      ({ command, code }) => {
        const w = window as unknown as HarnessWindow;
        w.failCommand = command;
        w.failCode = code;
      },
      { command: englishCommand, code },
    );
    await save(page).press("Enter");
    const field = form(page).getByRole("textbox", {
      name:
        code === "base_url_invalid"
          ? "English model base URL"
          : "English model name",
      exact: true,
    });
    await expect(field).toHaveAttribute("aria-invalid", "true");
    await expect(field).toHaveAccessibleDescription(/Enter an/);
    await expect(form(page).getByRole("alert")).toHaveCount(1);
    await expect(save(page)).toBeFocused();
  });
}

for (const enabled of [false, true]) {
  test(`profile checkbox explains global English cleanup ${enabled ? "on" : "off"} without a live region`, async ({
    page,
  }) => {
    await setup(page, "tauri", {
      english_cleanup: {
        enabled,
        base_url: "http://localhost:8082/v1",
        model: "S1-mini",
      },
    });
    const dialog = await editCleanup(page);
    const english = dialog.getByRole("checkbox", {
      name: checkboxName,
      exact: true,
    });
    const note = dialog.locator("#profile-english-disabled");
    await expect(english).toBeChecked();
    if (enabled) {
      await expect(note).toHaveCount(0);
      await expect(english).not.toHaveAccessibleDescription(
        /English cleanup is off/,
      );
    } else {
      await expect(note).toHaveText(
        "English cleanup is off in the English cleanup model settings, so this has no effect.",
      );
      await expect(note).toBeVisible();
      await expect(english).toHaveAccessibleDescription(
        /English cleanup is off in the English cleanup model settings, so this has no effect\./,
      );
      expect(await note.getAttribute("role")).toBeNull();
      expect(await note.getAttribute("aria-live")).toBeNull();
      await english.focus();
      await page.keyboard.press("Space");
      await expect(english).not.toBeChecked();
      await expect(note).toBeVisible();
      await expect(english).toBeFocused();
    }
  });
}

test("Cleanup seed can opt out then opt in with default controls and persist edits", async ({
  page,
}, info) => {
  await setup(page);
  let dialog = await editCleanup(page);
  const english = () =>
    dialog.getByRole("checkbox", { name: checkboxName, exact: true });
  await expect(english()).toBeChecked();
  await expect(english()).toHaveAccessibleName(checkboxName);
  await expect(english()).toHaveAccessibleDescription(
    /ignores this profile's prompt and custom words.*Leave this off for translation or rewriting profiles/,
  );
  await english().uncheck();
  await expect(
    dialog.getByRole("textbox", { name: "Profile styling", exact: true }),
  ).toHaveCount(0);
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect(
    (await stored(page)).processing_profiles![0].english_cleanup,
  ).toBeNull();
  await reload(page);
  dialog = await editCleanup(page);
  await expect(english()).not.toBeChecked();
  await english().check();
  for (const [field, value] of Object.entries(defaults)) {
    await expect(
      dialog.getByRole("textbox", { name: `Profile ${field}`, exact: true }),
    ).toHaveValue(value);
  }
  await dialog
    .getByRole("textbox", { name: "Profile styling", exact: true })
    .fill("  casual  ");
  await dialog
    .getByRole("textbox", { name: "Profile structure", exact: true })
    .fill("  bullets  ");
  await dialog
    .getByRole("textbox", { name: "Profile context", exact: true })
    .fill("  technical  ");
  await dialog.locator("#profile-english-controls").scrollIntoViewIfNeeded();
  await expect(dialog.locator("#profile-english-controls")).toBeInViewport({
    ratio: 1,
  });
  await screenshot(page, info, "profile-english-controls");
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Edit Cleanup", exact: true }),
  ).toBeFocused();
  await reload(page);
  dialog = await editCleanup(page);
  await expect(
    dialog.getByRole("textbox", { name: "Profile context", exact: true }),
  ).toHaveValue("technical");
  expect((await stored(page)).processing_profiles![0].english_cleanup).toEqual({
    styling: "casual",
    structure: "bullets",
    context: "technical",
  });
});

test("profile model override succeeds and its precedence note describes relevant controls", async ({
  page,
}, info) => {
  await setup(page);
  const dialog = await editCleanup(page);
  await dialog
    .getByRole("combobox", { name: "Profile model", exact: true })
    .selectOption("custom");
  const note =
    "This profile's own model replaces the English cleanup model, so the English cleanup model is not used.";
  await expect(dialog.getByText(note, { exact: true })).toBeVisible();
  await expect(
    dialog.getByRole("combobox", { name: "Profile model", exact: true }),
  ).toHaveAccessibleDescription(note);
  await expect(
    dialog.getByRole("checkbox", { name: checkboxName, exact: true }),
  ).toHaveAccessibleDescription(
    new RegExp("replaces the English cleanup model"),
  );
  await expect(
    dialog.getByRole("textbox", { name: "Profile styling", exact: true }),
  ).toHaveAccessibleDescription(/replaces the English cleanup model/);
  await dialog
    .getByRole("textbox", { name: "Profile model name", exact: true })
    .fill("qwen3.5:9b");
  await screenshot(page, info, "profile-english-override");
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect((await stored(page)).processing_profiles![0]).toMatchObject({
    model: { provider_id: "custom", model: "qwen3.5:9b" },
    english_cleanup: defaults,
  });
});

test("S1-named profile override uses its own controls and omits the unused-model note", async ({
  page,
}) => {
  await setup(page);
  const dialog = await editCleanup(page);
  const provider = dialog.getByRole("combobox", {
    name: "Profile model",
    exact: true,
  });
  await provider.selectOption("custom");
  const model = dialog.getByRole("textbox", {
    name: "Profile model name",
    exact: true,
  });
  for (const value of ["S1-MINI", "vendor/S1-MiNi:latest"]) {
    await model.fill(value);
    await expect(dialog.locator("#profile-english-override")).toHaveCount(0);
    await expect(provider).not.toHaveAccessibleDescription(
      /English cleanup model is not used/,
    );
    for (const field of Object.keys(defaults)) {
      await expect(
        dialog.getByRole("textbox", { name: `Profile ${field}`, exact: true }),
      ).not.toHaveAccessibleDescription(/English cleanup model is not used/);
    }
  }
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(dialog).toHaveCount(0);
  expect((await stored(page)).processing_profiles![0]).toMatchObject({
    model: { provider_id: "custom", model: "vendor/S1-MiNi:latest" },
    english_cleanup: defaults,
  });
});

test("server controls_invalid maps to all three fields and tells the user to fix them", async ({
  page,
}) => {
  await setup(page);
  const dialog = await editCleanup(page);
  await page.evaluate(() => {
    const w = window as unknown as HarnessWindow;
    w.failCommand = "update_processing_profile";
    w.failCode = "controls_invalid";
  });
  const saveProfile = dialog.getByRole("button", { name: "Save", exact: true });
  await saveProfile.press("Enter");
  await expect(dialog.getByRole("alert")).toHaveCount(3);
  for (const field of Object.keys(defaults)) {
    const input = dialog.getByRole("textbox", {
      name: `Profile ${field}`,
      exact: true,
    });
    await expect(input).toHaveAttribute("aria-invalid", "true");
    await expect(input).toHaveAccessibleDescription(
      "Use 1 to 40 characters without brackets or control characters. Fix the profile styling, structure and context fields, then save again.",
    );
    await expect(input).toHaveAttribute(
      "aria-describedby",
      `profile-english-help profile-${field}-error`,
    );
  }
  await expect(saveProfile).toBeFocused();
  expect(await calls(page, "update_processing_profile")).toBe(1);
  expect((await stored(page)).processing_profiles![0].english_cleanup).toEqual(
    defaults,
  );
  await page.evaluate(() => {
    (window as unknown as HarnessWindow).failCommand = undefined;
  });
  await dialog
    .getByRole("textbox", { name: "Profile styling", exact: true })
    .fill("formal");
  await saveProfile.press("Enter");
  await expect(dialog).toHaveCount(0);
});

for (const [field, value] of [
  ["styling", ""],
  ["structure", "[bullets]"],
  ["context", "x".repeat(41)],
  ["context", "bad\tcontext"],
]) {
  test(`profile ${field} rejects invalid S1 control ${JSON.stringify(value)}`, async ({
    page,
  }) => {
    await setup(page);
    const dialog = await editCleanup(page);
    const input = dialog.getByRole("textbox", {
      name: `Profile ${field}`,
      exact: true,
    });
    await input.fill(value);
    await dialog
      .getByRole("button", { name: "Save", exact: true })
      .press("Enter");
    await expect(input).toHaveAttribute("aria-invalid", "true");
    await expect(input).toHaveAccessibleDescription(
      `Use 1 to 40 characters without brackets or control characters. Profile ${field} must be 1 to 40 characters without brackets or control characters.`,
    );
    await expect(dialog.getByRole("alert")).toHaveCount(1);
    expect(await calls(page, "update_processing_profile")).toBe(0);
    await expect(
      dialog.getByRole("button", { name: "Save", exact: true }),
    ).toBeFocused();
  });
}

test("profile English fields stay focusable and immutable during a refused async save", async ({
  page,
}) => {
  await setup(page);
  const dialog = await editCleanup(page);
  await defer(page, "update_processing_profile", true);
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await pending(page);
  await dialog
    .getByRole("checkbox", { name: checkboxName, exact: true })
    .focus();
  await page.keyboard.press("Space");
  await expect(
    dialog.getByRole("checkbox", { name: checkboxName, exact: true }),
  ).toBeChecked();
  await expect(
    dialog.getByRole("textbox", { name: "Profile styling", exact: true }),
  ).toHaveAttribute("readonly", "");
  await page.keyboard.press("Escape");
  await expect(dialog).toBeVisible();
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  expect(await calls(page, "update_processing_profile")).toBe(1);
  await release(page);
  await expect(dialog.getByRole("alert")).toBeVisible();
  await expect(
    dialog.getByRole("checkbox", { name: checkboxName, exact: true }),
  ).toHaveAccessibleDescription(/Could not save/);
  await expect(
    dialog.getByRole("button", { name: "Save", exact: true }),
  ).toBeFocused();
});

test("translation profiles default to no English opt-in and selection commands have no English fields", async ({
  page,
}) => {
  const selection: ProcessingProfile = {
    id: "select",
    name: "Rewrite selection",
    kind: "selection_command",
    prompt_id: "assistant",
    model: null,
    speech_language: null,
    english_cleanup: null,
  };
  await setup(page, "tauri", { processing_profiles: [selection] });
  await page
    .getByRole("button", { name: "Edit Rewrite selection", exact: true })
    .press("Enter");
  await expect(
    page
      .getByRole("dialog")
      .getByRole("checkbox", { name: checkboxName, exact: true }),
  ).toHaveCount(0);
  await expect(
    page
      .getByRole("dialog")
      .getByRole("textbox", { name: "Profile styling", exact: true }),
  ).toHaveCount(0);
  await page.keyboard.press("Escape");
  await page
    .getByRole("button", { name: "Add dictation profile", exact: true })
    .press("Enter");
  const dialog = page.getByRole("dialog");
  await expect(
    dialog.getByRole("checkbox", { name: checkboxName, exact: true }),
  ).not.toBeChecked();
  await dialog
    .getByRole("textbox", { name: "Name", exact: true })
    .fill("Polish to English");
  await dialog
    .getByRole("button", { name: "Save", exact: true })
    .press("Enter");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  expect(
    (await stored(page)).processing_profiles!.find(
      (p) => p.name === "Polish to English",
    )!.english_cleanup,
  ).toBeNull();
});

test("History shows the saved profile name and tolerates null and legacy entries", async ({
  page,
}, info) => {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await setup(page);
  await page.evaluate(() => {
    const base = {
      file_name: "test.wav",
      timestamp: 1700000000,
      saved: false,
      title: "",
      post_processed_text: null,
      post_process_prompt: null,
      post_process_requested: false,
    };
    localStorage.setItem(
      "test-history",
      JSON.stringify([
        {
          ...base,
          id: 1,
          transcription_text: "Translated text",
          profile_id: "translator",
          profile_name: "Polish to English",
        },
        {
          ...base,
          id: 2,
          transcription_text: "Unnamed cleanup text",
          profile_id: "builtin_cleanup",
          profile_name: null,
        },
        { ...base, id: 3, transcription_text: "Legacy text" },
      ]),
    );
  });
  await page.getByRole("button", { name: "History", exact: true }).click();
  await expect(
    page.getByText("Profile: Polish to English", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText(/^Profile:/)).toHaveCount(1);
  await expect(page.getByText("Legacy text", { exact: true })).toBeVisible();
  await expect(
    page.getByText("Unnamed cleanup text", { exact: true }),
  ).toBeVisible();
  expect(errors).toEqual([]);
  await screenshot(page, info, "history-profile");
});

test("History retry explains a deleted profile and keeps generic errors distinct", async ({
  page,
}) => {
  await setup(page);
  await page.evaluate(() => {
    localStorage.setItem(
      "test-history",
      JSON.stringify([
        {
          id: 1,
          file_name: "test.wav",
          timestamp: 1700000000,
          saved: false,
          title: "",
          transcription_text: "Saved translation",
          post_processed_text: null,
          post_process_prompt: null,
          post_process_requested: true,
          profile_id: "deleted",
          profile_name: "Polish to English",
        },
      ]),
    );
    const w = window as unknown as HarnessWindow;
    w.failCommand = "retry_history_entry_transcription";
    w.failCode = "profile_not_found";
  });
  await page
    .getByRole("button", { name: "History", exact: true })
    .press("Enter");
  const retry = page.getByRole("button", {
    name: "Re-transcribe",
    exact: true,
  });
  await retry.press("Enter");
  await expect(
    page.getByText(
      "This recording's profile no longer exists. Create a profile and make a new recording.",
      { exact: true },
    ),
  ).toBeVisible();
  await expect(retry).toBeEnabled();
  await page.evaluate(() => {
    (window as unknown as HarnessWindow).failCode = "other_error";
  });
  await retry.press("Enter");
  await expect(
    page.getByText("Failed to re-transcribe. Please try again.", {
      exact: true,
    }),
  ).toBeVisible();
});
