#!/usr/bin/env python3
"""Turn a docs tree into fact tuples for k10r — the knowledge pipeline's pilot.

Deliberately MECHANICAL (no model in the loop): every markdown file becomes a
node, every heading a node, and only the edges the text proves are emitted —
`SECTION_OF` (a heading's home), `CITES` (one document naming another by id or
path), `MENTIONS_CRATE` (a document naming a crate). Atomic claims inside the
prose are NOT extracted here: that pass is a model's job, and this skeleton is
what makes its output checkable afterwards — every claim it produces gets a
section to live in, so a reader can always walk back to the paragraph.

Identity rule (§ the ruling this pilot tests): a node's VALUE is the readable
label, because retrieval embeds the value — a slug retrieves nothing — and the
stable identity rides in props (`slug`, `path`, `line`). A heading's value is its
text, so "what does the backup section say" is a search that can actually land.

Usage:
  scripts/kg_convert.py --base http://127.0.0.1:7760 --scope 1 \
      docs/PLAN.md docs/MODELING.md docs/SEAM.md docs/PROMPTS.md docs/adr/[0-9]*.md
  scripts/kg_convert.py ... --twins zh     # ingest the Chinese twins instead
  scripts/kg_convert.py ... --dry-run      # print the tuples, send nothing

A document has TWO texts (`.md` and `.zh-CN.md`) and one identity. Ingesting both
puts the same document in the graph twice, which doubles every recall answer for
no information — so exactly one side is taken, English by default. Retrieval does
not care which language was ingested: the embedder answers a Chinese query over
English nodes at 0.62 (measured on this corpus).

Exit code is non-zero if the service refused anything.
"""

import argparse
import json
import re
import sys
import urllib.request
from pathlib import Path

ADR_RE = re.compile(r"\bADR[- ]?(\d{4})\b")
PATH_RE = re.compile(r"(?:docs/)?([A-Za-z0-9_.-]+\.md)")
HEADING_RE = re.compile(r"^(#{1,6})\s+(.*)$")
CRATE_RE = re.compile(r"\bcrates/([a-z0-9-]+)\b")


def slug_of(path: Path) -> str:
    return path.stem


def kind_of(path: Path) -> str:
    return "adr" if re.match(r"^\d{4}-", path.stem) else "doc"


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
            stem = Path(match.group(1)).stem
            cites.add(("doc", stem))
        for match in CRATE_RE.finditer(line):
            crates.add(match.group(1))
    title = title or path.stem
    return title, sections, cites, crates


def build(paths, known_slugs):
    """Every tuple this docs tree proves, in ingest order: nodes, then edges."""
    docs = {}
    for path in paths:
        title, sections, cites, crates = read_doc(path)
        docs[slug_of(path)] = {
            "path": path,
            "title": title,
            "kind": kind_of(path),
            "sections": sections,
            "cites": cites,
            "crates": crates,
        }

    facts = []
    # 1. Sections, each claimed by its document. A document's node comes into
    #    being here: it is the tail of every one of its sections, so a file with
    #    no headings would produce no node at all -- the report shows that
    #    rather than inventing a lonely document node nothing points at.
    for slug, doc in docs.items():
        for index, (text, level, line) in enumerate(doc["sections"]):
            # A heading is not an identity: "Context" appears in ten ADRs, and a
            # node every one of them points at would make each query land on the
            # same word. The section's value carries its document, which keeps
            # the name readable and the node addressable.
            facts.append(
                tuple_(
                    f"{text} ({slug})",
                    "section",
                    {"doc": slug, "line": line, "level": level, "index": index},
                    "SECTION_OF",
                    {},
                    doc["title"],
                    doc["kind"],
                    {"slug": slug},
                )
            )
    # 2. Citations between documents: only ids this tree actually holds, so a
    #    dangling ADR number becomes a report line, not a ghost node.
    for slug, doc in docs.items():
        for kind, target in sorted(doc["cites"]):
            if target == slug or target not in known_slugs:
                continue
            other = docs[target]
            facts.append(
                tuple_(
                    doc["title"],
                    doc["kind"],
                    {"slug": slug},
                    "CITES",
                    {"by": kind},
                    other["title"],
                    other["kind"],
                    {"slug": target},
                )
            )
    # 3. The two renditions of one document. The pair is ONE document, so the
    #    graph says so instead of leaving two strangers with the same content.
    for slug, doc in docs.items():
        if slug.endswith(".zh-CN"):
            continue
        twin = docs.get(slug + ".zh-CN")
        if not twin:
            continue
        facts.append(
            tuple_(
                doc["title"],
                doc["kind"],
                {"slug": slug, "lang": "en"},
                "TWIN_OF",
                {},
                twin["title"],
                twin["kind"],
                {"slug": slug + ".zh-CN", "lang": "zh"},
            )
        )
    # 4. Crates a document names.
    for slug, doc in docs.items():
        for crate in sorted(doc["crates"]):
            facts.append(
                tuple_(
                    doc["title"],
                    doc["kind"],
                    {"slug": slug},
                    "MENTIONS_CRATE",
                    {},
                    crate,
                    "crate",
                    {},
                )
            )
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
        "--twins",
        choices=["both", "none", "zh"],
        default="both",
        help="which halves of a bilingual pair to ingest: both (default, joined by "
        "TWIN_OF), the English text only, or the .zh-CN twin only.",
    )
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    paths = [Path(p) for p in args.paths]
    if args.twins == "none":
        paths = [p for p in paths if not p.name.endswith(".zh-CN.md")]
    elif args.twins == "zh":
        paths = [p for p in paths if p.name.endswith(".zh-CN.md")]
    missing = [str(p) for p in paths if not p.is_file()]
    if missing:
        print("missing files:", ", ".join(missing), file=sys.stderr)
        return 2

    known = {slug_of(p) for p in paths}
    facts, docs = build(paths, known)
    nodes = {slug: doc["title"] for slug, doc in docs.items()}
    sections = sum(len(doc["sections"]) for doc in docs.values())
    print(
        f"{len(docs)} documents, {sections} sections -> {len(facts)} facts "
        f"({sum(1 for f in facts if f[3] == 'CITES')} citations, "
        f"{sum(1 for f in facts if f[3] == 'MENTIONS_CRATE')} crate mentions)"
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