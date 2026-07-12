// PostCSS pipeline for Tailwind v3. Autoprefixer covers browser
// prefixes; Tailwind emits its utility classes at build time.
module.exports = {
  plugins: {
    tailwindcss: {},
    autoprefixer: {},
  },
};
