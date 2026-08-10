# org-semantic

Search a tree of org-mode notes by meaning or by words. One static binary, no
database, no Python. It runs as a one-shot command, or stays resident for Emacs
— over a pipe, never a port.

**[Full documentation](https://alberti42.github.io/org-semantic/)**

```console
$ org-semantic index ~/notes --both
951 org files
  643 sections were divided to fit the 350-token budget
6531 chunks · 6531 to embed · scanned in 5.0s
model loaded in 0.9s
  embedding 6531/6531 · 34 chunk/s · 6.5k tok/s · eta 0s
embedded 6531 chunks in 194.1s (34/s)
wrote ~/notes/.org-semantic/semantic/e5-small (10.0 MB of vectors) in 200.1s total
951 org files
lexical index: 5969 chunks written in 1.2s

$ org-semantic search ~/notes "why do the atoms heat up and get lost from the trap"

0.883 (+2.1σ)  2025-06-06 Review - Probing topological matter > Observations: > Atom Loss: What causes it? > Rydberg excitation
       03 Literature review/2025-06-06 Review - Probing topological matter.org:672
       id:f73825a4-c877-4f69-a7e0-4ae305314b8d
       :Literature:
       Atoms are excited to the n=53 Rydberg state using a two-photon transition…

0.883 (+2.1σ)  2025-06-06 Review - Probing topological matter > Observations: > Atom Loss: What causes it? > Trap-induced loss
       03 Literature review/2025-06-06 Review - Probing topological matter.org:677
       id:f73825a4-c877-4f69-a7e0-4ae305314b8d
       :Literature:
       Optical tweezers are subject to power drifts and pointing instabilities…

0.882 (+2.0σ)  2025-06-06 Review - Probing topological matter > Observations: > Atom Loss: What causes it? > Motion and gate timing
       03 Literature review/2025-06-06 Review - Probing topological matter.org:682
       id:f73825a4-c877-4f69-a7e0-4ae305314b8d
       :Literature:
       During dynamical reconfiguration (AOD-based transport), atoms might spend too long outside…

[model load 698ms · query embed 7ms · search over 6531 vectors 1.7ms]
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
  - [Driving it from Emacs, or anything else](https://alberti42.github.io/org-semantic/#driving-it-from-an-editor) — `--json` and `serve`
  - [Letting an agent search for you](https://alberti42.github.io/org-semantic/#letting-an-agent-search-for-you) — RAG, and the skill in [`skills/`](skills/org-semantic/SKILL.md)
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

The site is generated from [`docs/manual.org`](docs/manual.org), which is the
canonical documentation; `make html` builds it locally into `public/`.
