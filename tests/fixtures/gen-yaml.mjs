// Generates yaml-golden.json: what js-yaml 5.4.3 produces for a corpus of
// frontmatter values, so the Rust format layer can be tested against it.
//
//   JS_YAML=/path/to/node_modules/js-yaml/dist/js-yaml.mjs node tests/fixtures/gen-yaml.mjs
//
// Values are encoded as JSON with {"$date": ms} for dates and {"$num": "NaN"}
// for numbers JSON can't hold.

import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const { dump, load, YAML11_SCHEMA } = await import(process.env.JS_YAML);

// Deterministic PRNG (mulberry32) so the corpus is stable
let seed = 0x7d0c0de;
function rand() {
  seed |= 0;
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const pick = (xs) => xs[Math.floor(rand() * xs.length)];

const edgeStrings = [
  "", " ", "  x", "x ", "\t", "a\tb", "2026-10-19", "2026-11-07T09:00:10.859Z", "2026-W44", "26-955",
  "yes", "Yes", "no", "on", "off", "y", "n", "Y", "true", "null", "~", "Null", "123", "0123", "0o17",
  "0x1F", "1.5", "1e3", "12:30", "1:20.5", ".inf", ".nan", "1_000", "+12", ".5", ".", "-", "--", "---",
  "--- x", "...", "TUI-DO Core: background", "Note:", "a #b", "a#b", "#tag", "- item", "-item", "? x",
  "?x", ":x", "*bold*", "&anchor", "!tag", "%x", "@user", "`code`", "{a}", "[a]", "a, b", "| x",
  "> x", "=", "<<", "it's", 'say "hi"', "it's \"x\"", "a\r\nb", "a\u00a0b", "\ufeffx", "a\u0007b",
  "a\u007fb", "æøå task", "emoji 🚀 launch", "™️ mark", "zero\u200bwidth", "narrow\u202fspace",
  "line1\nline2", "line1\n\nline2", "trailing\n", "trailing\n\n", "\n", "\nleading", " indented\nnext",
  "/Users/me/Library/Mobile Documents/com~apple~CloudDocs/Planning", "~/", "C:\\path",
  "https://example.com/a?b=c#d", "a: b: c", "key=value", "50%", "#", "x #", "x:", "x: ", "a\u2028b",
];

const words = [
  "add", "fix", "the", "TUI-DO", "core:", "background", "#1", "daemon", "with", "remote", "clients", "a",
  "it's", '"quoted"', "æøå", "🚀", "-", "--", "x:y", "key:", "#tag", "50%", "(paren)", "[bracket]", "{brace}",
  "&amp", "*star", "!bang", "@at", "`tick`", "|", ">", "?", "very-long-hyphenated-identifier-without-spaces",
  "2026-10-19", "yes", "null", "1.5", "\t", " ", "  ",
];

function fuzzString() {
  const n = 1 + Math.floor(rand() * (rand() < 0.3 ? 40 : 10));
  let s = "";
  for (let i = 0; i < n; i++) {
    s += pick(words);
    s += pick([" ", " ", " ", "", "\n", "  ", ", ", ": ", " #"]);
  }
  return rand() < 0.5 ? s.trim() : s;
}

const cases = [];
const add = (name, data) => cases.push({ name, data, yaml: dump(data, { schema: YAML11_SCHEMA }) });

edgeStrings.forEach((s, i) => add(`edge-${i}`, { value: s }));
edgeStrings.forEach((s, i) => add(`edge-key-${i}`, { [s || "empty"]: 1 }));
for (let len = 70; len <= 90; len++) {
  add(`fold-${len}`, { title: Array.from({ length: len }, (_, i) => (i % 6 === 5 ? " " : "w")).join("") });
}
add("fold-summary", {
  summary: "Short summary.\nSecond line that is quite long and goes on and on and on and on and on and on forever.",
});
add("fold-long-word", { title: "x".repeat(120) });
add("fold-indented", { summary: "first line\n  indented line that is long enough to need folding if it were not more indented ok" });
for (let i = 0; i < 400; i++) add(`fuzz-${i}`, { title: fuzzString(), summary: fuzzString() });
add("task", {
  id: "26-955",
  title: "TUI-DO Core: background daemon with remote clients",
  state: "in-progress",
  project: "tui-do",
  due: null,
  recurrence: null,
  plan: "26-955-molten-banner",
  created: "2026-11-07T09:00:10.859Z",
  position: -3,
  completed: null,
  attachments: ["/Users/me/a.png", "b c.png"],
});
add("project", { title: "Cube", muted: false, directory: "~/", ssh: { host: "cube", user: "nwa", port: 2222, identityFile: "~/.ssh/id_ed25519" } });
add("empty-collections", { attachments: [], ssh: {} });
add("numbers", { a: 0, b: -525, c: 1.5, d: 1e21, e: -0, f: 0.1, g: 123456789, h: 2 ** 53, i: 1e-7 });
add("dates", { a: new Date(Date.UTC(2026, 2, 1)), b: new Date("2025-11-03T19:55:18.600Z") });
add("bools", { a: true, b: false });
add("nested-seq", { a: [[1, 2], [], ["x"]], b: [{ k: "v", k2: "v2" }] });

// Load cases: YAML text → value
const loadInputs = [
  "due: 2026-03-01", "due: '2026-03-01'", "t: 2026-02-20T23:33:57.161Z", "t: 2026-02-20 23:33:57",
  "t: 2026-1-5t1:02:03+01", "t: 2025-11-03T19:55:18.600725Z", "d: 2026-02-30", "a: yes\nb: No\nc: on\nd: y",
  "n: ~\nm:\no: null", "i: 0123\nj: 0x1F\nk: 1_000\nl: 1:30\nm: 0o17\nn: 08\no: 09:30", "f: 1.5\ng: .5\nh: 1e3\ni: 1.e+21\nj: .inf\nk: -.inf",
  "title: >-\n  Add particles that are long\n  and folded", "summary: |\n  line one\n  line two\n", "s: \"a\\tb\\x41\\u00e9\"",
  "a: [1, two, 'three']\nb: {x: 1}", "list:\n  - a\n  - b", "base: &b {a: 1}\nchild:\n  <<: *b\n  c: 2", "yes: 1\n1: 2",
];
const loads = loadInputs.map((yaml) => ({ yaml, data: load(yaml, { schema: YAML11_SCHEMA }) }));

function encode(v) {
  if (v instanceof Date) return { $date: v.getTime() };
  if (typeof v === "number" && !Number.isFinite(v)) return { $num: String(v) };
  if (typeof v === "number" && Object.is(v, -0)) return { $num: "-0" };
  if (Array.isArray(v)) return v.map(encode);
  if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, encode(x)]));
  return v;
}

const out = {
  generator: "js-yaml 5.4.3 dump/load with YAML11_SCHEMA",
  dumps: cases.map((c) => ({ ...c, data: encode(c.data) })),
  loads: loads.map((c) => ({ ...c, data: encode(c.data) })),
};
const here = dirname(fileURLToPath(import.meta.url));
writeFileSync(join(here, "yaml-golden.json"), JSON.stringify(out, null, 1) + "\n");
console.log(`${out.dumps.length} dump cases, ${out.loads.length} load cases`);
