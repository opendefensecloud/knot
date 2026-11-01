import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { SyncStatus } from "./StatusDot";

afterEach(cleanup);

describe("SyncStatus", () => {
  it("tells the user to reload after the server refused their changes", () => {
    render(<SyncStatus sync={{ status: "refused", pendingBytes: 0 }} />);

    const status = screen.getByTestId("sync-status");
    expect(status).toHaveTextContent("Changes refused");
    expect(status).toHaveAttribute("title", expect.stringContaining("Reload the page"));
  });

  it("gives no hint where the label says enough", () => {
    render(<SyncStatus sync={{ status: "connected", pendingBytes: 0 }} />);

    expect(screen.getByTestId("sync-status")).not.toHaveAttribute("title");
  });
});
