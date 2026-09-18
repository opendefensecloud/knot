import type { Config } from "tailwindcss";

export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  darkMode: ["class", '[data-theme="dark"]'],
  theme: {
    extend: {
      colors: {
        bg: "var(--color-bg)",
        surface: "var(--color-surface)",
        border: "var(--color-border)",
        muted: "var(--color-muted)",
        fg: "var(--color-fg)",
        "fg-muted": "var(--color-fg-muted)",
        accent: "var(--color-accent)",
        "accent-fg": "var(--color-accent-fg)",
        destructive: "var(--color-destructive)",
      },
      // Fonts and radii come from the active skin; see styles/tokens.css.
      fontFamily: {
        sans: "var(--font-ui)",
        body: "var(--font-body)",
        mono: "var(--font-mono)",
      },
      borderRadius: {
        sm: "calc(var(--radius-unit) * 2)",
        DEFAULT: "calc(var(--radius-unit) * 3)",
        md: "calc(var(--radius-unit) * 4)",
        lg: "calc(var(--radius-unit) * 6)",
      },
      transitionTimingFunction: { swift: "cubic-bezier(0.16, 1, 0.3, 1)" },
    },
  },
} satisfies Config;
