"""Render the documentation set into the static site published on GitHub Pages."""
import html
import json
import posixpath
import re
import shutil
import sys
from pathlib import Path
from string import Template

import markdown
from markdown.extensions.toc import TocExtension

ROOT = Path(__file__).resolve().parents[1]
DOCS = ROOT / "docs"
SPEC = ROOT / "api" / "openapi.json"
NAV_NAME = "nav.md"
NAV_FILE = DOCS / NAV_NAME
REPOSITORY = "https://github.com/cyclikal94/atlas"

# Sections, order, labels and descriptions all come from docs/nav.md.
ENTRY = re.compile(r"[-*] \[([^\]]+)\]\(([^)\s]+)\)\s+\u2014\s+(.+)")

PALETTE = """:root{color-scheme:light;--bg:#ffffff;--fg:#1b1f24;--muted:#5b6470;--line:#e2e6ea;--accent:#2f5fd0;--code:#f4f6f8;--card:#fafbfc}
@media (prefers-color-scheme:dark){:root{color-scheme:dark;--bg:#14171b;--fg:#e5e9ee;--muted:#98a2ae;--line:#272c33;--accent:#88a9ff;--code:#1a1e24;--card:#181c21}}"""

# The documents and the API explorers share one header, so it must not depend on
# anything else on the page.
CHROME = """*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);
 font:16px/1.65 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
 -webkit-text-size-adjust:100%}
a{color:var(--accent)}
header.top{position:sticky;top:0;z-index:20;background:var(--bg);border-bottom:1px solid var(--line)}
header.top div{max-width:1180px;margin:0 auto;padding:14px 24px;display:flex;gap:16px;align-items:baseline}
header.top .brand{font-weight:650;color:var(--fg);text-decoration:none;letter-spacing:-.01em}
header.top .spacer{flex:1}
header.top a.plain{color:var(--muted);text-decoration:none;font-size:14px}
header.top a.plain:hover{color:var(--accent)}
header.top a.plain[aria-current=page]{color:var(--fg);font-weight:600}
@media (max-width:880px){header.top div{flex-wrap:wrap;gap:14px;padding:12px 16px}}"""

PAGE = """.layout{max-width:1180px;margin:0 auto;padding:0 24px;display:grid;grid-template-columns:220px minmax(0,1fr);gap:56px;align-items:start}
.layout.wide{grid-template-columns:minmax(0,1fr)}
nav.side{position:sticky;top:57px;padding:32px 0;font-size:14px;max-height:calc(100vh - 57px);overflow-y:auto}
nav.side h2{font-size:11px;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);margin:20px 0 8px}
nav.side h2:first-child{margin-top:0}
nav.side ul{list-style:none;margin:0;padding:0}
nav.side li{margin:0}
nav.side a{display:block;padding:4px 10px;margin-left:-10px;border-radius:6px;text-decoration:none;color:var(--fg)}
nav.side a:hover{background:var(--card);color:var(--accent)}
nav.side a[aria-current=page]{background:var(--card);color:var(--accent);font-weight:600}
main{padding:36px 0 96px;min-width:0}
main h1{font-size:31px;line-height:1.25;letter-spacing:-.02em;margin:0 0 8px}
main h2{font-size:22px;letter-spacing:-.01em;margin:40px 0 10px;padding-top:8px;border-top:1px solid var(--line)}
main h3{font-size:17px;margin:26px 0 8px}
main p,main li{overflow-wrap:break-word}
main code{background:var(--code);border:1px solid var(--line);border-radius:5px;padding:.1em .35em;font-size:.88em}
main pre{background:var(--code);border:1px solid var(--line);border-radius:9px;padding:14px 16px;overflow-x:auto}
main pre code{background:none;border:0;padding:0;font-size:13.5px;line-height:1.55}
main blockquote{margin:20px 0;padding:2px 0 2px 16px;border-left:3px solid var(--line);color:var(--muted)}
.tablewrap{overflow-x:auto;margin:20px 0;border:1px solid var(--line);border-radius:9px}
.tablewrap table{width:100%;border-collapse:collapse;font-size:14.5px}
.tablewrap th,.tablewrap td{text-align:left;padding:9px 14px;border-bottom:1px solid var(--line);vertical-align:top}
.tablewrap tr:last-child td{border-bottom:0}
.tablewrap th{background:var(--card);font-size:12.5px;text-transform:uppercase;letter-spacing:.05em;color:var(--muted)}
.headerlink{margin-left:.4em;color:var(--muted);text-decoration:none;opacity:0;font-weight:400}
h2:hover .headerlink,h3:hover .headerlink,.headerlink:focus{opacity:1}
.onpage{margin:24px 0 32px;padding:14px 18px;background:var(--card);border:1px solid var(--line);border-radius:9px;font-size:14px}
.onpage p{margin:0 0 8px;font-size:11px;text-transform:uppercase;letter-spacing:.08em;color:var(--muted)}
.onpage ul{margin:0;padding-left:18px}
.onpage ul ul{padding-left:16px;color:var(--muted)}
.onpage li{margin:2px 0}
.lede{font-size:18px;color:var(--muted);margin:0 0 28px}
.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(260px,1fr));gap:14px;margin:0 0 8px;padding:0;list-style:none}
.cards a{display:block;height:100%;padding:16px 18px;background:var(--card);border:1px solid var(--line);
 border-radius:11px;text-decoration:none;color:var(--fg)}
.cards a:hover{border-color:var(--accent)}
.cards strong{display:block;font-size:15.5px;margin-bottom:4px;color:var(--accent)}
.cards span{font-size:14px;color:var(--muted);line-height:1.5}
.section{margin:40px 0 0}
.section>h2{font-size:13px;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);
 border:0;margin:0 0 14px;padding:0}
footer{border-top:1px solid var(--line);color:var(--muted);font-size:13.5px;padding:22px 0 40px;margin-top:8px}
footer a{color:var(--muted)}
@media (max-width:880px){
 .layout{grid-template-columns:minmax(0,1fr);gap:0;padding:0 16px}
 nav.side{position:static;max-height:none;overflow:visible;padding:18px 0 16px;border-bottom:1px solid var(--line)}
 nav.side ul{display:flex;flex-wrap:wrap;gap:2px 4px}
 nav.side li{max-width:100%;min-width:0}
 nav.side a{margin-left:0;padding:4px 8px;overflow-wrap:break-word}
 nav.side h2{margin:14px 0 6px}
 main{padding-top:24px}
}"""

# Swagger UI ships a full dark theme behind the class it never sets itself, and
# Scalar follows the system theme on its own. Only the content width is ours, so
# that it lines up with the shared header.
EXPLORER = """.swagger-ui .wrapper{max-width:1180px;padding:0 24px}
.swagger-ui .info{margin:28px 0}
/* Its first cell in a row is the only one given bottom padding, which drops the
   first column below its siblings. Fall back to the padding they all share.
   The servers table is the exception: there every cell is padded, so the first
   one keeps its bottom padding to stay level with them. */
.swagger-ui table tbody tr td:first-of-type{padding:10px 0 0}
.swagger-ui .servers table tbody tr td:first-of-type{padding-bottom:10px}
/* Its dark theme covers .info .errors-wrapper but not the spec-resolution panel,
   which keeps light-theme text on a dark ground. Same colours it uses elsewhere. */
html.dark-mode .swagger-ui .errors-wrapper hgroup h4,
html.dark-mode .swagger-ui .errors-wrapper .errors h4{color:#e4e6e6}
html.dark-mode .swagger-ui .errors-wrapper .errors small{color:#a8b0b3}
html.dark-mode .swagger-ui .errors-wrapper .btn.errors__clear-btn{border-color:#e4e6e6;color:#e4e6e6}
/* Response header names and types are another uncovered light-theme colour. */
html.dark-mode .swagger-ui table.headers td{color:#909ec4}"""

# Swagger UI gates its dark theme on html.dark-mode rather than a media query, so
# the class has to track the system setting. In the head, before first paint.
EXPLORER_SCRIPT = """<script>
(function () {
  var query = window.matchMedia('(prefers-color-scheme: dark)');
  var apply = function () {
    document.documentElement.classList.toggle('dark-mode', query.matches);
  };
  apply();
  query.addEventListener('change', apply);
})();
</script>"""

SHELL = Template("""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>$title</title>
<meta name="description" content="$description">
<style>
$palette
$chrome
$page
</style>
</head>
<body>
$header
<div class="layout$layout_class">
$sidebar
<main>
$content
<footer>$footer</footer>
</main>
</div>
</body>
</html>
""")

EXPLORER_HEAD = Template("""<style>
$palette
$chrome
$explorer
</style>
$script""")


def fail(message):
    raise SystemExit("build_site: " + message)


def navigation():
    """Read docs/nav.md into the index heading, its introduction and the sections.

    Everything above the first '## ' heading is the index page: one '# ' heading
    and whatever prose follows it. Each '## ' heading then opens a group whose
    lines are '- [Label](file) \u2014 description' entries.
    """
    if not NAV_FILE.is_file():
        fail("docs/%s is missing; it defines the site navigation" % NAV_NAME)
    heading = None
    intro = []
    sections = []
    for number, raw in enumerate(NAV_FILE.read_text().splitlines(), 1):
        line = raw.strip()
        if line.startswith("## "):
            sections.append((line[3:].strip(), []))
            continue
        if not sections:
            if line.startswith("# "):
                if heading is not None:
                    fail("docs/%s has more than one '# ' heading" % NAV_NAME)
                heading = line[2:].strip()
            else:
                intro.append(raw)
            continue
        if not line:
            continue
        if not line.startswith(("- ", "* ")):
            fail("docs/%s line %d sits under '%s' but is not an entry; prose belongs "
                 "above the first '## ' heading: %s"
                 % (NAV_NAME, number, sections[-1][0], line))
        match = ENTRY.fullmatch(line)
        if not match:
            fail("docs/%s line %d is not '- [Label](file) \u2014 description': %s"
                 % (NAV_NAME, number, line))
        label, target, description = (value.strip() for value in match.groups())
        sections[-1][1].append((label, target, description))
    if heading is None:
        fail("docs/%s needs a '# ' heading to title the index page" % NAV_NAME)
    # Comments in the navigation file guide whoever edits it; they are not content.
    text = re.sub(r"<!--.*?-->", "", "\n".join(intro), flags=re.S)
    return heading, text.strip(), sections


def check_navigation(sections):
    """Hold the navigation and the directory to exactly the same set of files."""
    empty = [name for name, entries in sections if not entries]
    if empty:
        fail("docs/%s has sections with no entries: %s" % (NAV_NAME, ", ".join(empty)))
    entries = [entry for _, items in sections for entry in items]
    if not entries:
        fail("docs/%s lists no entries" % NAV_NAME)
    targets = [target for _, target, _ in entries]
    repeated = sorted({target for target in targets if targets.count(target) > 1})
    if repeated:
        fail("docs/%s lists these more than once: %s" % (NAV_NAME, ", ".join(repeated)))
    if NAV_NAME in targets:
        fail("docs/%s cannot list itself" % NAV_NAME)
    strange = sorted(t for t in targets if not t.endswith((".md", ".html")))
    if strange:
        fail("docs/%s can only list .md documents and .html explorers, not: %s"
             % (NAV_NAME, ", ".join(strange)))
    listed = [target for target in targets if target.endswith(".md")]
    present = sorted(path.name for path in DOCS.glob("*.md") if path.name != NAV_NAME)
    missing = sorted(set(present) - set(listed))
    unknown = sorted(set(listed) - set(present))
    if missing:
        fail("documents not listed in docs/%s: %s" % (NAV_NAME, ", ".join(missing)))
    if unknown:
        fail("docs/%s lists documents that do not exist: %s" % (NAV_NAME, ", ".join(unknown)))
    explorers = [entry for entry in entries if entry[1].endswith(".html")]
    for _, target, _ in explorers:
        if not (DOCS / target).is_file():
            fail("docs/%s lists missing explorer docs/%s" % (NAV_NAME, target))
    return present, explorers


def page_name(document):
    return document[: -len(".md")] + ".html"


def rewrite_target(target, source):
    """Map a link written for the repository onto its published location.

    Targets are resolved against the source file, so the same rule serves the
    documents and the navigation file. A document becomes a sibling page, the
    description keeps its path, and any other repository file becomes a link
    back to the source on GitHub.
    """
    if target.startswith(("http://", "https://", "mailto:", "#", "data:")):
        return target
    path, _, fragment = target.partition("#")
    fragment = "#" + fragment if fragment else ""
    if not path:
        return target
    resolved = posixpath.normpath(posixpath.join(posixpath.dirname(source), path))
    if resolved.startswith("../"):
        fail("%s links outside the repository: %r" % (source, target))
    if not (ROOT / resolved).is_file():
        fail("%s links to missing file %r" % (source, target))
    if resolved == "api/openapi.json":
        return "api/openapi.json" + fragment
    if resolved == "docs/" + NAV_NAME:
        return "index.html" + fragment
    if resolved.startswith("docs/") and resolved.endswith(".md"):
        return page_name(posixpath.basename(resolved)) + fragment
    return "%s/blob/main/%s%s" % (REPOSITORY, resolved, fragment)


def rewrite_links(body, source):
    def replace(match):
        target = rewrite_target(match.group(2), source)
        return '%s="%s"' % (match.group(1), target)

    return re.sub(r'(href|src)="([^"]*)"', replace, body)


def title_of(document):
    for line in (DOCS / document).read_text().splitlines():
        if line.startswith("# "):
            return line[2:].strip()
    fail("%s has no top-level heading" % document)


def header(explorers, current=""):
    """The shared top bar. Identical everywhere, so nothing below it can shift it."""
    parts = ['<a class="brand" href="index.html">Atlas</a>', '<span class="spacer"></span>']
    for label, target, _ in explorers:
        marker = ' aria-current="page"' if target == current else ""
        parts.append('<a class="plain" href="%s"%s>%s</a>' % (target, marker, label))
    parts.append('<a class="plain" href="%s">GitHub</a>' % REPOSITORY)
    return '<header class="top"><div>\n%s\n</div></header>' % "\n".join(parts)


def href_of(target):
    return page_name(target) if target.endswith(".md") else target


def sidebar(sections, current):
    parts = ['<nav class="side" aria-label="Documentation">']
    for section, entries in sections:
        parts.append("<h2>%s</h2><ul>" % section)
        for label, target, _ in entries:
            href = href_of(target)
            marker = ' aria-current="page"' if href == current else ""
            parts.append('<li><a href="%s"%s>%s</a></li>' % (href, marker, label))
        parts.append("</ul>")
    parts.append("</nav>")
    return "\n".join(parts)


def on_this_page(tokens):
    """An 'on this page' list, omitted when a document has too little structure."""
    if len(tokens) < 2:
        return ""
    items = []
    for token in tokens:
        children = "".join(
            '<li><a href="#%s">%s</a></li>' % (child["id"], child["name"])
            for child in token["children"]
        )
        nested = "<ul>%s</ul>" % children if children else ""
        items.append('<li><a href="#%s">%s</a>%s</li>' % (token["id"], token["name"], nested))
    return '<div class="onpage"><p>On this page</p><ul>%s</ul></div>' % "".join(items)


def render(text):
    converter = markdown.Markdown(
        extensions=["extra", "sane_lists", TocExtension(permalink="#", toc_depth="2-3")],
        output_format="html",
    )
    body = converter.convert(text)
    body = body.replace("<table>", '<div class="tablewrap"><table>')
    body = body.replace("</table>", "</table></div>")
    return body, converter.toc_tokens


def shell(title, description, content, sidebar_html, footer, header_html):
    return SHELL.substitute(
        title=html.escape(title),
        description=html.escape(description),
        palette=PALETTE,
        chrome=CHROME,
        page=PAGE,
        header=header_html,
        layout_class="" if sidebar_html else " wide",
        sidebar=sidebar_html,
        content=content,
        footer=footer,
    )


def build_document(document, output, sections, header_html):
    body, tokens = render((DOCS / document).read_text())
    heading, _, remainder = body.partition("</h1>")
    if not remainder:
        fail("%s did not render a top-level heading" % document)
    content = heading + "</h1>" + on_this_page(tokens) + remainder
    title = title_of(document)
    footer = 'Part of the <a href="index.html">Atlas documentation</a>. <a href="%s/blob/main/docs/%s">Edit this page</a>.' % (
        REPOSITORY,
        document,
    )
    page = shell(
        title="%s — Atlas" % title,
        description="Atlas documentation: %s." % title.lower(),
        content=rewrite_links(content, "docs/" + document),
        sidebar_html=sidebar(sections, page_name(document)),
        footer=footer,
        header_html=header_html,
    )
    (output / page_name(document)).write_text(page)


def summary(body, fallback):
    """The first paragraph as plain text, for the page description."""
    match = re.search(r"<p[^>]*>(.*?)</p>", body, re.S)
    if not match:
        return fallback
    text = re.sub(r"<[^>]+>", " ", match.group(1))
    return " ".join(html.unescape(text).split())


def cards(entries):
    items = "".join(
        '<li><a href="%s"><strong>%s</strong><span>%s</span></a></li>' % (href, name, description)
        for href, name, description in entries
    )
    return '<ul class="cards">%s</ul>' % items


def build_index(output, heading, intro, sections, header_html):
    body = ""
    if intro:
        rendered, _ = render(intro)
        body = rewrite_links(rendered, "docs/" + NAV_NAME)
        if body.startswith("<p>"):
            body = '<p class="lede">' + body[len("<p>"):]
    blocks = []
    for section, entries in sections:
        blocks.append(
            '<div class="section"><h2>%s</h2>%s</div>'
            % (section, cards([(href_of(target), label, description)
                               for label, target, description in entries]))
        )
    content = "<h1>%s</h1>" % html.escape(heading) + body + "".join(blocks)
    footer = 'Source and issues on <a href="%s">GitHub</a>.' % REPOSITORY
    page = shell(
        title=heading,
        description=summary(body, heading),
        content=content,
        sidebar_html="",
        footer=footer,
        header_html=header_html,
    )
    (output / "index.html").write_text(page)


def build_explorers(output, explorers):
    for _, name, _ in explorers:
        text = (DOCS / name).read_text()
        if "../api/openapi.json" not in text:
            fail("docs/%s no longer references ../api/openapi.json" % name)
        for marker in ("<head>", "</head>", "<body>"):
            if marker not in text:
                fail("docs/%s has no %s to build the page around" % (name, marker))
        text = text.replace("../api/openapi.json", "api/openapi.json")
        head = EXPLORER_HEAD.substitute(
            palette=PALETTE,
            chrome=CHROME,
            explorer=EXPLORER,
            script=EXPLORER_SCRIPT,
        )
        text = text.replace("</head>", head + "\n</head>", 1)
        text = text.replace("<body>", "<body>\n" + header(explorers, name), 1)
        (output / name).write_text(text)


def build_spec(output):
    try:
        json.loads(SPEC.read_text())
    except ValueError as error:
        fail("api/openapi.json is not valid JSON: %s" % error)
    (output / "api").mkdir(parents=True, exist_ok=True)
    shutil.copyfile(SPEC, output / "api" / "openapi.json")


def main():
    output = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "site").resolve()
    if output == ROOT or output == DOCS:
        fail("refusing to build into %s" % output)
    if output.exists():
        shutil.rmtree(output)
    output.mkdir(parents=True)

    heading, intro, sections = navigation()
    listed, explorers = check_navigation(sections)
    chrome = header(explorers)
    for document in listed:
        build_document(document, output, sections, chrome)
    build_index(output, heading, intro, sections, chrome)
    build_explorers(output, explorers)
    build_spec(output)
    (output / ".nojekyll").write_text("")

    print("Built %d documents, %d API explorers and the index into %s"
          % (len(listed), len(explorers), output))


if __name__ == "__main__":
    main()
