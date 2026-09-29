#!/usr/bin/env python3
"""
CLI helper for Prosodic rhyming.
This is invoked as a separate process by the Rust CLI (via IPC/stdout)
so that the Rust codebase remains strictly separate and does not link with or inherit
GPL/external constraints.
"""
import sys
import json
import sqlite3
import os

try:
    import prosodic
except ImportError:
    sys.stderr.write("prosodic is not installed in the virtual environment.\n")
    if "--get-rime" in sys.argv:
        print(json.dumps(None))
    else:
        print(json.dumps([]))
    sys.exit(0)

VOWELS = {"a", "e", "iː", "j", "o", "uː", "w", "æ", "ɑ", "ɔ", "ɔː", "ə", "ɛ", "ɛː", "ɪ", "ʊ", "ʌ"}


def parse_syll_rime(syll_ipa: str) -> str:
    phones = prosodic.words.syllables._parse_ipa_cached(syll_ipa)
    for i, ph in enumerate(phones):
        if ph in VOWELS:
            return "".join(phones[i:])
    return "".join(phones)


def word_rime_from_ipa(ipa: str):
    sylls = ipa.split(".")
    num_sylls = len(sylls)
    if not sylls:
        return "", 0
    last_syll = sylls[-1]
    last_stressed = ("'" in last_syll) or ("`" in last_syll)

    if not last_stressed and num_sylls > 1:
        second_to_last = sylls[-2]
        sec_stressed = ("'" in second_to_last) or ("`" in second_to_last)
        if sec_stressed:
            r1 = parse_syll_rime(second_to_last)
            r2 = "".join(prosodic.words.syllables._parse_ipa_cached(last_syll))
            return r1 + r2, num_sylls

    return parse_syll_rime(last_syll), num_sylls


def split_rime(rime: str) -> tuple[str, str]:
    """Split a rime string into (nucleus, coda)."""
    phones = prosodic.words.syllables._parse_ipa_cached(rime)
    i = 0
    while i < len(phones) and phones[i] in VOWELS:
        i += 1
    return "".join(phones[:i]), "".join(phones[i:])


def get_default_db_path() -> str:
    return os.path.expanduser("~/.local/share/good-dict/webster1913.db")


def build_index(db_path: str = None) -> int:
    """Pre-generate the word_rimes table in SQLite from Prosodic's dictionary."""
    if not db_path:
        db_path = get_default_db_path()

    os.makedirs(os.path.dirname(db_path), exist_ok=True)
    conn = sqlite3.connect(db_path)
    cur = conn.cursor()

    cur.execute("""
    CREATE TABLE IF NOT EXISTS word_rimes (
        word TEXT PRIMARY KEY,
        rime TEXT NOT NULL,
        nucleus TEXT NOT NULL,
        coda TEXT NOT NULL,
        num_sylls INTEGER NOT NULL
    )
    """)
    cur.execute("CREATE INDEX IF NOT EXISTS idx_word_rimes_rime ON word_rimes(rime, num_sylls)")
    cur.execute("CREATE INDEX IF NOT EXISTS idx_word_rimes_coda ON word_rimes(coda, num_sylls)")
    cur.execute("CREATE INDEX IF NOT EXISTS idx_word_rimes_nuc ON word_rimes(nucleus, num_sylls)")

    lang = prosodic.Language("en")
    tsv_path = lang.path_token2ipa

    rows = []
    seen = set()
    with open(tsv_path, encoding="utf-8") as f:
        for line in f:
            parts = line.strip().split("\t")
            if len(parts) == 2:
                w = parts[0].lower()
                if not w.isalpha() or w in seen:
                    continue
                seen.add(w)
                ipa = parts[1]
                rime, num_sylls = word_rime_from_ipa(ipa)
                if rime:
                    nuc, coda = split_rime(rime)
                    rows.append((w, rime, nuc, coda, num_sylls))

    cur.executemany("INSERT OR REPLACE INTO word_rimes VALUES (?, ?, ?, ?, ?)", rows)
    conn.commit()
    count = len(rows)
    conn.close()
    return count


def get_word_rime(word_str: str, db_path: str = None):
    """Get the rime, nucleus, coda, and syllable count for a single word, caching in DB if available."""
    word_lower = word_str.lower().strip()
    if not db_path:
        db_path = get_default_db_path()

    # Check DB first
    if os.path.exists(db_path):
        try:
            conn = sqlite3.connect(db_path)
            cur = conn.cursor()
            cur.execute("SELECT rime, nucleus, coda, num_sylls FROM word_rimes WHERE word = ?", (word_lower,))
            row = cur.fetchone()
            conn.close()
            if row:
                return {
                    "word": word_lower,
                    "rime": row[0],
                    "nucleus": row[1],
                    "coda": row[2],
                    "num_sylls": row[3],
                }
        except Exception:
            pass

    # Fall back to computing via Prosodic
    pw = prosodic.Word(word_lower)
    if not pw.children or not pw.children[0].rime:
        return None
    wf = pw.children[0]
    rime = wf.rime.txt
    num_sylls = wf.num_sylls
    nuc, coda = split_rime(rime)

    # Save to DB cache if DB exists
    if os.path.exists(db_path) and rime:
        try:
            conn = sqlite3.connect(db_path)
            cur = conn.cursor()
            cur.execute(
                "INSERT OR REPLACE INTO word_rimes (word, rime, nucleus, coda, num_sylls) VALUES (?, ?, ?, ?, ?)",
                (word_lower, rime, nuc, coda, num_sylls),
            )
            conn.commit()
            conn.close()
        except Exception:
            pass

    return {
        "word": word_lower,
        "rime": rime,
        "nucleus": nuc,
        "coda": coda,
        "num_sylls": num_sylls,
    }


def get_similar_codas(coda: str) -> list[str]:
    """Phonetically related codas for assonance matching."""
    res = set()
    substitutions = [
        ("t", "d"), ("d", "t"),
        ("s", "z"), ("z", "s"),
        ("p", "b"), ("b", "p"),
        ("k", "ɡ"), ("ɡ", "k"),
        ("f", "v"), ("v", "f"),
        ("ʃ", "ʧ"), ("ʧ", "ʃ"),
        ("ʃ", "s"), ("s", "ʃ"),
        ("ʃ", "st"), ("ʃ", "sp"),
        ("nʧən", "nʃən"), ("nʃən", "nʧən"),
        ("ʧən", "ʃən"), ("ʃən", "ʧən"),
        ("m", "n"), ("n", "m"), ("n", "ŋ"), ("ŋ", "n"),
        ("m", "nd"), ("m", "nt"),
        ("rt", "rd"), ("rd", "rt"), ("rt", "rn"), ("rd", "rn"),
        ("st", "s"), ("s", "st"),
        ("nd", "nt"), ("nt", "nd"), ("nd", "n"), ("nt", "n"),
        ("ld", "lt"), ("lt", "ld"), ("ld", "l"),
        ("mp", "m"), ("mb", "m"),
        ("ʤɪk", "tɪk"), ("ʤɪk", "sɪk"), ("ʤɪk", "k"),
    ]
    for src, dst in substitutions:
        if coda.endswith(src):
            res.add(coda[:-len(src)] + dst)
        if src in coda:
            res.add(coda.replace(src, dst))

    if coda.endswith("s") or coda.endswith("z") or coda.endswith("d") or coda.endswith("t"):
        res.add(coda[:-1])

    res.discard(coda)
    res.discard("")
    return sorted(res)


def main():
    args = sys.argv[1:]
    if not args:
        print(json.dumps([]))
        return

    if args[0] == "--build-index":
        db_path = args[1] if len(args) > 1 else None
        count = build_index(db_path)
        print(json.dumps({"status": "ok", "indexed": count}))
        return

    if args[0] == "--get-rime":
        if len(args) < 2:
            print(json.dumps(None))
            return
        word = args[1]
        db_path = args[2] if len(args) > 2 else None
        info = get_word_rime(word, db_path)
        print(json.dumps(info))
        return

    # Default legacy / fallback: word [limit]
    word = args[0].strip()
    limit = 12
    if len(args) > 1:
        try:
            limit = int(args[1])
        except ValueError:
            pass

    db_path = get_default_db_path()
    info = get_word_rime(word, db_path)
    if not info:
        print(json.dumps([]))
        return

    rime = info["rime"]
    num_sylls = info["num_sylls"]

    # Query matching words from SQLite
    try:
        conn = sqlite3.connect(db_path)
        cur = conn.cursor()
        cur.execute("""
            SELECT DISTINCT r.word
            FROM word_rimes r
            JOIN entries e ON e.word = r.word
            WHERE r.rime = ? AND r.word != ?
            ORDER BY ABS(r.num_sylls - ?), length(r.word), r.word
            LIMIT ?
        """, (rime, word.lower(), num_sylls, limit))
        results = [r[0] for r in cur.fetchall()]
        if len(results) < limit:
            cur.execute("""
                SELECT word
                FROM word_rimes
                WHERE rime = ? AND word != ?
                ORDER BY ABS(num_sylls - ?), length(word), word
                LIMIT ?
            """, (rime, word.lower(), num_sylls, limit))
            for r in cur.fetchall():
                if r[0] not in results:
                    results.append(r[0])
                if len(results) >= limit:
                    break
        conn.close()
        print(json.dumps(results[:limit]))
    except Exception:
        print(json.dumps([]))


if __name__ == "__main__":
    main()
