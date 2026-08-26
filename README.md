# org-semantic

org-semantic brings embedding search to org-mode. A model reads each passage of
your notes and turns it into a vector, placed so that passages saying the same
thing sit close together, whatever words they used. Your query becomes a vector
in the same space, and the score is the cosine of the angle between it and each
passage. So a note can answer a question it shares no word with, and an English
question can be answered by an Italian note.

Traditional search is there too. The words themselves go into a second index,
ranked by BM25 with per-language stemming, for the searches where the exact word
is the point: a flag, a name, a phrase. The two rankings are kept apart and
never fused. You choose which one answers, because a phrase or an AND/OR/NOT
means nothing to an embedding.

It comes in two parts, and each works without the other. The search itself is a
single Rust binary, with no database, no Python and no service: run it as a
one-shot command, or drive it from your own tools. The Emacs package is its
client. It starts the binary, keeps it resident over a pipe rather than a port,
and draws the results. The model is downloaded once, and after that nothing
leaves your machine.

**[Full documentation](https://alberti42.github.io/org-semantic/)**

## What it does

- **It finds passages, not files.** Both ways of keeping notes work: a
  Zettelkasten of thousands of small files, or a few very large org files,
  where a hit is one section rather than the whole file. Two caps bound the
  list — how many notes it may show, and how many passages any one of them
  may contribute — so one crowded file cannot fill it.
- **Search by meaning.** Your query is embedded and scored against every
  passage in the vault. An English query finds an Italian note.
- **Search by word.** BM25 over a [tantivy](https://github.com/quickwit-oss/tantivy)
  index, stemmed for the language each note is written in. Phrases, AND/OR/NOT
  and parentheses.
- **Six embedding models.** Three [BGE](https://github.com/FlagOpen/FlagEmbedding)
  from BAAI for English — small, base and large, 384 to 1024 dimensions — and
  three multilingual [E5](https://github.com/microsoft/unilm/tree/master/e5)
  from Microsoft Research, covering 100 languages. `--model` picks one. Each
  keeps its own index, so two can sit side by side and be compared on your own
  notes.
- **The same predicates on both sides.** `tag:`, `dir:`, `todo:` and `lang:`,
  each negated with a leading `-`.
- **Org-mode, not text.** Tag inheritance from `#+filetags:` and every ancestor.
  TODO keywords, priorities and planning lines. `#+begin_src` bodies stay out of
  the meaning index and stay in the word index.
- **A language per note, detected.** fastText's `lid.176` covers 176 languages,
  and a `# ltex: language=…` line overrides it. That choice picks the stemmer,
  labels the passage, and answers `lang:de`.
- **Fast enough to type into.** Measured in Emacs, round trip included, on a
  vault of about 1,000 notes: 10 ms by meaning, 20 ms by word.
- **Incremental by passage.** Appending one meeting to a 4,500-line
  `meetings.org` costs one embedding rather than 901, and 0.4 s rather than
  7 s.
- **What takes time, and is paid once.** Building the meaning index for about
  1,000 notes takes some three minutes. The word index takes about a second.
  After that, a run that finds nothing changed takes about 30 ms, and the
  first query of a session loads the model: 0.3 s for BGE, about 1 s for E5.
- **Usable without Emacs.** The binary is the whole search engine, and the Emacs
  package is one client of it. `search --json` gives a script machine-readable
  results, and `serve` speaks JSON-RPC 2.0 over stdin and stdout for another
  editor, a tool of your own, or an agent.
- **No hand-rolled protocol.** The binary speaks JSON-RPC over a pipe with LSP's
  framing, through rust-analyzer's
  [`lsp-server`](https://github.com/rust-lang/rust-analyzer/tree/master/lib/lsp-server).
  Emacs reads it with the built-in `jsonrpc.el`, the same library Eglot drives a
  language server with, so the package carries no transport code of its own. The
  methods are its own, though: this is not a language server.
- **Built on** [fastembed](https://github.com/Anush008/fastembed-rs) and ONNX
  Runtime, compiled into the binary by [`ort`](https://ort.pyke.io/); tantivy
  for BM25; and [fastText](https://fasttext.cc/) for language detection.

The screenshot and the worked example below search [Daniel Bias's
braindump](https://github.com/denialbb/braindump), someone else's public vault
of 753 org notes in English and Italian, cloned into `braindump/`. So you can
run them as they stand.

## From Emacs

![The org-semantic results buffer, showing an English question answered by Italian notes and English ones ranked together](docs/images/results-buffer.png)

`M-x org-semantic-find` searches the vault the current buffer belongs to. The
question is in English, the note answering it is in Italian, and an English note
is ranked beside it.

`RET` goes to the line under point, `n` and `p` walk the passages, `k` and `+`
widen the list or deepen it, `g` asks again. It is a `next-error` client, so
`M-g M-n` walks the hits from anywhere.

`f` is the one to know: **follow mode** opens each passage in its own note as
point reaches it, without taking point out of the list — so `n` and `p` read the
vault rather than an index of it. It is off until you press `f`, or until the
`:hook` below turns it on for every results buffer.

### Setting it up

The package is in [`lisp/`](lisp/). There are no default global bindings and
there will not be — `C-c` and a plain letter is yours rather than a package's —
so a recommendation is as far as this goes:

```elisp
(use-package org-semantic-results
  :load-path "/path/to/org-semantic/lisp"
  :custom (org-semantic-vault-root "~/notes")
  :bind (("C-c n s" . org-semantic-find)
         ("C-c n S" . org-semantic-find-in-directory)
         ("C-c n ." . org-semantic-find-at-point)
         ("C-c n R" . org-semantic-reindex))
  ;; Follow mode, on for every results buffer.
  :hook (org-semantic-results-mode . next-error-follow-minor-mode)
  ;; Reindex a vault as its notes are saved.
  :init (org-semantic-auto-reindex-mode 1))
```

`S` is `s` scoped to the directory you are in: it fills the prompt with a `dir:`
predicate naming it, and that directory and everything under it is what answers.
`.` searches for the region, or the symbol at point.

`org-semantic-vault-root` is the one setting that has to be right: it says which
directory your notes are, and every buffer that says nothing else — `*scratch*`,
the agenda — searches it. With several vaults, leave it nil and let each one
declare itself in its own `.dir-locals.el`.

There is no `org-semantic-executable` here because there need not be: a binary
in `org-semantic-install-directory` is found on its own. Set it only to name one
somewhere else. [Searching from
Emacs](https://alberti42.github.io/org-semantic/#searching-from-emacs) covers
the rest, including every key the results buffer takes.

## From the CLI

```sh
org-semantic index  ~/notes --both      # build both indexes
org-semantic search ~/notes "how did we decide to do it that way"  # by meaning
org-semantic search ~/notes 'tag:meeting budget' --lexical         # by word
org-semantic serve                      # JSON-RPC over stdio, for an editor
```

`org-semantic -h` explains every command; how a vault is indexed — languages,
excluded subtrees, what happens to `src` blocks — lives in a JSON policy file,
starting from [`config.example.json`](config.example.json).

One run in full, over the vault named above:

```console
$ org-semantic index braindump/roam --both --model e5-small
  20200924090307-elementi_di_probabilita_e_statistica.org: could not be read, so it is not indexed: stream did not contain valid UTF-8
753 org files
  256 sections were divided to fit the 350-token budget
3038 chunks · 3038 to embed · scanned in 1.5s
model loaded in 0.9s
embedded 3038 chunks in 77.8s (39/s)
wrote braindump/roam/.org-semantic/semantic/e5-small (4.7 MB of vectors) in 80.3s total
  20200924090307-elementi_di_probabilita_e_statistica.org: could not be read, so it is not indexed: stream did not contain valid UTF-8
753 org files
lexical index: 2863 chunks written in 0.4s

$ org-semantic search braindump/roam "what happens when a process is scheduled off the cpu" 2 --per-file 2

0.860 (+1.7σ)  Sistemi Operativi > Gestione Processi
       SO.org:278
       id:5c91241d-3da3-47e6-b27a-9afe7e0b4ff0
       :university:
       Componente del OS: =CPU Scheduler= - Sceglie processi in coda di ready - si attiva ogni 50/100 secondi - crea…

0.860 (+1.7σ)  Sistemi Operativi > Gestione Processi > Scheduling > Implementazione > Scheduler
       SO.org:628
       id:5c91241d-3da3-47e6-b27a-9afe7e0b4ff0
       :university:
       anche Short Term Scheduler decide quale processo in coda di ready sara' eseguito quando: 1. il processo in esecuzione passa…

0.854 (+1.5σ)  Microkernel Based Systems > Kernel Level > Scheduling > in Microkernel Based Systems
       microkernel_based_systems.org:194
       id:ad8e431b-7af6-4eb9-99a7-41af9cd0c4ce
       :erasmus:university:compsci:
       Different ideas: - Brian Ford - CPU Inheritance Scheduling + event \to mk \to root scheduler \to particular scheduler +…

0.850 (+1.4σ)  Microkernel Based Systems > Kernel Level > IPC
       microkernel_based_systems.org:29
       id:ad8e431b-7af6-4eb9-99a7-41af9cd0c4ce
       :erasmus:university:compsci:
       To send messages between threads you don't save and restore those register. The receiving end will declare beforehand to the…

[model load 733ms · query embed 8ms · search over 3038 vectors 1.0ms]
```

The top note's title — *Sistemi Operativi* — shares no word with the question.
Finding what you can describe but cannot name is the whole point of
org-semantic, and each score carries a σ because a raw cosine cannot be read
without one.

One note in that vault is UTF-16, and `index` says so once per index rather than
passing over it in silence.

Most of that three-quarters of a second is the model loading, paid once per
process. For anything interactive, run `org-semantic serve` instead: it keeps
the model and the vectors resident, and answers in 7–9 ms by meaning or 3 ms by
word — fast enough to search as you type.

## What it touches

Your notes are read and never written: everything it builds goes in one
`.org-semantic/` directory beside them, and deleting it leaves the vault exactly
as it was. Nothing about them leaves the machine — no service, no account, no
API key, no telemetry. The only thing that ever touches the network is fetching
the model and a small language classifier, once each, after which it works
offline. The
[manual](https://alberti42.github.io/org-semantic/#what-it-touches) has the
detail, and lists more public vaults if you would rather not start with your own
notes.

## Installing the binary

Both halves need it: the Emacs package drives this binary rather than carrying
one of its own.

Prebuilt ones are on the [releases
page](https://github.com/alberti42/org-semantic/releases) for Apple Silicon
macOS, Linux (x86_64 and arm64) and Windows, the macOS build Developer ID signed
and notarized. Unpack it into `org-semantic/` under your `user-emacs-directory`
and Emacs finds it with nothing configured at all — that is
`org-semantic-install-directory`, which is searched before `exec-path`. For
shell use, anywhere on your `PATH` does.

Or build it, which needs a Rust toolchain and nothing else — no Python, no
system ONNX Runtime, no package manager:

```sh
cargo install --git https://github.com/alberti42/org-semantic
```

Either way the embedding model downloads on first use.


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
