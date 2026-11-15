import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { Avatar, colorFor } from "./Avatar";

afterEach(cleanup);

/** The `style` attribute as written: jsdom normalises colours read through
 *  `style.background`, and has no value for a custom property it can't
 *  resolve. */
function styleOf(el: Element): string {
  return el.getAttribute("style") ?? "";
}

describe("Avatar", () => {
  it("is coloured from its seed", () => {
    const { container } = render(<Avatar name="Ann" seed="u-ann" />);
    const badge = container.firstElementChild!;
    expect(badge).toHaveStyle({ background: colorFor("u-ann") });
    expect(styleOf(badge)).not.toContain("var(--color-fg-muted)");
    expect(badge).toHaveTextContent("A");
  });

  it("is grey when muted, whatever its seed", () => {
    const { container } = render(<Avatar name="Unknown author" seed="" muted />);
    const badge = container.firstElementChild!;
    expect(styleOf(badge)).toContain("var(--color-fg-muted)");
    expect(badge).not.toHaveStyle({ background: colorFor("") });
    expect(badge).toHaveTextContent("U");
  });
});
