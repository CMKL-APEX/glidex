import { Routes, Route } from "react-router-dom";
import Header from "./components/Header";
import Dashboard from "./pages/Dashboard";
import VmDetail from "./pages/VmDetail";
import VmConsole from "./pages/VmConsole";
import NotFound from "./pages/NotFound";
import Credentials from "./pages/Credentials";
import Networking from "./pages/Networking";

export default function App() {
  return (
    <div className="min-h-screen">
      <Header />
      <main className="container mx-auto px-4 py-8">
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/vms/:id" element={<VmDetail />} />
          <Route path="/vms/:id/console" element={<VmConsole />} />
          <Route path="/credentials" element={<Credentials />} />
          <Route path="/networking" element={<Networking />} />
          <Route path="*" element={<NotFound />} />
        </Routes>
      </main>
    </div>
  );
}
