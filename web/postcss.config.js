// PostCSS pipeline for Tailwind v4. The v4 PostCSS plugin ships as
// its own package (`@tailwindcss/postcss`) rather than as a mode of
// the `tailwindcss` package. Autoprefixer stays for browser prefixes.
module.exports = {
  plugins: {
    "@tailwindcss/postcss": {},
    autoprefixer: {},
  },
};
