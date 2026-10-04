import { useEffect, useState } from "react";
import { NavLink } from "react-router-dom";
import { healthCheck } from "../api";
import { useSession } from "../session";
import Activity from "./Activity";

export default function Header() {
  const session = useSession();
  const { me, host, projects, project } = session;
  const [healthy, setHealthy] = useState<boolean | null>(null);

  useEffect(() => {
    const check = () => {
      healthCheck()
        .then(() => setHealthy(true))
        .catch(() => setHealthy(false));
    };
    check();
    const id = setInterval(check, 5000);
    return () => clearInterval(id);
  }, []);

  const ownsAProject = me.project_roles.some((r) => r.role === "role.owner");
  const nav = [
    { to: "/", label: "VMs", show: true },
    { to: "/images", label: "Images", show: true },
    { to: "/disks", label: "Disks", show: true },
    { to: "/credentials", label: "Credentials", show: true },
    { to: "/networking", label: "Networking", show: true },
    { to: "/projects", label: "Projects", show: true },
    { to: "/access", label: "Access", show: host.listUsers || host.listTeams || host.readSystemBindings },
    { to: "/tokens", label: "Tokens", show: !me.token },
    { to: "/policies", label: "Policies", show: host.readPolicy },
    { to: "/audit", label: "Audit", show: host.readAudit || ownsAProject },
  ].filter((n) => n.show);

  const who = me.user?.display_name ?? me.token?.name ?? "unknown";

  return (
    <header className="bg-white shadow-sm border-b border-gray-200">
      <div className="container mx-auto px-4">
        <div className="flex flex-wrap items-center justify-between min-h-16 py-2 gap-2">
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
            <a href="/" className="text-2xl font-bold text-sky-700">
              GlideX
            </a>
            <nav className="flex flex-wrap items-center gap-1">
              {nav.map(({ to, label }) => (
                <NavLink
                  key={to}
                  to={to}
                  end={to === "/"}
                  className={({ isActive }) =>
                    `px-3 py-1.5 text-sm rounded-lg transition-colors ${
                      isActive ? "bg-sky-50 text-sky-700 font-medium" : "text-gray-600 hover:bg-gray-100"
                    }`
                  }
                >
                  {label}
                </NavLink>
              ))}
            </nav>
          </div>
          <div className="flex items-center gap-3">
            <Activity />
            <label className="flex items-center gap-1 text-sm text-gray-600">
              <span>Project</span>
              <select
                aria-label="Project"
                className="px-2 py-1 border border-gray-300 rounded-lg bg-white text-sm"
                value={project ?? ""}
                onChange={(e) => session.selectProject(e.target.value)}
                disabled={projects.length === 0}
              >
                {projects.length === 0 && <option value="">none</option>}
                {projects.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </select>
            </label>
            <span className="text-sm text-gray-700" title={me.user?.id ?? me.token?.id}>
              {who}
              {me.break_glass && <span className="ml-1 text-xs text-amber-600">(admin)</span>}
            </span>
            {me.method !== "disabled" && me.method !== "peer" && (
              <button
                className="px-3 py-1.5 text-sm text-gray-600 hover:bg-gray-100 rounded-lg"
                onClick={() => session.logout()}
              >
                Log out
              </button>
            )}
            <span className="flex items-center" title="Control plane">
              {healthy === null ? (
                <span className="w-2 h-2 bg-gray-400 rounded-full animate-pulse" />
              ) : healthy ? (
                <span className="w-2 h-2 bg-green-500 rounded-full" />
              ) : (
                <span className="w-2 h-2 bg-red-500 rounded-full" />
              )}
              <span className={`ml-1 text-xs ${healthy === false ? "text-red-600" : "text-gray-500"}`}>
                {healthy === null ? "API ..." : healthy ? "API" : "API offline"}
              </span>
            </span>
          </div>
        </div>
      </div>
    </header>
  );
}
