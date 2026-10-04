import { Routes, Route } from "react-router-dom";
import Header from "./components/Header";
import Footer from "./components/Footer";
import Dashboard from "./pages/Dashboard";
import VmDetail from "./pages/VmDetail";
import VmConsole from "./pages/VmConsole";
import NotFound from "./pages/NotFound";
import Credentials from "./pages/Credentials";
import Networking from "./pages/Networking";
import Images from "./pages/Images";
import Disks from "./pages/Disks";
import Projects from "./pages/Projects";
import ProjectDetail from "./pages/ProjectDetail";
import Access from "./pages/Access";
import Tokens from "./pages/Tokens";
import Policies from "./pages/Policies";
import Audit from "./pages/Audit";
import { SessionProvider, useSession } from "./session";
import { LiveProvider } from "./live";

function Shell() {
  const { project } = useSession();
  return (
    <LiveProvider>
      <div className="min-h-screen flex flex-col">
        <Header />
        {/* Remount the pages when the project changes: they list its resources. */}
        <main key={project ?? "-"} className="container mx-auto px-4 py-8 flex-1">
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
            <Route path="/access" element={<Access />} />
            <Route path="/tokens" element={<Tokens />} />
            <Route path="/policies" element={<Policies />} />
            <Route path="/audit" element={<Audit />} />
            <Route path="*" element={<NotFound />} />
          </Routes>
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
