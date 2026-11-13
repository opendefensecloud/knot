import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { createContext, useContext, type ReactNode } from "react";

import { type ApiError } from "../lib/api";

import { authApi, type Session } from "./session.api";

type SessionQuery = UseQueryResult<{ ok: Session } | { error: ApiError }, Error>;

const Ctx = createContext<SessionQuery | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const q = useQuery({
    queryKey: ["session"],
    queryFn: () => authApi.session(),
    retry: false,
  });
  return <Ctx.Provider value={q}>{children}</Ctx.Provider>;
}

export function useSession(): SessionQuery {
  const q = useContext(Ctx);
  if (!q) throw new Error("useSession must be used inside SessionProvider");
  return q;
}
