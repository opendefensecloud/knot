# Frontend dependency audit notes

`make audit.web` runs `pnpm audit --prod`. It currently exits non-zero — the
remaining advisories are **real but have no compatible fix** and are tracked
here. Re-check this list whenever `@excalidraw/excalidraw` / `mermaid` are
upgraded (that's the path to resolving them).

## Fixed

- **dompurify** → `^3.4.11` (direct dep + a pnpm override so mermaid's and
  excalidraw's copies patch too). This is our HTML/SVG sanitizer
  (`src/lib/sanitize.ts`); the bump clears the ALLOWED_ATTR-pollution and
  Trusted-Types advisories. `sanitize.test.ts` still green on 3.4.11.
- **nanoid 3.x** → `^3.3.8` via override (in-major patch).

## Open — no compatible fix yet (awaiting upstream excalidraw/mermaid)

All are transitive, pulled in only via
`@excalidraw/excalidraw > @excalidraw/mermaid-to-excalidraw > …`:

- **GHSA-r5fr-rjxr-66jc** (lodash-es, *high*, `_.template` code injection) and
  **GHSA-f23m-r3pf-42rh** / **GHSA-xxjr-mmjv-4gpg** (lodash-es, *moderate*,
  prototype pollution). No patched `lodash-es` release exists. Reached only
  through Chevrotain's grammar→`.d.ts` codegen (`…langium > chevrotain >
  @chevrotain/cst-dts-gen`) — build/parse tooling that never receives our app's
  user data at runtime, so not exploitable in our usage, but flagged at its
  upstream severity.
- **GHSA-mwcw-c2x4-8c55** (nanoid `4.x`, *moderate*, predictable ids for
  non-integer sizes). The only fix is `nanoid@5`, which mermaid's parser doesn't
  yet support. nanoid here generates diagram element ids, not security tokens.

## Re-check

```sh
make audit.web                       # pnpm audit --prod (currently exits 1)
cd web && pnpm why lodash-es nanoid  # confirm the dependency paths
```
