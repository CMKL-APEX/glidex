/// <reference types="vite/client" />

/** Version tag, git branch and commit of this build (vite.config.ts). */
declare const __GLIDEX_BUILD__: {
  /** The version tag on the built commit. */
  tag: string | null;
  /** That tag, else the latest release tag before the commit. */
  release: string | null;
  /** Commits after `release` (0 on a tagged build). */
  ahead: number;
  branch: string | null;
  commit: string | null;
  commitFull: string | null;
  /** Built from a tree with uncommitted changes. */
  dirty: boolean;
  /** The repository the footer links to, `https://github.com/<owner>/<repo>`. */
  repo: string;
};
