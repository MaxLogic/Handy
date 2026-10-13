const FOCUSABLE_SELECTOR = [
  "a[href]",
  "button:not([disabled])",
  "textarea:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "[tabindex]:not([tabindex='-1'])",
].join(",");

const WAIT_MS = 10_000;

const isVisible = (element: HTMLElement) => {
  const style = window.getComputedStyle(element);
  return style.visibility !== "hidden" && style.display !== "none";
};

/**
 * The first control of the screen shown after the import dialog: the current
 * settings section in the sidebar, otherwise the first control of the
 * onboarding step.
 */
const nextScreenControl = (): HTMLElement | null => {
  const section = document.querySelector<HTMLElement>(
    '[id^="settings-section-"][aria-current="page"]',
  );
  if (section) return section;
  return (
    Array.from(
      document.querySelectorAll<HTMLElement>(`#root ${FOCUSABLE_SELECTOR}`),
    ).find(
      (element) =>
        element.getAttribute("aria-hidden") !== "true" && isVisible(element),
    ) ?? null
  );
};

/**
 * Move keyboard focus to the screen that replaces the closed import dialog.
 * The next screen renders asynchronously (settings or an onboarding step), so
 * this waits for its first control. It gives up when something else took
 * focus (for example a dialog that opened) or after a few seconds.
 */
export const focusNextScreen = () => {
  const origin = document.activeElement;
  let observer: MutationObserver | null = null;
  let timer = 0;
  const stop = () => {
    observer?.disconnect();
    window.clearTimeout(timer);
  };
  const focusedElsewhere = () => {
    const active = document.activeElement;
    return (
      active instanceof HTMLElement &&
      active !== document.body &&
      active !== origin &&
      active.isConnected
    );
  };
  const attempt = () => {
    if (focusedElsewhere()) return true;
    const target = nextScreenControl();
    if (!target) return false;
    target.focus();
    return document.activeElement === target;
  };
  // Let the dialog unmount first.
  window.requestAnimationFrame(() => {
    if (attempt()) return;
    observer = new MutationObserver(() => {
      if (attempt()) stop();
    });
    observer.observe(document.body, { childList: true, subtree: true });
    timer = window.setTimeout(stop, WAIT_MS);
  });
};
