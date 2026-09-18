// Build the self-hosted docs: `mint export` (needs an authenticated CLI: the
// exported client only carries page content for a logged-in export), unzip
// into out/, point the bundled search UI at our own /api/search, and write the
// search index that backend reads. Fails loudly at every step that matters.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { buildSearchIndex } from "./search-index.mjs";

const root = path.resolve(new URL(".", import.meta.url).pathname, "..");
const out = path.join(root, "out");
const zip = path.join(root, "export.zip");
const mint = path.join(root, "node_modules", ".bin", "mint");

fs.rmSync(out, { recursive: true, force: true });
fs.rmSync(zip, { force: true });
execFileSync(mint, ["export", "--output", zip, "--telemetry=false"], { cwd: root, stdio: "inherit" });
fs.mkdirSync(out, { recursive: true });
execFileSync("unzip", ["-q", zip, "-d", out], { stdio: "inherit" });
for (const junk of ["serve.js", "Start Docs.bat", "Start Docs.command"]) fs.rmSync(path.join(out, junk), { force: true });

// The client posts searches to `${AI_MESSAGE_HOST}/api/search/<subdomain>`,
// a Mintlify-hosted service. Rewrite that to a relative path so our function
// answers instead. The pattern is asserted so a client upgrade that changes
// it fails the build rather than silently shipping a dead search box.
const chunks = path.join(out, "_next", "static", "chunks");
let patched = 0;
for (const name of fs.readdirSync(chunks)) {
  if (!name.endsWith(".js")) continue;
  const file = path.join(chunks, name);
  const src = fs.readFileSync(file, "utf8");
  const next = src.replace(/`\$\{([A-Za-z_$][\w$]*)\.NEXT_PUBLIC\.AI_MESSAGE_HOST\}\/api\/search\/\$\{([A-Za-z_$][\w$]*)\}`/g, () => {
    patched += 1;
    return "`/api/search/${$2}`".replace("$2", "e");
  });
  if (next !== src) fs.writeFileSync(file, next);
}
if (patched === 0) {
  throw new Error("search host pattern not found in the exported client; the Mintlify client changed and the search rewrite needs updating");
}
console.log(`patched ${patched} search call site(s)`);

// Page content must be in the bundle. An unauthenticated export renders only
// the chrome; catch that here instead of on the live site.
const probe = fs.readFileSync(path.join(out, "install", "index.html"), "utf8");
if (!/npm install -g @viraatdas\/rudder/.test(probe)) {
  throw new Error("exported pages carry no content: run `mint login` and export again");
}

const index = buildSearchIndex(root);
fs.mkdirSync(path.join(root, "api", "search"), { recursive: true });
fs.writeFileSync(path.join(root, "api", "search", "index.json"), JSON.stringify(index));
console.log(`search index: ${index.length} sections`);
