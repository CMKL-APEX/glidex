import { lazy, Suspense } from "react";
import { Navigate, Routes, Route, useLocation } from "react-router-dom";
import Header from "./components/Header";
import Footer from "./components/Footer";
import Dashboard from "./pages/Dashboard";
import NotFound from "./pages/NotFound";
import { SessionProvider, useSession } from "./session";
import { LiveProvider } from "./live";
import { Loading } from "./components/Loading";
import ClusterBanner from "./components/ClusterBanner";

// Each page but the landing one loads when it is first opened (the console
// and its terminal too): the first load is the shell, React and the RPC client.
const VmDetail = lazy(() => import("./pages/VmDetail"));
const Credentials = lazy(() => import("./pages/Credentials"));
const Networking = lazy(() => import("./pages/Networking"));
const Images = lazy(() => import("./pages/Images"));
const Disks = lazy(() => import("./pages/Disks"));
const Datastores = lazy(() => import("./pages/Datastores"));
const Projects = lazy(() => import("./pages/Projects"));
const ProjectDetail = lazy(() => import("./pages/ProjectDetail"));
const Tokens = lazy(() => import("./pages/Tokens"));
const Audit = lazy(() => import("./pages/Audit"));
const Cluster = lazy(() => import("./pages/Cluster"));
const HostDetail = lazy(() => import("./pages/HostDetail"));
const Hosts = lazy(() => import("./pages/Hosts"));
const OldNodePage = lazy(() => import("./pages/Hosts").then((m) => ({ default: m.OldNodePage })));
const ImportPlan = lazy(() => import("./pages/ImportPlan"));
const Usage = lazy(() => import("./pages/Usage"));
const TenantDetail = lazy(() => import("./pages/TenantDetail"));
const Admissions = lazy(() => import("./pages/Admissions"));
const VmConsole = lazy(() => import("./pages/VmConsole"));
const Invite = lazy(() => import("./pages/Invite"));
const Account = lazy(() => import("./pages/Account"));
const Device = lazy(() => import("./pages/Device"));
const Directories = lazy(() => import("./pages/Directories"));
const Directory = lazy(() => import("./pages/Directory"));

function Shell() {
  const { project } = useSession();
  return (
    <LiveProvider>
      <div className="min-h-screen flex flex-col">
        <Header />
        <ClusterBanner />
        {/* Remount the pages when the project changes: they list its resources. */}
        <main key={project ?? "-"} className="container mx-auto px-4 py-8 flex-1">
          <Suspense fallback={<Loading />}>
          <Routes>
            <Route path="/" element={<Dashboard />} />
            <Route path="/vms/:id" element={<VmDetail />} />
            <Route path="/vms/:id/console" element={<VmConsole />} />
            <Route path="/credentials" element={<Credentials />} />
            <Route path="/networking" element={<Networking />} />
            <Route path="/images" element={<Images />} />
            <Route path="/disks" element={<Disks />} />
            <Route path="/datastores" element={<Datastores />} />
            <Route path="/projects" element={<Projects />} />
            <Route path="/projects/:id" element={<ProjectDetail />} />
            <Route path="/directory" element={<Directory />} />
            <Route path="/directory/:tab" element={<Directory />} />
            {/* The directory's pages are its tabs now; old links still work. */}
            <Route path="/tenants" element={<Navigate to="/directory/tenants" replace />} />
            <Route path="/tenants/:id" element={<TenantDetail />} />
            <Route path="/admissions" element={<Admissions />} />
            <Route path="/connectors" element={<Navigate to="/directory/connectors" replace />} />
            <Route path="/access" element={<Navigate to="/directory/access" replace />} />
            <Route path="/tokens" element={<Tokens />} />
            <Route path="/policies" element={<Navigate to="/directory/policies" replace />} />
            <Route path="/audit" element={<Audit />} />
            <Route path="/cluster" element={<Cluster />} />
            <Route path="/hosts" element={<Hosts />} />
            <Route path="/hosts/:id" element={<HostDetail />} />
            <Route path="/cluster/nodes/:id" element={<OldNodePage />} />
            <Route path="/cluster/imports/:plan" element={<ImportPlan />} />
            <Route path="/usage" element={<Usage />} />
            <Route path="/account" element={<Account />} />
            <Route path="/device" element={<Device />} />
            <Route path="/directories" element={<Directories />} />
            {/* A directory's login page, once signed in (spec/tenancy.md D61). */}
            <Route path="/login/:directory" element={<Navigate to="/" replace />} />
            <Route path="*" element={<NotFound />} />
          </Routes>
          </Suspense>
        </main>
        <Footer />
      </div>
    </LiveProvider>
  );
}

export default function App() {
  const { pathname } = useLocation();
  // An invitation link works signed out: it is its own way to log in.
  if (pathname.startsWith("/invite/")) {
    return (
      <Suspense fallback={<Loading />}>
        <Invite />
      </Suspense>
    );
  }
  return (
    <SessionProvider>
      <Shell />
    </SessionProvider>
  );
}
