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


def read_doc(path: Path):
    """Title, sections and the reference facts of one markdown file."""
    lines = path.read_text(encoding="utf-8").splitlines()
    title = None
    sections = []
    cites = set()
    crates = set()
    for number, line in enumerate(lines, start=1):
        heading = HEADING_RE.match(line)
        if heading:
            level, text = len(heading.group(1)), heading.group(2).strip()
            if level == 1 and title is None:
                title = text
            elif level >= 2:
                sections.append((text, level, number))
        for match in ADR_RE.finditer(line):
            cites.add(("adr", match.group(1)))
        for match in PATH_RE.finditer(line):
            cites.add(("doc", match.group(1)))
        for match in CRATE_RE.finditer(line):
            crates.add(match.group(1))
    title = title or path.stem
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
        "path": doc["rel_path"],
    }
    for field in ("blurb", "blurb_en", "url"):
        if doc.get(field):
            props[field] = doc[field]
    return props


def project_props(project) -> dict:
    return {
        "key": project["key"],
        "path": project.get("path", ""),
        "repo": project.get("repo", ""),
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
        for index, (text, level, line) in enumerate(doc["sections"]):
            # A heading is not an identity: "Context" appears in ten ADRs, and a
            # node every one of them points at would make each query land on the
            # same word. The section's value carries its document, which keeps
            # the name readable and the node addressable.
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
            headings = {text for text, _, _ in doc["sections"]}
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
                        {"doc": slug, "key": key, "lang": doc["lang"], "index": index},
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
    with urllib.request.urlopen(request, timeout=120) as response:
        return json.loads(response.read().decode())


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("paths", nargs="+")
    parser.add_argument("--base", default="http://127.0.0.1:7760")
    parser.add_argument("--scope", type=int, default=1)
    parser.add_argument("--batch", type=int, default=200)
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