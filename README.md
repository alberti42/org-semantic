# org-semantic

Search a tree of org-mode notes by meaning or by words. One static binary, no
database, no Python. It runs as a one-shot command, or stays resident for Emacs
— over a pipe, never a port.

**[Full documentation](https://alberti42.github.io/org-semantic/)**

```console
$ org-semantic index ~/notes --both
951 org files
  638 sections were divided to fit the 350-token budget
6522 chunks · 6522 to embed · scanned in 5.0s
model loaded in 0.8s
  embedding 6522/6522 · 35 chunk/s · 6.9k tok/s · eta 0s
embedded 6522 chunks in 186.8s (35/s)
wrote ~/notes/.org-semantic/semantic/e5-small (10.0 MB of vectors) in 192.9s total
951 org files
lexical index: 5809 chunks written in 1.2s

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

[model load 745ms · query embed 7ms · search over 6522 vectors 2.0ms]
```

Not one word of that query appears in the top note's title, and the passage it
points at is 672 lines in. Finding what you can describe but cannot name is the
whole point of org-semantic.

Most of that half-second is the model loading, paid once per process. For
anything interactive, run `org-semantic serve` instead: it keeps the model and
the vectors resident, and answers in 7–9 ms by meaning or 3 ms by word — fast
enough to search as you type.

## From Emacs

![The org-semantic results buffer, showing an English question answered by Italian notes and English ones ranked together](docs/images/results-buffer.png)

`M-x org-semantic-find` searches the vault the current buffer belongs to and
draws the reply. The question there is in English, the note answering it is in
Italian, and an English note is ranked beside it — these are the notes of a
public [bilingual vault](https://github.com/denialbb/braindump), and a
multilingual model does not much care which language an answer happens to be
written in.

Every hit carries its score with a σ beside it — a raw cosine cannot be read
without one — then the note, the outline path down to the section, and the lines
the passage came from. `RET` goes to the line under point, `n` and `p` walk the
passages, `k` and `+` widen the list or deepen it, and `g` asks again. It is a
`next-error` client, so `M-g M-n` walks the hits from anywhere, and `f` turns on
follow mode, which shows each passage in its note as point reaches it.

Your notes are read and never written: everything it builds goes in one
`.org-semantic/` directory beside them, and deleting it leaves the vault exactly
as it was. Nothing about them leaves the machine — no service, no account, no
API key, no telemetry. The only thing that ever touches the network is fetching
the model and a small language classifier, once each, after which it works
offline. [What it
touches](https://alberti42.github.io/org-semantic/#what-it-touches) says it in
full, and points at some public vaults if you would rather try it on notes that
are not yours.

## Install

```sh
cargo install --git https://github.com/alberti42/org-semantic
```

Requires a Rust toolchain. Nothing else — no Python, no system ONNX Runtime, no
package manager. The embedding model downloads on first use.

### In Emacs

The package is in [`lisp/`](lisp/). There are no default global bindings and
there will not be — `C-c` and a plain letter is yours rather than a package's —
so a recommendation is as far as this goes:

```elisp
(use-package org-semantic-results
  :load-path "/path/to/org-semantic/lisp"
  :custom ((org-semantic-executable "/path/to/org-semantic")
           (org-semantic-vault-root "~/notes"))
  :bind (("C-c n s" . org-semantic-find)
         ("C-c n S" . org-semantic-find-at-point)
         ("C-c n R" . org-semantic-reindex))
  ;; Show each passage in its note as point reaches it.
  :hook (org-semantic-results-mode . next-error-follow-minor-mode)
  ;; Reindex a vault as its notes are saved.
  :init (org-semantic-auto-reindex-mode 1))
```

`org-semantic-vault-root` is the one setting that has to be right: it says which
directory your notes are, and every buffer that says nothing else — `*scratch*`,
the agenda — searches it. With several vaults, leave it nil and let each one
declare itself in its own `.dir-locals.el`. [Searching from
Emacs](https://alberti42.github.io/org-semantic/#searching-from-emacs) covers
the rest, including every key the results buffer takes.

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
  - [Searching from Emacs](https://alberti42.github.io/org-semantic/#searching-from-emacs) — the results buffer, and the keys that walk it
    - [Settings](https://alberti42.github.io/org-semantic/#emacs-settings) — every variable the package exposes
  - [Driving it from Emacs, or anything else](https://alberti42.github.io/org-semantic/#driving-it-from-an-editor) — `--json` and `serve`
    - [Which binary you are talking to](https://alberti42.github.io/org-semantic/#version) — one repo, and a version floor rather than a match
    - [Errors you are meant to act on](https://alberti42.github.io/org-semantic/#errors-a-client-acts-on) — labelled, so a client can offer the fix
    - [Watching an index happen](https://alberti42.github.io/org-semantic/#progress) — `$/progress` while a reindex runs
    - [Stopping a run](https://alberti42.github.io/org-semantic/#cancelling) — `$/cancelRequest`, by the id it answers under
    - [Warnings that do not stop the run](https://alberti42.github.io/org-semantic/#remarks) — what indexing found but survived
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
