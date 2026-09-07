#!/usr/bin/env python3
"""Render the deep doc pages (docs/*.md) into the terminal-themed HTML that
serves on the atrium Pages site. Standard library only, no third-party deps and
no Jekyll: the markdown here uses a small, fixed subset (headings, fenced code,
GFM pipe tables, flat lists, inline bold/italic/code/links), so a focused
converter is enough and stays honest about what it supports.

Run from the docs/ directory (or anywhere):  python build.py
It reads each PAGES entry's <slug>.md and writes <slug>.html beside it, sharing
assets/style.css with index.html. Markdown stays the source of truth.
"""

import html
import os
import re
import sys

# Ordered so the in-page doc nav reads as a natural tour.
PAGES = [
    ("architecture", "architecture"),
    ("agent-status", "agent status"),
    ("identity", "identity"),
    ("fleets", "fleets"),
    ("control-plane", "control plane"),
    ("trust-and-security", "trust & security"),
    ("reaping", "reaping"),
]
SLUGS = {slug for slug, _ in PAGES}

README_URL = "https://github.com/nativelite/atrium#readme"


def esc(text):
    return html.escape(text, quote=False)


def rewrite_link(url):
    """Local .md links become sibling .html; ../README.md points at GitHub;
    everything else (external, anchors) is left alone."""
    target, sep, frag = url.partition("#")
    if target in ("../README.md", "README.md"):
        return README_URL
    if target.endswith(".md"):
        base = os.path.basename(target)[:-3]
        out = base + ".html"
        return out + (sep + frag if sep else "")
    return url


CODE_RE = re.compile(r"`([^`]+)`")
LINK_RE = re.compile(r"\[([^\]]+)\]\(([^)\s]+)\)")
BOLD_RE = re.compile(r"\*\*([^*]+)\*\*")
ITALIC_RE = re.compile(r"(?<!\*)\*([^*]+)\*(?!\*)")


def inline(text):
    """Convert inline markdown on already-structural text. Order matters:
    escape HTML, stash code spans so their contents are never re-processed,
    then links, bold, italic, and finally restore the code spans."""
    text = esc(text)
    stash = []

    def stash_code(m):
        stash.append("<code>" + m.group(1) + "</code>")
        return "\x00%d\x00" % (len(stash) - 1)

    text = CODE_RE.sub(stash_code, text)
    text = LINK_RE.sub(
        lambda m: '<a href="%s">%s</a>' % (rewrite_link(m.group(2)), m.group(1)),
        text,
    )
    text = BOLD_RE.sub(r"<b>\1</b>", text)
    text = ITALIC_RE.sub(r"<i>\1</i>", text)
    text = re.sub(r"\x00(\d+)\x00", lambda m: stash[int(m.group(1))], text)
    return text


PIPE_SPLIT = re.compile(r"(?<!\\)\|")


def table_cells(row):
    cells = PIPE_SPLIT.split(row)
    # Rows are written with leading/trailing pipes -> drop the empty ends.
    if cells and cells[0].strip() == "":
        cells = cells[1:]
    if cells and cells[-1].strip() == "":
        cells = cells[:-1]
    return [c.strip().replace("\\|", "|") for c in cells]


def is_sep_row(row):
    return bool(re.match(r"^\s*\|?[\s:|-]*-[-\s:|-]*\|?\s*$", row)) and "-" in row


LIST_RE = re.compile(r"^(\s*)([-*]|\d+\.)\s+(.*)$")


def convert(md):
    lines = md.split("\n")
    out = []
    i = 0
    n = len(lines)
    while i < n:
        line = lines[i]

        # fenced code
        if line.startswith("```"):
            i += 1
            buf = []
            while i < n and not lines[i].startswith("```"):
                buf.append(lines[i])
                i += 1
            i += 1  # skip closing fence
            out.append("<pre><code>" + esc("\n".join(buf)) + "</code></pre>")
            continue

        # table: a pipe row followed by a separator row
        if line.lstrip().startswith("|") and i + 1 < n and is_sep_row(lines[i + 1]):
            header = table_cells(line)
            i += 2  # header + separator
            rows = []
            while i < n and lines[i].lstrip().startswith("|"):
                rows.append(table_cells(lines[i]))
                i += 1
            t = ["<table>", "<thead><tr>"]
            t += ["<th>%s</th>" % inline(c) for c in header]
            t.append("</tr></thead><tbody>")
            for r in rows:
                t.append("<tr>" + "".join("<td>%s</td>" % inline(c) for c in r) + "</tr>")
            t.append("</tbody></table>")
            out.append("".join(t))
            continue

        # heading
        m = re.match(r"^(#{1,6})\s+(.*)$", line)
        if m:
            level = len(m.group(1))
            out.append("<h%d>%s</h%d>" % (level, inline(m.group(2)), level))
            i += 1
            continue

        # list (flat, with wrapped-continuation lines)
        m = LIST_RE.match(line)
        if m:
            ordered = m.group(2)[0].isdigit()
            items = []
            while i < n:
                lm = LIST_RE.match(lines[i])
                if lm:
                    items.append(lm.group(3))
                    i += 1
                elif lines[i].strip() and lines[i][:1] in (" ", "\t") and items:
                    # indented continuation of the current item
                    items[-1] += " " + lines[i].strip()
                    i += 1
                else:
                    break
            tag = "ol" if ordered else "ul"
            out.append(
                "<%s>%s</%s>"
                % (tag, "".join("<li>%s</li>" % inline(it) for it in items), tag)
            )
            continue

        # blank
        if not line.strip():
            i += 1
            continue

        # paragraph: gather consecutive plain lines
        buf = [line]
        i += 1
        while i < n and lines[i].strip() and not lines[i].startswith("```") \
                and not re.match(r"^#{1,6}\s", lines[i]) \
                and not LIST_RE.match(lines[i]) \
                and not lines[i].lstrip().startswith("|"):
            buf.append(lines[i])
            i += 1
        out.append("<p>%s</p>" % inline(" ".join(s.strip() for s in buf)))

    return "\n".join(out)


def docnav(active):
    links = ['<a href="index.html">home</a>']
    for slug, label in PAGES:
        cls = ' class="active"' if slug == active else ""
        links.append('<a href="%s.html"%s>%s</a>' % (slug, cls, esc(label)))
    return '<div class="docnav">' + "".join(links) + "</div>"


def page_html(slug, title, body):
    desc = "atrium documentation: %s. tmux for coding agents, zero third-party dependencies." % title
    return """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — atrium docs</title>
<meta name="description" content="{desc}">
<link rel="stylesheet" href="assets/style.css">
</head>
<body>

<header><div class="wrap bar">
  <span class="dots"><span class="dot r"></span><span class="dot y"></span><span class="dot g"></span></span>
  <span class="title"><a href="index.html"><b>atrium</b></a> — docs</span>
  <nav>
    <a href="index.html">home</a>
    <a href="https://crates.io/crates/atrium">crates.io</a>
    <a href="https://github.com/nativelite/atrium">github</a>
  </nav>
</div></header>

<main class="wrap doc">
{nav}
{body}
</main>

<footer><div class="wrap">
  <div class="row">
    <a href="index.html">home</a>
    <a href="https://crates.io/crates/atrium">crates.io/atrium</a>
    <a href="https://github.com/nativelite/atrium">github.com/nativelite/atrium</a>
    <a href="https://github.com/nativelite/marketplace">marketplace</a>
    <a href="https://github.com/nativelite">nativelite</a>
  </div>
  <div>Built on the nativelite stack. Zero third-party dependencies. MIT licensed.</div>
</div></footer>

</body>
</html>
""".format(title=esc(title), desc=esc(desc), nav=docnav(slug), body=body)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    built = []
    for slug, _ in PAGES:
        src = os.path.join(here, slug + ".md")
        with open(src, encoding="utf-8") as f:
            md = f.read()
        m = re.search(r"^#\s+(.*)$", md, re.M)
        title = m.group(1).strip() if m else slug
        body = convert(md)
        dst = os.path.join(here, slug + ".html")
        with open(dst, "w", encoding="utf-8", newline="\n") as f:
            f.write(page_html(slug, title, body))
        built.append(slug + ".html")
    print("built %d pages: %s" % (len(built), ", ".join(built)))


if __name__ == "__main__":
    sys.exit(main())
