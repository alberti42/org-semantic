---
name: org-semantic
description: Search a vault of org-mode notes by meaning or by exact words, using the org-semantic binary. Use when the user asks about something in their notes, refers to what they wrote or read before, or asks a question the answer to which is likely in their own vault rather than in general knowledge.
---

# Searching a vault of org-mode notes

`org-semantic` searches a directory of org files two ways. It answers with JSON,
so you never have to parse prose.

## Which search to use

**Semantic** — describe what you are looking for. Use this when the user's
question is about a topic, an idea, or something they cannot name exactly.

```sh
org-semantic search <vault> "why the atoms escape the trap" 8 --json
```

**Lexical** — exact words, with `AND`/`OR`/`NOT`, phrases and parentheses. Use
this for a name, an identifier, a flag, a filename, an error message, or anything
you would otherwise grep for.

```sh
org-semantic search <vault> '"optical tweezer" AND cooling' 8 --lexical --json
```

They are separate rankings and are never merged. If one returns nothing useful,
try the other before concluding the vault has no answer: a surname or a shell
flag is invisible to semantic search, and a question phrased in the user's own
words is often invisible to lexical search.

## Narrowing

Either mode accepts predicates, which are stripped from the text before it is
searched:

| predicate | meaning |
|---|---|
| `tag:x` | chunk carries org tag `x`; repeat to require several |
| `-tag:x` | chunk does not carry it |
| `dir:x` | note lives under directory `x`; repeat to widen |
| `todo:x` | nearest heading has TODO keyword `x` |
| `lang:x` | note is in language `x` — **lexical only** |

```sh
org-semantic search <vault> 'tag:Literature dir:"03 Reviews" quantum error correction' --json
```

## Reading the result

Each hit is an object:

```json
{ "score": 0.883, "z": 2.1, "file": "/abs/path/note.org", "line": 672,
  "id": "f73825a4-…", "title": "…", "section": "…", "tags": ["Literature"],
  "text": "the passage itself" }
```

- **`z` is the figure to trust for semantic hits** — how many standard
  deviations above the corpus's own background a hit sits. Raw `score` is mostly
  a constant offset and differs between models. Above ~2σ is a real match;
  below ~1σ, treat it as a miss and say so rather than reporting it.
- **For lexical hits `z` is null.** BM25 scores have no fixed scale, so read
  only the ordering, never the number.
- A hit is an **outline node**, not a file: `heading` is the full path to it and
  `line` is where it starts. In a vault that keeps three hundred meetings in one
  `meetings.org`, `title` is the file's and tells you nothing — `section` and
  `heading` are what locate the hit.
- **The address of a hit is `file` (or `path`) and `line`.** `line` is the line
  of the heading that owns the passage, so it names the section. `heading` is
  the outline path, for saying *where* a hit is in prose. `id` is an org-id only
  when the note carries one, and it is the *nearest enclosing* node's — in a
  large file every hit may report the same one, so never use it as an address.
- `text` is the passage itself, so you usually need not open the file. Read it
  when you need surrounding context.

## When every hit comes from the same file

Two numbers bound the list: the positional `k` caps how many **files** appear
(default 8), and `--per-file` caps how many passages any one of them may
contribute (default 3).

If the vault keeps notes in a few large files, the default of 3 is the binding
constraint and raising `k` does nothing — it counts files, and there are only a
few. Raise the other one instead:

```sh
org-semantic search <vault> "cryostat vibration" --per-file 25
```

A short result list whose hits all share one `file` is the sign that you are
looking at this cap rather than at the whole of what matched.

## Before searching

The vault must be indexed. Check with:

```sh
org-semantic models <vault>     # shows which indexes exist
```

If nothing is built, **ask the user before indexing** — a semantic index takes
minutes and downloads a model. A lexical index takes a second:

```sh
org-semantic index <vault> --lexical
```

After the user edits notes, re-running the same `index` command updates only what
changed. It is incremental; it does not redo the corpus.

## What not to do

- Do not `grep` the vault as a substitute. It misses everything phrased
  differently, which is the whole point of the semantic index.
- Do not reindex unprompted, and never pass `--full` without asking: it discards
  the existing index and re-embeds everything.
- Do not report a hit you have not read. `text` is right there.
- Do not merge the two rankings into one list. If you ran both, present them as
  what they are — two different questions asked of the same notes.
