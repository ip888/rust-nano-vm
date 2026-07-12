import type { Config } from "tailwindcss";

// Tailwind v3 config. Kept minimal — the design system is "sensible
// defaults + a few brand accents". Extend when a real design lands.
const config: Config = {
  content: ["./app/**/*.{ts,tsx}", "./components/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        brand: {
          50: "#f5f8ff",
          500: "#3b6ff5",
          600: "#2f5be0",
          700: "#254bbf",
        },
      },
      fontFamily: {
        sans: ["ui-sans-serif", "system-ui", "sans-serif"],
        mono: ["ui-monospace", "SFMono-Regular", "monospace"],
      },
    },
  },
  plugins: [],
};

export default config;
