// registry.local.json is developer-created (gitignored — copy
// registry.local.example.json), so on a clean checkout tsc cannot resolve
// index.local.ts's import statically. This ambient wildcard makes the module
// typecheck as `unknown` when the file is absent; when it exists, TypeScript's
// real JSON resolution wins. Wrangler's bundler still requires the real file at
// dev time — `npm run dev:local` creates it from the example if missing.
declare module "*registry.local.json" {
  const raw: unknown;
  export default raw;
}
