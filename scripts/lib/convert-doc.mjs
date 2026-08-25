import mammoth from "mammoth";
import TurndownService from "turndown";
import { gfm } from "turndown-plugin-gfm";
import { writeFileSync, mkdirSync } from "node:fs";
import { basename, dirname, join } from "node:path";

const [, , inPath, outPath, title, provenance] = process.argv;

// Figures are extracted to docs/images/<slug>-figN.<ext> rather than inlined as base64
// data URIs, which would bloat the markdown ~24x and be unreadable to any tool.
const slug = basename(outPath, ".md");
const imgDir = join(dirname(outPath), "images");
mkdirSync(imgDir, { recursive: true });
let figCount = 0;

const EXT = {
  "image/png": "png",
  "image/jpeg": "jpg",
  "image/gif": "gif",
  "image/x-emf": "emf",
  "image/x-wmf": "wmf",
  "image/svg+xml": "svg",
};

const imageHandler = mammoth.images.imgElement(async (image) => {
  const buf = await image.read();
  const ext = EXT[image.contentType] || "bin";
  const name = `${slug}-fig${++figCount}.${ext}`;
  writeFileSync(join(imgDir, name), buf);
  return { src: `images/${name}`, alt: image.altText || `Figure ${figCount}` };
});

const td = new TurndownService({
  headingStyle: "atx",
  codeBlockStyle: "fenced",
  bulletListMarker: "-",
  emDelimiter: "_",
});
td.use(gfm);

td.addRule("dropEmptyParagraph", {
  filter: (node) =>
    node.nodeName === "P" && node.textContent.trim() === "" && !node.querySelector("img"),
  replacement: () => "",
});

const { value: html, messages } = await mammoth.convertToHtml(
  { path: inPath },
  {
    styleMap: [
      "p[style-name='Title'] => h1:fresh",
      "p[style-name='Subtitle'] => p.subtitle:fresh",
      "p[style-name='Heading 1'] => h2:fresh",
      "p[style-name='Heading 2'] => h3:fresh",
      "p[style-name='Heading 3'] => h4:fresh",
      "p[style-name='Heading 4'] => h5:fresh",
    ],
    ignoreEmptyParagraphs: true,
    convertImage: imageHandler,
  }
);

// A literal pipe in cell prose (e.g. "op upsert|delete") would split the GFM column.
// An HTML entity is no good: turndown decodes it back to a bare pipe. Use a sentinel
// string that appears in no real document, then restore it as an escaped pipe after
// conversion, once turndown can no longer touch it.
const PIPE_SENTINEL = "QQCREDSYNCPIPEQQ";

// Word wraps every table cell's content in <p>. Turndown renders <p> as a block, which
// injects newlines inside cells and shatters the row. Flatten cells to inline first:
// multiple paragraphs become <br>, a single one loses its wrapper entirely.
const flattenCells = (h) =>
  h.replace(/<(t[dh])([^>]*)>([\s\S]*?)<\/\1>/g, (_m, tag, attrs, inner) => {
    const flat = inner
      .replace(/<\/p>\s*<p[^>]*>/g, "<br>")
      .replace(/<\/?p[^>]*>/g, "")
      .replace(/\r?\n/g, " ")
      // Escape pipes in text runs only, never inside a tag.
      .replace(/(^|>)([^<]*)/g, (_s, lead, text) =>
        lead + text.replace(/\|/g, PIPE_SENTINEL)
      )
      .trim();
    return `<${tag}${attrs}>${flat}</${tag}>`;
  });

let md = td.turndown(flattenCells(html));

md = md.split(PIPE_SENTINEL).join("\\|");

// Word styles every heading bold, so turndown emits "## **3\. Language decision**".
// Unwrap the bold and unescape the numbering so headings are plain and anchor-able.
md = md
  .replace(/^(#{1,6})\s+\*\*(.+?)\*\*\s*$/gm, "$1 $2")
  .replace(/^(#{1,6}\s+\d+)\\\./gm, "$1.");

// Collapse runs of >2 blank lines, and normalise the non-breaking spaces Word emits.
md = md
  .replace(/ /g, " ")
  .replace(/\n{3,}/g, "\n\n")
  .replace(/[ \t]+$/gm, "")
  .trim();

const header = `<!--
  ${title}
  Converted from ${basename(inPath)} - do not edit by hand.
  Source of truth: ${provenance}
  Regenerate: scripts/convert-docs.sh
-->

`;

writeFileSync(outPath, header + md + "\n", "utf8");

// Report structure so a silent conversion regression is visible in CI output.
const tableHeaders = (md.match(/^\| --- /gm) || []).length;
const headings = (md.match(/^#{1,5} /gm) || []).length;
const warn = messages.filter((m) => m.type === "warning").length;

// Ragged rows: consecutive table lines whose column count differs.
let ragged = 0;
let prev = 0;
for (const line of md.split("\n")) {
  if (line.startsWith("|")) {
    const n = (line.match(/(?<!\\)\|/g) || []).length;
    if (prev && n !== prev) ragged++;
    prev = n;
  } else prev = 0;
}

console.log(
  `${basename(outPath).padEnd(30)} ${String(md.length).padStart(7)} chars  ` +
    `${String(headings).padStart(3)} headings  ${String(tableHeaders).padStart(3)} tables  ` +
    `${String(figCount).padStart(2)} figures  ${ragged} ragged  ${warn} warnings`
);

if (ragged > 0) {
  console.error(`  FAIL: ${ragged} ragged table row(s) in ${basename(outPath)}`);
  process.exit(1);
}
