import { afterEach, describe, expect, it } from "vitest";

import { restorePlatform, stubPlatform } from "../test/platform";
import { isApplePlatform } from "./platform";

afterEach(restorePlatform);

describe("isApplePlatform", () => {
  it.each(["MacIntel", "iPhone", "iPad", "iPod"])("is true on %s", (platform) => {
    stubPlatform(platform);
    expect(isApplePlatform()).toBe(true);
  });

  it.each(["Linux x86_64", "Win32", ""])("is false on %j", (platform) => {
    stubPlatform(platform);
    expect(isApplePlatform()).toBe(false);
  });

  it("reads the platform at call time", () => {
    stubPlatform("MacIntel");
    expect(isApplePlatform()).toBe(true);
    stubPlatform("Win32");
    expect(isApplePlatform()).toBe(false);
  });
});
