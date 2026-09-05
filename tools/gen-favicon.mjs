// Rasterises web/public/favicon.svg into the PNG sizes browsers and app
// launchers ask for. The SVG is the master; these are derived, so re-run this
// after editing it:
//
//     node tools/gen-favicon.mjs
//
// Chromium comes from the e2e workspace (`make install`), which is the only
// place this repo already pins a renderer.

import { createRequire } from "node:module";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const req = createRequire(join(root, "e2e", "package.json"));
const pw = await import(pathToFileURL(req.resolve("@playwright/test")).href);
const chromium = pw.chromium ?? pw.default?.chromium;

const publicDir = join(root, "web", "public");
const svg = readFileSync(join(publicDir, "favicon.svg"), "utf8");

const TILE = "#2563EB";

// `bleed` paints the tile colour behind the mark: iOS composites black behind
// any transparency, and Android masks maskable icons to its own shape, so both
// need opaque corners. Everything else keeps the SVG's rounded, transparent
// tile so it sits cleanly on browser chrome.
const targets = [
  { file: "favicon-32.png", size: 32, scale: 1, bleed: false },
  { file: "apple-touch-icon.png", size: 180, scale: 1, bleed: true },
  { file: "icon-192.png", size: 192, scale: 1, bleed: false },
  { file: "icon-512.png", size: 512, scale: 1, bleed: false },
  // Android crops maskable icons to a platform shape; 0.6 keeps the mark
  // inside the 80%-diameter safe zone whatever shape it picks.
  { file: "icon-maskable-512.png", size: 512, scale: 0.6, bleed: true },
];

const browser = await chromium.launch();
try {
  for (const { file, size, scale, bleed } of targets) {
    const page = await browser.newPage({
      viewport: { width: size, height: size },
      deviceScaleFactor: 1,
    });
    await page.setContent(
      `<body style="margin:0;width:${size}px;height:${size}px;display:grid;place-items:center;${
        bleed ? `background:${TILE}` : ""
      }">
         <div style="width:${size * scale}px;height:${size * scale}px">${svg}</div>
       </body>`,
    );
    writeFileSync(
      join(publicDir, file),
      await page.screenshot({ omitBackground: !bleed }),
    );
    await page.close();
    console.log(`${file}  ${size}x${size}`);
  }
} finally {
  await browser.close();
}
