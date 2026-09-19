// TypeScript 7 tightens side-effect imports: `import './globals.css'`
// requires a module declaration or the compiler errors with TS2882.
// Ambient module declaration keeps Next's CSS pipeline (which resolves
// the import at build time) working with strict TS.
declare module "*.css";
declare module "*.svg";
