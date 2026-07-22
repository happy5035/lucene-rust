#!/usr/bin/env python3
"""
Extract Wikipedia XML dump to line-doc format.

Each output line = one Wikipedia article:
  <title>\t<body_text>

Body text has newlines/tabs replaced with spaces so each article is one line.
Articles with empty body are skipped.
Namespace (NS) entries, redirects, and disambiguation pages are included as-is
(their body text is short, but they exercise edge cases).

Usage:
  python3 wiki-extract.py <input.xml.bz2> [--max-docs N] > output.txt
"""

import bz2
import sys
import xml.etree.ElementTree as ET


def detect_ns(filepath: str) -> str:
    """Read the first 2048 bytes to find the XML namespace."""
    opener = bz2.open(filepath, "rb") if filepath.endswith(".bz2") else open(filepath, "rb")
    with opener as f:
        head = f.read(4096).decode("utf-8", errors="ignore")
    # Find xmlns="..." in the <mediawiki> tag
    import re
    m = re.search(r'xmlns="(http://www\.mediawiki\.org/xml/export-[^"]+)"', head)
    if m:
        return m.group(1)
    # Fallback to most common
    return "http://www.mediawiki.org/xml/export-0.11/"


def main():
    if len(sys.argv) < 2:
        print(f"usage: {sys.argv[0]} <input.xml.bz2> [--max-docs N]", file=sys.stderr)
        sys.exit(2)

    filepath = sys.argv[1]
    max_docs = None
    if len(sys.argv) >= 4 and sys.argv[2] == "--max-docs":
        max_docs = int(sys.argv[3])

    ns = detect_ns(filepath)
    print(f"# namespace: {ns}", file=sys.stderr)

    TAG_PAGE = f"{{{ns}}}page"
    TAG_TITLE = f"{{{ns}}}title"
    TAG_REVISION = f"{{{ns}}}revision"
    TAG_TEXT = f"{{{ns}}}text"
    TAG_NS_FIELD = f"{{{ns}}}ns"  # namespace field (0=article, 14=category, etc.)

    count = 0
    skipped_empty = 0
    skipped_ns = 0

    opener = bz2.open(filepath, "rb") if filepath.endswith(".bz2") else open(filepath, "rb")
    with opener as f:
        context = ET.iterparse(f, events=("end",))
        for event, elem in context:
            if elem.tag != TAG_PAGE:
                continue

            # Only include main namespace articles (ns=0)
            ns_el = elem.find(TAG_NS_FIELD)
            if ns_el is not None and ns_el.text and ns_el.text != "0":
                skipped_ns += 1
                elem.clear()
                continue

            title_el = elem.find(TAG_TITLE)
            title = title_el.text if title_el is not None and title_el.text else ""

            rev_el = elem.find(TAG_REVISION)
            body = ""
            if rev_el is not None:
                text_el = rev_el.find(TAG_TEXT)
                if text_el is not None and text_el.text:
                    body = text_el.text

            if not body.strip():
                skipped_empty += 1
                elem.clear()
                continue

            # Clean body text: collapse whitespace, replace newlines/tabs
            body_clean = " ".join(body.split())

            # Output: title \t body (one line per article)
            print(f"{title}\t{body_clean}")

            count += 1
            if count % 10000 == 0:
                print(f"# {count} docs extracted...", file=sys.stderr)

            if max_docs and count >= max_docs:
                break

            elem.clear()

    print(f"# Done: {count} docs, {skipped_empty} empty, {skipped_ns} non-main-ns skipped",
          file=sys.stderr)


if __name__ == "__main__":
    main()
