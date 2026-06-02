export type ConnStatus = "connecting" | "connected" | "offline" | "unauthorised" | "conflict";

const colorOf: Record<ConnStatus, string> = {
  connecting: "#c46c0a",
  connected: "#1f7a1f",
  offline: "#777",
  unauthorised: "#b00020",
  conflict: "#b00020",
};

export function StatusDot({ status }: { status: ConnStatus }) {
  return (
    <span
      data-testid="status-dot"
      data-status={status}
      title={status}
      style={{
        display: "inline-block",
        width: 8,
        height: 8,
        borderRadius: "50%",
        background: colorOf[status],
        marginRight: 6,
      }}
    />
  );
}
