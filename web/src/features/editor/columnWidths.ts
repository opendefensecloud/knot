/**
 * Table column widths as proportions.
 *
 * Widths live in prosemirror-tables' `colwidth` cell attribute — an array
 * with one number per column the cell spans — because that is what its
 * table commands (add/delete column, merge/split, fixTables) already carry
 * along. knot reads those numbers as relative weights, not pixels: a column
 * renders at `weight / total` of the table, and the table always fills the
 * available width, so a layout made on one screen keeps its proportions on
 * every other one.
 *
 * Every drag rewrites all columns normalised to WEIGHT_TOTAL. A fixed total
 * keeps two editors on different screen widths from mixing scales. Pixel
 * widths stored before this existed read as weights against that same
 * total: a fully pinned table keeps its ratios exactly.
 */

import type { Node as PMNode } from "@tiptap/pm/model";
import type { Transaction } from "@tiptap/pm/state";
import { TableMap } from "@tiptap/pm/tables";

/** Committed weights add up to exactly this. */
export const WEIGHT_TOTAL = 1000;

/** The narrowest a drag may make a column, in rendered pixels. */
export const MIN_COLUMN_PX = 40;

const SHARE_EPSILON = 1e-6;

const isWeight = (value: number | null | undefined): value is number =>
  typeof value === "number" && Number.isFinite(value) && value > 0;

/**
 * Fractions of the table width (summing to 1) for stored weights. Columns
 * with no usable weight share what the set ones leave of WEIGHT_TOTAL — so a
 * column re-added after a delete takes the freed share, and a table resized
 * by prosemirror-tables (which only ever wrote the dragged column) keeps
 * that column the widest. When nothing is left, as for a column added to a
 * normalised table, each gets the mean; with no weights at all, columns are
 * equal.
 */
export function resolveShares(weights: readonly (number | null)[]): number[] {
  const known = weights.filter(isWeight);
  const knownTotal = known.reduce((total, weight) => total + weight, 0);
  const unsetCount = weights.length - known.length;
  const leftover = WEIGHT_TOTAL - knownTotal;
  let fallback = 1;
  if (known.length > 0) {
    fallback = leftover > 0 && unsetCount > 0 ? leftover / unsetCount : knownTotal / known.length;
  }
  const filled = weights.map((weight) => (isWeight(weight) ? weight : fallback));
  const total = filled.reduce((sum, weight) => sum + weight, 0);
  return filled.map((weight) => weight / total);
}

/**
 * Moves the border after column `border` by `delta` (a fraction of the table
 * width, positive to the right). Only the two neighbours change, so the
 * total stays put and the border tracks the pointer. Neither neighbour goes
 * below `minShare` — or below its current share, when it already is
 * narrower, so a small drag never makes it jump.
 */
export function moveBorder(
  shares: readonly number[],
  border: number,
  delta: number,
  minShare: number,
): number[] {
  const next = [...shares];
  const left = shares[border];
  const right = shares[border + 1];
  if (left === undefined || right === undefined) return next;

  const pair = left + right;
  const leftFloor = Math.min(minShare, left);
  const rightFloor = Math.min(minShare, right);
  const newLeft = Math.min(Math.max(left + delta, leftFloor), pair - rightFloor);
  next[border] = newLeft;
  next[border + 1] = pair - newLeft;
  return next;
}

/**
 * Integer weights summing to exactly WEIGHT_TOTAL (largest-remainder
 * rounding). None is zero: a stored zero reads back as "unset" and would
 * hand that column the mean share.
 */
export function toWeights(shares: readonly number[]): number[] {
  if (shares.length === 0) return [];
  const total = shares.reduce((sum, share) => sum + share, 0);
  const exact = shares.map((share) => (share / total) * WEIGHT_TOTAL);
  const remainder = WEIGHT_TOTAL - exact.reduce((sum, value) => sum + Math.floor(value), 0);
  const roundsUp = new Set(
    exact
      .map((value, index) => ({ index, fraction: value - Math.floor(value) }))
      .sort((a, b) => b.fraction - a.fraction || a.index - b.index)
      .slice(0, remainder)
      .map(({ index }) => index),
  );
  const weights = exact.map((value, index) => Math.floor(value) + (roundsUp.has(index) ? 1 : 0));

  const zeroCount = weights.filter((weight) => weight === 0).length;
  if (zeroCount === 0) return weights;
  const widest = weights.indexOf(Math.max(...weights));
  return weights.map((weight, index) => {
    if (weight === 0) return 1;
    return index === widest ? weight - zeroCount : weight;
  });
}

export function sharesEqual(a: readonly number[], b: readonly number[]): boolean {
  return (
    a.length === b.length &&
    a.every((share, index) => Math.abs(share - (b[index] ?? Number.NaN)) < SHARE_EPSILON)
  );
}

/** A share as a CSS percentage, trimmed to three decimals ("33.333%"). */
export function sharePercent(share: number): string {
  return `${Number((share * 100).toFixed(3))}%`;
}

/**
 * One stored weight per grid column, `null` where none is set. Read from the
 * first row — nothing spans into it, so its cells cover every column — via
 * TableMap, so a colspan there still yields one entry per column.
 */
export function readColumnWeights(table: PMNode): (number | null)[] {
  const map = TableMap.get(table);
  const weights: (number | null)[] = [];
  for (let column = 0; column < map.width; column++) {
    const cellPos = map.map[column];
    if (cellPos === undefined) {
      weights.push(null);
      continue;
    }
    const cell = table.nodeAt(cellPos);
    const colwidth = cell?.attrs.colwidth as number[] | null | undefined;
    const weight = colwidth?.[column - map.colCount(cellPos)];
    weights.push(isWeight(weight) ? weight : null);
  }
  return weights;
}

/**
 * Writes `weights` (one per grid column) onto every cell of `table`, which
 * starts its content at `tableStart`. Each cell gets the slice for the
 * columns it spans. Cells that already match are left alone, so an
 * unchanged layout produces no doc change.
 */
export function setColumnWeights(
  tr: Transaction,
  tableStart: number,
  table: PMNode,
  weights: readonly number[],
): Transaction {
  const map = TableMap.get(table);
  const visited = new Set<number>();
  for (const cellPos of map.map) {
    if (visited.has(cellPos)) continue;
    visited.add(cellPos);
    const cell = table.nodeAt(cellPos);
    if (!cell) continue;

    const firstColumn = map.colCount(cellPos);
    const colspan = (cell.attrs.colspan as number | undefined) ?? 1;
    const colwidth = weights.slice(firstColumn, firstColumn + colspan);
    const current = cell.attrs.colwidth as number[] | null;
    if (current && current.length === colwidth.length && current.every((w, i) => w === colwidth[i])) {
      continue;
    }
    tr.setNodeMarkup(tableStart + cellPos, null, { ...cell.attrs, colwidth });
  }
  return tr;
}
