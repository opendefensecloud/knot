/**
 * Pretend to run on another platform by overriding `navigator.platform`.
 *
 * jsdom reports "" (no platform), so every test runs as non-Apple unless it
 * says otherwise. jsdom defines `platform` as a getter on Navigator.prototype;
 * an own property on the instance shadows it, and deleting that property
 * brings jsdom's value back.
 *
 * Code that reads the platform at call time (lib/platform.ts) follows the
 * stub at once. Modules that read it when they load — prosemirror-keymap's
 * `mac`, which decides what `Mod-` means — only see a stub set in
 * `vi.hoisted`, before the imports.
 */
export function stubPlatform(platform: string): void {
  Object.defineProperty(window.navigator, "platform", { value: platform, configurable: true });
}

export function restorePlatform(): void {
  delete (window.navigator as { platform?: string }).platform;
}
