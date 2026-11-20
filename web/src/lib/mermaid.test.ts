import { beforeAll, describe, expect, it } from "vitest";

import { getMermaid } from "./mermaid";
import { sanitizeSvg } from "./sanitize";

// jsdom does no layout: Mermaid measures label text with these, so give it
// a plausible size instead of letting it throw.
beforeAll(() => {
  const proto = window.SVGElement.prototype as unknown as Record<string, unknown>;
  proto.getBBox ??= function (this: SVGElement) {
    const len = (this.textContent ?? "").length;
    return { x: 0, y: 0, width: len * 8, height: 16 };
  };
  proto.getComputedTextLength ??= function (this: SVGElement) {
    return (this.textContent ?? "").length * 8;
  };
});

/** The text the user sees, one entry per drawn line: a `<tspan class="row">`
 *  (Mermaid puts each word of a row in its own inner tspan), or a whole
 *  `<text>` that has no rows. */
function drawnLines(svg: string): string[] {
  const doc = new DOMParser().parseFromString(svg, "image/svg+xml");
  const texts: Element[] = Array.from(doc.querySelectorAll("text"));
  return texts
    .flatMap((text): Element[] => {
      const rows: Element[] = Array.from(text.querySelectorAll("tspan.row"));
      return rows.length ? rows : [text];
    })
    .map((el: Element): string => (el.textContent ?? "").replace(/\s+/g, " ").trim())
    .filter(Boolean);
}

describe("Mermaid flowchart in the editor preview", () => {
  it("keeps the box and arrow labels after sanitizeSvg, with <br/> still breaking the line", async () => {
    const m = await getMermaid();
    const { svg } = await m.render(
      "knot-test-flowchart",
      [
        "flowchart LR",
        '  G["GitLab Staging"] <-->|"Jobs abholen"| M["Runner-Manager<br/>Pod in GKE"]',
        '  M --> V["Job-VM"]',
      ].join("\n"),
    );

    const lines = drawnLines(sanitizeSvg(svg));

    // Box labels
    expect(lines).toContain("GitLab Staging");
    expect(lines).toContain("Job-VM");
    // Arrow label
    expect(lines).toContain("Jobs abholen");
    // <br/> splits the label into two drawn lines
    expect(lines).toContain("Runner-Manager");
    expect(lines).toContain("Pod in GKE");
  });
});
