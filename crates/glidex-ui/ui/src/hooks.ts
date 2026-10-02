import { useEffect, useState } from "react";
import * as api from "./api";
import type { Directory } from "./components/ui";
import { useSession } from "./session";

/** Users and teams, when the caller may list them (`listUsers`,
 * `listTeams`); `null` when it may list neither. */
export function useDirectory(): [Directory | null, () => void] {
  const { host } = useSession();
  const [dir, setDir] = useState<Directory | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!host.listUsers && !host.listTeams) {
      setDir(null);
      return;
    }
    let live = true;
    Promise.all([
      host.listUsers ? api.listUsers().catch(() => []) : Promise.resolve([]),
      host.listTeams ? api.listTeams().catch(() => []) : Promise.resolve([]),
    ]).then(([users, teams]) => live && setDir({ users, teams }));
    return () => {
      live = false;
    };
  }, [host.listUsers, host.listTeams, tick]);
  return [dir, () => setTick((t) => t + 1)];
}
