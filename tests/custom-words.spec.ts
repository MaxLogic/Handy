import { test, expect, type Page } from "@playwright/test";
import {
  setup,
  defer,
  pending,
  release,
  type HarnessWindow,
} from "./support/settings-harness";

const fileHelp =
  "Words are saved in custom_words.txt in the app data folder, one word or phrase per line. Lines starting with # are comments. Changes made in any text editor apply automatically.";

const input = (page: Page) =>
  page.getByRole("textbox", { name: "New custom word", exact: true });
const word = (page: Page, value: string) =>
  page.getByRole("button", { name: `Remove ${value}`, exact: true });
const openFile = (page: Page) =>
  page.getByRole("button", { name: "Open file", exact: true });

async function open(page: Page, words: string[]) {
  await setup(page, "tauri", { custom_words: words }, "Advanced");
  await expect(input(page)).toBeVisible();
}
async function args(page: Page, command: string) {
  return page.evaluate(
    (command) =>
      (window as unknown as HarnessWindow).commandArgs
        .filter((c) => c.command === command)
        .map((c) => c.args),
    command,
  );
}
async function focused(page: Page) {
  return page.evaluate(() => ({
    tag: document.activeElement?.tagName,
    label: document.activeElement?.getAttribute("aria-label"),
  }));
}
/** What the backend watcher does after custom_words.txt changes. */
async function externalEdit(page: Page, words: string[]) {
  await page.evaluate((words) => {
    const w = window as unknown as HarnessWindow;
    w.patchSettings({ custom_words: words });
    w.emitTestEvent("settings-changed", { setting: "custom_words" });
  }, words);
}

test("adds words through the file command and keeps focus in the field", async ({
  page,
}) => {
  await open(page, ["Alpha"]);
  await expect(openFile(page)).toHaveAccessibleDescription(fileHelp);
  await expect(input(page)).toHaveAccessibleDescription("");

  await input(page).fill("  TFDQuery ");
  await input(page).press("Enter");
  await expect(word(page, "TFDQuery")).toBeVisible();
  await expect(input(page)).toHaveValue("");
  await expect(input(page)).toBeFocused();

  await input(page).fill("C#");
  await page.getByRole("button", { name: "Add", exact: true }).click();
  await expect(word(page, "C#")).toBeVisible();
  // The Add button disables once the field clears; focus must not drop.
  await expect(input(page)).toBeFocused();

  expect(await args(page, "add_custom_word")).toEqual([
    { word: "TFDQuery" },
    { word: "C#" },
  ]);
  expect(await args(page, "update_custom_words")).toEqual([]);
});

test("a word starting with # is refused with a linked error that clears on typing", async ({
  page,
}) => {
  await open(page, []);
  await input(page).fill("#tag");
  await input(page).press("Enter");
  const message =
    "A word cannot start with #, because lines starting with # are comments in custom_words.txt.";
  await expect(page.getByRole("alert")).toHaveText(message);
  await expect(input(page)).toHaveAttribute("aria-invalid", "true");
  await expect(input(page)).toHaveAccessibleDescription(message);
  await expect(input(page)).toBeFocused();

  await input(page).fill("tag");
  await expect(input(page)).not.toHaveAttribute("aria-invalid");
  await expect(input(page)).toHaveAccessibleDescription("");
  await expect(page.getByRole("alert")).toHaveText("");
});

test("duplicates are reported without calling the backend", async ({
  page,
}) => {
  await open(page, ["Alpha"]);
  await input(page).fill("Alpha");
  await input(page).press("Enter");
  await expect(input(page)).toHaveAccessibleDescription(
    '"Alpha" already exists',
  );
  expect(await args(page, "add_custom_word")).toEqual([]);
});

test("removing words moves focus to a neighbour, then the field", async ({
  page,
}) => {
  await open(page, ["Alpha", "Beta", "Gamma"]);
  await word(page, "Beta").focus();
  await word(page, "Beta").press("Enter");
  await expect(word(page, "Beta")).toHaveCount(0);
  await expect(word(page, "Gamma")).toBeFocused();

  await word(page, "Gamma").press("Enter");
  await expect(word(page, "Alpha")).toBeFocused();

  await word(page, "Alpha").press("Enter");
  await expect(input(page)).toBeFocused();
  expect(await args(page, "remove_custom_word")).toEqual([
    { word: "Beta" },
    { word: "Gamma" },
    { word: "Alpha" },
  ]);
});

test("focus stays on a control while a word is being saved", async ({
  page,
}) => {
  await open(page, ["Alpha", "Beta"]);
  await defer(page, "remove_custom_word");
  await word(page, "Alpha").focus();
  await word(page, "Alpha").press("Enter");
  await pending(page);
  await release(page);
  await expect(word(page, "Beta")).toBeFocused();
});

test("an external edit of custom_words.txt refreshes the list without losing focus", async ({
  page,
}) => {
  await open(page, ["Alpha", "Beta", "Gamma"]);
  await word(page, "Beta").focus();
  await externalEdit(page, ["Alpha", "Gamma", "Delta"]);
  await expect(word(page, "Delta")).toBeVisible();
  await expect(word(page, "Beta")).toHaveCount(0);
  await expect(word(page, "Gamma")).toBeFocused();
  expect((await focused(page)).tag).not.toBe("BODY");

  // Words removed elsewhere while focus is in the field leave focus alone.
  await input(page).focus();
  await externalEdit(page, ["Alpha"]);
  await expect(word(page, "Gamma")).toHaveCount(0);
  await expect(input(page)).toBeFocused();
});

test("Open file opens custom_words.txt and links a failure to the button", async ({
  page,
}) => {
  await open(page, []);
  await openFile(page).click();
  expect(await args(page, "open_custom_words_file")).toEqual([{}]);
  await expect(page.getByRole("alert")).toHaveText("");

  await page.evaluate(() => {
    (window as unknown as HarnessWindow).failCommand = "open_custom_words_file";
  });
  await openFile(page).click();
  const message = "Could not open custom_words.txt: fixture rejected setting";
  await expect(page.getByRole("alert")).toHaveText(message);
  await expect(openFile(page)).toHaveAccessibleDescription(
    `${fileHelp} ${message}`,
  );
  await expect(openFile(page)).toBeFocused();
  await expect(input(page)).toHaveAccessibleDescription("");
});
