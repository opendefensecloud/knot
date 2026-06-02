import { useEffect } from "react";

import { useUi } from "../stores/ui";

export function Toast() {
  const toasts = useUi((s) => s.toasts);
  const dismiss = useUi((s) => s.dismiss);

  useEffect(() => {
    const timers = toasts.map((t) =>
      setTimeout(() => dismiss(t.id), 4000),
    );
    return () => { timers.forEach(clearTimeout); };
  }, [toasts, dismiss]);

  if (toasts.length === 0) return null;
  return (
    <div
      data-testid="toast-stack"
      style={{
        position: "fixed",
        bottom: 16,
        right: 16,
        display: "grid",
        gap: 8,
        zIndex: 50,
      }}
    >
      {toasts.map((t) => (
        <div
          key={t.id}
          data-testid={`toast-${t.kind}`}
          style={{
            padding: "10px 14px",
            borderRadius: 6,
            color: "white",
            background:
              t.kind === "error" ? "#b00020" : t.kind === "warn" ? "#c46c0a" : "#404040",
            boxShadow: "0 2px 8px rgba(0,0,0,0.2)",
          }}
        >
          {t.text}
        </div>
      ))}
    </div>
  );
}
