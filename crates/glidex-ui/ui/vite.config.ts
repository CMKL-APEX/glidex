import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

// GLIDEX_API_URL points the dev proxy at another control plane (the e2e
// suite runs a scratch one).
const apiUrl = process.env.GLIDEX_API_URL ?? "http://localhost:8841";

/** `git <args>`, trimmed; `undefined` outside a checkout or on failure. */
function git(...args: string[]): string | undefined {
  try {
    const out = execFileSync("git", args, { cwd: fileURLToPath(new URL(".", import.meta.url)), stdio: ["ignore", "pipe", "ignore"] }).toString().trim();
    return out || undefined;
  } catch {
    return undefined;
  }
}

/**
 * What the footer shows: the version tag on the built commit (if any), the
 * branch and the commit. GLIDEX_BUILD_TAG / _BRANCH / _COMMIT override them
 * for builds from a tarball or a detached CI checkout.
 */
function buildInfo() {
  const env = process.env;
  const branch = env.GLIDEX_BUILD_BRANCH ?? env.GITHUB_REF_NAME ?? git("rev-parse", "--abbrev-ref", "HEAD");
  return {
    tag: env.GLIDEX_BUILD_TAG ?? git("describe", "--tags", "--exact-match", "HEAD") ?? null,
    // A detached HEAD has no branch.
    branch: branch && branch !== "HEAD" ? branch : null,
    commit: env.GLIDEX_BUILD_COMMIT ?? git("rev-parse", "--short=10", "HEAD") ?? null,
    dirty: env.GLIDEX_BUILD_COMMIT ? false : git("status", "--porcelain", "--untracked-files=no") !== undefined,
  };
}

export default defineConfig({
  plugins: [react()],
  define: {
    __GLIDEX_BUILD__: JSON.stringify(buildInfo()),
  },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: apiUrl,
        changeOrigin: true,
        ws: true,
        rewrite: (path) => path.replace(/^\/api/, ""),
      },
    },
  },
});
