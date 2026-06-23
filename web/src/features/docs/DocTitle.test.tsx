import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { DocTitle } from "./DocPage";

afterEach(() => cleanup());

function renderTitle(editable: boolean) {
  const qc = new QueryClient();
  return render(
    <QueryClientProvider client={qc}>
      <DocTitle id="d1" initialTitle="Hello" editable={editable} />
    </QueryClientProvider>,
  );
}

describe("DocTitle gating", () => {
  it("is read-only when not editable", () => {
    renderTitle(false);
    expect(screen.getByTestId("doc-title")).toHaveAttribute("readonly");
  });

  it("is editable when editable", () => {
    renderTitle(true);
    expect(screen.getByTestId("doc-title")).not.toHaveAttribute("readonly");
  });
});
