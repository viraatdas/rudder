// Turn the MDX sources into searchable sections: one entry per heading, with
// the text under it. The search function scores against these.
import fs from "node:fs";
import path from "node:path";

const slugify = (t) => t.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
const strip = (s) =>
  s
    .replace(/```[\s\S]*?```/g, (m) => m.replace(/```\w*\n?/g, ""))
    .replace(/<[^>]+>/g, "")
    .replace(/\[([^\]]+)\]\([^)]+\)/g, "$1")
    .replace(/[*_`>|]/g, "")
    .replace(/\s+/g, " ")
    .trim();

export function buildSearchIndex(root) {
  const docs = JSON.parse(fs.readFileSync(path.join(root, "docs.json"), "utf8"));
  const entries = [];
  for (const group of docs.navigation.groups) {
    for (const page of group.pages) {
      const file = path.join(root, `${page}.mdx`);
      if (!fs.existsSync(file)) continue;
      const raw = fs.readFileSync(file, "utf8");
      const fm = raw.match(/^---\n([\s\S]*?)\n---\n/);
      const title = fm?.[1].match(/^title:\s*"?(.*?)"?\s*$/m)?.[1] ?? page;
      const body = raw.slice(fm ? fm[0].length : 0);
      const url = page === "index" ? "/" : `/${page}`;
      const sections = body.split(/^(?=##\s)/m);
      for (const section of sections) {
        const heading = section.match(/^##\s+(.*)$/m)?.[1]?.trim();
        const text = strip(heading ? section.replace(/^##.*$/m, "") : section);
        if (!text) continue;
        entries.push({
          page: url,
          title,
          breadcrumbs: [group.group, title],
          header: heading ?? title,
          hash: heading ? slugify(heading) : "",
          content: text.slice(0, 600),
          haystack: `${title} ${heading ?? ""} ${text}`.toLowerCase(),
        });
      }
    }
  }
  return entries;
}
