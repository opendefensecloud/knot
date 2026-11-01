import { getSchema, type JSONContent } from "@tiptap/core";
import type { Node as PMNode } from "@tiptap/pm/model";
import { EditorState } from "@tiptap/pm/state";
import StarterKit from "@tiptap/starter-kit";
import { describe, expect, it } from "vitest";

import {
  moveBorder,
  readColumnWeights,
  resolveShares,
  setColumnWeights,
  sharePercent,
  sharesEqual,
  toWeights,
} from "./columnWidths";
import { KnotTable, KnotTableCell, KnotTableHeader, KnotTableRow } from "./TableExtensions";

const schema = getSchema([StarterKit, KnotTable, KnotTableRow, KnotTableCell, KnotTableHeader]);

type CellSpec = { header?: boolean; colspan?: number; rowspan?: number; colwidth?: number[] | null };

function tableJson(rows: CellSpec[][]): JSONContent {
  return {
    type: "table",
    content: rows.map((cells) => ({
      type: "table_row",
      content: cells.map((cell) => ({
        type: cell.header ? "table_header" : "table_cell",
        attrs: {
          colspan: cell.colspan ?? 1,
          rowspan: cell.rowspan ?? 1,
          colwidth: cell.colwidth ?? null,
        },
        content: [{ type: "paragraph" }],
      })),
    })),
  };
}

function tableNode(rows: CellSpec[][]): PMNode {
  return schema.nodeFromJSON(tableJson(rows));
}

/** colwidth of every cell, row by row, as stored. */
function storedColwidths(table: PMNode): (number[] | null)[][] {
  const rows: (number[] | null)[][] = [];
  table.forEach((row) => {
    const cells: (number[] | null)[] = [];
    row.forEach((cell) => cells.push(cell.attrs.colwidth as number[] | null));
    rows.push(cells);
  });
  return rows;
}

const sum = (values: readonly number[]) => values.reduce((total, value) => total + value, 0);

describe("resolveShares", () => {
  it("gives every column an equal share when none is stored", () => {
    expect(resolveShares([null, null, null, null])).toEqual([0.25, 0.25, 0.25, 0.25]);
  });

  it("keeps the ratio of stored weights whatever their scale, so legacy pixels survive", () => {
    expect(resolveShares([200, 600])).toEqual([0.25, 0.75]);
    expect(resolveShares([250, 750])).toEqual([0.25, 0.75]);
  });

  it("gives a column added to a resized table the mean share, since nothing is left of the total", () => {
    const shares = resolveShares([400, null, 600]);
    expect(shares[0]).toBeCloseTo(400 / 1500);
    expect(shares[1]).toBeCloseTo(500 / 1500);
    expect(shares[2]).toBeCloseTo(600 / 1500);
  });

  it("lets unset columns share what is left of the total", () => {
    // prosemirror-tables only ever wrote the dragged column, so a table
    // resized before this existed typically looks like this. The dragged
    // column has to stay the widest, not drop back to an equal share.
    const shares = resolveShares([300, null, null, null]);
    expect(shares[0]).toBeCloseTo(0.3);
    for (const share of shares.slice(1)) expect(share).toBeCloseTo(0.7 / 3);
  });

  it("hands a column re-added after a delete the share the deleted one left", () => {
    expect(resolveShares([500, 250, null])).toEqual([0.5, 0.25, 0.25]);
  });

  it("treats zero, negative and non-finite weights as unset", () => {
    expect(resolveShares([0, 300, -5, Number.NaN])).toEqual(resolveShares([null, 300, null, null]));
  });

  it("returns no shares for no columns", () => {
    expect(resolveShares([])).toEqual([]);
  });
});

describe("moveBorder", () => {
  const quarters = [0.25, 0.25, 0.25, 0.25];

  it("moves width between the two neighbours only", () => {
    const moved = moveBorder(quarters, 0, 0.1, 0.05);
    expect(moved[0]).toBeCloseTo(0.35);
    expect(moved[1]).toBeCloseTo(0.15);
    expect(moved.slice(2)).toEqual([0.25, 0.25]);
    expect(sum(moved)).toBeCloseTo(1);
  });

  it("stops the left neighbour at the minimum", () => {
    const moved = moveBorder(quarters, 1, -0.5, 0.05);
    expect(moved[1]).toBeCloseTo(0.05);
    expect(moved[2]).toBeCloseTo(0.45);
  });

  it("stops the right neighbour at the minimum", () => {
    const moved = moveBorder(quarters, 0, 0.5, 0.05);
    expect(moved[0]).toBeCloseTo(0.45);
    expect(moved[1]).toBeCloseTo(0.05);
  });

  it("never makes a column already under the minimum jump wider or shrink further", () => {
    const narrow = [0.02, 0.48, 0.5];
    expect(moveBorder(narrow, 0, -0.01, 0.05)).toEqual(narrow);
    const nudged = moveBorder(narrow, 0, 0.01, 0.05);
    expect(nudged[0]).toBeCloseTo(0.03);
    expect(nudged[1]).toBeCloseTo(0.47);
  });

  it("ignores a border that does not exist", () => {
    expect(moveBorder(quarters, 3, 0.1, 0.05)).toEqual(quarters);
    expect(moveBorder(quarters, -1, 0.1, 0.05)).toEqual(quarters);
  });
});

describe("toWeights", () => {
  it("scales shares to integers that sum to exactly 1000", () => {
    expect(toWeights([0.35, 0.15, 0.25, 0.25])).toEqual([350, 150, 250, 250]);
  });

  it("hands rounding leftovers to the largest remainders", () => {
    const weights = toWeights([1 / 3, 1 / 3, 1 / 3]);
    expect(sum(weights)).toBe(1000);
    expect(weights).toEqual([334, 333, 333]);
  });

  it("never stores a zero, which would read back as unset", () => {
    const weights = toWeights([0.0001, 0.9999]);
    expect(weights).toEqual([1, 999]);
  });

  it("returns no weights for no columns", () => {
    expect(toWeights([])).toEqual([]);
  });
});

describe("sharesEqual", () => {
  it("ignores floating-point noise", () => {
    expect(sharesEqual([0.25, 0.75], [0.25, 0.7500000001])).toBe(true);
  });

  it("detects a moved border and a changed column count", () => {
    expect(sharesEqual([0.25, 0.75], [0.3, 0.7])).toBe(false);
    expect(sharesEqual([0.5, 0.5], [0.5, 0.25, 0.25])).toBe(false);
  });
});

describe("sharePercent", () => {
  it("formats a share as a short CSS percentage", () => {
    expect(sharePercent(0.25)).toBe("25%");
    expect(sharePercent(1 / 3)).toBe("33.333%");
  });
});

describe("readColumnWeights", () => {
  it("reads one weight per grid column, through a colspan in the first row", () => {
    const table = tableNode([
      [{ header: true, colspan: 2, colwidth: [100, 200] }, { header: true, colwidth: [300] }],
      [{}, {}, {}],
    ]);
    expect(readColumnWeights(table)).toEqual([100, 200, 300]);
  });

  it("reports unset and zero widths as null", () => {
    const table = tableNode([[{ colwidth: null }, { colwidth: [0] }, { colwidth: [120] }]]);
    expect(readColumnWeights(table)).toEqual([null, null, 120]);
  });
});

describe("setColumnWeights", () => {
  function apply(rows: CellSpec[][], weights: number[]) {
    const doc = schema.nodeFromJSON({ type: "doc", content: [tableJson(rows)] });
    const state = EditorState.create({ doc });
    const tr = setColumnWeights(state.tr, 1, doc.firstChild!, weights);
    return { tr, table: tr.doc.firstChild! };
  }

  it("writes the weights onto every cell of every row, sliced across a colspan", () => {
    const { table } = apply(
      [
        [{ header: true, colspan: 2 }, { header: true }],
        [{}, {}, {}],
      ],
      [500, 200, 300],
    );
    expect(storedColwidths(table)).toEqual([
      [[500, 200], [300]],
      [[500], [200], [300]],
    ]);
  });

  it("writes a rowspan cell once, and the shorter row below it by its real columns", () => {
    const { table } = apply(
      [
        [{ rowspan: 2 }, {}, {}],
        [{}, {}],
      ],
      [100, 300, 600],
    );
    expect(storedColwidths(table)).toEqual([
      [[100], [300], [600]],
      [[300], [600]],
    ]);
  });

  it("changes nothing when the weights are already stored", () => {
    const { tr } = apply([[{ colwidth: [400] }, { colwidth: [600] }]], [400, 600]);
    expect(tr.docChanged).toBe(false);
  });
});
