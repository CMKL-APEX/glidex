// A page loaded before an upgrade names the previous build's files in
// /assets (each named by its content). The installer keeps the previous
// build's files for such pages, but not those of builds before it, so a
// page open across two upgrades asks for a file that is gone: a lazily
// loaded page or the console's terminal fails ("Importing a module script
// failed" in Safari). Vite raises `vite:preloadError` then; reloading gets
// the new build's index.html. Once a minute at most, so a file missing
// from the new build too shows its error instead of reloading forever.

const KEY = "glidex.reloadedForStaleBuild";
const ONCE_PER_MS = 60_000;

/** Reload the page when one of the build's files fails to load. */
export function reloadOnStaleBuild(win: Window = window): void {
  win.addEventListener("vite:preloadError", () => {
    let last = 0;
    try {
      last = Number(win.sessionStorage.getItem(KEY)) || 0;
    } catch {
      // No session storage: no guard against a loop, so no reload.
      return;
    }
    if (Date.now() - last < ONCE_PER_MS) return;
    try {
      win.sessionStorage.setItem(KEY, String(Date.now()));
    } catch {
      return;
    }
    win.location.reload();
  });
}
