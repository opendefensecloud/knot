import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ConnStatus } from "../../components/StatusDot";
import { type ExportDeps, useExport } from "./useExport";

const limits = { max_body_bytes: 1000, max_part_bytes: 500, max_parts: 5, scale: 2 };
const png = new Blob(["x"], { type: "image/png" });
const doc = new Blob(["d"]);
const okFile = { ok: { blob: doc, fallbacks: { diagrams: 0, images: 0 } } };
const err = (status: number, code: string) => ({ error: { code, message: code, details: {}, status } });

function makeDeps(file: ReturnType<typeof vi.fn> = vi.fn(() => Promise.resolve(okFile)), over: Partial<ExportDeps> = {}) {
  const diagrams = vi.fn(() => Promise.resolve({ ok: { limits, diagrams: [] } }));
  const deps = {
    exportApi: { diagrams, file },
    renderDiagrams: vi.fn(() => Promise.resolve(new Map([["d1", png]]))),
    exportFileName: vi.fn((t: string, f: string) => `${t}.${f}`),
    fallbackMessage: vi.fn(() => null as string | null),
    saveBlob: vi.fn(),
    notify: vi.fn(),
    ...over,
  };
  return { deps: deps as unknown as ExportDeps, diagrams, file, mocks: deps };
}

type Props = { status: ConnStatus; pendingBytes: number };

function setup(deps: ExportDeps, initial: Props = { status: "connected", pendingBytes: 0 }) {
  return renderHook((p: Props) => useExport({ docId: "d", title: "T", ...p, deps }), { initialProps: initial });
}

describe("useExport", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("waits for pending edits, then exports and saves with the sanitized name", async () => {
    const { deps, diagrams, file, mocks } = makeDeps();
    const { result, rerender } = setup(deps, { status: "connected", pendingBytes: 10 });
    let p!: Promise<void>;
    act(() => {
      p = result.current.run("docx");
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    expect(diagrams).not.toHaveBeenCalled();
    expect(result.current.exporting).toBe(true);
    rerender({ status: "connected", pendingBytes: 0 });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(500);
      await p;
    });
    expect(file).toHaveBeenCalledWith("d", "docx", new Map([["d1", png]]));
    expect(mocks.fallbackMessage).toHaveBeenCalledWith({ diagrams: 0, images: 0 }, false);
    expect(mocks.saveBlob).toHaveBeenCalledWith(doc, "T.docx");
    expect(mocks.notify).not.toHaveBeenCalled();
    expect(result.current.exporting).toBe(false);
  });

  it("exports anyway after 5 s of unsynced edits and appends the warning", async () => {
    const msg = "Exported. Your latest edits weren't saved yet and may be missing.";
    const { deps, diagrams, mocks } = makeDeps(undefined, { fallbackMessage: vi.fn(() => msg) });
    const { result } = setup(deps, { status: "offline", pendingBytes: 0 });
    let p!: Promise<void>;
    act(() => {
      p = result.current.run("odt");
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(4000);
    });
    expect(diagrams).not.toHaveBeenCalled();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1500);
      await p;
    });
    expect(deps.fallbackMessage).toHaveBeenCalledWith({ diagrams: 0, images: 0 }, true);
    expect(mocks.saveBlob).toHaveBeenCalledWith(doc, "T.odt");
    expect(mocks.notify).toHaveBeenCalledWith("warn", msg);
  });

  it("retries once without pictures on 413", async () => {
    const file = vi.fn().mockResolvedValueOnce(err(413, "too_large")).mockResolvedValueOnce(okFile);
    const { deps, mocks } = makeDeps(file);
    const { result } = setup(deps);
    await act(async () => {
      await result.current.run("docx");
    });
    expect(file).toHaveBeenCalledTimes(2);
    expect((file.mock.calls[0] as [string, string, Map<string, Blob>])[2].size).toBe(1);
    expect((file.mock.calls[1] as [string, string, Map<string, Blob>])[2].size).toBe(0);
    expect(mocks.saveBlob).toHaveBeenCalledTimes(1);
  });

  it("shows the busy toast on export.busy", async () => {
    const { deps, mocks } = makeDeps(vi.fn(() => Promise.resolve(err(503, "export.busy"))));
    const { result } = setup(deps);
    await act(async () => {
      await result.current.run("docx");
    });
    expect(mocks.notify).toHaveBeenCalledWith("warn", "Export is busy, try again in a moment.");
    expect(mocks.saveBlob).not.toHaveBeenCalled();
    expect(result.current.exporting).toBe(false);
  });

  it("second_click_while_exporting_does_nothing", async () => {
    const { deps, diagrams, mocks } = makeDeps();
    const { result } = setup(deps);
    await act(async () => {
      const a = result.current.run("docx");
      const b = result.current.run("docx");
      await Promise.all([a, b]);
    });
    expect(diagrams).toHaveBeenCalledTimes(1);
    expect(mocks.saveBlob).toHaveBeenCalledTimes(1);
  });

  it("shows the generic error toast for other failures", async () => {
    const { deps, mocks } = makeDeps(vi.fn(() => Promise.resolve(err(500, "internal"))));
    const { result } = setup(deps);
    await act(async () => {
      await result.current.run("docx");
    });
    expect(mocks.notify).toHaveBeenCalledWith(
      "error",
      "Couldn't export this page. Try again, or export as Markdown.",
    );
    expect(result.current.exporting).toBe(false);
  });
});
