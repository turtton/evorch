// A small, safe Markdown subset.
//
// parseMarkdown() turns text into a tree of plain objects; renderMarkdown()
// builds DOM nodes from that tree with createElement and textContent only.
// Nothing is ever parsed as HTML: raw HTML in the source stays literal text.
// Links are kept only for absolute http(s) URLs, open in a new tab with
// rel="noopener noreferrer", and images are never loaded (they become links).
//
// Supported: paragraphs (single newlines are line breaks), ATX headings,
// fenced code blocks, block quotes, bullet and ordered lists, thematic breaks,
// code spans, **strong**, *emphasis*, ~~strikethrough~~, [links](url),
// <autolinks> and bare http(s) URLs, backslash escapes.

const MAX_DEPTH = 8;
/** Larger inputs are shown as plain text: the parser is not built for them. */
export const MAX_MARKDOWN_BYTES = 256 * 1024;

const FENCE = /^ {0,3}(`{3,}|~{3,})[ \t]*([^`]*?)[ \t]*$/;
const HR = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const HEADING = /^ {0,3}(#{1,6})(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$/;
const QUOTE = /^ {0,3}> ?(.*)$/;
const ITEM = /^( {0,3})([-*+]|(\d{1,9})[.)])(?:[ \t]+(.*))?$/;

/** The URL if it is an absolute http or https URL, otherwise null. */
export function safeHref(raw) {
  if (typeof raw !== "string") return null;
  const text = raw.trim();
  if (!/^https?:\/\//i.test(text)) return null;
  let url;
  try {
    url = new URL(text);
  } catch {
    return null;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return null;
  return url.href;
}

export function parseMarkdown(text) {
  const lines = String(text).replace(/\r\n?/g, "\n").split("\n");
  return parseBlocks(lines, 0);
}

function startsBlock(line) {
  return FENCE.test(line) || HR.test(line) || HEADING.test(line) || QUOTE.test(line) || ITEM.test(line);
}

function parseBlocks(lines, depth) {
  const blocks = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === "") {
      i += 1;
      continue;
    }
    let m;
    if ((m = FENCE.exec(line))) {
      const fence = m[1];
      const body = [];
      i += 1;
      while (i < lines.length) {
        const close = /^ {0,3}(`{3,}|~{3,})[ \t]*$/.exec(lines[i]);
        if (close && close[1][0] === fence[0] && close[1].length >= fence.length) {
          i += 1;
          break;
        }
        body.push(lines[i]);
        i += 1;
      }
      blocks.push({ t: "code", lang: m[2] || "", v: body.join("\n") });
      continue;
    }
    if (HR.test(line)) {
      blocks.push({ t: "hr" });
      i += 1;
      continue;
    }
    if ((m = HEADING.exec(line))) {
      blocks.push({ t: "h", level: m[1].length, c: parseInline(m[2] || "", 0) });
      i += 1;
      continue;
    }
    if (QUOTE.test(line)) {
      const inner = [];
      while (i < lines.length && (m = QUOTE.exec(lines[i]))) {
        inner.push(m[1]);
        i += 1;
      }
      blocks.push(
        depth < MAX_DEPTH
          ? { t: "quote", c: parseBlocks(inner, depth + 1) }
          : { t: "p", c: [{ t: "text", v: inner.join("\n") }] },
      );
      continue;
    }
    if ((m = ITEM.exec(line))) {
      const ordered = m[3] !== undefined;
      const list = { t: ordered ? "ol" : "ul", items: [] };
      if (ordered && m[3] !== "1") list.start = Number(m[3]);
      while (i < lines.length) {
        const item = ITEM.exec(lines[i]);
        if (!item || (item[3] !== undefined) !== ordered) break;
        const indent = item[1].length + item[2].length + 1;
        const content = [item[4] || ""];
        i += 1;
        while (i < lines.length) {
          const next = lines[i];
          if (next.trim() === "") {
            // A blank line continues the item only if indented text follows.
            const after = lines[i + 1];
            if (after !== undefined && after.trim() !== "" && leadingSpaces(after) >= indent) {
              content.push("");
              i += 1;
              continue;
            }
            break;
          }
          if (leadingSpaces(next) >= indent) {
            content.push(next.slice(indent));
          } else if (ITEM.test(next) || startsBlock(next)) {
            break;
          } else {
            content.push(next);
          }
          i += 1;
        }
        list.items.push(
          depth < MAX_DEPTH
            ? parseBlocks(content, depth + 1)
            : [{ t: "p", c: [{ t: "text", v: content.join("\n") }] }],
        );
        // Blank lines between items of the same list.
        let j = i;
        while (j < lines.length && lines[j].trim() === "") j += 1;
        const again = j < lines.length ? ITEM.exec(lines[j]) : null;
        if (j > i && again && (again[3] !== undefined) === ordered) i = j;
      }
      blocks.push(list);
      continue;
    }
    const para = [line];
    i += 1;
    while (i < lines.length && lines[i].trim() !== "" && !startsBlock(lines[i])) {
      para.push(lines[i]);
      i += 1;
    }
    blocks.push({ t: "p", c: parseInline(para.join("\n"), 0) });
  }
  return blocks;
}

function leadingSpaces(line) {
  let n = 0;
  for (const ch of line) {
    if (ch === " ") n += 1;
    else if (ch === "\t") n += 4;
    else break;
  }
  return n;
}

const PUNCT = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";
/** A bare URL: only characters a URL may contain, so it ends at Japanese text. */
const BARE_URL = /^https?:\/\/[A-Za-z0-9\-._~:/?#[\]@!$&'()*+,;=%]+/i;

function isWordChar(ch) {
  return ch !== undefined && /[\p{L}\p{N}]/u.test(ch);
}

/**
 * Inline content. `failed` remembers, per closing delimiter, a position from
 * which no closer exists, so unmatched delimiters stay linear.
 */
export function parseInline(text, depth, failed = new Map()) {
  const out = [];
  let buf = "";
  const flush = () => {
    if (buf) {
      const last = out[out.length - 1];
      if (last && last.t === "text") last.v += buf;
      else out.push({ t: "text", v: buf });
      buf = "";
    }
  };
  const findCloser = (delim, from) => {
    const key = delim;
    const known = failed.get(key);
    if (known !== undefined && from >= known) return -1;
    let at = text.indexOf(delim, from);
    while (at !== -1 && /\s/.test(text[at - 1] || " ")) at = text.indexOf(delim, at + 1);
    if (at === -1) failed.set(key, Math.min(known ?? Infinity, from));
    return at;
  };
  let i = 0;
  while (i < text.length) {
    const ch = text[i];
    if (ch === "\\" && i + 1 < text.length && PUNCT.includes(text[i + 1])) {
      buf += text[i + 1];
      i += 2;
      continue;
    }
    if (ch === "\n") {
      flush();
      out.push({ t: "br" });
      i += 1;
      continue;
    }
    if (ch === "`") {
      let run = 1;
      while (text[i + run] === "`") run += 1;
      const fence = "`".repeat(run);
      let close = text.indexOf(fence, i + run);
      while (close !== -1 && text[close + run] === "`") {
        let skip = close;
        while (text[skip] === "`") skip += 1;
        close = text.indexOf(fence, skip);
      }
      if (close !== -1) {
        flush();
        let code = text.slice(i + run, close).replace(/\n/g, " ");
        if (/^ .*[^ ].* $/.test(code)) code = code.slice(1, -1);
        out.push({ t: "code", v: code });
        i = close + run;
        continue;
      }
      buf += fence;
      i += run;
      continue;
    }
    if (ch === "<") {
      const m = /^<(https?:\/\/[^\s<>]+)>/i.exec(text.slice(i, i + 2048));
      const href = m && safeHref(m[1]);
      if (href) {
        flush();
        out.push({ t: "link", href, c: [{ t: "text", v: m[1] }] });
        i += m[0].length;
        continue;
      }
    }
    if ((ch === "h" || ch === "H") && !/[A-Za-z0-9]/.test(text[i - 1] ?? "")) {
      const m = BARE_URL.exec(text.slice(i, i + 2048));
      if (m) {
        let url = m[0];
        url = url.replace(/[.,;:!?'"*_~]+$/, "");
        while (url.endsWith(")") && count(url, "(") < count(url, ")")) url = url.slice(0, -1);
        const href = safeHref(url);
        if (href) {
          flush();
          out.push({ t: "link", href, c: [{ t: "text", v: url }] });
          i += url.length;
          continue;
        }
      }
    }
    if (ch === "[" || (ch === "!" && text[i + 1] === "[")) {
      const image = ch === "!";
      const open = image ? i + 1 : i;
      const link = parseLinkAt(text, open);
      if (link) {
        flush();
        const href = safeHref(link.url);
        if (image) {
          const label = [{ t: "text", v: `画像: ${link.label || link.url}` }];
          out.push(href ? { t: "link", href, image: true, c: label } : { t: "text", v: label[0].v });
        } else if (href && depth < MAX_DEPTH) {
          out.push({ t: "link", href, c: stripLinks(parseInline(link.label, depth + 1)) });
        } else {
          out.push(...parseInline(link.label, depth + 1, new Map()));
        }
        i = link.end;
        continue;
      }
    }
    if ((ch === "*" || ch === "_") && depth < MAX_DEPTH) {
      const double = text[i + 1] === ch;
      const delim = double ? ch + ch : ch;
      const start = i + delim.length;
      const intraword = ch === "_" && isWordChar(text[i - 1]);
      if (!intraword && start < text.length && !/\s/.test(text[start])) {
        const close = findCloser(delim, start + 1);
        const closeOk =
          close !== -1 && !(ch === "_" && isWordChar(text[close + delim.length]));
        if (closeOk) {
          flush();
          const inner = parseInline(text.slice(start, close), depth + 1);
          out.push({ t: double ? "strong" : "em", c: inner });
          i = close + delim.length;
          continue;
        }
      }
    }
    if (ch === "~" && text[i + 1] === "~" && depth < MAX_DEPTH) {
      const start = i + 2;
      if (start < text.length && !/\s/.test(text[start])) {
        const close = findCloser("~~", start + 1);
        if (close !== -1) {
          flush();
          out.push({ t: "del", c: parseInline(text.slice(start, close), depth + 1) });
          i = close + 2;
          continue;
        }
      }
    }
    buf += ch;
    i += 1;
  }
  flush();
  return out;
}

function count(text, ch) {
  let n = 0;
  for (const c of text) if (c === ch) n += 1;
  return n;
}

/** Links inside a link label are flattened to their text. */
function stripLinks(nodes) {
  return nodes.flatMap((node) => (node.t === "link" ? node.c : [node]));
}

/** `[label](url "title")` starting at `open`, or null. */
function parseLinkAt(text, open) {
  let depth = 0;
  let close = -1;
  for (let j = open; j < text.length && j < open + 4096; j += 1) {
    const ch = text[j];
    if (ch === "\\") {
      j += 1;
      continue;
    }
    if (ch === "[") depth += 1;
    else if (ch === "]") {
      depth -= 1;
      if (depth === 0) {
        close = j;
        break;
      }
    } else if (ch === "\n" && text[j + 1] === "\n") return null;
  }
  if (close === -1 || text[close + 1] !== "(") return null;
  const m = /^\(\s*(<[^<>\n]*>|[^\s()]*(?:\([^\s()]*\)[^\s()]*)*)(?:\s+("[^"]*"|'[^']*'))?\s*\)/.exec(
    text.slice(close + 1, close + 1 + 4096),
  );
  if (!m) return null;
  let url = m[1];
  if (url.startsWith("<")) url = url.slice(1, -1);
  return { label: text.slice(open + 1, close), url, end: close + 1 + m[0].length };
}

// ---- DOM ----------------------------------------------------------------

/**
 * Build DOM nodes for `text` into a DocumentFragment. Only elements from a
 * fixed list are created and only `href`, `rel`, `target`, `referrerpolicy`,
 * `start` and `class` attributes are set.
 */
export function renderMarkdown(text, doc = globalThis.document) {
  const fragment = doc.createDocumentFragment();
  if (new TextEncoder().encode(text).length > MAX_MARKDOWN_BYTES) {
    const pre = doc.createElement("pre");
    pre.className = "md-plain";
    pre.textContent = text;
    fragment.append(pre);
    return fragment;
  }
  for (const block of parseMarkdown(text)) fragment.append(renderBlock(block, doc));
  return fragment;
}

function renderBlock(block, doc) {
  switch (block.t) {
    case "p":
      return withInline(doc.createElement("p"), block.c, doc);
    case "h": {
      const level = Math.min(6, Math.max(1, block.level));
      // Message headings sit inside the page's own h1/h2 structure.
      const el = doc.createElement(`h${Math.min(6, level + 2)}`);
      el.className = `md-h md-h${level}`;
      return withInline(el, block.c, doc);
    }
    case "code": {
      const pre = doc.createElement("pre");
      const code = doc.createElement("code");
      code.textContent = block.v;
      pre.append(code);
      return pre;
    }
    case "quote": {
      const el = doc.createElement("blockquote");
      for (const child of block.c) el.append(renderBlock(child, doc));
      return el;
    }
    case "ul":
    case "ol": {
      const el = doc.createElement(block.t);
      if (block.t === "ol" && block.start !== undefined) el.start = block.start;
      for (const item of block.items) {
        const li = doc.createElement("li");
        for (const child of item) li.append(renderBlock(child, doc));
        el.append(li);
      }
      return el;
    }
    case "hr":
      return doc.createElement("hr");
    default:
      return doc.createTextNode("");
  }
}

function withInline(el, nodes, doc) {
  for (const node of nodes) el.append(renderInline(node, doc));
  return el;
}

function renderInline(node, doc) {
  switch (node.t) {
    case "text":
      return doc.createTextNode(node.v);
    case "br":
      return doc.createElement("br");
    case "code": {
      const el = doc.createElement("code");
      el.textContent = node.v;
      return el;
    }
    case "strong":
    case "em":
    case "del":
      return withInline(doc.createElement(node.t), node.c, doc);
    case "link": {
      const href = safeHref(node.href);
      if (!href) return withInline(doc.createElement("span"), node.c, doc);
      const a = doc.createElement("a");
      a.href = href;
      a.rel = "noopener noreferrer";
      a.target = "_blank";
      a.referrerPolicy = "no-referrer";
      if (node.image) a.className = "md-image-link";
      return withInline(a, node.c, doc);
    }
    default:
      return doc.createTextNode("");
  }
}
