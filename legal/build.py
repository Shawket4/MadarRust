#!/usr/bin/env python3
"""Render the legal Markdown into a static site.

Deliberately dependency-free: neither this Mac nor the VPS has python-markdown or
pandoc, and a legal site that cannot be rebuilt because a toolchain drifted is a
liability. The Markdown subset here is exactly what these documents use.
"""
import gzip as gziplib
import html
import pathlib
import re
import shutil

SRC = pathlib.Path(__file__).parent / "en"
OUT = pathlib.Path(__file__).parent / "dist"

ORDER = [
    ("privacy-policy", "Privacy Policy", "How we handle personal data."),
    ("terms-of-service", "Terms of Service", "The agreement with restaurants using Madar."),
    ("dpa", "Data Processing Agreement", "Our obligations as a processor."),
    ("subprocessors", "Sub-processors", "Who else touches the data, and where."),
    ("employee-privacy-notice", "Employee Privacy Notice", "For staff using the Dawam app."),
    ("data-retention", "Data Retention", "How long each kind of record is kept."),
    ("delete-account", "Delete Your Account", "How to remove your account and data."),
    ("security", "Security", "How the platform is protected."),
]

# ── inline ────────────────────────────────────────────────────
def inline(t: str) -> str:
    t = html.escape(t, quote=False)
    t = re.sub(r"`([^`]+)`", r"<code>\1</code>", t)
    t = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", t)
    t = re.sub(r"(?<!\*)\*([^*]+)\*(?!\*)", r"<em>\1</em>", t)
    t = re.sub(r"\[([^\]]+)\]\(([^)]+)\)", r'<a href="\2">\1</a>', t)
    # Exclude only contexts where the address is already part of a URL/attribute
    # (mailto:, href="). A preceding ">" is fine - that is just <strong>.
    t = re.sub(r'(?<![:"\w.+-])([\w.+-]+@[\w-]+\.[\w.]+)\b', r'<a href="mailto:\1">\1</a>', t)
    return t

def slug(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")

# ── block ─────────────────────────────────────────────────────
def render(md: str):
    meta = {}
    if md.startswith("---"):
        _, fm, md = md.split("---", 2)
        for line in fm.strip().splitlines():
            if ":" in line:
                k, v = line.split(":", 1)
                meta[k.strip()] = v.strip()

    lines, out, toc, i = md.splitlines(), [], [], 0
    while i < len(lines):
        line = lines[i]

        if not line.strip():
            i += 1
            continue

        if line.startswith("#"):
            level = len(line) - len(line.lstrip("#"))
            text = line[level:].strip()
            if level == 1:
                out.append(f"<h1>{inline(text)}</h1>")
            else:
                sid = slug(text)
                out.append(f'<h{level} id="{sid}">{inline(text)}</h{level}>')
                if level == 2:
                    toc.append((sid, text))
            i += 1
            continue

        if line.startswith("|"):                                  # table
            block = []
            while i < len(lines) and lines[i].startswith("|"):
                block.append(lines[i]); i += 1
            cells = lambda r: [c.strip() for c in r.strip().strip("|").split("|")]
            head, body = cells(block[0]), block[2:]
            out.append('<div class="table-wrap"><table><thead><tr>'
                       + "".join(f"<th>{inline(c)}</th>" for c in head)
                       + "</tr></thead><tbody>")
            for row in body:
                out.append("<tr>" + "".join(f"<td>{inline(c)}</td>" for c in cells(row)) + "</tr>")
            out.append("</tbody></table></div>")
            continue

        if re.match(r"^\s*([-*]|\d+\.|[a-z]\.)\s", line):         # list
            items, ordered = [], bool(re.match(r"^\s*(\d+\.|[a-z]\.)\s", line))
            while i < len(lines) and re.match(r"^\s*([-*]|\d+\.|[a-z]\.)\s", lines[i]):
                items.append(re.sub(r"^\s*([-*]|\d+\.|[a-z]\.)\s+", "", lines[i])); i += 1
                while i < len(lines) and lines[i].startswith("  ") and lines[i].strip() \
                        and not re.match(r"^\s*([-*]|\d+\.|[a-z]\.)\s", lines[i]):
                    items[-1] += " " + lines[i].strip(); i += 1
            tag = "ol" if ordered else "ul"
            out.append(f"<{tag}>" + "".join(f"<li>{inline(x)}</li>" for x in items) + f"</{tag}>")
            continue

        if line.startswith(">"):                                  # quote
            buf = []
            while i < len(lines) and lines[i].startswith(">"):
                buf.append(lines[i].lstrip("> ").rstrip()); i += 1
            out.append(f"<blockquote>{inline(' '.join(buf))}</blockquote>")
            continue

        if line.strip() == "---":
            out.append("<hr>"); i += 1; continue

        buf = []                                                  # paragraph
        while i < len(lines) and lines[i].strip() and not re.match(
                r"^(#|\||>|\s*([-*]|\d+\.|[a-z]\.)\s|---$)", lines[i]):
            buf.append(lines[i].strip()); i += 1
        if buf:
            out.append(f"<p>{inline(' '.join(buf))}</p>")

    return meta, "\n".join(out), toc

# ── template ──────────────────────────────────────────────────
CSS = """
/* Light only, deliberately. These are legal documents: the goal is that they read
   like print, not like an app. No dark mode, no hover effects, no motion. */
:root{
  --bg:#ffffff; --panel:#fcfcfa; --ink:#1a1a18; --muted:#63615c; --line:#e3e0da;
  --accent:#0d6273; --accent-soft:#eff5f6;
  --serif:ui-serif,Charter,Georgia,'Times New Roman',serif;
  --sans:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;
}
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--ink);font-family:var(--serif);
     font-size:17.5px;line-height:1.72;text-rendering:optimizeLegibility}
a{color:var(--accent);text-underline-offset:2px}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.88em;
     background:var(--accent-soft);padding:.12em .38em;border-radius:3px}

.topbar{border-bottom:1px solid var(--line)}
.topbar .in{max-width:1060px;margin:0 auto;padding:16px 24px;display:flex;align-items:baseline;gap:14px}
.brand{display:inline-flex;align-items:center;text-decoration:none}
.brand svg{height:22px;width:auto;display:block}
/* Divider gets an explicit height so it is centred against the mark rather than
   sized by the text line-box, which is shorter than the logo. */
.brand .sub{font-family:var(--sans);font-size:14.5px;font-weight:500;color:var(--muted);
            height:20px;display:flex;align-items:center;
            margin-left:12px;padding-left:12px;border-left:1px solid var(--line)}

.shell.solo{grid-template-columns:minmax(0,1fr);max-width:820px}
.shell{max-width:1060px;margin:0 auto;padding:44px 24px 90px;display:grid;
       grid-template-columns:215px minmax(0,1fr);gap:60px;align-items:start}
nav.side{position:sticky;top:32px;font-family:var(--sans);font-size:14px}
nav.side h4{font-size:11px;letter-spacing:.09em;text-transform:uppercase;color:var(--muted);
            margin:0 0 12px;font-weight:600}
nav.side a{display:block;padding:5px 0;color:var(--muted);text-decoration:none;line-height:1.4}
nav.side a[aria-current]{color:var(--accent);font-weight:600}
nav.side .toc{margin-top:28px;padding-top:20px;border-top:1px solid var(--line)}
nav.side .toc a{font-size:13.5px}

article{min-width:0}
h1{font-size:2.25rem;line-height:1.18;letter-spacing:-.021em;margin:0 0 12px;font-weight:600}
h2{font-size:1.3rem;letter-spacing:-.011em;margin:2.5em 0 .7em;font-weight:600;
   padding-top:.55em;border-top:1px solid var(--line)}
h3{font-size:1.05rem;margin:1.8em 0 .5em;font-weight:650}
p{margin:0 0 1.05em}
ul,ol{margin:0 0 1.15em;padding-left:1.35em}
li{margin:.36em 0}
blockquote{margin:1.4em 0;padding:14px 18px;background:var(--accent-soft);
           border-left:3px solid var(--accent);border-radius:0 6px 6px 0;font-size:.96em}
hr{border:0;border-top:1px solid var(--line);margin:2.4em 0}
strong{font-weight:650}

.meta{font-family:var(--sans);font-size:13px;color:var(--muted);margin:0 0 2.4em;
      display:flex;gap:16px;flex-wrap:wrap}

.table-wrap{overflow-x:auto;margin:0 0 1.5em;border:1px solid var(--line);border-radius:6px}
table{border-collapse:collapse;width:100%;font-family:var(--sans);font-size:14.5px}
th,td{text-align:left;padding:11px 15px;border-bottom:1px solid var(--line);vertical-align:top}
th{font-weight:600;background:var(--panel);white-space:nowrap}
tr:last-child td{border-bottom:0}

.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(250px,1fr));gap:1px;
       margin:2em 0 0;background:var(--line);border:1px solid var(--line);border-radius:6px;
       overflow:hidden}
.card{display:block;padding:20px 22px;background:var(--bg);text-decoration:none;color:var(--ink)}
.card b{display:block;font-family:var(--sans);font-size:15px;font-weight:600;margin-bottom:5px;
        color:var(--accent)}
.card span{font-family:var(--sans);font-size:13.5px;color:var(--muted);line-height:1.5}

footer{max-width:1060px;margin:0 auto;padding:24px;border-top:1px solid var(--line);
       font-family:var(--sans);font-size:13px;color:var(--muted)}
footer a{color:var(--muted)}

@media(max-width:900px){
  .shell{grid-template-columns:1fr;gap:26px;padding-top:26px}
  nav.side{position:static;border-bottom:1px solid var(--line);padding-bottom:18px}
  nav.side .toc{display:none}
  h1{font-size:1.8rem}
  body{font-size:16.5px}
}
@media print{
  .topbar,nav.side,footer{display:none}
  .shell{display:block;max-width:none;padding:0}
  body{font-size:11pt}
  a{color:inherit;text-decoration:none}
}
"""

def page(title, body, toc, current, meta=None):
    nav = "".join(
        f'<a href="/{s}.html"{" aria-current=\"page\"" if s == current else ""}>{t}</a>'
        for s, t, _ in ORDER)
    tocs = ""
    if toc:
        tocs = '<div class="toc"><h4>On this page</h4>' + "".join(
            f'<a href="#{i}">{html.escape(t)}</a>' for i, t in toc) + "</div>"
    bits = ""
    if meta:
        if meta.get("version"):
            bits += f'<span>Version {html.escape(meta["version"])}</span>'
        if meta.get("effective"):
            bits += f'<span>Effective {html.escape(meta["effective"])}</span>'
    return f"""<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{html.escape(title)} — Madar</title>
<meta name="description" content="Madar legal documents: {html.escape(title)}.">
<meta name="robots" content="index,follow">
<link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'><text y='26' font-size='26'>%F0%9F%93%84</text></svg>">
<style>{CSS}</style></head><body>
<div class="topbar"><div class="in">
  <a class="brand" href="/"><svg role="img" aria-label="Madar" xmlns="http://www.w3.org/2000/svg" viewBox="100 200 420 120" width="429.19" height="130.39"><svg x="100" y="200" width="420" height="120.0" viewBox="0 0 322 92" overflow="visible"><g stroke="#14181E" stroke-width="13.5" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M14 30 L14 72"/><path d="M14 40 A14 10 0 0 1 42 40"/><path d="M42 40 L42 72"/><path d="M42 40 A14 10 0 0 1 70 40"/><path d="M70 40 L70 72"/><circle cx="111" cy="51" r="21"/><path d="M132 30 L132 72"/><circle cx="235" cy="51" r="21"/><path d="M256 30 L256 72"/><path d="M279 30 L279 72"/><path d="M279 39 A18 13 0 0 1 305 30"/></g><g stroke="#0D6273" stroke-width="13.5" fill="none" stroke-linecap="round" stroke-linejoin="round"><circle cx="173" cy="51" r="21"/><path d="M194 10 L194 72"/></g></svg></svg><span class="sub">Legal</span></a>
</div></div>
<div class="shell{'' if current else ' solo'}">
  {f'<nav class="side"><h4>Documents</h4>{nav}{tocs}</nav>' if current else ''}
  <article>{f'<div class="meta">{bits}</div>' if bits else ''}{body}</article>
</div>
<footer>© Madar · <a href="/">All documents</a> · <a href="mailto:privacy@madar-pos.cloud">privacy@madar-pos.cloud</a></footer>
</body></html>"""

def build():
    OUT.mkdir(exist_ok=True)
    for s, t, _ in ORDER:
        meta, body, toc = render((SRC / f"{s}.md").read_text())
        (OUT / f"{s}.html").write_text(page(meta.get("title", t), body, toc, s, meta))
        print("  built", s + ".html")

    cards = "".join(
        f'<a class="card" href="/{s}.html"><b>{t}</b><span>{d}</span></a>' for s, t, d in ORDER)
    idx = (f"<h1>Legal</h1><p>The documents governing how Madar handles data and provides "
           f"the service. Each states its version and effective date; earlier versions "
           f"remain available on request.</p><div class=\"cards\">{cards}</div>")
    (OUT / "index.html").write_text(page("Legal", idx, [], "", None))
    print("  built index.html")

    # Pre-compress for nginx `gzip_static`. Done HERE, not as a deploy step: if a
    # rebuild shipped fresh .html beside a stale .gz, gzip_static would keep
    # serving the OLD page to every client that accepts gzip — which is almost
    # all of them — and the site would look unchanged for no visible reason.
    for f in sorted(OUT.glob("*.html")):
        with f.open("rb") as src, gziplib.GzipFile(str(f) + ".gz", "wb", 9, mtime=0) as dst:
            shutil.copyfileobj(src, dst)
    print(f"  gzipped {len(list(OUT.glob('*.gz')))} files for gzip_static")

if __name__ == "__main__":
    build()
