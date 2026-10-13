import { test, expect, type Page } from "@playwright/test";
import type { AssistantRecoveryItem } from "../src/bindings";
import {
  setup,
  screenshot,
  emitKeys,
  defer,
  pending,
  release,
  type HarnessWindow,
} from "./support/settings-harness";
test("accessible settings and keyboard prompt dialog entry, trap and return", async ({
  page,
}, testInfo) => {
  await setup(page);
  await expect(
    page.getByRole("textbox", { name: "Base URL", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("textbox", { name: "API Key", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("combobox", { name: "Model", exact: true }),
  ).toBeVisible();
  const trigger = page.getByRole("button", {
    name: "Edit Selected-text assistant",
    exact: true,
  });
  await trigger.focus();
  await page.keyboard.press("Enter");
  const dialog = page.getByRole("dialog", {
    name: "Edit Selected-text assistant",
  });
  const name = dialog.getByRole("textbox", {
    name: "Name",
    exact: true,
  });
  await expect(name).toBeFocused();
  await page.keyboard.press("Shift+Tab");
  await expect(
    dialog.getByRole("button", { name: "Close", exact: true }),
  ).toBeFocused();
  await page.keyboard.press("Shift+Tab");
  await expect(
    dialog.getByRole("button", { name: "Save", exact: true }),
  ).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(trigger).toBeFocused();
  await trigger.press("Enter");
  await name.fill("Personal assistant");
  await dialog
    .getByRole("combobox", { name: "Profile prompt", exact: true })
    .selectOption("cleanup");
  await dialog.getByRole("button", { name: "Save", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Edit Personal assistant", exact: true }),
  ).toBeFocused();
  await page.reload();
  await page.getByRole("button", { name: "Post Process", exact: true }).click();
  await page.getByRole("button", { name: "Edit Personal assistant" }).click();
  await expect(
    page
      .getByRole("dialog")
      .getByRole("textbox", { name: "Name", exact: true }),
  ).toHaveValue("Personal assistant");
  await screenshot(page, testInfo, "assistant-prompt-dialog");
});

test("endpoint, model, assistant prompt, options and shortcut persist after reload", async ({
  page,
}, testInfo) => {
  await setup(page);
  await page
    .getByRole("textbox", { name: "Base URL", exact: true })
    .fill("http://127.0.0.1:8080/v1");
  await page
    .getByRole("textbox", { name: "Base URL", exact: true })
    .press("Tab");
  const model = page.getByRole("combobox", { name: "Model", exact: true });
  await model.fill("qwen3.5:9b");
  await model.press("Enter");
  await page
    .getByRole("button", { name: "Edit Selected-text assistant", exact: true })
    .click();
  await page
    .getByRole("combobox", { name: "Profile prompt", exact: true })
    .selectOption("cleanup");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .click();
  await page
    .getByRole("spinbutton", { name: "Temperature (optional)", exact: true })
    .fill("0.3");
  await page
    .getByRole("spinbutton", {
      name: "Maximum output tokens (optional)",
      exact: true,
    })
    .fill("1024");
  await page
    .getByRole("spinbutton", { name: "Overall timeout (seconds)", exact: true })
    .fill("90");
  await page
    .getByRole("spinbutton", {
      name: "Connection timeout (seconds)",
      exact: true,
    })
    .fill("7");
  await page.getByRole("button", { name: "Save request options" }).click();
  const shortcut = page.getByRole("button", {
    name: /Change Selected[- ]text assistant shortcut/i,
  });
  await shortcut.focus();
  await shortcut.press("Enter");
  await expect(shortcut).toHaveAttribute("aria-pressed", "true");
  await page.keyboard.down("Control");
  await page.keyboard.down("Alt");
  await page.keyboard.press("F10");
  await page.keyboard.up("Alt");
  await page.keyboard.up("Control");
  await expect(shortcut).toHaveAttribute("aria-pressed", "false");
  await expect(shortcut).toBeFocused();
  await page.reload();
  await page.getByRole("button", { name: "Post Process", exact: true }).click();
  await expect(
    page.getByRole("textbox", { name: "Base URL", exact: true }),
  ).toHaveValue("http://127.0.0.1:8080/v1");
  await page
    .getByRole("button", { name: "Edit Selected-text assistant", exact: true })
    .click();
  await expect(
    page.getByRole("combobox", { name: "Profile prompt", exact: true }),
  ).toHaveValue("cleanup");
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Cancel", exact: true })
    .click();
  await expect(
    page.getByRole("spinbutton", {
      name: "Temperature (optional)",
      exact: true,
    }),
  ).toHaveValue("0.3");
  await expect(shortcut).toContainText("F10");
  const saved = await page.evaluate(() =>
    JSON.parse(localStorage.getItem("test-settings")!),
  );
  expect(saved.post_process_models.custom).toBe("qwen3.5:9b");
  expect(saved.post_process_request_options).toEqual({
    temperature: 0.3,
    max_output_tokens: 1024,
    timeout_secs: 90,
    connect_timeout_secs: 7,
  });
  await page
    .getByRole("button", { name: "Save request options" })
    .scrollIntoViewIfNeeded();
  await screenshot(page, testInfo, "assistant-settings");
});

test("validation and backend rejection are accessible and preserve persisted values", async ({
  page,
}, testInfo) => {
  await setup(page);
  const connection = page.getByRole("spinbutton", {
    name: "Connection timeout (seconds)",
    exact: true,
  });
  await connection.fill("70");
  await page.getByRole("button", { name: "Save request options" }).click();
  await expect(connection).toHaveAttribute("aria-invalid", "true");
  await expect(page.getByRole("alert")).toContainText(
    "Connection timeout must be a whole number",
  );
  await connection.fill("6");
  await page.evaluate(() => {
    (window as unknown as HarnessWindow).failCommand =
      "change_post_process_request_options";
  });
  await page.getByRole("button", { name: "Save request options" }).click();
  await expect(page.getByRole("alert")).toContainText("Could not save");
  expect(
    await page.evaluate(
      () =>
        JSON.parse(localStorage.getItem("test-settings")!)
          .post_process_request_options.connect_timeout_secs,
    ),
  ).toBe(5);
  await page.evaluate(() => {
    (window as unknown as HarnessWindow).failCommand =
      "update_processing_profile";
  });
  await page
    .getByRole("button", { name: "Edit Selected-text assistant" })
    .click();
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true })
    .click();
  await expect(page.getByRole("dialog").getByRole("alert")).toContainText(
    "Could not save",
  );
  await screenshot(page, testInfo, "assistant-save-error");
});

test("cleanup and context error notices are distinct, retained and open History", async ({
  page,
}, testInfo) => {
  await setup(page);
  await page.evaluate(() =>
    localStorage.setItem(
      "test-history",
      JSON.stringify([
        {
          id: 1,
          file_name: "fixture.wav",
          timestamp: 1,
          transcription_text: "Raw technical transcript",
          post_process_requested: true,
          post_processed_text: null,
          post_process_prompt: null,
          saved: false,
        },
      ]),
    ),
  );
  await page.evaluate(() =>
    (window as unknown as HarnessWindow).emitTestEvent(
      "cleanup-fallback",
      null,
    ),
  );
  await expect(page.getByRole("alert")).toContainText(
    "Cleanup failed. Raw text was used.",
  );
  await expect(
    page.getByText("Transcription failed", { exact: true }),
  ).toHaveCount(0);
  await page.evaluate(() =>
    (window as unknown as HarnessWindow).emitTestEvent(
      "assistant-error-event",
      {
        stage: "generation",
        code: "llm_context_exceeded",
        reason: "context_exceeded",
        detail: null,
      },
    ),
  );
  await expect(page.getByRole("alert")).toContainText(
    "Text too long for this model.",
  );
  await page
    .getByRole("button", { name: "History", exact: true })
    .last()
    .click();
  await expect(page.getByRole("alert")).toContainText(
    "Text too long for this model.",
  );
  await expect(
    page.getByText("Raw technical transcript", { exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await page.evaluate(() =>
    (window as unknown as HarnessWindow).emitTestEvent(
      "assistant-error-event",
      {
        stage: "generation",
        code: "llm_provider",
        reason: "provider",
        detail: null,
      },
    ),
  );
  await expect(page.getByRole("alert")).toContainText("Assistant failed.");
});

test("History recovery uses explicit keyboard Copy and Dismiss without a dialog", async ({
  page,
}, testInfo) => {
  await setup(page);
  const item: AssistantRecoveryItem = {
    id: 1,
    created_at: 1,
    target: "selected",
    outcome: "refused",
    reason: "target_stale",
    detail: "focus_changed",
    clipboard: "untouched",
    instruction: "Make it shorter",
    generated: "Generated reply",
  };
  await page.evaluate(
    (item) => localStorage.setItem("test-recovery", JSON.stringify([item])),
    item,
  );
  await page.getByRole("button", { name: "History", exact: true }).click();
  await expect(
    page.getByRole("textbox", { name: "Generated text" }),
  ).toHaveValue("Generated reply");
  const copy = page.getByRole("button", { name: "Copy generated text" });
  await copy.focus();
  await copy.press("Enter");
  expect(await page.evaluate(() => localStorage.getItem("test-copy"))).toBe(
    "generated",
  );
  await expect(
    page.getByRole("textbox", { name: "Generated text" }),
  ).toBeVisible();
  await screenshot(page, testInfo, "history-recovery");
  await page
    .getByRole("button", { name: "Dismiss", exact: true })
    .press("Enter");
  await expect(
    page.getByRole("textbox", { name: "Generated text" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("heading", {
      name: "Assistant results awaiting recovery",
      exact: true,
    }),
  ).toBeFocused();
  await expect(page.getByRole("dialog")).toHaveCount(0);
});

test("handy-keys shortcut has keyboard activation and accessible conflict error", async ({
  page,
}) => {
  await setup(page, "handy_keys");
  const shortcut = page.getByRole("button", {
    name: /Change Selected[- ]text assistant shortcut/i,
  });
  await shortcut.focus();
  await shortcut.press("Enter");
  await expect(shortcut).toHaveAttribute("aria-pressed", "true");
  await page.evaluate(() => {
    const w = window as unknown as HarnessWindow;
    w.emitTestEvent("handy-keys-event", {
      hotkey_string: "ctrl+space",
      is_key_down: true,
      key: "space",
      modifiers: ["ctrl"],
    });
    w.emitTestEvent("handy-keys-event", {
      hotkey_string: "ctrl+space",
      is_key_down: false,
      key: "space",
      modifiers: ["ctrl"],
    });
  });
  await expect(shortcut).toHaveAttribute("aria-pressed", "false");
  await expect(page.getByRole("alert").first()).toContainText("already used");
  await expect(shortcut).toBeFocused();
});

for (const backend of ["tauri", "handy_keys"]) {
  test(`${backend}: Tab then Enter on Cancel does not become a shortcut`, async ({
    page,
  }) => {
    await setup(page, backend);
    const shortcut = page.getByRole("button", {
      name: /Change Selected[- ]text assistant shortcut/i,
    });
    await shortcut.focus();
    await shortcut.press("Enter");
    await expect(shortcut).toHaveAttribute("aria-pressed", "true");
    await page.keyboard.press("Tab");
    await expect(
      page.getByRole("button", { name: "Cancel", exact: true }),
    ).toBeFocused();
    if (backend === "handy_keys") await emitKeys(page, "Enter", "enter");
    await page.keyboard.press("Enter");
    await expect(shortcut).toHaveAttribute("aria-pressed", "false");
    await expect(shortcut).toBeFocused();
    expect(await binding(page)).toBe("ctrl+alt+space");
  });

  test(`${backend}: Escape cancels capture without a binding write`, async ({
    page,
  }) => {
    await setup(page, backend);
    const shortcut = page.getByRole("button", {
      name: /Change Selected[- ]text assistant shortcut/i,
    });
    for (const route of backend === "handy_keys"
      ? ["dom", "backend"]
      : ["dom"]) {
      await shortcut.focus();
      await shortcut.press("Enter");
      await expect(shortcut).toHaveAttribute("aria-pressed", "true");
      if (route === "dom") await page.keyboard.press("Escape");
      else await emitKeys(page, "Escape", "escape");
      await expect(shortcut).toHaveAttribute("aria-pressed", "false");
      await expect(shortcut).toBeFocused();
      expect(await binding(page)).toBe("ctrl+alt+space");
    }
    expect(
      await page.evaluate(
        () =>
          (window as unknown as HarnessWindow).commandCalls.filter(
            (c) => c === "change_binding",
          ).length,
      ),
    ).toBe(0);
  });

  test(`${backend}: capture exposes binding and state, Tab reaches Cancel and Enter/Space cancel safely`, async ({
    page,
  }) => {
    await setup(page, backend);
    const shortcut = page.getByRole("button", {
      name: /Change Selected[- ]text assistant shortcut/i,
    });
    await expect(shortcut).toHaveAccessibleDescription(/Ctrl.*Alt.*Space/);
    for (const activate of ["Enter", "Space"]) {
      await shortcut.focus();
      await shortcut.press("Enter");
      await expect(shortcut).toHaveAttribute("aria-pressed", "true");
      await expect(shortcut).toHaveAccessibleDescription(/Press keys/);
      const status = shortcut.getByRole("status");
      await expect(status).toHaveAttribute("aria-live", "polite");
      await page.keyboard.press("Tab");
      const cancel = page.getByRole("button", { name: "Cancel", exact: true });
      await expect(cancel).toBeFocused();
      if (backend === "handy_keys") {
        await emitKeys(page, activate, activate.toLowerCase());
      }
      await page.keyboard.press(activate);
      await expect(shortcut).toHaveAttribute("aria-pressed", "false");
      await expect(shortcut).toBeFocused();
      await expect(shortcut).toHaveAccessibleDescription(/Ctrl.*Alt.*Space/);
      expect(await binding(page)).toBe("ctrl+alt+space");
      expect(
        await page.evaluate(
          () =>
            (window as unknown as HarnessWindow).commandCalls.filter(
              (c) => c === "change_binding",
            ).length,
        ),
      ).toBe(0);
    }
  });

  test(`${backend}: Shift+Tab and Escape never save navigation keys, bare F-keys still work`, async ({
    page,
  }) => {
    await setup(page, backend);
    const shortcut = page.getByRole("button", {
      name: /Change Selected[- ]text assistant shortcut/i,
    });
    await shortcut.focus();
    await shortcut.press("Enter");
    await page.keyboard.press("Shift+Tab");
    if (backend === "handy_keys")
      await emitKeys(page, "Tab", "shift+tab", ["shift"]);
    await expect(shortcut).toHaveAttribute("aria-pressed", "false");
    expect(await binding(page)).toBe("ctrl+alt+space");
    await shortcut.focus();
    await shortcut.press("Enter");
    await page.keyboard.press("Escape");
    await expect(shortcut).toHaveAttribute("aria-pressed", "false");
    await expect(shortcut).toBeFocused();
    if (backend === "handy_keys") {
      await shortcut.press("Enter");
      await emitKeys(page, "Escape", "escape");
      await expect(shortcut).toHaveAttribute("aria-pressed", "false");
      await expect(shortcut).toBeFocused();
      expect(await binding(page)).toBe("ctrl+alt+space");
    }
    await shortcut.press("Enter");
    if (backend === "handy_keys") {
      await page.evaluate(() =>
        (window as unknown as HarnessWindow).emitTestEvent("handy-keys-event", {
          modifiers: [],
          key: "F10",
          is_key_down: true,
          hotkey_string: "f10",
        }),
      );
    } else await page.keyboard.down("F10");
    await expect(shortcut).toHaveAccessibleDescription(/F10/);
    if (backend === "handy_keys")
      await page.evaluate(() =>
        (window as unknown as HarnessWindow).emitTestEvent("handy-keys-event", {
          modifiers: [],
          key: "F10",
          is_key_down: false,
          hotkey_string: "f10",
        }),
      );
    else await page.keyboard.up("F10");
    await expect(shortcut).toHaveAttribute("aria-pressed", "false");
    await expect(shortcut).toHaveAccessibleDescription(/F10/);
    expect(await binding(page)).toBe("f10");
  });

  test(`${backend}: an unmodified Enter cannot replace the binding`, async ({
    page,
  }) => {
    await setup(page, backend);
    const shortcut = page.getByRole("button", {
      name: /Change Selected[- ]text assistant shortcut/i,
    });
    await shortcut.focus();
    await shortcut.press("Enter");
    if (backend === "handy_keys") await emitKeys(page, "Enter", "enter");
    else await page.keyboard.press("Enter");
    await expect(page.getByRole("alert").first()).toContainText(
      "Enter, Escape and Tab",
    );
    expect(await binding(page)).toBe("ctrl+alt+space");
    await expect(shortcut).toBeFocused();
  });
}

async function binding(page: Page) {
  return page.evaluate(
    () =>
      JSON.parse(localStorage.getItem("test-settings")!).bindings.assistant
        .current_binding,
  );
}

async function captureCommands(page: Page, backend: string) {
  const commands =
    backend === "tauri"
      ? ["suspend_all_bindings", "resume_all_bindings"]
      : ["start_handy_keys_recording", "stop_handy_keys_recording"];
  return page.evaluate(
    (commands) =>
      (window as unknown as HarnessWindow).commandCalls.filter((c) =>
        commands.includes(c),
      ),
    commands,
  );
}

for (const backend of ["tauri", "handy_keys"]) {
  for (const activation of ["Enter", "Space"]) {
    test(`${backend}: orphan ${activation} release after Change activation keeps capture open`, async ({
      page,
    }) => {
      await setup(page, backend);
      const shortcut = page.getByRole("button", {
        name: /Change Selected[- ]text assistant shortcut/i,
      });
      await shortcut.focus();
      await shortcut.press(activation);
      await expect(shortcut).toHaveAttribute("aria-pressed", "true");
      if (backend === "handy_keys") {
        await page.evaluate(
          (key) =>
            (window as unknown as HarnessWindow).emitTestEvent(
              "handy-keys-event",
              {
                key,
                is_key_down: false,
                hotkey_string: key,
                modifiers: [],
              },
            ),
          activation.toLowerCase(),
        );
      } else await page.keyboard.up(activation);
      await expect(shortcut).toHaveAccessibleDescription(/Press keys/);
      await expect(shortcut).toHaveAttribute("aria-pressed", "true");
      await expect(page.getByRole("alert")).toHaveCount(0);
      expect(await binding(page)).toBe("ctrl+alt+space");
      expect(await captureCommands(page, backend)).toEqual([
        backend === "tauri"
          ? "suspend_all_bindings"
          : "start_handy_keys_recording",
      ]);
      if (backend === "handy_keys")
        await emitKeys(page, "F10", "ctrl+f10", ["ctrl"]);
      else await page.keyboard.press("Control+F10");
      await expect(shortcut).toHaveAttribute("aria-pressed", "false");
      expect(await binding(page)).toBe("ctrl+f10");
      await expect(page.getByRole("alert")).toHaveCount(0);
    });
  }

  test(`${backend}: language change during capture keeps its session until commit`, async ({
    page,
  }) => {
    await setup(page, backend);
    const shortcut = page.locator(
      'button[aria-describedby~="shortcut-assistant-value"]',
    );
    await shortcut.focus();
    await shortcut.press("Enter");
    await expect(shortcut).toHaveAttribute("aria-pressed", "true");
    await page.evaluate(async () => {
      const modulePath = "/src/i18n/index.ts";
      const { default: i18n } = await import(modulePath);
      await i18n.changeLanguage("de");
    });
    await expect(page.locator("html")).toHaveAttribute("lang", "de");
    await expect(shortcut).toHaveAttribute("aria-pressed", "true");
    expect(await captureCommands(page, backend)).toEqual([
      backend === "tauri"
        ? "suspend_all_bindings"
        : "start_handy_keys_recording",
    ]);
    if (backend === "handy_keys")
      await emitKeys(page, "F10", "ctrl+f10", ["ctrl"]);
    else await page.keyboard.press("Control+F10");
    await expect(shortcut).toHaveAttribute("aria-pressed", "false");
    expect(await binding(page)).toBe("ctrl+f10");
    expect(await captureCommands(page, backend)).toEqual(
      backend === "tauri"
        ? ["suspend_all_bindings", "resume_all_bindings"]
        : ["start_handy_keys_recording", "stop_handy_keys_recording"],
    );
  });
}

test("cleanup and paste failure without History gives one re-dictation alert", async ({
  page,
}) => {
  await setup(page);
  await page.evaluate(() =>
    (window as unknown as HarnessWindow).emitTestEvent("paste-error", {
      cleanup_failed: true,
      raw_history_available: false,
    }),
  );
  const alert = page.getByRole("alert");
  await expect(alert).toHaveCount(1);
  await expect(alert).toContainText("Cleanup and paste failed");
  await expect(alert).toContainText("could not be saved to History");
  await expect(alert).toContainText("Dictate it again");
  await expect(
    page.getByText("Failed to Paste Text", { exact: true }),
  ).toHaveCount(0);
  // Only the sidebar History button remains, with no unavailable recovery action.
  await expect(
    page.getByRole("button", { name: "History", exact: true }),
  ).toHaveCount(1);
});

for (const backend of ["tauri", "handy_keys"]) {
  test(`${backend}: modified navigation commits when modifiers are released first`, async ({
    page,
  }) => {
    await setup(page, backend);
    const shortcut = page.getByRole("button", {
      name: /Change Selected[- ]text assistant shortcut/i,
    });
    for (const key of ["Tab", "Escape"]) {
      await shortcut.focus();
      await shortcut.press("Enter");
      await expect(shortcut).toHaveAttribute("aria-pressed", "true");
      if (backend === "tauri") {
        await page.keyboard.down("Control");
        await page.keyboard.down(key);
        await page.keyboard.up("Control");
        await page.keyboard.up(key);
      } else {
        await page.evaluate((key) => {
          const w = window as unknown as HarnessWindow;
          w.emitTestEvent("handy-keys-event", {
            modifiers: ["ctrl"],
            key,
            is_key_down: true,
            hotkey_string: `ctrl+${key.toLowerCase()}`,
          });
          w.emitTestEvent("handy-keys-event", {
            modifiers: [],
            key: null,
            is_key_down: false,
            hotkey_string: "",
          });
          w.emitTestEvent("handy-keys-event", {
            modifiers: [],
            key,
            is_key_down: false,
            hotkey_string: key.toLowerCase(),
          });
        }, key);
      }
      await expect(shortcut).toHaveAttribute("aria-pressed", "false");
      expect(await binding(page)).toBe(
        `ctrl+${backend === "tauri" && key === "Escape" ? "esc" : key.toLowerCase()}`,
      );
    }
  });
  for (const exit of [
    "Tab out",
    "Shift+Tab out",
    "click outside",
    "section unmount",
    "window blur",
    "Cancel",
    "Escape",
    "commit",
    "error",
    "exact reviewer probe",
    "pending start unmount",
    "pending start focus out",
    "pending start window blur",
  ]) {
    test(`${backend}: capture lifecycle balances commands on ${exit}`, async ({
      page,
    }) => {
      await setup(page, backend);
      const shortcut = page.getByRole("button", {
        name: /Change Selected[- ]text assistant shortcut/i,
      });
      const general = page.getByRole("button", {
        name: "General",
        exact: true,
      });
      const start =
        backend === "tauri"
          ? "suspend_all_bindings"
          : "start_handy_keys_recording";
      const stop =
        backend === "tauri"
          ? "resume_all_bindings"
          : "stop_handy_keys_recording";
      if (exit.startsWith("pending start")) await defer(page, start);
      await shortcut.focus();
      await shortcut.press("Enter");
      if (exit.startsWith("pending start")) {
        await pending(page);
        await shortcut.press("Enter");
        if (exit === "pending start unmount") await general.click();
        else if (exit === "pending start focus out")
          await page.keyboard.press("Shift+Tab");
        else await page.evaluate(() => window.dispatchEvent(new Event("blur")));
        await release(page);
      } else {
        await expect(shortcut).toHaveAttribute("aria-pressed", "true");
        if (exit === "Tab out") {
          for (let i = 0; i < 3; i++) await page.keyboard.press("Tab");
        } else if (exit === "Shift+Tab out") {
          await page.keyboard.press("Shift+Tab");
        } else if (exit === "click outside") {
          await page
            .getByRole("textbox", { name: "Base URL", exact: true })
            .click();
        } else if (exit === "section unmount") {
          // A programmatic section switch exercises unmount without a prior blur.
          await general.evaluate((button: HTMLButtonElement) => button.click());
        } else if (exit === "window blur") {
          await page.evaluate(() => window.dispatchEvent(new Event("blur")));
        } else if (exit === "Cancel") {
          await page.keyboard.press("Tab");
          await page.keyboard.press("Enter");
        } else if (exit === "Escape") {
          await page.keyboard.press("Escape");
        } else if (exit === "exact reviewer probe") {
          await page.keyboard.press("Tab");
          await page.keyboard.press("Tab");
          for (
            let i = 0;
            i < 40 &&
            !(await general.evaluate(
              (button) => button === document.activeElement,
            ));
            i++
          )
            await page.keyboard.press("Shift+Tab");
          await expect(general).toBeFocused();
          await page.keyboard.press("Enter");
        } else {
          if (exit === "error")
            await page.evaluate(() => {
              (window as unknown as HarnessWindow).failCommand =
                "change_binding";
            });
          if (backend === "handy_keys") await emitKeys(page, "F10", "f10");
          else await page.keyboard.press("F10");
          await expect(shortcut).toHaveAttribute("aria-pressed", "false");
          if (exit === "error")
            await expect(page.getByRole("alert").first()).toContainText(
              "fixture rejected setting",
            );
        }
      }
      await expect
        .poll(() => captureCommands(page, backend))
        .toEqual([start, stop]);
      if (!exit.includes("unmount") && exit !== "exact reviewer probe")
        await expect(shortcut).toHaveAttribute("aria-pressed", "false");
      // Subsequent unmount/blur must not stop or resume the completed session again.
      await general.click();
      await page.evaluate(() => window.dispatchEvent(new Event("blur")));
      expect(await captureCommands(page, backend)).toEqual([start, stop]);
      if (exit !== "commit") expect(await binding(page)).toBe("ctrl+alt+space");
    });
  }
  test(`${backend}: modified Escape and Tab remain recordable`, async ({
    page,
  }) => {
    await setup(page, backend);
    const shortcut = page.getByRole("button", {
      name: /Change Selected[- ]text assistant shortcut/i,
    });
    for (const [keys, key, hotkey, modifiers] of [
      [
        "Control+Shift+Escape",
        "Escape",
        "ctrl+shift+escape",
        ["ctrl", "shift"],
      ],
      ["Control+Tab", "Tab", "ctrl+tab", ["ctrl"]],
      ["Alt+Escape", "Escape", "alt+escape", ["alt"]],
    ] as const) {
      await shortcut.focus();
      await shortcut.press("Enter");
      await expect(shortcut).toHaveAttribute("aria-pressed", "true");
      // The DOM path must leave modified keys for the HandyKeys recorder.
      await page.keyboard.press(keys);
      if (backend === "handy_keys") {
        await expect(shortcut).toHaveAttribute("aria-pressed", "true");
        await emitKeys(page, key, hotkey, [...modifiers]);
      }
      await expect(shortcut).toHaveAttribute("aria-pressed", "false");
      expect(await binding(page)).toBe(
        backend === "tauri" ? hotkey.replace("escape", "esc") : hotkey,
      );
    }
  });
}

test("active sidebar identifies the current page after notice dismissal", async ({
  page,
}) => {
  await setup(page);
  await page.evaluate(() =>
    (window as unknown as HarnessWindow).emitTestEvent(
      "cleanup-fallback",
      null,
    ),
  );
  await page
    .getByRole("button", { name: "Dismiss", exact: true })
    .press("Enter");
  const active = page.getByRole("button", {
    name: "Post Process",
    exact: true,
  });
  await expect(active).toBeFocused();
  await expect(active).toHaveAttribute("aria-current", "page");
  await expect(active.locator("p")).toHaveCount(0);
});

test("cleanup and paste failure is one retained notice with accurate raw History receipt", async ({
  page,
}) => {
  await setup(page);
  await page.evaluate(() =>
    (window as unknown as HarnessWindow).emitTestEvent("paste-error", {
      raw_history_available: true,
    }),
  );
  await expect(page.getByRole("alert")).toHaveCount(1);
  await expect(page.getByRole("alert")).toContainText(
    "Raw text is available in History",
  );
  await expect(page.getByRole("alert")).toContainText("paste");
  await expect(
    page.getByRole("button", { name: "History", exact: true }),
  ).toHaveCount(2);
});

test("cleanup with no paste never claims delivery or unsaved History", async ({
  page,
}) => {
  await setup(page);
  for (const history of [true, false]) {
    await page.evaluate(
      (history) =>
        (window as unknown as HarnessWindow).emitTestEvent("cleanup-fallback", {
          history_available: history,
          raw_delivered: false,
        }),
      history,
    );
    await expect(page.getByRole("alert")).not.toContainText(
      "Raw text was used",
    );
    await expect(page.getByRole("alert")).toContainText(
      history
        ? "Raw text is available in History"
        : "could not be saved to History",
    );
  }
});

test("async settings and rejected modal Save retain keyboard focus and guard re-entry", async ({
  page,
}) => {
  await setup(page);
  const save = page.getByRole("button", {
    name: "Save request options",
    exact: true,
  });
  await defer(page, "change_post_process_request_options");
  await save.focus();
  await save.press("Enter");
  await pending(page);
  await expect(save).toBeFocused();
  await save.press("Enter");
  await release(page);
  await expect(
    page.getByRole("status").filter({ hasText: "Saved" }),
  ).toBeVisible();
  await expect(save).toBeFocused();
  expect(
    await page.evaluate(
      () =>
        (window as unknown as HarnessWindow).commandCalls.filter(
          (c) => c === "change_post_process_request_options",
        ).length,
    ),
  ).toBe(1);

  await page
    .getByRole("button", { name: "Edit Selected-text assistant", exact: true })
    .press("Enter");
  const select = page.getByRole("combobox", {
    name: "Profile prompt",
    exact: true,
  });
  await select.focus();
  await select.selectOption("cleanup");
  await expect(select).toBeFocused();
  const firstSave = page
    .getByRole("dialog")
    .getByRole("button", { name: "Save", exact: true });
  await defer(page, "update_processing_profile");
  await firstSave.focus();
  await firstSave.press("Enter");
  await pending(page);
  await expect(firstSave).toBeFocused();
  await firstSave.press("Enter");
  await release(page);
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect(
    page.getByRole("button", {
      name: "Edit Selected-text assistant",
      exact: true,
    }),
  ).toBeFocused();
  expect(
    await page.evaluate(
      () =>
        (window as unknown as HarnessWindow).commandCalls.filter(
          (c) => c === "update_processing_profile",
        ).length,
    ),
  ).toBe(1);
  await page.evaluate(() => {
    (window as unknown as HarnessWindow).commandCalls = [];
  });
  await page
    .getByRole("button", { name: "Edit Selected-text assistant", exact: true })
    .press("Enter");
  const dialog = page.getByRole("dialog");
  const dialogSave = dialog.getByRole("button", {
    name: "Save",
    exact: true,
  });
  await defer(page, "update_processing_profile", true);
  await dialogSave.focus();
  await dialogSave.press("Enter");
  await pending(page);
  await expect(dialogSave).toBeFocused();
  await dialogSave.press("Enter");
  await release(page);
  await expect(dialog.getByRole("alert")).toContainText("Could not save");
  await expect(dialogSave).toBeFocused();
  expect(
    await page.evaluate(
      () => !!document.activeElement?.closest('[role="dialog"]'),
    ),
  ).toBe(true);
  expect(
    await page.evaluate(
      () =>
        (window as unknown as HarnessWindow).commandCalls.filter(
          (c) => c === "update_processing_profile",
        ).length,
    ),
  ).toBe(1);
});

test("badInput is rejected with field-specific errors and range help remains described", async ({
  page,
}) => {
  await setup(page);
  const temperature = page.getByRole("spinbutton", {
    name: "Temperature (optional)",
  });
  await expect(temperature).toHaveAttribute("step", "0.1");
  for (const value of ["1e", "-"]) {
    await temperature.fill("");
    await temperature.pressSequentially(value);
    expect(
      await temperature.evaluate(
        (node: HTMLInputElement) => node.validity.badInput,
      ),
    ).toBe(true);
    await page
      .getByRole("button", { name: "Save request options" })
      .press("Enter");
    await expect(temperature).toHaveAttribute("aria-invalid", "true");
    await expect(temperature).toHaveAccessibleDescription(
      /Temperature.*0 to 2/,
    );
    await expect(page.getByRole("alert")).toContainText(
      "Temperature must be a number from 0 to 2",
    );
    expect(
      await page.evaluate(
        () =>
          JSON.parse(localStorage.getItem("test-settings")!)
            .post_process_request_options.temperature,
      ),
    ).toBe(0);
    expect(
      await page.evaluate(() =>
        (window as unknown as HarnessWindow).commandCalls.includes(
          "change_post_process_request_options",
        ),
      ),
    ).toBe(false);
  }
  await temperature.fill("0.3");
  const tokens = page.getByRole("spinbutton", {
    name: "Maximum output tokens (optional)",
  });
  await tokens.fill("");
  await tokens.pressSequentially("1e");
  await page
    .getByRole("button", { name: "Save request options" })
    .press("Enter");
  await expect(page.getByRole("alert")).toContainText(
    "Maximum output tokens must be a whole number",
  );
  await expect(tokens).toHaveAccessibleDescription(
    /Output tokens: 1 to 131072.*Maximum output tokens must/,
  );
  await tokens.fill("");
  await page
    .getByRole("spinbutton", { name: "Overall timeout (seconds)", exact: true })
    .fill("0");
  await page
    .getByRole("button", { name: "Save request options" })
    .press("Enter");
  await expect(page.getByRole("alert")).toContainText(
    "Overall timeout must be a whole number",
  );
  await expect(page.getByRole("alert")).not.toContainText("Connection timeout");
});

test("recovery items have distinct names, Copy retains focus and announces success, Dismiss focuses next item or heading", async ({
  page,
}) => {
  await setup(page);
  const items: AssistantRecoveryItem[] = [1, 2].map((id) => ({
    id,
    created_at: id,
    target: "selected",
    outcome: "refused",
    reason: "target_stale",
    detail: null,
    clipboard: "untouched",
    instruction:
      id === 1 ? "Shorten the first draft" : "Translate the second draft",
    generated: `Reply ${id}`,
  }));
  await page.evaluate(
    (items) => localStorage.setItem("test-recovery", JSON.stringify(items)),
    items,
  );
  await page
    .getByRole("button", { name: "History", exact: true })
    .press("Enter");
  const first = page.getByRole("article", { name: /Shorten the first draft/ });
  const second = page.getByRole("article", {
    name: /Translate the second draft/,
  });
  await expect(first).toHaveAccessibleName(
    /Automatic insertion.*Shorten the first draft/,
  );
  const copy = first.getByRole("button", { name: "Copy generated text" });
  await defer(page, "copy_assistant_recovery_text");
  await copy.focus();
  await copy.press("Enter");
  await pending(page);
  await expect(copy).toBeFocused();
  await copy.press("Enter");
  await release(page);
  await expect(first.getByRole("status")).toContainText(
    "Generated text copied",
  );
  await expect(copy).toBeFocused();
  expect(
    await page.evaluate(
      () =>
        (window as unknown as HarnessWindow).commandCalls.filter(
          (c) => c === "copy_assistant_recovery_text",
        ).length,
    ),
  ).toBe(1);
  await first
    .getByRole("button", { name: "Dismiss", exact: true })
    .press("Enter");
  await expect(second).toBeFocused();
  await second
    .getByRole("button", { name: "Dismiss", exact: true })
    .press("Enter");
  await expect(
    page.getByRole("heading", { name: "Assistant results awaiting recovery" }),
  ).toBeFocused();
});

test("workflow notice Dismiss returns focus to the active settings section", async ({
  page,
}) => {
  await setup(page);
  await page.evaluate(() =>
    (window as unknown as HarnessWindow).emitTestEvent(
      "cleanup-fallback",
      null,
    ),
  );
  const dismiss = page.getByRole("button", { name: "Dismiss", exact: true });
  await dismiss.focus();
  await dismiss.press("Enter");
  await expect(page.getByRole("alert")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Post Process", exact: true }),
  ).toBeFocused();
  expect(await page.evaluate(() => document.activeElement?.tagName)).not.toBe(
    "BODY",
  );
});

test("assistant refusal notices explain the actual remedy and model fetch failures are distinct", async ({
  page,
}) => {
  await setup(page);
  for (const [code, expected] of [
    ["recovery_full", "Dismiss assistant recovery items in History"],
    ["selection_too_large", "Select less text"],
    ["password_field", "Password fields"],
    ["read_only", "editable"],
    ["busy", "already running"],
  ]) {
    await page.evaluate(
      (code) =>
        (window as unknown as HarnessWindow).emitTestEvent(
          "assistant-error-event",
          { stage: "capture", code, reason: code, detail: null },
        ),
      code,
    );
    await expect(page.getByRole("alert")).toContainText(expected);
  }
  await page
    .getByRole("button", { name: "Dismiss", exact: true })
    .press("Enter");
  await page.evaluate(() => {
    (window as unknown as HarnessWindow).failCommand =
      "fetch_post_process_models";
  });
  await page
    .getByRole("button", { name: "Refresh models", exact: true })
    .press("Enter");
  await expect(page.getByRole("alert")).toContainText("Could not fetch models");
  await expect(page.getByRole("alert")).not.toContainText("Could not save");
});

test("cleanup notice only points to saved History and distinguishes a History retry from paste", async ({
  page,
}) => {
  await setup(page);
  await page.evaluate(() =>
    (window as unknown as HarnessWindow).emitTestEvent("cleanup-fallback", {
      history_available: false,
      raw_delivered: true,
    }),
  );
  await expect(page.getByRole("alert")).toContainText(
    "could not be saved to History",
  );
  await expect(
    page.getByRole("button", { name: "History", exact: true }),
  ).toHaveCount(1);
  await page.evaluate(() =>
    (window as unknown as HarnessWindow).emitTestEvent("cleanup-fallback", {
      history_available: true,
      raw_delivered: false,
    }),
  );
  await expect(page.getByRole("alert")).toContainText(
    "Raw text is available in History",
  );
  await expect(page.getByRole("alert")).not.toContainText("Raw text was used");
  await expect(
    page.getByRole("button", { name: "History", exact: true }),
  ).toHaveCount(2);
});

test("Base URL exposes its disabled reason during an async save", async ({
  page,
}) => {
  await setup(page);
  const url = page.getByRole("textbox", { name: "Base URL", exact: true });
  await defer(page, "change_post_process_base_url_setting");
  await url.fill("http://127.0.0.1:8080/v1");
  await url.press("Tab");
  await pending(page);
  await expect(url).toHaveAttribute("aria-disabled", "true");
  await expect(url).toHaveAccessibleDescription(/Saving the base URL/);
  await url.focus();
  await expect(url).toBeFocused();
  await release(page);
  await expect(url).toHaveAttribute("aria-disabled", "false");
  await expect(url).toBeFocused();
});
