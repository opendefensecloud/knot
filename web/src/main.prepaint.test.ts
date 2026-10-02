import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// main.tsx mounts the whole app as it loads. Mounting is not what these
// tests are about — what main.tsx does before it is — so the root is a stub.
vi.mock("react-dom/client", () => {
  const createRoot = () => ({ render: () => {} });
  return { default: { createRoot }, createRoot };
});

const realWidth = window.innerWidth;

function setViewportWidth(w: number) {
  Object.defineProperty(window, "innerWidth", { value: w, configurable: true, writable: true });
}

/** Runs main.tsx afresh, as a page load would. */
async function loadMain() {
  vi.resetModules();
  await import("./main");
}

function stampedWidth() {
  return document.documentElement.style.getPropertyValue("--knot-sidebar-w");
}

describe("main.tsx, before first paint", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.style.removeProperty("--knot-sidebar-w");
    setViewportWidth(1280);
  });

  afterEach(() => {
    setViewportWidth(realWidth);
  });

  it("stamps the saved sidebar width, so the default never flashes first", async () => {
    localStorage.setItem("knot.sidebarWidth", "340");
    await loadMain();
    expect(stampedWidth()).toBe("340px");
  });

  it("stamps the width the window allows", async () => {
    localStorage.setItem("knot.sidebarWidth", "480");
    setViewportWidth(800);
    await loadMain();
    expect(stampedWidth()).toBe("420px");
  });

  it("stamps the default when nothing is saved", async () => {
    await loadMain();
    expect(stampedWidth()).toBe("260px");
  });
});
