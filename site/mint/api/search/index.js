// Search backend for the self-hosted Mintlify export. The exported client
// posts {query} here (after the build rewrote its Mintlify host) and expects
// {results:[{page, header, content, metadata:{title, breadcrumbs, hash}}]}.
import fs from "node:fs";
import path from "node:path";

let INDEX = null;
function index() {
  if (!INDEX) INDEX = JSON.parse(fs.readFileSync(path.join(process.cwd(), "api", "search", "index.json"), "utf8"));
  return INDEX;
}

function score(entry, terms) {
  let s = 0;
  for (const term of terms) {
    if (!entry.haystack.includes(term)) return 0;
    const inTitle = entry.title.toLowerCase().includes(term);
    const inHeader = entry.header.toLowerCase().includes(term);
    s += inHeader ? 6 : inTitle ? 4 : 1;
    s += Math.min(5, entry.haystack.split(term).length - 1) * 0.5;
  }
  return s;
}

function snippet(entry, terms) {
  const text = entry.content;
  const at = terms.map((t) => text.toLowerCase().indexOf(t)).filter((i) => i >= 0).sort((a, b) => a - b)[0] ?? 0;
  const start = Math.max(0, at - 60);
  return (start > 0 ? "…" : "") + text.slice(start, start + 220) + (start + 220 < text.length ? "…" : "");
}

export default async function handler(req, res) {
  res.setHeader("Access-Control-Allow-Origin", "*");
  res.setHeader("Access-Control-Allow-Headers", "Content-Type");
  if (req.method === "OPTIONS") return res.status(204).end();
  let query = "";
  if (req.method === "POST") {
    const body = typeof req.body === "string" ? JSON.parse(req.body || "{}") : req.body ?? {};
    query = String(body.query ?? "");
  } else {
    query = String(req.query?.q ?? req.query?.query ?? "");
  }
  const terms = query.toLowerCase().split(/\s+/).filter((t) => t.length > 1);
  if (terms.length === 0) return res.status(200).json({ results: [] });
  const results = index()
    .map((entry) => ({ entry, s: score(entry, terms) }))
    .filter(({ s }) => s > 0)
    .sort((a, b) => b.s - a.s)
    .slice(0, 12)
    .map(({ entry }) => ({
      page: entry.page,
      header: entry.header,
      content: snippet(entry, terms),
      metadata: { title: entry.title, breadcrumbs: entry.breadcrumbs, hash: entry.hash, icon: null, openapi: null },
    }));
  return res.status(200).json({ results });
}
