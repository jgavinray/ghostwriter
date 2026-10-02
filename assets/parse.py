#!/usr/bin/env python3
"""Extract the ASD-STE100 unapproved-vocabulary dictionary.

Reads `pdftotext -layout` output of the ASD-STE100 Issue 8 specification
(part 2, the controlled dictionary) and writes the shipped TSV:

    word<TAB>part-of-speech<TAB>APPROVED REPLACEMENT

with `#` comment lines first. Only entries whose approved-meaning column is
pure uppercase-approved vocabulary (plus small connective words) are kept, so
approved headwords and example-column bleed are dropped.

Provenance of the shipped `ste100-unapproved.tsv`: extracted 2026-10-02 from
the free ASD copy of Issue 8 via

    pdftotext -layout asd-ste100-issue8.pdf ste100.txt
    python3 assets/parse.py ste100.txt assets/ste100-unapproved.tsv

The dictionary is word-level replacement data (word, part of speech, approved
replacement); the manual's prose is not reproduced in this repository.

Copyright note: individual words and part-of-speech tags are not protected
expression; only ASD's manual text is copyrighted, and none of it is stored.
"""

import re
import sys

ENTRY_RE = re.compile(
    r"^([a-z][A-Za-z0-9''\-]*) \((n|v|adj|adv|prep|conj|aux|det|interj|modal)\)\s\s+(.*)$"
)
POS = r"(n|v|adj|adv|prep|conj|aux|det|interj|modal)"
SKIP = re.compile(
    r"^\s*(Issue 8|Page \d+-|2021-04-30|ASD.?STE100|Word\s*$|Word\s+|"
    r"Approved meaning|\(part of speech\)|Blank Page|Not approved example|APPROVED EXAMPLE)"
)
HDR = re.compile(r"APPROVED EXAMPLE\s+Not approved example")
STOP = {
    "or", "see", "not", "the", "a", "an", "to", "of", "be", "do",
    "if", "as", "no", "is", "are", "into", "away", "off", "on",
}


def extract(lines):
    """Yield {word, pos, repl} dicts from pdftotext -layout lines."""
    start = next(i for i, l in enumerate(lines) if re.match(r"^accuracy \(n\)", l))
    out = []
    cur = None
    last_hdr_x = None

    def flush():
        nonlocal cur
        if cur is None:
            return
        r = " ".join(" ".join(cur["repl"]).split())
        r = re.sub(r"\s*\(" + POS + r"\)", "", r)
        # technical-verb / technical-noun annotation tags
        r = re.sub(r"\s*\((TN|TV)\)", "", r)
        r = re.sub(r"\s*\[(TN|TV)\]", "", r)
        # the manual's fill-in-the-blank ellipsis: keep the leading form
        r = re.split(r"…|\.\.\.", r)[0]
        r = " ".join(r.split()).strip().rstrip(".").strip()
        words = re.sub(r"[^\w\s/-]", "", r).split()
        if (
            words
            and all(
                (w.isupper() and len(w) > 1) or w.lower() in STOP for w in words
            )
            and len(words) <= 6
        ):
            out.append({"word": cur["word"], "pos": cur["pos"], "repl": r})
        cur = None

    for l in lines[start:]:
        if HDR.search(l):
            last_hdr_x = l.index("APPROVED EXAMPLE")
            continue
        m = ENTRY_RE.match(l)
        if m and not l.startswith(" "):
            flush()
            rest_x = m.start(3)
            repl = l[rest_x:last_hdr_x] if last_hdr_x and len(l) > rest_x else ""
            cur = {"word": m.group(1), "pos": m.group(2), "repl": [repl]}
        elif cur is not None:
            if SKIP.match(l) or not l.strip():
                continue
            if l[:1] != " ":
                flush()
                continue
            repl = l[:last_hdr_x] if last_hdr_x else l
            if repl.strip():
                cur["repl"].append(repl)
    flush()
    return out


def main(src, dst):
    lines = open(src).read().split("\n")
    entries = extract(lines)
    seen = set()
    with open(dst, "w") as f:
        f.write(
            "# ASD-STE100 (Simplified Technical English) Issue 8, part 2 dictionary.\n"
            "# Unapproved word -> approved replacement; columns:\n"
            "# word<TAB>part-of-speech<TAB>approved replacement(s), space-joined.\n"
        )
        for e in sorted(entries, key=lambda e: (e["word"], e["pos"])):
            key = (e["word"], e["pos"])
            if key in seen:
                continue
            seen.add(key)
            f.write(f"{e['word']}\t{e['pos']}\t{e['repl']}\n")
    print(f"{len(seen)} unapproved->replacement entries -> {dst}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit("usage: parse.py <pdftotext-layout.txt> <out.tsv>")
    main(sys.argv[1], sys.argv[2])
