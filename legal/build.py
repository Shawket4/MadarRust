#!/usr/bin/env python3
"""Render the legal Markdown into a static site.

Deliberately dependency-free: neither this Mac nor the VPS has python-markdown or
pandoc, and a legal site that cannot be rebuilt because a toolchain drifted is a
liability. The Markdown subset here is exactly what these documents use.
"""
import datetime
import gzip as gziplib
import html
import json
import pathlib
import re
import shutil
import subprocess

SRC = pathlib.Path(__file__).parent / "en"
OUT = pathlib.Path(__file__).parent / "dist"

SITE = "https://legal.madar-pos.cloud"
MARKETING = "https://get.madar-pos.cloud/"

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

# ── search and agent metadata ─────────────────────────────────
# Kept here rather than in each document's front matter: touching a source file
# moves its git date, and that date is what the sitemap and dateModified report.
# A description is a summary of the page, not part of the legal text, so editing
# one must never look like a change to the document itself.
# Search engines show about 155 characters; the build warns outside 120-155.
INDEX_DESCRIPTION = ("Legal documents for Madar POS, the café and restaurant point of sale: "
                     "privacy, terms, data processing, security and data retention.")
DESCRIPTION = {
    "privacy-policy": "How Madar handles personal data of diners, loyalty members and restaurant "
                      "staff: what is collected, who receives it, where it is stored, and your rights.",
    "terms-of-service": "Terms between Madar and restaurants using its hosted point-of-sale service: "
                        "accounts, acceptable use, data, availability, fees, termination and liability.",
    "dpa": "Data Processing Agreement between Madar as processor and the restaurant as controller: "
           "obligations, security, sub-processors, audit, breach and transfers.",
    "subprocessors": "Third parties that process personal data for Madar's customers, including "
                     "Hostinger, Cloudflare, Google, WhatsApp and Apple: what each receives and where.",
    "employee-privacy-notice": "What the Dawam staff app records about restaurant employees, including "
                               "GPS location at clock-in and clock-out, who sees it, retention and your rights.",
    "data-retention": "How long Madar keeps each kind of record: orders and payroll for 5 years, "
                      "attendance GPS coordinates for 90 days, error reports for 30 days, and backups.",
    "delete-account": "How to delete a Madar account or have personal data removed, for account holders, "
                      "diners, loyalty members and Dawam employees, and which records are kept.",
    "security": "How the Madar platform protects data: database-level tenant isolation, single-person "
                "admin access, encryption, SSH-key server access and verified backups.",
}

# Exactly the marketing site's block (get.madar-pos.cloud), so both sites describe
# one organisation under one @id. Change it there and here together.
ORGANIZATION = {
    "@context": "https://schema.org",
    "@type": "Organization",
    "@id": "https://get.madar-pos.cloud/#organization",
    "name": "Madar POS",
    "alternateName": ["Madar", "مدار"],
    "url": "https://get.madar-pos.cloud/",
    "logo": "https://get.madar-pos.cloud/icon-512.png",
    "email": "shawket.4@icloud.com",
    "address": {"@type": "PostalAddress", "addressLocality": "Cairo", "addressCountry": "EG"},
    "contactPoint": [{"@type": "ContactPoint", "telephone": "+201211116899", "contactType": "sales",
                      "areaServed": "EG", "availableLanguage": ["en", "ar"]}],
    "sameAs": ["https://www.instagram.com/madar.cloud/",
               "https://www.facebook.com/profile.php?id=61591636403380",
               "https://apps.apple.com/app/id6815221877"],
}
PUBLISHER = {"@id": ORGANIZATION["@id"]}
WEBSITE = {"@type": "WebSite", "@id": f"{SITE}/#website", "url": f"{SITE}/",
           "name": "Madar POS legal documents", "inLanguage": "en", "publisher": PUBLISHER}

def git(cwd, *args) -> str:
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True,
                          check=True).stdout.strip()

def last_modified(path: pathlib.Path) -> str:
    """When the file last changed, as ISO 8601: its last commit, else its mtime.

    A file with uncommitted edits reports its mtime, because its last commit
    predates the text being built - the usual order is edit, build, then commit
    the source and dist together. The mtime fallback also covers a copy of the
    folder with no git at all.
    """
    try:
        if not git(path.parent, "status", "--porcelain", "--", path.name):
            committed = git(path.parent, "log", "-1", "--format=%cI", "--", path.name)
            if committed:
                return committed
    except (OSError, subprocess.CalledProcessError):
        pass
    mtime = datetime.datetime.fromtimestamp(path.stat().st_mtime, datetime.timezone.utc)
    return mtime.astimezone().isoformat(timespec="seconds")

def jsonld(obj) -> str:
    # "<" escaped so no value can ever close the script element early.
    data = json.dumps(obj, ensure_ascii=False, separators=(",", ":")).replace("<", "\\u003c")
    return f'<script type="application/ld+json">{data}</script>'

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

def seo_head(title, description, url, og_type, blocks, markdown_url=None):
    """Canonical, Open Graph and JSON-LD tags for one indexable page."""
    esc = lambda v: html.escape(v, quote=True)
    tags = [f'<link rel="canonical" href="{esc(url)}">']
    if markdown_url:
        tags.append(f'<link rel="alternate" type="text/markdown" href="{esc(markdown_url)}">')
    tags += [
        f'<meta property="og:title" content="{esc(title)} — Madar">',
        f'<meta property="og:description" content="{esc(description)}">',
        f'<meta property="og:url" content="{esc(url)}">',
        f'<meta property="og:type" content="{og_type}">',
        '<meta property="og:site_name" content="Madar POS">',
        '<meta name="twitter:card" content="summary">',
    ]
    tags += [jsonld(b) for b in blocks]
    return "\n".join(tags)

def page(title, body, toc, current, meta=None, description=None, head="", robots="index,follow"):
    # The `aria-current` attribute is built outside the f-string: a backslash in
    # an f-string expression is a syntax error before Python 3.12, and this file
    # has to run on whatever python3 the VPS happens to have.
    def nav_link(s, t):
        current_attr = ' aria-current="page"' if s == current else ""
        return f'<a href="/{s}.html"{current_attr}>{t}</a>'

    nav = "".join(nav_link(s, t) for s, t, _ in ORDER)
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
    desc = f'<meta name="description" content="{html.escape(description)}">\n' if description else ""
    extra = f"{head}\n" if head else ""
    return f"""<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{html.escape(title)} — Madar</title>
{desc}<meta name="robots" content="{robots}">
{extra}<link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'><text y='26' font-size='26'>%F0%9F%93%84</text></svg>">
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

def write(name, text):
    (OUT / name).write_text(text, encoding="utf-8")
    print("  built", name)

def build():
    OUT.mkdir(exist_ok=True)
    for name, d in [("index", INDEX_DESCRIPTION), *DESCRIPTION.items()]:
        if not 120 <= len(d) <= 155:
            print(f"  WARNING: {name} description is {len(d)} characters (aim for 120-155)")

    # A shallow clone has one commit, so git names it as the last change to every
    # file. CI checks out with full history for this reason (deploy.yml).
    try:
        if git(SRC, "rev-parse", "--is-shallow-repository") == "true":
            print("  WARNING: shallow clone - every page will carry the newest commit's date")
    except (OSError, subprocess.CalledProcessError):
        pass

    docs = []                                  # (slug, nav title, meta, url, modified)
    for s, t, _ in ORDER:
        src = SRC / f"{s}.md"
        meta, body, toc = render(src.read_text(encoding="utf-8"))
        title, url, modified = meta.get("title", t), f"{SITE}/{s}.html", last_modified(src)
        webpage = {"@context": "https://schema.org", "@type": "WebPage", "@id": f"{url}#webpage",
                   "name": title, "url": url, "description": DESCRIPTION[s], "inLanguage": "en",
                   "isPartOf": WEBSITE, "publisher": PUBLISHER, "dateModified": modified}
        head = seo_head(title, DESCRIPTION[s], url, "article", [ORGANIZATION, webpage],
                        markdown_url=f"{SITE}/{s}.md")
        write(f"{s}.html", page(title, body, toc, s, meta, DESCRIPTION[s], head))
        # The source itself, byte for byte, for agents: served at /<slug>.md and,
        # through the vhost, for /<slug>.html requested with Accept: text/markdown.
        shutil.copyfile(src, OUT / f"{s}.md")
        print("  copied", s + ".md")
        docs.append((s, t, meta, url, modified))

    # The index changes when any document does. Git may print UTC as "Z", which
    # fromisoformat only accepts from Python 3.11.
    index_modified = max((d[4] for d in docs), key=lambda v: datetime.datetime.fromisoformat(
        v.replace("Z", "+00:00")))
    cards = "".join(
        f'<a class="card" href="/{s}.html"><b>{t}</b><span>{d}</span></a>' for s, t, d in ORDER)
    idx = (f"<h1>Legal</h1><p>{html.escape(INDEX_DESCRIPTION)} Each states its version and "
           f"effective date; earlier versions remain available on request.</p>"
           f"<div class=\"cards\">{cards}</div>")
    collection = {"@context": "https://schema.org", "@type": "CollectionPage",
                  "@id": f"{SITE}/#webpage", "name": WEBSITE["name"], "url": f"{SITE}/",
                  "description": INDEX_DESCRIPTION, "inLanguage": "en", "isPartOf": WEBSITE,
                  "publisher": PUBLISHER, "dateModified": index_modified}
    head = seo_head("Legal", INDEX_DESCRIPTION, f"{SITE}/", "website", [ORGANIZATION, collection],
                    markdown_url=f"{SITE}/index.md")
    write("index.html", page("Legal", idx, [], "", None, INDEX_DESCRIPTION, head))

    # Served by the vhost's error_page for any missing path, at any depth: every
    # link is root-relative or absolute, and nothing is loaded from a file.
    missing = (f'<h1>Page not found</h1><p>There is no document at this address. Every '
               f'current document is listed below and at <a href="{SITE}/">legal.madar-pos.cloud</a>; '
               f'Madar POS itself is at <a href="{MARKETING}">get.madar-pos.cloud</a>.</p>'
               f'<div class="cards">{cards}</div>')
    write("404.html", page("Page not found", missing, [], "", None, None,
                           jsonld(ORGANIZATION), robots="noindex"))

    # ── discovery files ───────────────────────────────────────
    write("index.md", "\n".join([
        f"# {WEBSITE['name']}",
        "",
        INDEX_DESCRIPTION,
        "",
        "Each document states its version and effective date; earlier versions remain "
        "available on request.",
        "",
        *(f"- [{t}]({SITE}/{s}.md): {DESCRIPTION[s]} Version {m.get('version', '-')}, "
          f"effective {m.get('effective', '-')}." for s, t, m, _, _ in docs),
        "",
    ]))
    write("llms.txt", "\n".join([
        f"# {WEBSITE['name']}",
        "",
        f"> {INDEX_DESCRIPTION}",
        "",
        "Each document states its version and effective date. Every document is published "
        f"as HTML at {SITE}/<name>.html and as Markdown at {SITE}/<name>.md.",
        "",
        "## Documents",
        "",
        *(f"- [{t}]({SITE}/{s}.md): {DESCRIPTION[s]}" for s, t, _, _, _ in docs),
        "",
        "## Related",
        "",
        f"- [Madar POS]({MARKETING}): the product these documents cover, in English and Arabic.",
        "",
    ]))
    write("sitemap.xml", "\n".join([
        '<?xml version="1.0" encoding="UTF-8"?>',
        '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">',
        *(f"  <url><loc>{html.escape(u)}</loc><lastmod>{m}</lastmod></url>"
          for u, m in [(f"{SITE}/", index_modified), *((d[3], d[4]) for d in docs)]),
        "</urlset>",
        "",
    ]))
    write("robots.txt", "\n".join([
        "User-agent: *",
        "Content-Signal: search=yes, ai-input=yes, ai-train=yes",
        "Allow: /",
        f"Sitemap: {SITE}/sitemap.xml",
        "",
    ]))

    # Pre-compress for nginx `gzip_static`. Done HERE, not as a deploy step: if a
    # rebuild shipped fresh .html beside a stale .gz, gzip_static would keep
    # serving the OLD page to every client that accepts gzip — which is almost
    # all of them — and the site would look unchanged for no visible reason.
    # Every text file the site serves, so the Markdown and discovery files get
    # the same treatment as the pages.
    for f in sorted(OUT.iterdir()):
        if f.suffix in (".html", ".md", ".txt", ".xml"):
            with f.open("rb") as src, gziplib.GzipFile(str(f) + ".gz", "wb", 9, mtime=0) as dst:
                shutil.copyfileobj(src, dst)
    print(f"  gzipped {len(list(OUT.glob('*.gz')))} files for gzip_static")

if __name__ == "__main__":
    build()
