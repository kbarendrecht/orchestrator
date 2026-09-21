// Markdown, as a tree. No HTML anywhere in it.
//
// **Nothing is imported here, and that is the point.** `tools/check-markdown.mjs`
// drives this in node, where there is no `window` for `core.js` to read — the
// same shape `pathlink.js` has, and for the same reason: the part with the cases
// is the part that has to be drivable without a daemon and a page. The tree it
// answers with is painted by `viewer.js`, which is where this app's other
// text-into-nodes work already lives.
//
// **Nodes, never markup.** The SPA has zero HTML sinks — ESLint refuses
// `innerHTML` outright — so the painter builds elements. That is a constraint and
// also a safety property: a note an agent wrote cannot carry markup into the
// page.
//
// **A subset, deliberately, and this is the list**: headings, fenced code,
// blockquotes, lists (nested, ordered and not), rules, pipe tables, paragraphs,
// and inline code, emphasis, links and images-as-links. What it does not do:
// reference links, footnotes, HTML blocks, loose-versus-tight list semantics,
// setext headings. Those are absent because the text this reads is an agent's
// notes and a repo's docs, and the cost of a full CommonMark parser is a
// dependency or a thousand lines.

/**
 * @typedef {{ kind: 'heading', level: number, text: string }
 *   | { kind: 'code', lang: string, text: string }
 *   | { kind: 'quote', lines: string[] }
 *   | { kind: 'list', ordered: boolean, items: { text: string, depth: number }[] }
 *   | { kind: 'rule' }
 *   | { kind: 'table', head: string[], rows: string[][] }
 *   | { kind: 'para', text: string }} Block
 */

/** The blocks in `text`, in order.
 *
 *  @param {string} text
 *  @returns {Block[]} */
export function parse(text) {
  /** @type {Block[]} */
  const out = [];
  const lines = text.split('\n');
  let i = 0;
  while (i < lines.length) {
    const line = lines[i] ?? '';

    // A fence runs to its closing fence or to the end of the file, and nothing
    // inside it is markdown — which is the whole point of one.
    const fence = /^(\s*)(`{3,}|~{3,})\s*([^\s`]*)/.exec(line);
    if (fence) {
      const [, pad = '', bars = '```', lang = ''] = fence;
      const body = [];
      i++;
      while (i < lines.length && !new RegExp(`^\\s*${bars[0]}{${bars.length},}\\s*$`).test(lines[i] ?? '')) {
        body.push((lines[i] ?? '').startsWith(pad) ? (lines[i] ?? '').slice(pad.length) : lines[i] ?? '');
        i++;
      }
      i++; // the closing fence, or the end
      out.push({ kind: 'code', lang, text: body.join('\n') });
      continue;
    }

    const heading = /^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line);
    if (heading) {
      out.push({ kind: 'heading', level: (heading[1] ?? '#').length, text: heading[2] ?? '' });
      i++;
      continue;
    }

    // Three or more of the same mark, and nothing else. `---` under text would be
    // a setext heading in CommonMark; it is a rule here, which is the reading
    // that never swallows the line above it.
    if (/^ {0,3}([-*_])(\s*\1){2,}\s*$/.test(line)) {
      out.push({ kind: 'rule' });
      i++;
      continue;
    }

    if (/^ {0,3}>/.test(line)) {
      const quoted = [];
      while (i < lines.length && /^ {0,3}>/.test(lines[i] ?? '')) {
        quoted.push((lines[i] ?? '').replace(/^ {0,3}>\s?/, ''));
        i++;
      }
      out.push({ kind: 'quote', lines: quoted });
      continue;
    }

    const bullet = listItem(line);
    if (bullet) {
      /* **A run of items ends where the marker changes.** Taking the first
         item's kind and holding it made `- one` followed by `1. two` one bulleted
         list with a numbered item in it, which is neither of the two things that
         were written. */
      const items = [];
      while (i < lines.length) {
        const item = listItem(lines[i] ?? '');
        if (!item || item.ordered !== bullet.ordered) break;
        items.push({ text: item.text, depth: item.depth });
        i++;
      }
      out.push({ kind: 'list', ordered: bullet.ordered, items });
      continue;
    }

    // A table needs its separator row, or two lines of prose with a pipe in them
    // become a one-column table.
    if (line.includes('|') && /^\s*\|?[\s:-]*-[\s:|-]*\|?\s*$/.test(lines[i + 1] ?? '')) {
      const head = cells(line);
      const rows = [];
      i += 2;
      while (i < lines.length && (lines[i] ?? '').includes('|')) {
        rows.push(cells(lines[i] ?? ''));
        i++;
      }
      out.push({ kind: 'table', head, rows });
      continue;
    }

    if (!line.trim()) {
      i++;
      continue;
    }

    // A paragraph runs to a blank line or to anything that starts a block of its
    // own, so a heading directly under prose is still a heading.
    const para = [];
    while (i < lines.length && (lines[i] ?? '').trim() && !startsBlock(lines[i] ?? '', lines[i + 1] ?? '')) {
      para.push((lines[i] ?? '').trim());
      i++;
    }
    if (para.length) out.push({ kind: 'para', text: para.join(' ') });
    else i++;
  }
  return out;
}

/** Whether a line begins a block that is not a paragraph. */
function startsBlock(/** @type {string} */ line, /** @type {string} */ next) {
  return /^(\s*)(`{3,}|~{3,})/.test(line)
    || /^ {0,3}#{1,6}\s/.test(line)
    || /^ {0,3}([-*_])(\s*\1){2,}\s*$/.test(line)
    || /^ {0,3}>/.test(line)
    || !!listItem(line)
    || (line.includes('|') && /^\s*\|?[\s:-]*-[\s:|-]*\|?\s*$/.test(next));
}

/** One list item, or `null`. Depth is two spaces to a level, which is what an
 *  agent writes and what every editor in this stack inserts. */
function listItem(/** @type {string} */ line) {
  const m = /^(\s*)(?:([-*+])|(\d{1,9})[.)])\s+(.*)$/.exec(line);
  if (!m) return null;
  return {
    ordered: !!m[3],
    depth: Math.floor((m[1] ?? '').replace(/\t/g, '  ').length / 2),
    text: m[4] ?? '',
  };
}

/** A table row's cells, without the edges. */
function cells(/** @type {string} */ line) {
  return line.trim().replace(/^\||\|$/g, '').split('|').map((c) => c.trim());
}
