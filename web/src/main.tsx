import "./styles/global.css";

import { QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import ReactDOM from "react-dom/client";
import { RouterProvider } from "react-router-dom";

import { SessionProvider } from "./auth/SessionContext";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { queryClient } from "./lib/queryClient";
import { router } from "./routes";
import { readInitialSkin, stampSkin } from "./stores/ui";

// Stamp data-skin + data-theme before first paint so a dark-skin user
// never sees the light palette flash in before React hydrates.
stampSkin(readInitialSkin());

// Stamped before first paint, like the theme above, so a wide-mode user
// never sees the narrow column flash in before React hydrates.
let initialDocWidth = "fixed";
try {
  if (localStorage.getItem("knot.docWidth") === "wide") initialDocWidth = "wide";
} catch { /* storage unavailable */ }
document.documentElement.setAttribute("data-doc-width", initialDocWidth);

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary>
      <QueryClientProvider client={queryClient}>
        <SessionProvider>
          <RouterProvider router={router} />
        </SessionProvider>
      </QueryClientProvider>
    </ErrorBoundary>
  </React.StrictMode>,
);
