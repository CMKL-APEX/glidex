import { useEffect, useState } from "react";
import { NavLink } from "react-router-dom";
import { healthCheck } from "../api";

export default function Header() {
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

  return (
    <header className="bg-white shadow-sm border-b border-gray-200">
      <div className="container mx-auto px-4">
        <div className="flex items-center justify-between h-16">
          <div className="flex items-center space-x-4">
            <a href="/" className="text-2xl font-bold text-sky-700">
              GlideX
            </a>
            <span className="text-sm text-gray-500">VM Control Panel</span>
            <nav className="flex items-center space-x-1 pl-4">
              {[
                { to: "/", label: "VMs" },
                { to: "/images", label: "Images" },
                { to: "/disks", label: "Disks" },
                { to: "/credentials", label: "Credentials" },
                { to: "/networking", label: "Networking" },
              ].map(({ to, label }) => (
                <NavLink
                  key={to}
                  to={to}
                  end={to === "/"}
                  className={({ isActive }) =>
                    `px-3 py-1.5 text-sm rounded-lg transition-colors ${
                      isActive
                        ? "bg-sky-50 text-sky-700 font-medium"
                        : "text-gray-600 hover:bg-gray-100"
                    }`
                  }
                >
                  {label}
                </NavLink>
              ))}
            </nav>
          </div>
          <div className="flex items-center space-x-2">
            <span className="text-sm text-gray-600">API:</span>
            {healthy === null ? (
              <span className="flex items-center">
                <span className="w-2 h-2 bg-gray-400 rounded-full animate-pulse" />
                <span className="ml-2 text-sm text-gray-500">...</span>
              </span>
            ) : healthy ? (
              <span className="flex items-center">
                <span className="w-2 h-2 bg-green-500 rounded-full" />
                <span className="ml-2 text-sm text-green-600">Healthy</span>
              </span>
            ) : (
              <span className="flex items-center">
                <span className="w-2 h-2 bg-red-500 rounded-full" />
                <span className="ml-2 text-sm text-red-600">Offline</span>
              </span>
            )}
          </div>
        </div>
      </div>
    </header>
  );
}
