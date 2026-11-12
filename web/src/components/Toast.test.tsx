import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useUi } from "../stores/ui";
import { Toast } from "./Toast";

beforeEach(() => {
  vi.useFakeTimers();
  useUi.setState({ toasts: [] });
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("Toast", () => {
  it("dismisses a toast after four seconds", () => {
    render(<Toast />);
    act(() => useUi.getState().notify("error", "Couldn't post comment"));
    expect(screen.getByTestId("toast-error")).toHaveTextContent("Couldn't post comment");
    act(() => { vi.advanceTimersByTime(4000); });
    expect(screen.queryByTestId("toast-error")).toBeNull();
  });

  it("restarts a keyed toast's timer when it is replaced", () => {
    render(<Toast />);
    act(() => useUi.getState().notify("warn", "Try again in 7 s.", { key: "limit" }));
    act(() => { vi.advanceTimersByTime(3000); });
    act(() => useUi.getState().notify("warn", "Try again in 3 s.", { key: "limit" }));

    const shown = screen.getAllByTestId("toast-warn");
    expect(shown).toHaveLength(1);
    expect(shown[0]).toHaveTextContent("Try again in 3 s.");

    // Past the first toast's four seconds: the replacement is still up.
    act(() => { vi.advanceTimersByTime(2000); });
    expect(screen.getByTestId("toast-warn")).toHaveTextContent("Try again in 3 s.");
    act(() => { vi.advanceTimersByTime(2000); });
    expect(screen.queryByTestId("toast-warn")).toBeNull();
  });

  it("keeps a toast up for the duration it asks for", () => {
    render(<Toast />);
    act(() => useUi.getState().notify("info", "Uploads paused — continuing in 20 s…", { key: "limit", durationMs: 20_000 }));
    act(() => { vi.advanceTimersByTime(19_000); });
    expect(screen.getByTestId("toast-info")).toHaveTextContent("Uploads paused");
    act(() => { vi.advanceTimersByTime(1000); });
    expect(screen.queryByTestId("toast-info")).toBeNull();
  });

  it("gives a replacement the default duration unless it asks for its own", () => {
    render(<Toast />);
    act(() => useUi.getState().notify("info", "Uploads paused — continuing in 20 s…", { key: "limit", durationMs: 20_000 }));
    act(() => useUi.getState().notify("warn", "You've reached your file storage limit.", { key: "limit" }));
    act(() => { vi.advanceTimersByTime(4000); });
    expect(screen.queryByTestId("toast-warn")).toBeNull();
  });

  it("does not extend older toasts when a new one arrives", () => {
    render(<Toast />);
    act(() => useUi.getState().notify("error", "first"));
    act(() => { vi.advanceTimersByTime(3000); });
    act(() => useUi.getState().notify("info", "second"));
    act(() => { vi.advanceTimersByTime(1000); });
    expect(screen.queryByTestId("toast-error")).toBeNull();
    expect(screen.getByTestId("toast-info")).toHaveTextContent("second");
  });
});
