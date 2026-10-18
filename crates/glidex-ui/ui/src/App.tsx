import { lazy, Suspense } from "react";
import { Routes, Route } from "react-router-dom";
import Header from "./components/Header";
import Footer from "./components/Footer";
import Dashboard from "./pages/Dashboard";
import NotFound from "./pages/NotFound";
import { SessionProvider, useSession } from "./session";
import { LiveProvider } from "./live";
import { Loading } from "./components/Loading";
import ClusterBanner from "./components/ClusterBanner";

// Each page but the landing one loads when it is first opened, the console
// (xterm) included: the first load is the shell, React and the RPC client.
const VmDetail = lazy(() => import("./pages/VmDetail"));
const Credentials = lazy(() => import("./pages/Credentials"));
const Networking = lazy(() => import("./pages/Networking"));
const Images = lazy(() => import("./pages/Images"));
const Disks = lazy(() => import("./pages/Disks"));
const Projects = lazy(() => import("./pages/Projects"));
const ProjectDetail = lazy(() => import("./pages/ProjectDetail"));
const Access = lazy(() => import("./pages/Access"));
const Tokens = lazy(() => import("./pages/Tokens"));
const Policies = lazy(() => import("./pages/Policies"));
const Audit = lazy(() => import("./pages/Audit"));
const Cluster = lazy(() => import("./pages/Cluster"));
const NodeDetail = lazy(() => import("./pages/NodeDetail"));
const ImportPlan = lazy(() => import("./pages/ImportPlan"));
const Usage = lazy(() => import("./pages/Usage"));
const Tenants = lazy(() => import("./pages/Tenants"));
const TenantDetail = lazy(() => import("./pages/TenantDetail"));
const Admissions = lazy(() => import("./pages/Admissions"));
const Connectors = lazy(() => import("./pages/Connectors"));
const VmConsole = lazy(() => import("./pages/VmConsole"));

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
            <Route path="/projects" element={<Projects />} />
            <Route path="/projects/:id" element={<ProjectDetail />} />
            <Route path="/tenants" element={<Tenants />} />
            <Route path="/tenants/:id" element={<TenantDetail />} />
            <Route path="/admissions" element={<Admissions />} />
            <Route path="/connectors" element={<Connectors />} />
            <Route path="/access" element={<Access />} />
            <Route path="/tokens" element={<Tokens />} />
            <Route path="/policies" element={<Policies />} />
            <Route path="/audit" element={<Audit />} />
            <Route path="/cluster" element={<Cluster />} />
            <Route path="/cluster/nodes/:id" element={<NodeDetail />} />
            <Route path="/cluster/imports/:plan" element={<ImportPlan />} />
            <Route path="/usage" element={<Usage />} />
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
  return (
    <SessionProvider>
      <Shell />
    </SessionProvider>
  );
}
