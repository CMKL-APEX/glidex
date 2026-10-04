/** The build this UI came from: version tag (if any), branch and commit. */
export default function Footer() {
  const { tag, branch, commit, dirty } = __GLIDEX_BUILD__;
  const parts = [
    tag && { label: "Version", value: tag },
    branch && { label: "Branch", value: branch },
    commit && { label: "Commit", value: dirty ? `${commit} (modified)` : commit },
  ].filter((p): p is { label: string; value: string } => !!p);
  return (
    <footer className="border-t border-gray-200 mt-8" data-testid="build-info">
      <div className="container mx-auto px-4 py-4 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-gray-500">
        <span className="font-medium text-gray-600">GlideX</span>
        {parts.length === 0 && <span>build information unavailable</span>}
        {parts.map((p) => (
          <span key={p.label}>
            {p.label} <span className="font-mono text-gray-700">{p.value}</span>
          </span>
        ))}
      </div>
    </footer>
  );
}
