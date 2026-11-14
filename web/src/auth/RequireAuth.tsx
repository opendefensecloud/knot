import { Navigate, Outlet, useLocation } from "react-router-dom";

import { useSession } from "./SessionContext";

export function RequireAuth() {
  const q = useSession();
  const loc = useLocation();
  if (q.isLoading) return <div style={{ padding: 24 }}>Loading…</div>;
  const data = q.data;
  if (!data || "error" in data) {
    // The whole link, so `?thread=` / `?suggestion=` survive the sign-in.
    const from = loc.pathname + loc.search + loc.hash;
    return <Navigate to="/login" replace state={{ from }} />;
  }
  return <Outlet />;
}
