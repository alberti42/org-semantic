# org-semantic

Search a tree of org-mode notes by meaning or by words. One static binary, no
database, no Python, nothing listening on a port.

**[Full documentation](https://alberti42.github.io/org-semantic/)**

```console
$ org-semantic index ~/notes --both
951 org files
  180 sections ran past 512 tokens and were divided
5794 chunks · 5794 to embed · scanned in 1.5s
model loaded in 0.7s
  embedding 5794/5794 · 29 chunk/s · 6.1k tok/s · eta 0s
embedded 5794 chunks in 198.9s (29/s)
wrote ~/notes/.org-semantic/semantic/e5-small (8.9 MB of vectors) in 201.3s total
951 org files
lexical index: 5780 chunks written in 1.2s

$ org-semantic search ~/notes "why do the atoms heat up and get lost from the trap"

0.883 (+2.1σ)  2025-06-06 Review - Probing topological matter and fermion dynamics
       03 Literature review/2025-06-06 Review - Probing topological matter.org:672
       id:f73825a4-c877-4f69-a7e0-4ae305314b8d
       :Literature:
       · 0.883 L672   Observations: > Atom Loss: What causes it? > Rydberg excitation
               Atoms are excited to the n=53 Rydberg state using a two-photon transition…
       · 0.883 L677   Observations: > Atom Loss: What causes it? > Trap-induced loss
               Optical tweezers are subject to power drifts and pointing instabilities…

0.876 (+1.9σ)  2024-04-12 Atom sorting specifications
       01 Daily notes/2024/2024-04-12 Atom sorting specifications.org:43
       id:ce45cf4b-1cf8-434e-9129-fc2952877ea9
       :Daily:
       · 0.876 L43    Atom sorting -- shared specification document
               Rearranging a partially filled array into a defect-free one, with the…

[model load 640ms · query embed 9ms · search over 5794 vectors 1.3ms]
```

Not one word of that query appears in the top note's title, and the passage it
points at is 672 lines in. Finding what you can describe but cannot name is the
whole point of org-semantic.

Most of that half-second is the model loading, paid once per process. For
anything interactive, run `org-semantic serve` instead: it keeps the model and
the vectors resident, and answers in 7–9 ms by meaning or 3 ms by word — fast
enough to search as you type.

## Install

```sh
cargo install --git https://github.com/alberti42/org-semantic
```

Requires a Rust toolchain. Nothing else — no Python, no system ONNX Runtime, no
package manager. The embedding model downloads on first use.

## At a glance

```sh
org-semantic index  ~/notes --both      # build both indexes
org-semantic search ~/notes "why do the atoms heat up"        # by meaning
org-semantic search ~/notes 'tag:Literature Rabi' --lexical   # by word
org-semantic serve                      # JSON-RPC over stdio, for an editor
```

`org-semantic -h` explains every command; how a vault is indexed — languages,
excluded subtrees, what happens to `src` blocks — lives in a JSON policy file,
starting from [`config.example.json`](config.example.json).

## Documentation

- [Why](https://alberti42.github.io/org-semantic/#why) — what else exists, and why this is org-only
- [Install](https://alberti42.github.io/org-semantic/#install)
- [Use](https://alberti42.github.io/org-semantic/#use)
  - [Driving it from an editor](https://alberti42.github.io/org-semantic/#driving-it-from-an-editor) — `--json` and `serve`
  - [Scores](https://alberti42.github.io/org-semantic/#scores-and-why-the-raw-one-is-not-worth-showing) — and why the raw one is not worth showing
  - [Choosing an embedding model](https://alberti42.github.io/org-semantic/#choosing-an-embedding-model) — English or multilingual
  - [Two indexes, built separately](https://alberti42.github.io/org-semantic/#two-indexes-built-separately)
  - [Two rankings, never merged](https://alberti42.github.io/org-semantic/#two-rankings-never-merged)
  - [Filters](https://alberti42.github.io/org-semantic/#filters) — `tag:`, `dir:`, `todo:`, `lang:`
  - [What gets indexed](https://alberti42.github.io/org-semantic/#what-gets-indexed) — the policy file
  - [Languages](https://alberti42.github.io/org-semantic/#languages)
  - [What it writes](https://alberti42.github.io/org-semantic/#what-it-writes)
- [Design](https://alberti42.github.io/org-semantic/#design) — chunking, the token limit, why no ANN
- [Status](https://alberti42.github.io/org-semantic/#status) — what works, what is missing
- [Related work](https://alberti42.github.io/org-semantic/#related-work)
- [Licence](https://alberti42.github.io/org-semantic/#licence) — MIT

The site is generated from [`README.org`](README.org), which is the canonical
documentation; `make html` builds it locally into `public/`.
