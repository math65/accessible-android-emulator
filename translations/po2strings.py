#!/usr/bin/env python3
"""Writes a gettext PO catalog as an Apple Localizable.strings file.

    translations/po2strings.py translations/fr.po path/to/fr.lproj/Localizable.strings

SwiftUI translates the labels written in the Mac app's views from this file.
Entries without a translation are left out, so they stay in English.
"""

import itertools
import re
import sys

# A part filled in: {name}, {} or {0}, as aae_core::l10n reads them.
SLOT = re.compile(r"(?<!\{)\{([A-Za-z0-9_]*)(?::[^}]*)?\}(?!\})")


def unquote(text):
    inner = text.strip()
    if inner.startswith('"') and inner.endswith('"'):
        inner = inner[1:-1]
    out = []
    i = 0
    while i < len(inner):
        c = inner[i]
        if c == "\\" and i + 1 < len(inner):
            nxt = inner[i + 1]
            out.append({"n": "\n", "r": "\r", "t": "\t"}.get(nxt, nxt))
            i += 2
            continue
        out.append(c)
        i += 1
    return "".join(out)


def entries(po):
    """(msgid, msgstr) of each entry; with a msgctxt, the msgid is
    "context\x04msgid", as gettext writes it."""
    msgid = msgstr = context = None
    field = None

    def key():
        return msgid if context is None else f"{context}\x04{msgid}"

    for raw in po.splitlines():
        line = raw.strip()
        if not line:
            if msgid is not None:
                yield key(), msgstr or ""
            msgid = msgstr = field = context = None
            continue
        if line.startswith("#"):
            continue
        if line.startswith("msgctxt "):
            if msgid is not None:
                yield key(), msgstr or ""
                msgid = msgstr = None
            context, field = unquote(line[8:]), "ctxt"
        elif line.startswith("msgid "):
            if msgid is not None:
                yield key(), msgstr or ""
                context = None
            msgid, msgstr, field = unquote(line[6:]), None, "id"
        elif line.startswith("msgstr "):
            msgstr, field = unquote(line[7:]), "str"
        elif line.startswith('"'):
            if field == "id":
                msgid += unquote(line)
            elif field == "str":
                msgstr += unquote(line)
            elif field == "ctxt":
                context += unquote(line)
    if msgid is not None:
        yield key(), msgstr or ""


def slot_keys(text):
    """Each slot's key, as aae_core::l10n numbers them: its name, or for
    unnamed ones, their position among the unnamed ({0} names it too)."""
    keys = []
    unnamed = 0
    for m in SLOT.finditer(text):
        name = m.group(1)
        if name and not name.isdigit():
            keys.append(name)
        elif name.isdigit():
            keys.append("#" + name)
        else:
            keys.append("#" + str(unnamed))
            unnamed += 1
    return keys


def swiftui_keys(english, translated):
    """The keys SwiftUI looks up for a label with parts, such as
    Text("Apps on \\(name)"): "Apps on %@" for text, "%lld" for a number,
    with the translation's parts by position."""
    english_keys = slot_keys(english)
    if not english_keys:
        return [(english, translated)]
    if len(english_keys) > 3:
        return []
    position = {}
    for i, key in enumerate(english_keys):
        position.setdefault(key, i + 1)
    translated_keys = slot_keys(translated)
    if any(key not in position for key in translated_keys):
        return []
    keys = []
    for kinds in itertools.product(["@", "lld"], repeat=len(english_keys)):
        kind_of = {}
        for key, kind in zip(english_keys, kinds):
            kind_of.setdefault(key, kind)
        # In a format, a percent sign of the text itself is written %%.
        parts = iter(english_keys)
        key = SLOT.sub(lambda m: "%" + kind_of[next(parts)], english.replace("%", "%%"))
        parts = iter(translated_keys)

        def place(m):
            k = next(parts)
            return f"%{position[k]}${kind_of[k]}"

        keys.append((key, SLOT.sub(place, translated.replace("%", "%%"))))
    return keys


def quote(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n") + '"'


def main():
    source, target = sys.argv[1], sys.argv[2]
    with open(source, encoding="utf-8") as f:
        po = f.read()
    lines = []
    seen = set()
    for english, translated in entries(po):
        # SwiftUI has no contexts; those entries are for tr(_:context:).
        if english and translated and "\x04" not in english:
            for key, value in swiftui_keys(english, translated):
                if key not in seen:
                    seen.add(key)
                    lines.append(f"{quote(key)} = {quote(value)};")
    with open(target, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
