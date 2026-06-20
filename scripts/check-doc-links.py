#!/usr/bin/env python3
"""Validate relative Markdown links across the repo.

Scans every `.md` file (outside `target/`, `.git/`, vendored dirs),
extracts inline links of the form `](target)`, and checks that each
non-external target resolves to an existing file or directory.

- External links (`http(s)://`, `mailto:`, `tel:`) and pure anchors
  (`#section`) are skipped.
- A trailing `#anchor` is stripped before the existence check — we
  verify the target file exists, not that the anchor is valid.
- Fenced code blocks (``` / ~~~) are skipped so code samples that
  happen to contain `](...)` don't produce false positives.

Exits 1 (listing every broken link as `file:line -> target`) if any
relative link is broken; used as the `doc links` CI gate so a moved or
renamed doc can't silently break a reference.
"""

import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SKIP_DIRS = {".git", "target", "node_modules", ".venv", ".data"}
LINK_RE = re.compile(r"\[[^\]]*\]\(([^)]+)\)")


def md_files():
    for root, dirs, files in os.walk(REPO):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for f in files:
            if f.endswith(".md"):
                yield os.path.join(root, f)


def is_external(target):
    return (
        target.startswith(("http://", "https://", "mailto:", "tel:", "#"))
    )


def main():
    broken = []
    for path in md_files():
        with open(path, encoding="utf-8") as fh:
            in_fence = False
            for lineno, line in enumerate(fh, 1):
                stripped = line.lstrip()
                if stripped.startswith("```") or stripped.startswith("~~~"):
                    in_fence = not in_fence
                    continue
                if in_fence:
                    continue
                for match in LINK_RE.finditer(line):
                    target = match.group(1).strip()
                    if not target:
                        continue
                    # Drop an optional link title:  (path "Title")
                    target = target.split()[0]
                    if is_external(target):
                        continue
                    file_part = target.split("#", 1)[0]
                    if not file_part:
                        continue  # pure anchor
                    resolved = os.path.normpath(
                        os.path.join(os.path.dirname(path), file_part)
                    )
                    if not os.path.exists(resolved):
                        broken.append(
                            (os.path.relpath(path, REPO), lineno, target)
                        )

    if broken:
        print(f"Broken Markdown links ({len(broken)}):", file=sys.stderr)
        for f, ln, t in broken:
            print(f"  {f}:{ln}  ->  {t}", file=sys.stderr)
        return 1
    print("All Markdown links resolve.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
