#!/usr/bin/env python3
"""Turn a docs tree into fact tuples for k10r — the knowledge pipeline's pilot.

Deliberately MECHANICAL (no model in the loop): every markdown file becomes a
node, every heading a node, and only the edges the text or the file system
proves are emitted. Atomic claims inside the prose are NOT extracted here: that
pass is a model's job, and this skeleton is what makes its output checkable —
every claim it produces gets a section to live in, so a reader can always walk
back to the paragraph.

Identity rule: a node's VALUE is the readable label, because retrieval embeds
the value — a slug retrieves nothing — and the stable identity rides in props
(`key`, `slug`, `lang`, `line`). A heading's value is its text, so "what does
the backup section say" is a search that can actually land.

Four layers, each one below the last, which is what makes the picture readable
at a glance instead of a hairball:

    project ──CONTAINS──▶ document ──SECTION_OF──▶ section ──STATES──▶ claim

The top layer (project + document) is the only one a landing page needs to
draw; sections and claims are what a reader expands. `level` on a section is
what lets a renderer stop at `##` without a second field.

Language: a document has one identity and one or two texts. Both texts are
ingested (the embedder is not a translation layer — a Chinese query answers
over English nodes at 0.62, measured on this corpus), each node carries `lang`,
and the pair is joined by `TWIN_OF`. A renderer filters by `lang`; it never
infers a language from adjacency.

Usage:
  scripts/kg_convert.py --base http://127.0.0.1:7760 --scope 1 \
      --site docs/kg/site.json \
      docs/PLAN.md docs/MODELING.md docs/SEAM.md docs/PROMPTS.md README.md \
      docs/adr/[0-9]*.md \
      ~/.hermes/wiki/graph-memory*.md ~/.hermes/wiki/kv-storage-engine*.md
  scripts/kg_convert.py ... --claims docs/kg/claims.json
  scripts/kg_convert.py ... --dry-run      # print the tuples, send nothing

Exit code is non-zero if the service refused anything.

Re-running DOUBLES the corpus. A repeat assertion is a new edge unless the
relation is `functional` (then it rewrites the one edge); the mechanical facts
here are mostly multi-valued by nature (`CITES`, `MENTIONS_CRATE`, `STATES`), so
ingest into a fresh scope or a fresh database instead of layering a second run
on the first. `SECTION_OF` and `TWIN_OF` are marked functional: a section has
one home and a document has one twin, so those 854 facts do survive a rerun.
"""

import argparse
import json
import re
import sys
import urllib.request
from pathlib import Path

ADR_RE = re.compile(r"\bADR[- ]?(\d{4})\b")
PATH_RE = re.compile(r"(?:docs/|wiki/(?:blob/[a-z]+/)?)?([A-Za-z0-9_.-]+\.md)")
HEADING_RE = re.compile(r"^(#{1,6})\s+(.*)$")
CRATE_RE = re.compile(r"\bcrates/([a-z0-9-]+)\b")
WIKI_RE = re.compile(r"(?:\.hermes/)?wiki/(?:blob/[a-z]+/)?([a-z0-9-]+\.md)")
DEP_GIT_RE = re.compile(r"github\.com/orbsh/([a-z0-9-]+)")
DEP_PATH_RE = re.compile(r'path\s*=\s*"[^"]*\.\./(?:\.\./)*([a-z0-9-]+)/')
PACKAGE_NAME_RE = re.compile(r'^name\s*=\s*"([^"]+)"', re.M)


def identity(path: Path, trees) -> tuple[str, str]:
    """(doc key, language) — the key is language-independent, the lang is data.

    Two naming conventions live in this corpus and both must land on one key:
    the repo's `foo.md` / `foo.zh-CN.md` pair, and the wiki's `foo.md` (Chinese)
    / `foo-en.md` (English) pair. A tree in site.json states which language its
    unsuffixed files are in; a file's own suffix always wins.
    """
    stem = path.stem
    if stem.endswith(".zh-CN"):
        return stem[: -len(".zh-CN")], "zh"
    if stem.endswith("-en"):
        return stem[: -len("-en")], "en"
    try:
        resolved = path.expanduser().resolve()
    except OSError:
        return stem, "en"
    best = None
    for tree in trees or ():
        root = Path(tree["root"]).expanduser().resolve()
        if resolved.parent == root or root in resolved.parents:
            if best is None or len(str(root)) > best[0]:
                best = (len(str(root)), tree.get("default_lang", "en"))
    return stem, (best[1] if best else "en")


def tuple_(head, head_kind, head_props, rel, rel_props, tail, tail_kind, tail_props):
    """The 8 slots, positional (ADR-0004)."""
    return [head, head_kind, head_props, rel, rel_props, tail, tail_kind, tail_props]


SENTENCE_END = re.compile(r"(?<=[。！？!?])|(?<=\.)\s+")
TITLE_PREFIX = re.compile(r"^(?:ADR[- ]?\d{4}\s*[:：]\s*|\d{4}\s*[—–-]\s*)")
LIST_MARK = re.compile(r"^\s*(?:[-*+]|\d+\.)\s+")
FENCE = re.compile(r"^\s*(?:```|~~~)")
NUMBERING = re.compile(r"^\d+(?:\.\d+)*\.?\s+")
BACKTICKS = re.compile(r"`([^`]*)`")
LINK = re.compile(r"\[([^\]]*)\]\([^)]*\)")
EMPHASIS = re.compile(r"\*{2,}")
DANGLING = re.compile(r"[，,、；;：:/\\(（\[【—-]+$")
# Headings that say nothing on their own: a view needs them prefixed with the
# document, or ten nodes in a row all read "Context".
GENERIC = {
    "context", "decision", "decisions", "consequences", "background", "overview",
    "summary", "history", "scope", "status", "problem", "limits", "non-goals",
    "alternatives", "risks", "implementation", "example", "examples", "motivation",
    "rationale", "open questions", "references",
    "背景", "决定", "后果", "上下文", "概述", "摘要", "历史", "状态", "问题", "范围",
    "取舍", "风险", "实现", "示例", "动机", "开放问题", "参考",
}


def clean(text: str) -> str:
    """Markdown out of a label: backticks, links and emphasis are formatting."""
    text = LINK.sub(r"\1", text)
    text = BACKTICKS.sub(r"\1", text)
    return EMPHASIS.sub("", text).strip()


def short_title(raw: str, limit: int = 22) -> str:
    """A label a view can draw: prefix gone, subtitle after the dash gone.

    "ADR-0005: Weight System — Counters on the Edge Row, Read-Write Quadrants"
    is an identity, not a title — a node badge showing `ADR-0005` says nothing.
    The identity keeps the whole string; the title is what a reader sees.
    """
    text = clean(TITLE_PREFIX.sub("", raw.strip()))
    for dash in ("—", "–", " - ", "：", ": "):
        if dash in text:
            head = text.split(dash, 1)[0].strip()
            if len(head) >= 2:
                text = head
                break
    # A parenthetical is detail, never the label: "Memory kinds (who initiates)"
    # is a title plus an explanation, and a badge has room for the first only.
    if "(" in text:
        text = text.split("(", 1)[0].strip() or text
    if len(text) > limit:
        cut = text[:limit]
        space = cut.rfind(" ")
        text = cut[:space] if space > limit // 2 else cut
    return DANGLING.sub("", text).strip()


def clause_title(text: str, limit: int = 16) -> str:
    """A claim's label: its first clause, not its first N characters.

    A claim is a sentence; cutting it blind produces "The backup is one Par".
    """
    body = clean(text)
    for mark in ("，", ", ", "；", "; ", "：", " — "):
        if mark in body:
            head = body.split(mark, 1)[0].strip()
            if 3 <= len(head) <= limit:
                return head
    return short_title(body, limit)


CJK = re.compile(r"[\u3000-\u303f\u4e00-\u9fff\uff00-\uffef]")


def smart_join(lines: list[str]) -> str:
    """Glue wrapped lines without inventing a space inside Chinese text.

    Markdown wraps CJK mid-sentence; joining those lines with a space puts a gap
    the writer never wrote, while joining an English line break without one fuses
    two words. The character on each side decides.
    """
    out = ""
    for line in lines:
        if not line:
            continue
        if not out:
            out = line
            continue
        gap = "" if CJK.match(out[-1]) and CJK.match(line[0]) else " "
        out = f"{out}{gap}{line}"
    return out


def summarize(body: list[str], budget: int = 120) -> str:
    """The first sentence(s) of a section, up to `budget` characters.

    The graph is the SUMMARY layer: one node per section says what the section
    says, and `source` points back at the paragraph. Mirroring the prose instead
    would put the document back into the graph, one paragraph at a time.
    """
    kept = []
    in_fence = False
    for line in body:
        if FENCE.match(line):
            # A code block is not prose: its first line is `#[derive(...)]` or a
            # shell command, which reads as a fact and is not one.
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        text = LIST_MARK.sub("", line).strip()
        if not text or text.startswith("|"):
            continue
        if text.startswith(">") or text.startswith("<!--"):
            continue
        kept.append(clean(text))
    joined = smart_join(kept).strip()
    if not joined:
        return ""
    if len(joined) <= budget:
        return joined
    out = ""
    for piece in SENTENCE_END.split(joined):
        piece = piece.strip()
        if not piece:
            continue
        if out and len(out) + len(piece) + 1 > budget and len(out) >= budget // 2:
            break
        out = smart_join([out, piece]) if out else piece
        if len(out) >= budget:
            break
    if len(out) > budget or not out:
        # One sentence longer than the budget is still one sentence: cut it at a
        # word boundary rather than shipping a paragraph.
        cut = (out or joined)[:budget]
        space = cut.rfind(" ")
        if space > budget // 2:
            cut = cut[:space]
        out = cut.rstrip("，,、；;：: ") + "…"
    return out


def read_doc(path: Path):
    """Title, sections (each with its own body) and the references of one file."""
    lines = path.read_text(encoding="utf-8").splitlines()
    title = None
    headings = []
    cites = set()
    crates = set()
    for number, line in enumerate(lines, start=1):
        heading = HEADING_RE.match(line)
        if heading:
            level, text = len(heading.group(1)), heading.group(2).strip()
            if level == 1 and title is None:
                title = text
            elif level >= 2:
                headings.append((text, level, number))
        for match in ADR_RE.finditer(line):
            cites.add(("adr", match.group(1)))
        for match in PATH_RE.finditer(line):
            cites.add(("doc", match.group(1)))
        for match in CRATE_RE.finditer(line):
            crates.add(match.group(1))
    title = title or path.stem

    def body_between(start: int, stop: int) -> list[str]:
        return lines[start:stop]

    sections = []
    for index, (text, level, line_no) in enumerate(headings):
        # A section's own prose stops at the NEXT heading of any level; when the
        # heading is followed straight by a sub-heading, the body would be empty
        # and the section contributes nothing, so it takes the sub-tree instead.
        stop = len(lines)
        for other_text, other_level, other_line in headings[index + 1 :]:
            stop = other_line - 1
            break
        body = body_between(line_no, stop)
        if not "".join(part.strip() for part in body):
            for other_text, other_level, other_line in headings[index + 1 :]:
                if other_level <= level:
                    stop = other_line - 1
                    break
            body = body_between(line_no, stop)
        sections.append((text, level, line_no, body))
    return title, sections, cites, crates


def doc_props(doc) -> dict:
    """The fields a reader needs to place a document without reading it.

    `blurb` / `blurb_en` and `url` come from site.json (the editorial layer);
    the rest is derived. `type` keeps ADR-ness after `adr` stopped being a node
    class — one kind vocabulary makes a legend possible.
    """
    props = {
        "key": doc["key"],
        "slug": doc["slug"],
        "lang": doc["lang"],
        "type": doc["type"],
        "title": doc.get("display_title") or doc["title"],
        "source": doc.get("source") or doc["rel_path"],
        "path": doc["rel_path"],
    }
    if doc.get("content"):
        props["content"] = doc["content"]
    for field in ("blurb", "blurb_en", "url"):
        if doc.get(field):
            props[field] = doc[field]
    return props


def project_props(project) -> dict:
    return {
        "key": project["key"],
        "path": project.get("path", ""),
        "repo": project.get("repo", ""),
        "title": project["name"],
        "content": project.get("blurb", ""),
        "source": project.get("repo", ""),
        "blurb": project.get("blurb", ""),
        "blurb_en": project.get("blurb_en", ""),
    }


def build(paths, trees=None, site=None, claims=()):
    """Every tuple this corpus proves, in ingest order: nodes, then edges."""
    site = site or {}
    docs = {}
    by_key = {}
    number_index = {}
    for path in paths:
        key, lang = identity(path, trees)
        title, sections, cites, crates = read_doc(path)
        stem = path.stem
        entry = {
            "path": path,
            "rel_path": str(path),
            "title": title,
            "slug": stem,
            "key": key,
            "lang": lang,
            "type": "adr" if re.match(r"^\d{4}-", stem) else "doc",
            "sections": sections,
            "cites": cites,
            "crates": crates,
        }
        meta = (site.get("documents") or {}).get(key) or {}
        entry.update({f: meta[f] for f in ("blurb", "blurb_en", "url") if f in meta})
        # The three props a reader needs without opening the file (2026-10-23):
        # a short TITLE a view can draw, one sentence of CONTENT, and the SOURCE
        # that points back at the paragraph. Editorial where the corpus is
        # curated (site.json), derived where it is not.
        wanted = "title" if lang == "zh" else "title_en"
        fallback = "blurb" if lang == "zh" else "blurb_en"
        entry["display_title"] = (
            meta.get(wanted) or short_title(title) or title
        )
        entry["content"] = meta.get(fallback, "")
        entry["source"] = meta.get("url") or str(path)
        docs[stem] = entry
        by_key.setdefault(key, []).append(entry)
        adr = re.match(r"^(\d{4})-", key)
        if adr:
            number_index.setdefault(adr.group(1), key)

    def known_key(target: str):
        """Resolve a citation target (an ADR number or a file stem) to a key."""
        if target.endswith(".md"):
            target = target[: -len(".md")]
        if re.fullmatch(r"\d{4}", target):
            return number_index.get(target)
        key, _ = identity(Path(target + ".md"), trees)
        return key if key in by_key else None

    facts = []
    # 1. Sections, each claimed by its document. A document's node comes into
    #    being here: it is the tail of every one of its sections, so a file with
    #    no headings would produce no node at all -- the report shows that
    #    rather than inventing a lonely document node nothing points at.
    for stem, doc in docs.items():
        for index, (text, level, line, body) in enumerate(doc["sections"]):
            # A heading is not an identity: "Context" appears in ten ADRs, and a
            # node every one of them points at would make each query land on the
            # same word. The section's value carries its document, which keeps
            # the name readable and the node addressable; the TITLE is the short
            # label a view draws, prefixed with the document when the heading
            # alone is too generic to say anything ("Context" × 14).
            label = short_title(text)
            label = NUMBERING.sub("", label).strip() or label
            if doc.get("display_title") and (
                label.lower().strip(" .:：") in GENERIC or len(label) <= 4
            ):
                label = f"{doc['display_title']} · {label}"
            facts.append(
                tuple_(
                    f"{text} ({stem})",
                    "section",
                    {
                        "doc": stem,
                        "key": doc["key"],
                        "lang": doc["lang"],
                        "line": line,
                        "level": level,
                        "index": index,
                        "title": label[:40],
                        "content": summarize(body, 70 if doc["lang"] == "zh" else 120),
                        "source": f"{doc['rel_path']}:{line}",
                    },
                    "SECTION_OF",
                    {"functional": True},
                    doc["title"],
                    "document",
                    doc_props(doc),
                )
            )
    # 2. Citations between documents: only ids this corpus holds, so a dangling
    #    ADR number becomes a report line, not a ghost node. Both spellings of
    #    the same target (`ADR-0005` and `0005-weight-system.md`) collapse to
    #    one edge -- the graph states the citation, not how it was written.
    for stem, doc in docs.items():
        resolved = {}
        for kind, target in sorted(doc["cites"]):
            key = known_key(target)
            if key is None or key == doc["key"]:
                continue
            resolved.setdefault(key, kind)
        for key, kind in sorted(resolved.items()):
            other = by_key[key][0]
            facts.append(
                tuple_(
                    doc["title"],
                    "document",
                    doc_props(doc),
                    "CITES",
                    {"by": kind},
                    other["title"],
                    "document",
                    doc_props(other),
                )
            )
    # 3. The two texts of one document. The pair is ONE document, so the graph
    #    says so instead of leaving two strangers with the same content.
    for key, group in sorted(by_key.items()):
        en = next((d for d in group if d["lang"] == "en"), None)
        zh = next((d for d in group if d["lang"] == "zh"), None)
        if not en or not zh or en is zh:
            continue
        facts.append(
            tuple_(
                en["title"],
                "document",
                doc_props(en),
                "TWIN_OF",
                {"functional": True},
                zh["title"],
                "document",
                doc_props(zh),
            )
        )
    # 4. Crates a document names.
    for stem, doc in docs.items():
        for crate in sorted(doc["crates"]):
            facts.append(
                tuple_(
                    doc["title"],
                    "document",
                    doc_props(doc),
                    "MENTIONS_CRATE",
                    {},
                    crate,
                    "crate",
                    {},
                )
            )
    # 5. The project layer. Everything here is measured, not asserted: a
    #    dependency is a line in a Cargo.toml, a crate is a directory, a document
    #    belongs to the tree that holds its file.
    projects = site.get("projects") or []
    project_by_key = {p["key"]: p for p in projects}
    for project in projects:
        root = Path(project["path"]).expanduser()
        try:
            root_res = root.resolve()
        except OSError:
            continue
        props = project_props(project)
        # 5a. CONTAINS: the documents that live in this project's tree. The edge
        #     states the hierarchy a landing page draws as its first level.
        for stem, doc in docs.items():
            try:
                Path(doc["path"]).expanduser().resolve().relative_to(root_res)
            except (ValueError, OSError):
                continue
            facts.append(
                tuple_(
                    project["name"],
                    "project",
                    props,
                    "CONTAINS",
                    {},
                    doc["title"],
                    "document",
                    doc_props(doc),
                )
            )
        # 5b. HAS_CRATE: the workspace's own crates, by directory.
        for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
            found = PACKAGE_NAME_RE.search(manifest.read_text(encoding="utf-8"))
            if not found:
                continue
            facts.append(
                tuple_(
                    project["name"],
                    "project",
                    props,
                    "HAS_CRATE",
                    {},
                    found.group(1),
                    "crate",
                    {},
                )
            )
        # 5c. DEPENDS_ON: another project of this family, when the manifest says
        #     so by git URL or by a path climbing out of the tree.
        deps = set()
        for manifest in list(root.glob("Cargo.toml")) + list(root.glob("crates/*/Cargo.toml")):
            text = manifest.read_text(encoding="utf-8")
            deps.update(m.lower() for m in DEP_GIT_RE.findall(text))
            deps.update(m.lower() for m in DEP_PATH_RE.findall(text))
        for dep in sorted(deps):
            other = project_by_key.get(dep)
            if not other or dep == project["key"]:
                continue
            facts.append(
                tuple_(
                    project["name"],
                    "project",
                    props,
                    "DEPENDS_ON",
                    {},
                    other["name"],
                    "project",
                    project_props(other),
                )
            )
        # 5d. DOCUMENTED_BY: a design document this project's README names and
        #     that lives OUTSIDE the tree (in-tree docs are CONTAINS).
        readme = root / "README.md"
        if readme.is_file():
            text = readme.read_text(encoding="utf-8")
            seen = set()
            # Only a WIKI URL counts here. A bare "PLAN.md" in another project's
            # README means that project's own PLAN, and resolving it against this
            # corpus invents a relationship the text never stated.
            for match in WIKI_RE.finditer(text):
                key = known_key(match.group(1))
                if key is None or key in seen:
                    continue
                group = by_key[key]
                doc = next((d for d in group if d["lang"] == "en"), group[0])
                try:
                    inside = Path(doc["path"]).expanduser().resolve().is_relative_to(root_res)
                except OSError:
                    inside = False
                if inside:
                    continue
                seen.add(key)
                facts.append(
                    tuple_(
                        project["name"],
                        "project",
                        props,
                        "DOCUMENTED_BY",
                        {},
                        doc["title"],
                        "document",
                        doc_props(doc),
                    )
                )
    # 6. Claims: one assertion per fact, attached to the section it names. A set
    #    is keyed by the DOCUMENT's key, but the sentences are one language's, so
    #    they hang on that rendition -- the twin carries the same document, not
    #    the same text. A claim with no home section is an ERROR, not a warning:
    #    a floating triple is exactly what this pipeline exists to prevent.
    for path in claims:
        payload = json.loads(Path(path).read_text(encoding="utf-8"))
        for key, entry in sorted((payload.get("documents") or {}).items()):
            group = by_key.get(key)
            if not group:
                raise SystemExit(f"{path}: unknown document `{key}`")
            doc = next((d for d in group if d["lang"] == "en"), group[0])
            slug = doc["slug"]
            headings = {text for text, _, _, _ in doc["sections"]}
            claim_rows = entry.get("claims", [])
            for index, claim in enumerate(claim_rows):
                text = claim["text"]
                heading = claim["section"]
                if heading not in headings:
                    raise SystemExit(
                        f"{path}: claim {index} names section `{heading}`, which "
                        f"`{key}` does not have (headings: {sorted(headings)})"
                    )
                facts.append(
                    tuple_(
                        f"{heading} ({slug})",
                        "section",
                        {"doc": slug, "key": key, "lang": doc["lang"]},
                        "STATES",
                        {"index": index},
                        text,
                        "claim",
                        {
                            "doc": slug,
                            "key": key,
                            "lang": doc["lang"],
                            "index": index,
                            "title": clause_title(text, 18),
                            "content": text,
                            "source": f"{doc['rel_path']} · {heading}",
                        },
                    )
                )
                for predicate in ("concerns", "rejects", "requires"):
                    for term in claim.get(predicate, []):
                        facts.append(
                            tuple_(
                                text,
                                "claim",
                                {"doc": slug, "key": key},
                                predicate.upper(),
                                {},
                                term,
                                "term",
                                {},
                            )
                        )
            print(f"  claims: {len(claim_rows)} from {key}")

    return facts, docs


def post(base: str, scope: int, batch):
    body = json.dumps(batch).encode()
    request = urllib.request.Request(
        f"{base}/memories/{scope}",
        data=body,
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=900) as response:
        return json.loads(response.read().decode())


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("paths", nargs="+")
    parser.add_argument("--base", default="http://127.0.0.1:7760")
    parser.add_argument("--scope", type=int, default=1)
    parser.add_argument("--batch", type=int, default=50)
    parser.add_argument(
        "--site",
        metavar="FILE",
        help="the editorial layer: per-tree default language, the projects, and "
        "one sentence per document (docs/kg/site.json)",
    )
    parser.add_argument(
        "--claims",
        action="append",
        default=[],
        metavar="FILE",
        help="the claims file: one JSON keyed by document, each set attached to "
        "the sections it names. Repeat the flag when sets live in several files.",
    )
    parser.add_argument(
        "--langs",
        default="en,zh",
        help="comma-separated languages to ingest (default both). One language "
        "alone halves the corpus but costs Chinese-query hit quality.",
    )
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    site = {}
    trees = []
    if args.site:
        site = json.loads(Path(args.site).read_text(encoding="utf-8"))
        trees = site.get("trees") or []

    paths = [Path(p) for p in args.paths]
    wanted = {lang.strip() for lang in args.langs.split(",") if lang.strip()}
    paths = [p for p in paths if identity(p, trees)[1] in wanted]
    missing = [str(p) for p in paths if not p.is_file()]
    if missing:
        print("missing files:", ", ".join(missing), file=sys.stderr)
        return 2

    facts, docs = build(paths, trees, site, args.claims)
    sections = sum(len(doc["sections"]) for doc in docs.values())
    by_predicate = {}
    for fact in facts:
        by_predicate[fact[3]] = by_predicate.get(fact[3], 0) + 1
    by_lang = {}
    for doc in docs.values():
        by_lang[doc["lang"]] = by_lang.get(doc["lang"], 0) + 1
    print(
        f"{len(docs)} documents ({by_lang}), {sections} sections -> {len(facts)} facts\n"
        + "  "
        + "  ".join(f"{k} {v}" for k, v in sorted(by_predicate.items()))
    )

    if args.dry_run:
        for fact in facts[:5]:
            print(" ", json.dumps(fact, ensure_ascii=False))
        print("  ...")
        return 0

    outcomes = {"linked": 0, "merged": 0, "refused": 0, "other": 0}
    refused = []
    for start in range(0, len(facts), args.batch):
        chunk = facts[start : start + args.batch]
        answers = post(args.base, args.scope, chunk)
        for fact, answer in zip(chunk, answers):
            outcome = answer.get("outcome", "other")
            outcomes[outcome] = outcomes.get(outcome, 0) + 1
            if outcome == "refused":
                refused.append((fact, answer.get("error")))
        print(f"  batch {start // args.batch + 1}: {len(chunk)} facts")

    print("outcomes:", json.dumps(outcomes))
    for fact, error in refused[:5]:
        print("  refused:", error, "|", json.dumps(fact, ensure_ascii=False)[:150])
    return 0 if not refused else 1


if __name__ == "__main__":
    sys.exit(main())