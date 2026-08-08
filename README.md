# org-semantic

Semantic search over a tree of org-mode notes. One static binary, no server, no
database, no Python.

```console
$ org-semantic index ~/notes
951 org files
6328 chunks · 6328 to embed · scanned in 2.3s
model loaded in 0.16s
  embedding 6328/6328 · 29 chunk/s · 6.6k tok/s · eta 0s
embedded 6328 chunks in 220.7s (29/s)
wrote ~/notes/.org-semantic (9.7 MB of vectors) in 223.0s total

$ org-semantic search ~/notes "why do the atoms heat up and get lost from the trap"

0.755  2025-06-06 Review - Probing topological matter and fermion dynamics
       03 Literature review/2025-06-06 Review - Probing topological matter.org:677
       id:f73825a4-c877-4f69-a7e0-4ae305314b8d
       · 0.755 L677   Observations: > Atom Loss: What causes it? > Trap-induced loss
               Optical tweezers are subject to power drifts and pointing instabilities…
       · 0.736 L682   Observations: > Atom Loss: What causes it? > Motion and gate timing
               During dynamical reconfiguration (AOD-based transport), atoms might spend…

0.733  2024-08-27 Heating rate in optical traps
       03 Literature review/2024-08-27 Heating rate in optical traps.org:9
       id:b4c9ac08-0dfd-4439-a599-329109fa0bc3
       · 0.733 L9     References:
               M. E. Gehm, K. M. O'hara, T. A. Savard and J. E. Thomas, Dynamics of…

[model load 120ms · query embed 7ms · search over 6328 vectors 1.4ms]
```

No word of that query appears in either note's title. That is the point.

## Why

Existing options either run a Python service — one popular org indexer pulls 129
packages including torch, CLIP and the Azure SDK — or are built for Markdown and
know nothing about org structure.

org-semantic is one 34 MB binary. ONNX Runtime is statically linked; the only
thing it ever downloads is the embedding model (129 MB, once, into your cache).

**It is deliberately org-only.** Covering every markup format converges on the
least common denominator — headings and paragraphs — which is where the
generic tools already are, and where a dedicated one will always be better at
its own format. Knowing it is org buys things a format-agnostic tool cannot
reach:

- Property drawers stay out of the embedded text, so `:ID:` and `:MODIFIED:`
  do not dilute a chunk's meaning.
- `#+title:` names the note, rather than guessing from the filename.
- Every chunk carries its enclosing `:ID:`, so an editor can jump to a hit
  through `org-id` instead of by file position — surviving renames and moves.
- Tags are parsed with org's inheritance rules and become search filters, as do
  TODO keywords and priorities.
- Heading breadcrumbs (`Note > Section > Subsection`) are prepended to each
  chunk before embedding, so a passage carries the context it sits under.

If you work in Markdown, [markdown-vdb](https://github.com/geckse/markdown-vdb)
reaches a very similar architecture for that format and you should use it
instead.

## Install

```sh
cargo install --git https://github.com/alberti42/org-semantic
```

Or build it:

```sh
git clone https://github.com/alberti42/org-semantic && cd org-semantic
cargo build --release          # target/release/org-semantic
```

Requires a Rust toolchain. Nothing else — no Python, no system ONNX Runtime, no
package manager. The BGE-small-en-v1.5 model downloads on first use to
`$XDG_CACHE_HOME/fastembed`.

## Use

```
org-semantic index   <dir> [--full|--rehash]  refresh the index (incremental by default)
org-semantic search  <dir> <query> [k]        semantic search, grouped per note
org-semantic keyword <dir> <query> [k]        lexical search, same predicates
org-semantic chunks  <dir> <path-substring>    show chunking decisions, no embedding
org-semantic tokens  <dir> [limit]             token-length distribution of the corpus
org-semantic bench   <dir> [n] [config]        embedding throughput
```

### Two modes, deliberately separate

`search` ranks by meaning; `keyword` ranks by words, with tantivy's query
language — phrases, boolean operators, field boosts. They are separate commands
rather than one fused ranking because a phrase or a boolean means nothing to an
embedding: fusing them would mix results that honoured your query with results
that could not.

The difference is not academic. Searching your notes for the surname `Gehm`:

```console
$ org-semantic keyword ~/notes Gehm
13.181  2024-08-27 Heating rate in optical traps      ← the note citing Gehm 1998

$ org-semantic search ~/notes Gehm
0.660   01 Deutsche Wörter 2024                        ← noise
```

A surname carries no meaning for an embedding model. Equally, `why do the atoms
heat up and get lost from the trap` finds the right passage semantically and
nothing at all lexically, since none of those words appear in it.

### Filters

A query may carry predicates, which narrow *which* chunks are searched before
anything is embedded:

```sh
org-semantic search ~/notes 'tag:Literature estimating eigenvalues on hardware'
org-semantic search ~/notes 'dir:"01 Daily notes" atom sorting in a tweezer array'
org-semantic search ~/notes '-tag:Deutschlernen -tag:Computer atom heating'
```

| predicate | meaning |
|---|---|
| `tag:x` | chunk carries tag `x`; repeating narrows (all must match) |
| `-tag:x` | chunk does not carry it |
| `dir:x` | note lives under directory `x`; repeating widens (any may match) |
| `todo:x` | nearest enclosing heading has TODO keyword `x` |

Tags follow org's own inheritance — `#+filetags:` plus every ancestor heading's
tags — so a chunk under `* Project :work:` is found by `tag:work` whether or not
its own subheading says so. Values with spaces take quotes; matching is
case-insensitive; anything unrecognised (`2:1`, a URL) stays as search text.

Predicates are stripped before embedding, so query syntax never reaches the
model — `tag:work` would otherwise have it looking for notes *about* the words
"tag" and "work".

### What it writes

Everything goes in one hidden directory beside your notes. **No note is ever
modified** — org-semantic only reads them.

| path | 951-note vault | what it is |
|---|---|---|
| `.org-semantic/chunks.json` | 6.0 MB | every chunk: text, heading path, line, `:ID:`, tags, TODO |
| `.org-semantic/vectors.f32` | 9.7 MB | one 384-float embedding per chunk, in the same order |
| `.org-semantic/manifest.json` | 0.2 MB | per-note hash and `(mtime, size)`, so a re-run knows what changed |
| `.org-semantic/tantivy/` | 2.5 MB | the lexical index |
| | **18 MB** | |

`chunks.json` is the source of truth and is shared: `search` scores against
`vectors.f32` and `keyword` against `tantivy/`, but both resolve a hit back to
the same record, so either mode can be displayed, filtered and jumped to
identically. `manifest.json` is used only while indexing, never while searching.

Add `/.org-semantic/` to the vault's `.gitignore`. All of it is derived — delete
the directory and `org-semantic index` rebuilds it in one pass.

One thing lives outside the vault: the embedding model, cached once in
`$XDG_CACHE_HOME/fastembed` (~190 MB) and shared by every vault.

**Indexing is incremental by default.** A file whose modification time and size
are unchanged is not even read; one whose timestamp moved is read and hashed, and
re-embedded only if its content actually differs. Deleted notes are dropped.

| | |
|---|---|
| nothing changed | 0.025 s |
| one note edited | ~0.5 s |
| `--rehash` — read and hash everything | 0.09 s |
| `--full` — rebuild from nothing | ~4 min |

`--rehash` is the backstop for a change that left both mtime and size untouched:
a timestamp-preserving restore, `rsync --times`, `touch -r`. At 0.09 s it is
cheap enough to run on every Emacs start.

## Design

**No ANN index, deliberately.** A thousand notes is 1.5M tokens and under 10 MB
of `f32`. A brute-force dot product over all of it takes 1.4 ms and is exact.
FAISS, HNSW and quantisers like TurboQuant address corpora three orders of
magnitude larger, and trade recall for memory this problem does not lack.

**Chunked by section, then paragraph, never past 512 tokens.** The limit is
enforced with the tokenizer, not a character budget — this corpus runs 2.0
characters per token in LaTeX-heavy notes against 4.0 in prose, and the
underlying library truncates silently, so a character heuristic quietly discards
the tail of long chunks. Consecutive pieces overlap by one paragraph so an idea
cut at a boundary is still embedded whole somewhere.

**Results are grouped per note.** A note that matches a query tends to match in
several places, and a flat top-k spends every slot on one document.

**The index belongs to the vault it describes**, so pointing org-semantic at
another vault is a different argument, not a different configuration.

## Status

Early, and useful. It indexes an org tree, searches it, and updates
incrementally.

The roadmap is org depth rather than more formats: honouring `:noexport:` and
archived subtrees; treating `#+begin_src` blocks distinctly, since code embedded
as prose pollutes results; a lexical mode over the same index, for the exact
identifiers embeddings are weak at; and an Emacs command that jumps to a hit.

Known gaps: no lexical search yet, and re-split pieces of a long section share
their section's line number.

## Prior art

[markdown-vdb](https://github.com/geckse/markdown-vdb) reaches a very similar
architecture for Markdown — filesystem-native, CLI-first, no server.
[mdvault](https://pypi.org/project/mdvault/) and
[markdown-vault-mcp](https://github.com/pvliesdonk/markdown-vault-mcp) add hybrid
BM25/FTS5 search, which org-semantic does not have yet. All three are
Markdown-first; org-semantic exists because none of them parse org.

## Licence

MIT. Embeddings via [fastembed-rs](https://github.com/Anush008/fastembed-rs)
(Apache-2.0) over [ort](https://github.com/pykeio/ort) (Apache-2.0).
