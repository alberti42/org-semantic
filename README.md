# semnotes

Semantic search over a directory of notes. One static binary, no server, no
database, no Python.

```console
$ semnotes index ~/notes
951 org files
6328 chunks · 6328 to embed · scanned in 2.3s
model loaded in 0.16s
  embedding 6328/6328 · 29 chunk/s · 6.6k tok/s · eta 0s
embedded 6328 chunks in 220.7s (29/s)
wrote ~/notes/.semnotes (9.7 MB of vectors) in 223.0s total

$ semnotes search ~/notes "why do the atoms heat up and get lost from the trap"

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

Existing options for this either run a Python service — one popular org indexer
pulls 129 packages including torch, CLIP and the Azure SDK — or are built for
Markdown and know nothing about org structure.

semnotes is one 34 MB binary. ONNX Runtime is statically linked; the only thing
it ever downloads is the embedding model (129 MB, once, into your cache).

It is also, as far as I know, the only tool of its kind that understands org:
property drawers stay out of the embedded text, `#+title:` names the note, and
every chunk carries the enclosing `:ID:` so an editor can jump to a hit through
`org-id` rather than by file position.

## Install

```sh
cargo install --git https://github.com/alberti42/semnotes
```

Or build it:

```sh
git clone https://github.com/alberti42/semnotes && cd semnotes
cargo build --release          # target/release/semnotes
```

Requires a Rust toolchain. Nothing else — no Python, no system ONNX Runtime, no
package manager. The BGE-small-en-v1.5 model downloads on first use to
`$XDG_CACHE_HOME/fastembed`.

## Use

```
semnotes index  <dir> [--full|--rehash]   refresh the index (incremental by default)
semnotes search <dir> <query> [k]         query it, results grouped per note
semnotes chunks <dir> <path-substring>    show chunking decisions, no embedding
semnotes tokens <dir> [limit]             token-length distribution of the corpus
semnotes bench  <dir> [n] [config]        embedding throughput
```

The index lives in `<dir>/.semnotes/` — three files, about 10 MB for a thousand
notes. Add it to your `.gitignore`; it is derived data and rebuilds in one pass.

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
cheap enough to run on every editor start.

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

**The index belongs to the directory it describes**, so pointing semnotes at
another vault is a different argument, not a different configuration.

## Status

Early. It indexes org files; Markdown, typst and LaTeX need a chunker each and
nothing else, since the tokenizer and everything downstream are format-blind.

Known gaps: no hybrid keyword search (embeddings are weak on exact identifiers),
no editor integration yet, and re-split pieces of a long section share their
section's line number.

## Prior art

[markdown-vdb](https://github.com/geckse/markdown-vdb) reaches a very similar
architecture for Markdown — filesystem-native, CLI-first, no server.
[mdvault](https://pypi.org/project/mdvault/) and
[markdown-vault-mcp](https://github.com/pvliesdonk/markdown-vault-mcp) add hybrid
BM25/FTS5 search, which semnotes does not have. If you work in Markdown rather
than org, look at those first.

## Licence

MIT. Embeddings via [fastembed-rs](https://github.com/Anush008/fastembed-rs)
(Apache-2.0) over [ort](https://github.com/pykeio/ort) (Apache-2.0).
