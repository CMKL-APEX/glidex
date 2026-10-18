// The console's terminal: xterm.js, or ghostty-web (experimental: Ghostty's
// VT engine as WebAssembly). Chosen on the console page and remembered in
// this browser. ghostty-web is loaded only when chosen.
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon as XFitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

export type TerminalKind = "xterm" | "ghostty";

export const TERMINAL_KINDS: { kind: TerminalKind; label: string }[] = [
  { kind: "xterm", label: "xterm.js" },
  { kind: "ghostty", label: "ghostty-web (experimental)" },
];

const STORAGE_KEY = "glidex.console.terminal";

export function savedTerminalKind(): TerminalKind {
  try {
    return localStorage.getItem(STORAGE_KEY) === "ghostty" ? "ghostty" : "xterm";
  } catch {
    return "xterm";
  }
}

export function saveTerminalKind(kind: TerminalKind) {
  try {
    localStorage.setItem(STORAGE_KEY, kind);
  } catch {
    /* storage disabled: the choice lasts this page */
  }
}

/** What the console page uses of a terminal: the part both share. */
export interface ConsoleTerminal {
  write(data: string | Uint8Array): void;
  onData(listener: (data: string) => void): { dispose(): void };
  fit(): void;
  dispose(): void;
}

const OPTIONS = {
  fontFamily: "Menlo, Monaco, Consolas, monospace",
  fontSize: 13,
  cursorBlink: true,
  convertEol: true,
  theme: { background: "#000000", foreground: "#e6e6e6" },
};

/** A terminal of `kind`, open in `parent`. */
export function openTerminal(kind: TerminalKind, parent: HTMLElement): Promise<ConsoleTerminal> {
  return kind === "ghostty" ? openGhostty(parent) : Promise.resolve(openXterm(parent));
}

/** Put `text` on the clipboard; outside a secure context (plain HTTP)
 *  `navigator.clipboard` is missing, so copy from a scratch textarea. */
function copyText(text: string) {
  if (navigator.clipboard?.writeText) {
    navigator.clipboard.writeText(text).catch(() => {});
    return;
  }
  const scratch = document.createElement("textarea");
  scratch.value = text;
  scratch.style.position = "fixed";
  scratch.style.opacity = "0";
  document.body.appendChild(scratch);
  scratch.select();
  document.execCommand("copy");
  scratch.remove();
}

/** Ctrl+Shift+<key>, as terminals copy and paste where Ctrl+C is ^C. */
const ctrlShift = (ev: KeyboardEvent, code: string) =>
  ev.ctrlKey && ev.shiftKey && !ev.altKey && !ev.metaKey && ev.code === code;

function openXterm(parent: HTMLElement): ConsoleTerminal {
  const term = new XTerm(OPTIONS);
  const fitAddon = new XFitAddon();
  term.loadAddon(fitAddon);
  term.open(parent);
  fitAddon.fit();

  // Ctrl+Shift+C / Ctrl+Shift+V copy and paste; Ctrl+C stays ^C for the
  // guest. (On macOS ⌘C / ⌘V already do, through xterm's own copy and
  // paste handlers.) Paste is left to the browser: its paste event
  // reaches xterm, which asks for no clipboard permission. Returning
  // false keeps a key from xterm.
  term.attachCustomKeyEventHandler((ev) => {
    if (ctrlShift(ev, "KeyC")) {
      // Chrome's Ctrl+Shift+C opens the element inspector.
      ev.preventDefault();
      if (ev.type === "keydown" && term.hasSelection()) {
        copyText(term.getSelection());
      }
      return false;
    }
    return !ctrlShift(ev, "KeyV");
  });

  return {
    write: (data) => term.write(data),
    onData: (listener) => term.onData(listener),
    fit: () => fitAddon.fit(),
    dispose: () => {
      // xterm 5.5 queues a setTimeout(syncScrollArea) in open(); disposing
      // before it runs (StrictMode's mount/unmount, a quick navigation)
      // makes it throw on the disposed renderer. Take the terminal off the
      // page now and dispose it once that timer has run.
      term.element?.remove();
      setTimeout(() => term.dispose(), 0);
    },
  };
}

// The WebAssembly module, compiled once. Loaded from /assets: ghostty-web's
// default is a data: URL, which the UI's connect-src does not allow.
let ghostty: Promise<import("ghostty-web").Ghostty> | null = null;

async function openGhostty(parent: HTMLElement): Promise<ConsoleTerminal> {
  const [lib, { default: wasmUrl }] = await Promise.all([
    import("ghostty-web"),
    import("ghostty-web/ghostty-vt.wasm?url"),
  ]);
  ghostty ??= lib.Ghostty.load(wasmUrl).catch((e) => {
    ghostty = null;
    throw e;
  });
  const term = new lib.Terminal({ ...OPTIONS, ghostty: await ghostty });
  const fitAddon = new lib.FitAddon();
  term.loadAddon(fitAddon);
  term.open(parent);
  fitAddon.fit();

  // ghostty-web already lets the browser paste on Ctrl+V / Ctrl+Shift+V
  // and copies a selection when it is made. Ctrl+Shift+C copies too, as
  // with xterm.js. Here returning true means "handled": the key is kept
  // from the terminal and its default prevented.
  term.attachCustomKeyEventHandler((ev) => {
    if (!ctrlShift(ev, "KeyC")) return false;
    if (term.hasSelection()) copyText(term.getSelection());
    return true;
  });

  return {
    write: (data) => term.write(data),
    onData: (listener) => term.onData(listener),
    fit: () => fitAddon.fit(),
    dispose: () => term.dispose(),
  };
}
