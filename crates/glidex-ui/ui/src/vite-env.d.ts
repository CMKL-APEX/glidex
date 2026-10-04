/// <reference types="vite/client" />

/** Version tag, git branch and commit of this build (vite.config.ts). */
declare const __GLIDEX_BUILD__: {
  tag: string | null;
  branch: string | null;
  commit: string | null;
  /** Built from a tree with uncommitted changes. */
  dirty: boolean;
};
