import { expect, test, type Page } from "@playwright/test";
import type {
  AppSettings,
  UpstreamImportOffer,
  UpstreamImportReport,
} from "../src/bindings";
import {
  defer,
  release,
  setup,
  type HarnessWindow,
} from "./support/settings-harness";

const GiB = 1024 * 1024 * 1024;

const offer: UpstreamImportOffer = {
  source_dir: "C:\\Users\\fixture\\AppData\\Roaming\\com.pais.handy",
  destination_dir: "D:\\Tools\\Handy MaxLogic\\Data",
  settings_available: true,
  replaces_settings: false,
  model_count: 2,
  model_bytes: 3.1 * GiB,
  history_available: true,
  history_bytes: 5 * 1024 * 1024,
  replaces_history: false,
};

const report: UpstreamImportReport = {
  settings_imported: true,
  models_copied: 2,
  models_skipped: 0,
  history_imported: false,
  bytes_copied: 3.1 * GiB,
  cancelled: false,
  failures: [],
  skipped_links: [],
};

type ImportWindow = HarnessWindow & {
  importOffer: UpstreamImportOffer | null;
  cancelRequested: boolean;
};

async function openWithOffer(
  page: Page,
  options: {
    offer?: UpstreamImportOffer | null;
    report?: UpstreamImportReport;
    runError?: string;
    restartError?: string;
    settings?: AppSettings;
    microphoneDenied?: boolean;
  } = {},
) {
  await page.addInitScript(
    ({ offer, report, runError, restartError, microphoneDenied }) => {
      const w = window as unknown as ImportWindow;
      w.importOffer = offer;
      w.cancelRequested = false;
      w.commandHook = (command) => {
        switch (command) {
          case "get_upstream_import_offer":
            return { value: w.importOffer };
          case "decline_upstream_import":
            // The backend records the decision, so the offer is gone.
            w.importOffer = null;
            return { value: null };
          case "cancel_upstream_import":
            w.cancelRequested = true;
            return { value: null };
          case "run_upstream_import":
            if (runError) return { error: runError };
            return {
              value: w.cancelRequested
                ? { ...report, settings_imported: false, cancelled: true }
                : report,
            };
          case "plugin:process|restart":
            return restartError ? { error: restartError } : { value: null };
          case "is_update_checks_locked":
            return { value: true };
          case "get_windows_microphone_permission_status":
            return microphoneDenied
              ? { value: { supported: true, overall_access: "denied" } }
              : undefined;
          default:
            return undefined;
        }
      };
    },
    {
      offer: options.offer === undefined ? offer : options.offer,
      report: options.report ?? report,
      runError: options.runError ?? "",
      restartError: options.restartError ?? "",
      microphoneDenied: options.microphoneDenied ?? false,
    },
  );
  await setup(page, "tauri", options.settings ?? {}, null);
}

const dialog = (page: Page) =>
  page.getByRole("dialog", { name: "Import data from Handy" });

const calls = (page: Page, command: string) =>
  page.evaluate(
    (command) =>
      (window as unknown as HarnessWindow).commandArgs.filter(
        (call) => call.command === command,
      ),
    command,
  );

/** WCAG contrast of an element's text against the dialog background. */
const contrast = (page: Page, text: RegExp) =>
  dialog(page)
    .getByText(text)
    .first()
    .evaluate((node) => {
      // A canvas resolves any CSS color syntax (Tailwind emits oklab/oklch).
      const rgb = (value: string) => {
        const context = document.createElement("canvas").getContext("2d")!;
        context.fillStyle = value;
        context.fillRect(0, 0, 1, 1);
        return Array.from(context.getImageData(0, 0, 1, 1).data.slice(0, 3));
      };
      const luminance = (value: string) => {
        const [r, g, b] = rgb(value).map((c) => {
          const v = c / 255;
          return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
        });
        return 0.2126 * r + 0.7152 * g + 0.0722 * b;
      };
      const fg = luminance(getComputedStyle(node).color);
      const bg = luminance(
        getComputedStyle(node.closest("[role=dialog]")!).backgroundColor,
      );
      return (Math.max(fg, bg) + 0.05) / (Math.min(fg, bg) + 0.05);
    });

test.describe("upstream Handy import offer", () => {
  test("is shown before settings with named choices and keyboard focus inside", async ({
    page,
  }) => {
    await openWithOffer(page);

    const modal = dialog(page);
    await expect(modal).toBeVisible();
    // The shared-shortcut warning is part of the description read on open,
    // not only text a screen reader user would have to find.
    await expect(modal).toHaveAccessibleDescription(
      /found data from the original Handy app.*never changed\..*same default shortcuts.*Close the original Handy or change its shortcuts before using Handy MaxLogic\./,
    );
    await expect(modal).not.toHaveAccessibleDescription(/once/);
    await expect(modal).toBeFocused();
    await expect(
      page.getByRole("button", { name: "General", exact: true }),
    ).toHaveCount(0);

    const group = modal.getByRole("group", { name: "What to copy" });
    await expect(
      group.getByRole("checkbox", {
        name: "Settings, shortcuts, prompts and custom sounds",
      }),
    ).toBeChecked();
    await expect(
      group.getByRole("checkbox", {
        name: "Speech models: 2 files, 3.1 GB",
      }),
    ).toBeChecked();
    await expect(
      group.getByRole("checkbox", { name: "History and recordings, 5 MB" }),
    ).not.toBeChecked();
    await expect(modal.getByText("3.1 GB will be copied.")).toBeVisible();
    await expect(
      modal.getByText(
        "Copy from: C:\\Users\\fixture\\AppData\\Roaming\\com.pais.handy",
      ),
    ).toBeVisible();
    await expect(
      modal.getByText("Copy to: D:\\Tools\\Handy MaxLogic\\Data"),
    ).toBeVisible();
    await expect(
      modal.getByText(
        "Close the original Handy or change its shortcuts before using Handy MaxLogic.",
        { exact: false },
      ),
    ).toBeVisible();
    await expect(
      modal.getByRole("button", { name: "Import", exact: true }),
    ).toHaveAccessibleDescription("3.1 GB will be copied.");
    await expect(
      modal.getByRole("button", { name: "Not now" }),
    ).toHaveAccessibleDescription(
      "Not now: Handy MaxLogic asks again the next time it starts.",
    );
    await expect(
      modal.getByRole("button", { name: "Don't import" }),
    ).toHaveAccessibleDescription(
      "Don't import: Handy MaxLogic will not ask again. You can copy files by hand from C:\\Users\\fixture\\AppData\\Roaming\\com.pais.handy.",
    );

    // Escape does not dismiss: the choice must be explicit.
    await page.keyboard.press("Escape");
    await expect(modal).toBeVisible();

    // Tab stays inside the dialog: three choices, then Import, Not now and
    // the final Don't import last.
    const names: string[] = [];
    for (let i = 0; i < 7; i++) {
      await page.keyboard.press("Tab");
      names.push(
        await page.evaluate(() => {
          const active = document.activeElement as HTMLInputElement | null;
          return (
            active?.labels?.[0]?.textContent ??
            active?.getAttribute("aria-label") ??
            active?.textContent ??
            ""
          );
        }),
      );
      expect(
        await modal.evaluate((node) => node.contains(document.activeElement)),
      ).toBe(true);
    }
    expect(names.slice(3, 7)).toEqual([
      "Import",
      "Not now",
      "Don't import",
      "Settings, shortcuts, prompts and custom sounds",
    ]);
  });

  test("Don't import records the choice once and continues to the app", async ({
    page,
  }) => {
    await openWithOffer(page);
    await dialog(page).getByRole("button", { name: "Don't import" }).click();

    await expect(dialog(page)).toHaveCount(0);
    // Focus lands on the current settings section, not on the page body.
    await expect(
      page.getByRole("button", { name: "General", exact: true }),
    ).toBeFocused();
    expect(await calls(page, "decline_upstream_import")).toHaveLength(1);
    expect(await calls(page, "run_upstream_import")).toHaveLength(0);
  });

  test("Not now records nothing, does not ask again this session and focuses settings", async ({
    page,
  }) => {
    await openWithOffer(page);
    await dialog(page).getByRole("button", { name: "Not now" }).focus();
    const offerChecks = (await calls(page, "get_upstream_import_offer")).length;
    await page.keyboard.press("Enter");

    await expect(dialog(page)).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: "General", exact: true }),
    ).toBeFocused();
    expect(await calls(page, "decline_upstream_import")).toHaveLength(0);
    expect(await calls(page, "run_upstream_import")).toHaveLength(0);
    // The backend still has the offer; the app does not ask for it again.
    expect(await calls(page, "get_upstream_import_offer")).toHaveLength(
      offerChecks,
    );
  });

  test("closing the offer before onboarding focuses the onboarding step", async ({
    page,
  }) => {
    await openWithOffer(page, {
      settings: { onboarding_completed: false },
      microphoneDenied: true,
    });
    await dialog(page).getByRole("button", { name: "Not now" }).click();

    await expect(dialog(page)).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: "Open System Settings" }),
    ).toBeFocused();
  });

  test("settings only: no 0-byte total and Import has no size description", async ({
    page,
  }) => {
    await openWithOffer(page);
    const modal = dialog(page);
    await modal
      .getByRole("checkbox", { name: "Speech models: 2 files, 3.1 GB" })
      .uncheck();
    await expect(modal.getByText(/will be copied/)).toHaveCount(0);
    await expect(
      modal.getByRole("button", { name: "Import", exact: true }),
    ).not.toHaveAttribute("aria-describedby");
  });

  test("import sends the keyboard choice, reports progress and focuses Restart now", async ({
    page,
  }) => {
    await openWithOffer(page);
    const modal = dialog(page);
    await modal
      .getByRole("checkbox", { name: "History and recordings, 5 MB" })
      .focus();
    await page.keyboard.press("Space");

    await defer(page, "run_upstream_import");
    await modal.getByRole("button", { name: "Import", exact: true }).click();

    const cancel = modal.getByRole("button", { name: "Cancel import" });
    await expect(cancel).toBeFocused();
    await expect(modal.getByRole("status")).toHaveText(
      "Copying speech models…",
    );
    await page.evaluate(() =>
      (window as unknown as HarnessWindow).emitTestEvent(
        "upstream-import-progress",
        {
          phase: "history",
          copied_bytes: 1024 * 1024,
          total_bytes: 2 * 1024 * 1024,
        },
      ),
    );
    await expect(modal.getByRole("status")).toHaveText("Copying history…");
    await expect(
      modal.getByRole("progressbar", { name: "Import progress" }),
    ).toBeVisible();
    await expect(modal.getByText("1 MB of 2 MB")).toBeVisible();
    // Choices are locked while copying but remain readable.
    await expect(
      modal.getByRole("checkbox", { name: "History and recordings, 5 MB" }),
    ).toHaveAttribute("aria-disabled", "true");

    await release(page);

    const restart = modal.getByRole("button", { name: "Restart now" });
    await expect(restart).toBeFocused();
    await expect(restart).toHaveAccessibleDescription(
      "Settings imported. 2 speech models copied. Restart Handy MaxLogic to use the imported data.",
    );
    await expect(modal.getByRole("status")).toHaveText("Import finished.");
    await expect(modal.getByText("Settings imported.")).toBeVisible();
    await expect(modal.getByText("2 speech models copied.")).toBeVisible();
    await expect(modal.getByRole("alert")).toHaveCount(0);
    expect((await calls(page, "run_upstream_import"))[0].args).toEqual({
      choice: { settings: true, models: true, history: true },
    });

    await page.keyboard.press("Enter");
    await expect
      .poll(() => calls(page, "plugin:process|restart"))
      .toHaveLength(1);
  });

  test("partial import reports problems politely with a recovery path", async ({
    page,
  }) => {
    await openWithOffer(page, {
      report: {
        ...report,
        models_copied: 1,
        failures: [
          {
            item: "models/ggml-large-v3-turbo.bin",
            error: "There is not enough space on the disk.",
          },
        ],
      },
    });
    await dialog(page)
      .getByRole("button", { name: "Import", exact: true })
      .click();

    const modal = dialog(page);
    await expect(modal.getByRole("status")).toHaveText(
      "Import finished with 1 problem.",
    );
    // One announcement: the status and the focused button's description,
    // no competing alert.
    await expect(modal.getByRole("alert")).toHaveCount(0);
    const restart = modal.getByRole("button", { name: "Restart now" });
    await expect(restart).toBeFocused();
    await expect(restart).toHaveAccessibleDescription(
      /1 item could not be copied:.*models\/ggml-large-v3-turbo\.bin: There is not enough space on the disk\..*offered again.*Speech models already copied are skipped.*settings and history are replaced only if you check them.*Close the original Handy first, or copy the items by hand from C:\\Users\\fixture\\AppData\\Roaming\\com\.pais\.handy to D:\\Tools\\Handy MaxLogic\\Data\..*Restart Handy MaxLogic/,
    );
    expect(await contrast(page, /could not be copied/)).toBeGreaterThanOrEqual(
      4.5,
    );
  });

  test("whole-run failure describes Try again with the error first", async ({
    page,
  }) => {
    await openWithOffer(page, { runError: "the data folder is read-only" });
    await dialog(page)
      .getByRole("button", { name: "Import", exact: true })
      .click();

    const retry = dialog(page).getByRole("button", { name: "Try again" });
    await expect(retry).toBeFocused();
    await expect(retry).toHaveAccessibleDescription(
      "Import failed: the data folder is read-only 3.1 GB will be copied.",
    );
    expect(await contrast(page, /Import failed/)).toBeGreaterThanOrEqual(4.5);
  });

  test("links to folders that were skipped are listed in the result", async ({
    page,
  }) => {
    await openWithOffer(page, {
      report: { ...report, skipped_links: ["models/shared-model"] },
    });
    await dialog(page)
      .getByRole("button", { name: "Import", exact: true })
      .click();
    await expect(
      dialog(page).getByRole("button", { name: "Restart now" }),
    ).toHaveAccessibleDescription(
      /Not copied because it is a link to a folder \(copy it by hand if you need it\): models\/shared-model\./,
    );
  });

  test("cancel keeps the offer open and focuses Try again", async ({
    page,
  }) => {
    await openWithOffer(page);
    await defer(page, "run_upstream_import");
    await dialog(page)
      .getByRole("button", { name: "Import", exact: true })
      .click();
    await page.keyboard.press("Enter");
    await expect
      .poll(() => calls(page, "cancel_upstream_import"))
      .toHaveLength(1);
    await release(page);

    await expect(dialog(page).getByRole("status")).toHaveText(
      "Import cancelled. Files that finished copying were kept. You can try again.",
    );
    await expect(
      dialog(page).getByRole("button", { name: "Try again" }),
    ).toBeFocused();
    await expect(
      dialog(page).getByRole("button", { name: "Don't import" }),
    ).toBeVisible();
  });

  test("nothing selected blocks import with an explanation", async ({
    page,
  }) => {
    await openWithOffer(page);
    const modal = dialog(page);
    for (const name of [
      "Settings, shortcuts, prompts and custom sounds",
      "Speech models: 2 files, 3.1 GB",
    ])
      await modal.getByRole("checkbox", { name }).uncheck();

    const importButton = modal.getByRole("button", {
      name: "Import",
      exact: true,
    });
    await expect(importButton).toHaveAttribute("aria-disabled", "true");
    await expect(importButton).toHaveAccessibleDescription(
      "Select at least one item, or choose Not now or Don't import.",
    );
    // aria-disabled keeps the button focusable; activating it does nothing.
    await importButton.focus();
    await page.keyboard.press("Enter");
    await expect(importButton).toBeFocused();
    expect(await calls(page, "run_upstream_import")).toHaveLength(0);
  });

  test("choices that would replace fork data start unchecked and say what is lost", async ({
    page,
  }) => {
    // Offered again after Not now or a failed run, when the fork has its own
    // settings and history by now: one Import press must not overwrite them.
    await openWithOffer(page, {
      offer: { ...offer, replaces_settings: true, replaces_history: true },
    });
    const modal = dialog(page);
    const settings = modal.getByRole("checkbox", {
      name: "Settings, shortcuts, prompts and custom sounds",
    });
    const history = modal.getByRole("checkbox", {
      name: "History and recordings, 5 MB",
    });
    await expect(settings).not.toBeChecked();
    await expect(history).not.toBeChecked();
    await expect(
      modal.getByRole("checkbox", { name: "Speech models: 2 files, 3.1 GB" }),
    ).toBeChecked();
    await expect(settings).toHaveAccessibleDescription(
      "Replaces this app's current settings, profiles and hotkeys. They will be lost.",
    );
    await expect(history).toHaveAccessibleDescription(
      "Replaces this app's current history. Its entries will be lost.",
    );

    await modal.getByRole("button", { name: "Import", exact: true }).click();
    expect((await calls(page, "run_upstream_import"))[0].args).toEqual({
      choice: { settings: false, models: true, history: false },
    });
  });

  test("settings start checked when the fork has none of its own", async ({
    page,
  }) => {
    await openWithOffer(page, {
      offer: { ...offer, replaces_settings: false, replaces_history: true },
    });
    const modal = dialog(page);
    await expect(
      modal.getByRole("checkbox", {
        name: "Settings, shortcuts, prompts and custom sounds",
      }),
    ).toBeChecked();
    await expect(
      modal.getByRole("checkbox", { name: "History and recordings, 5 MB" }),
    ).not.toBeChecked();
  });

  test("restart failure is announced", async ({ page }) => {
    await openWithOffer(page, { restartError: "access denied" });
    await dialog(page)
      .getByRole("button", { name: "Import", exact: true })
      .click();
    await dialog(page).getByRole("button", { name: "Restart now" }).click();
    await expect(dialog(page).getByRole("alert")).toHaveText(
      "Restart failed: access denied. Close Handy MaxLogic and start it again.",
    );
  });

  test("no offer goes straight to settings", async ({ page }) => {
    await openWithOffer(page, { offer: null });
    await expect(
      page.getByRole("button", { name: "General", exact: true }),
    ).toBeVisible();
    await expect(page.getByRole("dialog")).toHaveCount(0);
  });
});

test.describe("fork identity in settings", () => {
  test("About names the fork, keeps upstream attribution and updates stay off", async ({
    page,
  }) => {
    await openWithOffer(page, { offer: null });
    await page.getByRole("button", { name: "About", exact: true }).click();

    await expect(
      page.getByRole("heading", { name: "Handy MaxLogic", exact: true }),
    ).toBeVisible();
    await page.getByRole("button", { name: "View fork on GitHub" }).click();
    await expect
      .poll(async () =>
        (await calls(page, "plugin:opener|open_url")).map(
          (call) => call.args.url,
        ),
      )
      .toEqual(["https://github.com/MaxLogic/Handy"]);
    await expect(
      page.getByRole("button", { name: "View on GitHub", exact: true }),
    ).toBeVisible();

    await expect(page.getByText("Update Checking Disabled")).toBeVisible();
    expect(await calls(page, "plugin:updater|check")).toHaveLength(0);
  });
});
