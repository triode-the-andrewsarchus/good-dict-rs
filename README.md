# good-dict

> *"Your destination is the dictionary."* — John McPhee

A Rust CLI for looking up words in **Webster's 1913 Revised Unabridged Dictionary** — the good dictionary, as championed by [James Somers](https://jsomers.net/blog/dictionary).

Unlike modern dictionaries that give you desiccated "husks of technocratic meaningese," Webster's 1913 was written by a person with a poet's ear. It gives you definitions that are themselves literature, example sentences from Milton and Shakespeare and Tennyson, and usage notes that illuminate the fine differences between words.

## Install

```bash
git clone https://github.com/youruser/good-dict-rs
cd good-dict-rs
cargo install --path .
```

On first run, the dictionary (~14 MB compressed, ~58 MB as XML) is automatically downloaded from [ibiblio.org](https://www.ibiblio.org/webster/) and indexed into a local SQLite database (~30 MB). This takes about 30–60 seconds and only happens once.

## Usage

```bash
# Look up a word
dict flash

# Look up multiple words
dict pathos fustian sport

# No flags needed — etymology, quotes, and synonyms are shown by default
# Suppress them individually:
dict --no-etymology flash
dict --no-quotes flash
dict --no-synonyms flash

# Re-download and re-index the dictionary
dict --refresh
```

## Example Output

```
flash  (flăsh)  v.i.

  [Cf. Sw. & Dan. flasa, to blaze...]

  1. To burst or break forth with a sudden and transient flood of flame and light.
  2. To convey as by a flash; as, to flash a message along the wires; to flash
     conviction on the mind.
  3. To appear suddenly and transiently.

  "A thought flashed through me, which I clothed in act."
          — Tennyson.

  Synonyms:  gleam | glitter | glisten
```

## The Dictionary

The data comes from the **GCIDE** (GNU Collaborative International Dictionary of English), a free, collaboratively maintained digital edition of Webster's 1913 Revised Unabridged Dictionary. It is hosted at [ibiblio.org/webster](https://www.ibiblio.org/webster/).

Webster's 1913 is in the public domain. The GCIDE is licensed under the GPL.

## Inspiration

- James Somers, ["You're probably using the wrong dictionary"](https://jsomers.net/blog/dictionary) (2014)
- John McPhee, ["Draft #4"](https://jsomers.net/mcphee-draft-no-4.pdf), *The New Yorker*
