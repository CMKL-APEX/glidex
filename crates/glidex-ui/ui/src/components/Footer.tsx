/** The build this UI came from: its release (the version tag, or the
 * release it follows and how far), branch and commit, linked to the code on
 * GitHub (spec/07-web-ui.md, Footer). */
type Part = { label: string; value: string; href?: string; title?: string; note?: string };

export default function Footer() {
  const { tag, release, ahead, branch, commit, commitFull, dirty, repo } = __GLIDEX_BUILD__;
  const rev = commitFull ?? commit;
  const version: Omit<Part, "label"> | null = tag
    ? { value: tag, href: `${repo}/releases/tag/${encodeURIComponent(tag)}`, title: `The ${tag} release` }
    : release
      ? {
          value: `${release}+${ahead}`,
          // What this build has on top of the release.
          href: rev ? `${repo}/compare/${encodeURIComponent(release)}...${rev}` : `${repo}/releases/tag/${encodeURIComponent(release)}`,
          title: `${ahead} commit${ahead === 1 ? "" : "s"} after ${release}`,
        }
      : null;
  const parts: Part[] = [];
  if (version) parts.push({ label: "Version", ...version });
  if (branch) parts.push({ label: "Branch", value: branch });
  if (commit)
    parts.push({
      label: "Commit",
      value: commit,
      href: rev ? `${repo}/commit/${rev}` : undefined,
      title: dirty ? "Built with uncommitted changes: the code differs from this commit" : "This commit on GitHub",
      note: dirty ? " (modified)" : undefined,
    });
  return (
    <footer className="border-t border-gray-200 mt-8" data-testid="build-info">
      <div className="container mx-auto px-4 py-4 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-gray-500">
        <span className="font-medium text-gray-600">GlideX</span>
        {parts.length === 0 && <span>build information unavailable</span>}
        {parts.map((p) => (
          <span key={p.label}>
            {p.label}{" "}
            {p.href ? (
              <a
                href={p.href}
                target="_blank"
                rel="noopener noreferrer"
                title={p.title}
                className="font-mono text-gray-700 underline decoration-gray-300 hover:text-blue-700 hover:decoration-blue-700"
              >
                {p.value}
              </a>
            ) : (
              <span className="font-mono text-gray-700">{p.value}</span>
            )}
            {p.note}
          </span>
        ))}
      </div>
    </footer>
  );
}
