import { useEffect } from "react";

import { useUi, type Toast as ToastData } from "../stores/ui";

const kindBorder: Record<string, string> = {
  error: "border-l-destructive",
  warn: "border-l-amber-500",
  info: "border-l-accent",
};

const DISMISS_AFTER_MS = 4000;

export function Toast() {
  const toasts = useUi((s) => s.toasts);

  if (toasts.length === 0) return null;
  return (
    <div
      data-testid="toast-stack"
      className="fixed bottom-4 right-4 z-50 grid gap-2"
    >
      {toasts.map((t) => (
        // A keyed toast keeps its element when it is replaced, so it doesn't
        // slide in again; its new id restarts its timer.
        <ToastItem key={t.key ?? `id-${t.id}`} toast={t} />
      ))}
    </div>
  );
}

function ToastItem({ toast }: { toast: ToastData }) {
  const dismiss = useUi((s) => s.dismiss);
  const { id } = toast;
  const dismissAfterMs = toast.durationMs ?? DISMISS_AFTER_MS;

  // One timer per toast, started when it (or its replacement) appears, so
  // a new toast never extends the ones already showing.
  useEffect(() => {
    const timer = setTimeout(() => dismiss(id), dismissAfterMs);
    return () => { clearTimeout(timer); };
  }, [id, dismissAfterMs, dismiss]);

  return (
    <div
      data-testid={`toast-${toast.kind}`}
      role="status"
      aria-live="polite"
      className={`max-w-sm rounded-md border border-border border-l-4 ${kindBorder[toast.kind] ?? "border-l-accent"} bg-surface shadow-lg px-4 py-3 text-sm text-fg`}
      style={{ animation: "slideIn 200ms ease-out" }}
    >
      {toast.text}
    </div>
  );
}
