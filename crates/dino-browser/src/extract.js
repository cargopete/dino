// Reads the page into Markdown, links, JSON-LD, meta and numbered actions.
// Returns a plain object; Page::read parses it as a Reading.
(() => {
  const SKIP = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "SVG", "CANVAS", "IFRAME", "OBJECT", "VIDEO", "AUDIO"]);
  const CHROME = /(^|[\s_-])(nav|menu|footer|header|sidebar|cookie|consent|banner|share|social|related|breadcrumb|advert|promo|newsletter|modal|popup)([\s_-]|$)/i;
  const MAX_MARKDOWN = 400000;

  const hidden = (el) => {
    if (el.hidden || el.getAttribute("aria-hidden") === "true") return true;
    const style = getComputedStyle(el);
    return style.display === "none" || style.visibility === "hidden";
  };
  const clean = (s) => s.replace(/[ \t ]+/g, " ").replace(/\s*\n\s*/g, "\n").trim();
  const inline = (s) => s.replace(/\s+/g, " ").trim();

  // The main block: an <article> or <main> with real text, otherwise the
  // element whose own text, less its link text, is densest.
  const textOf = (el) => (el.innerText || "").length;
  const linkText = (el) => [...el.querySelectorAll("a")].reduce((n, a) => n + (a.innerText || "").length, 0);
  const BANNER = /cookie|consent|gdpr|onetrust|didomi|usercentrics/i;
  const chromey = (el) => banner(el) || CHROME.test(`${el.id} ${typeof el.className === "string" ? el.className : ""} ${el.getAttribute("role") || ""}`) || ["NAV", "FOOTER", "ASIDE", "HEADER"].includes(el.tagName);
  const banner = (el) => BANNER.test(`${el.id} ${typeof el.className === "string" ? el.className : ""}`) || el.getAttribute("role") === "dialog" || el.getAttribute("aria-modal") === "true";
  const insideBanner = (el) => { for (let at = el; at && at !== document.body; at = at.parentElement) if (banner(at)) return true; return false; };
  const pickMain = () => {
    for (const sel of ["article", "main", "[role=main]", "#content", ".content", "#main"]) {
      const found = [...document.querySelectorAll(sel)].filter((el) => !hidden(el) && !insideBanner(el)).sort((a, b) => textOf(b) - textOf(a))[0];
      if (found && textOf(found) > 400) return found;
    }
    let best = document.body, bestScore = 0;
    for (const el of document.body ? document.body.querySelectorAll("div, section, td") : []) {
      if (chromey(el) || insideBanner(el)) continue;
      const total = textOf(el);
      if (total < 200) continue;
      const score = (total - 2 * linkText(el)) * Math.min(1, el.querySelectorAll("p").length / 3 + 0.3);
      if (score > bestScore) { best = el; bestScore = score; }
    }
    return best;
  };

  const abs = (href) => { try { return new URL(href, location.href).href; } catch { return null; } };

  const table = (el) => {
    const rows = [...el.querySelectorAll("tr")].map((tr) => [...tr.children].map((c) => inline(c.innerText || "").replace(/\|/g, "\\|")));
    if (!rows.length) return "";
    const width = Math.max(...rows.map((r) => r.length));
    const pad = (r) => [...r, ...Array(width - r.length).fill("")];
    const [head, ...body] = rows.map(pad);
    return ["| " + head.join(" | ") + " |", "|" + " --- |".repeat(width), ...body.map((r) => "| " + r.join(" | ") + " |")].join("\n");
  };

  const md = (node, out) => {
    if (out.length > MAX_MARKDOWN) return;
    if (node.nodeType === Node.TEXT_NODE) { out.push(node.textContent); return; }
    if (node.nodeType !== Node.ELEMENT_NODE) return;
    const el = node, tag = el.tagName;
    if (SKIP.has(tag) || hidden(el)) return;
    if (el !== root && (banner(el) || (chromey(el) && textOf(el) < 2000))) return;
    const kids = () => { for (const child of el.childNodes) md(child, out); };
    const level = /^H([1-6])$/.exec(tag);
    if (level) { out.push(`\n\n${"#".repeat(+level[1])} ${inline(el.innerText || "")}\n\n`); return; }
    switch (tag) {
      case "P": case "DIV": case "SECTION": case "ARTICLE": case "MAIN": case "HEADER": case "FIGURE": case "BLOCKQUOTE": case "DL": case "FORM":
        out.push("\n\n"); kids(); out.push("\n\n"); return;
      case "BR": out.push("\n"); return;
      case "HR": out.push("\n\n---\n\n"); return;
      case "LI": out.push("\n- "); kids(); return;
      case "UL": case "OL": out.push("\n"); kids(); out.push("\n"); return;
      case "DT": out.push("\n**"); kids(); out.push("**"); return;
      case "DD": out.push("\n: "); kids(); return;
      case "PRE": out.push("\n\n```\n" + (el.innerText || "").trim() + "\n```\n\n"); return;
      case "CODE": out.push("`" + inline(el.innerText || "") + "`"); return;
      case "TABLE": out.push("\n\n" + table(el) + "\n\n"); return;
      case "IMG": { const alt = inline(el.alt || ""); if (alt) out.push(`[image: ${alt}]`); return; }
      case "A": {
        const text = inline(el.innerText || "");
        const href = abs(el.getAttribute("href") || "");
        if (text && href && /^https?:/.test(href)) out.push(`[${text}](${href})`); else kids();
        return;
      }
      case "STRONG": case "B": out.push("**"); kids(); out.push("**"); return;
      case "EM": case "I": out.push("_"); kids(); out.push("_"); return;
      default: kids();
    }
  };

  const render = (el) => {
    root = el;
    const pieces = [];
    md(el, pieces);
    return pieces.join("").replace(/[ \t]+\n/g, "\n").replace(/\n{3,}/g, "\n\n").replace(/\*\*\s*\*\*/g, "").trim();
  };
  let root = null;
  let markdown = document.body ? render(pickMain()) : "";
  // The chosen block can be a stub on an odd layout; the whole body is better than nothing.
  if (markdown.length < 200 && document.body) markdown = render(document.body);
  if (markdown.length < 200 && document.body) markdown = clean(document.body.innerText || "");
  if (markdown.length > MAX_MARKDOWN) markdown = markdown.slice(0, MAX_MARKDOWN) + "\n\n[cut short]";

  const seen = new Set();
  const links = [];
  for (const a of document.querySelectorAll("a[href]")) {
    const href = a.href;
    if (!/^https?:/.test(href) || seen.has(href) || links.length >= 3000) continue;
    seen.add(href);
    links.push({ href, text: inline(a.innerText || a.getAttribute("aria-label") || a.title || "").slice(0, 200) });
  }

  const jsonld = [];
  for (const s of document.querySelectorAll('script[type="application/ld+json"]')) {
    try { jsonld.push(JSON.parse(s.textContent)); } catch { /* a broken block is skipped, not fatal */ }
  }

  const meta = {};
  for (const m of document.querySelectorAll("meta[name], meta[property]")) {
    const key = m.getAttribute("name") || m.getAttribute("property");
    if (/^(description|keywords|author|og:|twitter:|article:|product:)/i.test(key) && m.content) meta[key] = m.content;
  }

  const actions = [];
  let n = 0;
  const interactive = "a[href], button, input:not([type=hidden]), select, textarea, summary, [role=button], [role=link], [role=tab], [role=menuitem], [role=checkbox], [contenteditable=true], [onclick]";
  for (const el of document.querySelectorAll(interactive)) {
    if (actions.length >= 400) break;
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0 || hidden(el)) continue;
    n += 1;
    el.setAttribute("data-dino-ref", String(n));
    const role = el.getAttribute("role") || (el.tagName === "INPUT" ? `input:${el.type}` : el.tagName.toLowerCase());
    const label = el.labels && el.labels[0] ? el.labels[0].innerText : "";
    const name = inline(el.getAttribute("aria-label") || label || el.placeholder || el.innerText || el.value || el.title || el.alt || el.name || "").slice(0, 120);
    actions.push({ n, role, name });
  }

  const canonical = document.querySelector('link[rel="canonical"]');
  const nav = performance.getEntriesByType("navigation")[0];
  return {
    url: location.href,
    canonical: canonical ? abs(canonical.getAttribute("href")) : null,
    status: (nav && nav.responseStatus) || null,
    title: inline(document.title || ""),
    lang: document.documentElement.lang || null,
    markdown,
    links,
    jsonld,
    meta,
    actions,
  };
})()
