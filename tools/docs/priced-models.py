#!/usr/bin/env python3
"""Writes the model tables of docs/guides/priced-models.md from the bundled price table.

The tables are the page's "## Models" section from its first "### " heading to the next "## " heading or the end of
the page. The text before them is the page's own and is never rewritten.

Every name and example the tables show fullmatches its row's pattern (case-insensitive), and the first row that
matches it is that row, as the server selects rows without a start date. The patterns are expanded with Python's
own regex parser (re._parser), so this needs Python 3.11 or newer.

Run from anywhere:
    python3 -I tools/docs/priced-models.py          rewrites the tables from crates/crucible-normalize/data/prices.json
    python3 -I tools/docs/priced-models.py --check  exits 1 when the page's tables differ from that rewrite

Both exit 1 when a name the tables would show does not select its own row, or when the page has no tables.
"""

import difflib
import itertools
import json
import re
import re._constants as C
import re._parser as P
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
PAGE = ROOT / "docs" / "guides" / "priced-models.md"
rows = json.loads((ROOT / "crates" / "crucible-normalize" / "data" / "prices.json").read_text(encoding="utf-8"))
OPEN = "\x00"


def fail(message):
    sys.stdout.flush()
    print(f"priced-models: {message}", file=sys.stderr)
    sys.exit(1)


def expand(items):
    out = [""]
    for op, av in items:
        if op is C.LITERAL:
            alts = [chr(av)]
        elif op is C.ANY:
            alts = ["."]
        elif op is C.SUBPATTERN:
            alts = expand(av[3])
        elif op is C.BRANCH:
            alts = [s for b in av[1] for s in expand(b)]
        elif op in (C.MAX_REPEAT, C.MIN_REPEAT):
            alts = ([""] + expand(av[2])) if av[:2] == (0, 1) else [OPEN]
        elif op is C.IN:
            alts = [OPEN]
        else:
            fail(f"unhandled {op}")
        out = [a + b for a, b in itertools.product(out, alts)]
    return out


RX = [re.compile(f"(?i)^(?:{r['match']})$") for r in rows]
PREFIX = re.compile(r"^(openai/|anthropic/|google/|googleai/|~?typesafe(-ai)?/)")
REGION = re.compile(r"^((eu|us|apac|au|jp|global|ca)\.)?anthropic\.")
failed = False


def check(name, i):
    global failed
    first = next((j for j, rx in enumerate(RX) if rx.match(name)), None)
    if first != i:
        print(f"CHECK FAILED {name!r}: row {rows[i]['name']}, first match {first}", file=sys.stderr)
        failed = True
    return name


# Display names the expansion cannot produce: open parts, and labels whose dot stands for a hyphen in real names.
SPECIAL = {
    "gpt-4-turbo-vision": ["gpt-4-vision-preview", "gpt-4-1106-vision-preview"],
    "ft:gpt-3.5-turbo-1106": ["ft:gpt-3.5-turbo-1106:my-org:custom_suffix:id"],
    "ft:gpt-3.5-turbo-0613": ["ft:gpt-3.5-turbo-0613:my-org:custom_suffix:id"],
    "ft:davinci-002": ["ft:davinci-002:my-org:custom_suffix:id"],
    "ft:babbage-002": ["ft:babbage-002:my-org:custom_suffix:id"],
    "jev": ["jev", "jev-latest", "jev-preview", "jev-1.13.0", "jev-1.13-20260917"],
    "claude-3.7-sonnet-20250219": ["claude-3-7-sonnet-20250219"],
}


def forms(i):
    row = rows[i]
    names = sorted(set(expand(P.parse(row["match"]))), key=lambda s: (len(s), s))
    bare = [n for n in names if not (PREFIX.match(n) or REGION.match(n) or "@" in n or n.endswith("-V1"))]
    shown = SPECIAL.get(row["name"]) or [n for n in bare if OPEN not in n] or names
    shown = [check(n, i) for n in shown]
    extra = {}
    pre = [n for n in names if PREFIX.match(n) and OPEN not in n]
    if pre:
        extra["prefix"] = sorted({PREFIX.match(n).group(0) for n in pre})
    reg = [n.replace("claude-3.7", "claude-3-7") for n in names
           if REGION.match(n) and OPEN not in n and n.startswith("us.")]
    if reg:
        extra["region"] = check(sorted(reg, key=lambda s: (not s.endswith(":0"), -len(s), s))[0], i)
    at = [n for n in names if "@" in n and not PREFIX.match(n)]
    if at:
        ex = sorted(at, key=lambda s: (OPEN in s, len(s), s))[0].replace(OPEN, "001")
        extra["at"] = check(ex, i)
    return shown, extra


def provider(name):
    n = re.sub(r"^(us|eu|global)\.(?=openai\.|cohere\.)", "", name)
    if re.match(r"^(openai\.|gpt|chatgpt|o\d|text-(ada|babbage|curie|davinci|embedding)|babbage|davinci|ft:)", n):
        return "openai"
    if n.startswith("claude"):
        return "anthropic"
    if re.match(r"^(gemini|chat-bison|codechat|text-bison|text-unicorn|textembedding|code-)", n):
        return "google"
    if n.startswith("cohere"):
        return "cohere"
    return "other"


def family(name):
    n = re.sub(r"^((us|eu|global)\.)?openai\.", "", name)
    rules = [
        (r"^gpt-(3\.5|35)", "GPT-3.5"), (r"^(chat)?gpt-4o", "GPT-4o"), (r"^gpt-4\.1", "GPT-4.1"),
        (r"^gpt-4\.5", "GPT-4.5"), (r"^gpt-4(-|$)", "GPT-4"), (r"^gpt-5(-|$)", "GPT-5"),
        (r"^gpt-(\d\.\d)", None), (r"^gpt-6(-|$)", "GPT-6"), (r"^o\d", "o1, o3 and o4"),
        (r"^gpt-audio", "GPT audio"), (r"^text-embedding", "Embeddings"),
        (r"^(text-(ada|babbage|curie|davinci)-|babbage-|davinci-)", "Completions"), (r"^ft:", "Fine-tuned"),
        (r"^claude-(instant-1|1\.|2\.)", "Claude 1 and 2"), (r"^claude-3-(opus|sonnet|haiku)", "Claude 3"),
        (r"^claude-3-5", "Claude 3.5"), (r"^claude-3-7", "Claude 3.7"),
        (r"^claude-(sonnet|opus)-4(-latest|-2025\d+)?$", "Claude 4"), (r"^claude-opus-4-1", "Claude 4.1"),
        (r"^claude-\w+-4-(\d)", None), (r"^claude-\w+-5$", "Claude 5"), (r"^claude-\w+-5-(\d)", None),
        (r"^gemini-(\d(\.\d)?)", None), (r"^gemini-", "Other Gemini names"),
        (r"^(chat-bison|codechat|text-bison|text-unicorn|textembedding|code-)", "Bison, Gecko and Unicorn"),
        (r"^cohere\.embed-v4", "Embed v4"), (r"^jev", "jev"),
    ]
    for pattern, label in rules:
        m = re.match(pattern, n)
        if m:
            if label:
                return label
            if n.startswith("gpt"):
                return f"GPT-{m.group(1)}"
            if n.startswith("claude"):
                return f"Claude {'4' if '-4-' in n else '5'}.{m.group(1)}"
            return f"Gemini {m.group(1)}"
    fail(f"no family for {name}")


CELLS = {
    "Fine-tuned": "`ft:gpt-3.5-turbo-1106:`, `ft:gpt-3.5-turbo-0613:`, `ft:davinci-002:` and `ft:babbage-002:`, "
                  "each followed by the organization, a name and an id, such as "
                  "`ft:gpt-3.5-turbo-1106:my-org:custom_suffix:id`",
    "jev": "`jev`, `jev-latest`, `jev-preview`, and `jev-` with a version, such as `jev-1.13.0` or "
           "`jev-1.13-20260917`",
}
ORDER = ["GPT-3.5", "GPT-4", "GPT-4o", "GPT-4.1", "GPT-4.5", "GPT-5", "GPT-5.1", "GPT-5.2", "GPT-5.3", "GPT-5.4",
         "GPT-5.5", "GPT-5.6", "GPT-6", "GPT-6.1", "o1, o3 and o4", "GPT audio", "Embeddings", "Completions",
         "Fine-tuned", "Claude 1 and 2", "Claude 3", "Claude 3.5", "Claude 3.7", "Claude 4", "Claude 4.1",
         "Claude 4.5", "Claude 4.6", "Claude 4.7", "Claude 4.8", "Claude 5", "Claude 5.1", "Claude 5.2", "Claude 5.5",
         "Gemini 1.0", "Gemini 1.5", "Gemini 2.0", "Gemini 2.5", "Gemini 3", "Gemini 3.1", "Gemini 3.5", "Gemini 3.6",
         "Gemini 3.7", "Gemini 3.8", "Gemini 4", "Other Gemini names", "Bison, Gecko and Unicorn", "Embed v4", "jev"]
TITLES = {"openai": "OpenAI", "anthropic": "Anthropic", "google": "Google", "cohere": "Cohere", "other": "Other"}


def render():
    """The tables, from the first provider heading to the blank line after the last table."""
    groups = {}
    for i in range(len(rows)):
        shown, extra = forms(i)
        key = (provider(shown[0]), family(shown[0]))
        groups.setdefault(key, []).append((i, shown, extra))

    out = []
    for prov in ("openai", "anthropic", "google", "cohere", "other"):
        fams = sorted(((k[1], v) for k, v in groups.items() if k[0] == prov), key=lambda f: ORDER.index(f[0]))
        if not fams:
            continue
        out.append(f"### {TITLES[prov]}\n\n")
        out.append("| Family | Model names | Also matches, for example |\n")
        out.append("|---|---|---|\n")
        for fam, members in fams:
            # Split a family by the set of other forms its rows accept, so each line's examples hold for every name.
            sets = {}
            for i, shown, extra in members:
                sig = tuple(sorted((k, tuple(v) if isinstance(v, list) else "x") for k, v in extra.items()))
                sets.setdefault(sig, []).append((i, shown, extra))
            for sig, part in sets.items():
                names = [n for _, shown, _ in part for n in shown]
                ex = []
                i0, shown0, extra0 = part[0]
                if "prefix" in extra0:
                    first = sorted(n for _, shown, _ in part for n in shown)[0]
                    owner = next(i for i, shown, _ in part if first in shown)
                    ex.append(check(extra0["prefix"][0] + first, owner) if "jev" not in first
                              else check("typesafe/jev", i0))
                if "region" in extra0:
                    ex.append(extra0["region"])
                if "at" in extra0:
                    ex.append(extra0["at"])
                names = sorted(set(names))
                cell = CELLS.get(fam) or ", ".join(f"`{n}`" for n in names)
                out.append(f"| {fam} | {cell} | {', '.join(f'`{e}`' for e in ex)} |\n")
        out.append("\n")
    for i, row in enumerate(rows):
        if row["name"] in ("ft:gpt-3.5-turbo-1106", "jev"):
            for n in re.findall(r"`([^`]+)`", " ".join(CELLS.values())):
                if RX[i].match(n):
                    check(n, i)
    if failed:
        fail("a name above does not select its own row of the price table")
    return "".join(out)


def split(page):
    """The page before its tables, the tables, and the page after them."""
    lines = page.splitlines(keepends=True)
    if "## Models\n" not in lines:
        fail("docs/guides/priced-models.md has no '## Models' heading")
    for i in range(lines.index("## Models\n") + 1, len(lines)):
        if lines[i].startswith("## "):
            break
        if lines[i].startswith("### "):
            end = next((j for j in range(i, len(lines)) if lines[j].startswith("## ")), len(lines))
            return "".join(lines[:i]), "".join(lines[i:end]), "".join(lines[end:])
    fail("the '## Models' section of docs/guides/priced-models.md has no '### ' table heading")


def main():
    if sys.argv[1:] not in ([], ["--check"]):
        fail("usage: python3 -I tools/docs/priced-models.py [--check]")
    tables = render()
    before, committed, after = split(PAGE.read_text(encoding="utf-8"))
    # The tables end the page with one newline, or keep one blank line before the next section.
    wanted = tables.rstrip("\n") + ("\n\n" if after else "\n")
    if committed == wanted:
        print("priced-models: the tables of docs/guides/priced-models.md match the price table")
    elif sys.argv[1:] == ["--check"]:
        sys.stdout.writelines(difflib.unified_diff(
            committed.splitlines(keepends=True), wanted.splitlines(keepends=True),
            "docs/guides/priced-models.md", "rewritten from crates/crucible-normalize/data/prices.json"))
        fail("the tables of docs/guides/priced-models.md differ from the price table; "
             "rewrite them with: python3 -I tools/docs/priced-models.py")
    else:
        # LF on every platform, as the repository stores text.
        with open(PAGE, "w", encoding="utf-8", newline="\n") as page:
            page.write(before + wanted + after)
        print("priced-models: rewrote the tables of docs/guides/priced-models.md")


if __name__ == "__main__":
    main()
