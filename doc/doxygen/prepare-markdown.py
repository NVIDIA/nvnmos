#!/usr/bin/env python3

# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Stage Markdown as Doxygen ``.dox`` pages with stable ids.

Every ``doc/user/*.md`` file becomes a ``\\page`` whose id is the file stem
(``concepts.md`` -> ``concepts.html``). ``README.md`` becomes ``\\mainpage``
with its level-one heading as the Doxygen title.

Markdown files are staged as ``.dox`` comment files so Doxygen does not also
emit path-derived ``md_*`` pages. Relative links to user-guide Markdown are
rewritten to ``@ref`` so generated HTML uses the stable page ids.

After the H1 is consumed by the page command, remaining ``#``-style headings
are promoted one level so Doxygen treats them as sections (not orphan
subsections).

Section anchors use GitHub-style heading ids (lowercase; spaces and punctuation
become ``-``) so ``#container-images`` links work the same in GitHub and in the
staged Doxygen main page.
"""

import re
import shutil
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
STAGING = ROOT / "src" / ".doxygen-markdown"
USER_DOCS = ROOT / "doc" / "user"
README = Path("README.md")

HEADING_RE = re.compile(r"^(#{1,6})\s+(.+?)\s*$")
PROMOTE_RE = re.compile(r"^(#{2,6})(\s+.*)$")
EXPLICIT_ID_RE = re.compile(r"\{#[^}]+\}\s*$")
TRAILING_HASHES_RE = re.compile(r"\s+#+\s*$")
NON_SLUG_RE = re.compile(r"[^\w\s-]", re.UNICODE)
WHITESPACE_RE = re.compile(r"[-\s]+")
USER_PAGE_LINK_RE = re.compile(
    r"\[([^\]]+)\]\((?:doc/user/)?([A-Za-z0-9_-]+)\.md(#[A-Za-z0-9_-]+)?\)"
)


def replace_page_heading(text: str, page_command: str) -> str:
    """Replace the first ``#``-style H1 with a Doxygen page command and TOC.

    ``page_command`` is either ``mainpage`` or ``page <id>``.
    """
    lines = text.splitlines(keepends=True)

    for index, line in enumerate(lines):
        if line.startswith("# "):
            content = line.rstrip("\r\n")
            ending = line[len(content) :] or "\n"
            lines[index] = (
                f"\\{page_command} {content[2:]}{ending}"
                f"{ending}\\tableofcontents{ending}"
            )
            return "".join(lines)

    raise RuntimeError(f"Markdown page has no level-one heading: {page_command}")


def promote_headings(text: str) -> str:
    """Promote remaining ``#``-style headings one level after the H1 became a page title."""
    lines = text.splitlines(keepends=True)

    for index, line in enumerate(lines):
        content = line.rstrip("\r\n")
        match = PROMOTE_RE.match(content)
        if match is None:
            continue
        hashes, rest = match.groups()
        ending = line[len(content) :]
        lines[index] = f"{'#' * (len(hashes) - 1)}{rest}{ending}"

    return "".join(lines)


def github_heading_id(title: str) -> str:
    """Match GitHub Flavoured Markdown heading anchors."""
    slug = title.strip().lower()
    slug = NON_SLUG_RE.sub("", slug)
    slug = WHITESPACE_RE.sub("-", slug)
    return slug.strip("-")


def add_github_section_ids(text: str) -> str:
    """Attach ``{#slug}`` to ``#``-style headings so Doxygen keeps GitHub-compatible ids."""
    lines = text.splitlines(keepends=True)
    seen: dict[str, int] = {}

    for index, line in enumerate(lines):
        content = line.rstrip("\r\n")
        ending = line[len(content) :]
        match = HEADING_RE.match(content)
        if match is None or EXPLICIT_ID_RE.search(content):
            continue

        hashes, rest = match.groups()
        title = TRAILING_HASHES_RE.sub("", rest).strip()
        base = github_heading_id(title)
        if not base:
            continue

        count = seen.get(base, 0)
        seen[base] = count + 1
        section_id = base if count == 0 else f"{base}-{count}"
        lines[index] = f"{hashes} {title} {{#{section_id}}}{ending}"

    return "".join(lines)


def rewrite_user_doc_links(text: str, page_ids: set[str]) -> str:
    """Rewrite relative user-guide ``.md`` links to Doxygen ``@ref`` targets."""

    def replace(match: re.Match[str]) -> str:
        label, stem, anchor = match.group(1), match.group(2), match.group(3)
        if stem not in page_ids:
            return match.group(0)
        ref = anchor[1:] if anchor else stem
        return f"[{label}](@ref {ref})"

    return USER_PAGE_LINK_RE.sub(replace, text)


def to_dox(text: str) -> str:
    """Wrap page text as a Doxygen comment with Javadoc-style `` *`` line prefixes.

    Without those prefixes, Doxygen strips a leading ``*`` from each line and
    corrupts Markdown bold markers such as ``**Linux**`` into ``Linux**``.
    """
    if "*/" in text:
        raise RuntimeError("documentation contains */ which would close the .dox comment")
    body = "\n".join(f" * {line}" if line else " *" for line in text.rstrip().splitlines())
    return f"/*!\n{body}\n */\n"


def stage(destination_name: str, text: str, page_command: str, page_ids: set[str]) -> None:
    text = replace_page_heading(text, page_command)
    text = promote_headings(text)
    text = add_github_section_ids(text)
    text = rewrite_user_doc_links(text, page_ids)
    text = to_dox(text)

    destination = STAGING / destination_name
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(text, encoding="utf-8")


def main() -> None:
    shutil.rmtree(STAGING, ignore_errors=True)

    user_sources = sorted(USER_DOCS.glob("*.md"))
    page_ids = {source.stem for source in user_sources}

    stage(
        "mainpage.dox",
        (ROOT / README).read_text(encoding="utf-8"),
        page_command="mainpage",
        page_ids=page_ids,
    )

    for source in user_sources:
        stage(
            f"{source.stem}.dox",
            source.read_text(encoding="utf-8"),
            page_command=f"page {source.stem}",
            page_ids=page_ids,
        )


if __name__ == "__main__":
    main()
