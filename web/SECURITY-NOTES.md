# Frontend dependency audit notes

`make audit.web` runs `pnpm audit --prod`. As of the 0.2.0 dependency refresh
both it and a full (dev-inclusive) `pnpm audit` report **zero** advisories.

Keep it that way: re-run both after any dependency change, and prefer bumping a
parent over adding an override.

## How the remaining risk is held down

Everything below lives in [`pnpm-workspace.yaml`](pnpm-workspace.yaml). pnpm
≥ 11 no longer reads the `pnpm` field in `package.json`, which is where these
used to live — if you see a `[WARN] The "pnpm" field in package.json is no
longer read`, something has been moved back by mistake.

Three transitive packages are pinned by their parents to **exact** versions, so
no parent bump can reach a patched release and an override is the only fix:

- **nanoid** — `@excalidraw/excalidraw` hard-pins `3.3.3`, and
  `@excalidraw/mermaid-to-excalidraw` hard-pins `4.0.2`. Overridden to
  `^3.3.18` and `^5.1.16` respectively. The 4.x line has no patched release;
  5.x is the first fixed major and exports the same named symbol, which is all
  the consumer uses.
- **lodash-es** — `chevrotain` and `@chevrotain/{gast,cst-dts-gen}` hard-pin
  `4.17.21`. Overridden to `^4.18.1`, which patches the `_.template` code
  injection and both prototype-pollution advisories.
- **dompurify** — floored at `^3.4.13` regardless of what mermaid or Excalidraw
  request. This is knot's HTML/SVG sanitiser (`src/lib/sanitize.ts`), whose
  output feeds `dangerouslySetInnerHTML` in `ExcalidrawBoardView.tsx` and
  `MermaidCodeBlock.tsx` over SVG authored by other workspace users, so it is
  the one entry here with a genuine user-data path.

There are also three **deduplication** overrides on `prosemirror-model`,
`-state` and `-view`. Those are not security pins: `@tiptap/pm` and
`y-prosemirror` both declare wide `^1.x` ranges, and when pnpm resolves them to
different patches the editor gets two nominally distinct `Node` types and `tsc`
fails. Remove them only if the ProseMirror stack is deduplicated some other way.

## Superseded notes

Earlier revisions of this file recorded lodash-es and nanoid 4.x as having no
compatible fix, and dompurify as resolved at `^3.4.11`. All three statements are
now out of date: `lodash-es` 4.18.x shipped, a `nanoid@4 -> ^5` override
resolves cleanly, and 3.4.11 is itself vulnerable to GHSA-55q2-fjhq-7xh7 and
GHSA-c2j3-45gr-mqc4.

## Re-check

```sh
make audit.web                        # pnpm audit --prod
cd web && pnpm audit                  # includes devDependencies (vite/vitest)
cd web && pnpm why nanoid lodash-es dompurify
```

`--prod` alone hides the build toolchain. Run the unfiltered audit too — the
vite/vitest cluster only ever showed up there.
