# org-semantic

Search a tree of org-mode notes by meaning or by words. One static binary, no
database, no Python, nothing listening on a port.

```console
$ org-semantic index ~/notes
951 org files
6328 chunks · 6328 to embed · scanned in 2.3s
model loaded in 0.16s
  embedding 6328/6328 · 29 chunk/s · 6.6k tok/s · eta 0s
embedded 6328 chunks in 220.7s (29/s)
wrote ~/notes/.org-semantic/semantic/bge-small-en (9.7 MB of vectors) in 223.0s total

$ org-semantic search ~/notes "why do the atoms heat up and get lost from the trap"

0.755  2025-06-06 Review - Probing topological matter and fermion dynamics
       03 Literature review/2025-06-06 Review - Probing topological matter.org:677
       id:f73825a4-c877-4f69-a7e0-4ae305314b8d
       :Literature:
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

Not one word of that query appears in the top note's title, and the passage it
points at is 677 lines in. Finding what you can describe but cannot name is the
whole point of org-semantic.

## Why

Existing packages either run a Python service — one popular org indexer pulls in
129 dependencies including torch, CLIP and the Azure SDK — or are built for
Markdown and know nothing about org structure. [Related work](#related-work)
compares them one by one.

org-semantic is one 34 MB binary. ONNX Runtime is statically linked; the only
thing it ever downloads is the embedding model (129 MB, once, into your cache).

**By design, it specialises in org and nothing else.** Parsing one format
properly buys things a format-agnostic tool cannot reach:

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
usage: org-semantic <command> <vault> [options]

Two indexes are built and searched separately: a semantic one, which finds
notes by meaning, and a lexical one, which finds them by word.

  index  <vault> [--full|--rehash] [--model NAME] [--config FILE]
         Build the semantic index.  Minutes, and downloads a model once.
  index  <vault> --lexical|--both [--full|--rehash] [--config FILE]
         [--lang en-US[,de-DE,...]|auto] [--fold-diacritics]
         Build the word index (seconds), or --both in one run.
         Incremental by default; --full rebuilds, --rehash re-reads every note.

  search <vault> <query> [k] [--model NAME] [--json]
         Rank by meaning: describe what you are after, not its words.
  search <vault> <query> [k] --lexical [--any] [--json]
         Rank by word, with phrases and boolean operators.  Terms are ANDed;
         --any restores OR.  A query may carry predicates:
           tag:x  -tag:x  dir:x  todo:x  lang:x   (lang: is lexical only)

  chunks <vault> <path-substring> [--lexical] [--config FILE] [--model NAME]
         Show how notes would be split, without indexing anything.
  tokens <vault> [limit] [--model NAME]     token lengths, and what would truncate
  models [vault]                            embedding models, and which are built
  serve                                     JSON-RPC 2.0 over stdio, for an editor
  bench  <vault> [n] [config]               embedding throughput on a slice

Which subtrees are skipped, and what happens to src and example blocks, is
policy: a JSON file passed with --config, remembered afterwards so later runs
need not repeat it.  Copy config.example.json and edit it.

Each model keeps its own semantic index, so several can be built side by side;
`models <vault>` shows which are.
```

### Driving it from an editor

`search --json` returns the hits as data rather than prose, and `serve` keeps a
process alive so a query costs milliseconds instead of a model load:

```console
$ org-semantic serve        # JSON-RPC 2.0 over stdio, LSP framing
```

| request | time |
|---|---|
| first semantic query (loads the model) | 309 ms |
| the same query again, model resident | **9.5 ms** |
| lexical query | 23 ms |

That gap is the whole reason for a resident process: 10 ms is a keystroke, 300 ms
is not.

Framing is LSP's `Content-Length` because Emacs ships `jsonrpc.el` — the library
Eglot runs on — so the editor needs no protocol code: `make-process`, and
request/response correlation, notifications and cancellation come free. No
socket, no port, no authentication, and the server lives exactly as long as the
editor does.

Methods:

| method | params | returns |
|---|---|---|
| `search` | `vault`, `query`, `k`, `mode` (`semantic`\|`lexical`), `model`, `any` | `{"hits": [...]}` |
| `index` | `vault`, `mode` (`semantic`\|`lexical`\|`both`), `full`, `rehash`, `model`, `lang`, `fold` | what each index did, as numbers |
| `status` | `vault` | which indexes are built, and which are loaded |
| `reload` | — | drop cached indexes after a rebuild |
| `shutdown` | — | exit |

Both modalities take the same request and return the same shape, so an editor can
offer one command with a toggle and never branch on the reply. Each hit carries
an absolute `file`, the `line`, and the `:ID:` when the note has one — enough to
build a clickable link and jump through `org-id`, which survives the note being
moved or renamed.

An empty query returns no hits rather than an error, so it is safe to send on
every keystroke; debouncing is the editor's policy, not the server's.

**Reindexing happens in-process too.** Spawning a CLI for it would pay the model
load again, which is the cost the resident process exists to avoid — so the
loaded model is lent to the indexer. Re-indexing a vault after saving one note
takes ~95 ms and embeds only that note. The cached vectors and the score baseline
are refreshed straight after, so the next query cannot answer from what was just
replaced.

### Scores, and why the raw one is not worth showing

Every hit carries `score` (raw cosine) and `z`. **Prefer `z`.**

These embeddings are strongly anisotropic — they nearly all point the same way.
Averaging every unit vector in a 951-note vault leaves something 75% of unit
length under `bge-small-en` and 90% under `e5-small`, so *unrelated* chunks
already score 0.563 and 0.801 respectively. The raw number is mostly that
constant offset, and it is not comparable between models:

| | raw | z |
|---|---|---|
| `bge-small-en`, top hit | 0.755 | 2.52σ |
| `e5-small`, top hit | 0.883 | 2.17σ |

`z` is how far above the corpus's own noise floor a hit sits, in that corpus's
standard deviations. The two models disagree by 0.13 on the raw scale and land in
the same place on this one. It also exposes weak hits that look respectable: a
0.826 under E5 is only 0.66σ.

The floor is measured from the vectors themselves — 20k sampled pairs, ~37 ms
when an index is loaded and cached thereafter — rather than stored, so it cannot
drift from the vectors it describes. Lexical hits have `z: null`: BM25 is
unbounded and has no such floor.

**No threshold is ever applied.** `z` is presentation; what to do with it is the
caller's business.

### Choosing an embedding model

```console
$ org-semantic models
name             dim  trained on
bge-small-en     384  English  (default)
bge-base-en      768  English
bge-large-en    1024  English
e5-small         384  100 languages
e5-base          768  100 languages
e5-large        1024  100 languages
```

```sh
org-semantic index ~/notes --model e5-small --full
```

Pick a multilingual model if your notes are not all in one language. With
`e5-small`, an English query finds the German note it never mentions:

```console
$ org-semantic search ~/notes "why do atoms get lost from the trap"
0.891  Trap physics            ← English
0.838  Atome in der Falle      ← German, never using those words
0.736  Ricetta                 ← unrelated
```

**Each model keeps its own index**, so you can build several and compare them
without re-embedding for the one you already had:

```console
$ org-semantic models ~/notes
name             dim  trained on      status
bge-small-en     384  English         built default
e5-small         384  100 languages   built
…

$ org-semantic search ~/notes "why do atoms get lost from the trap" --model e5-small
```

`search --model` selects between built indexes; it cannot impose a model on
vectors built by another, because a query must be embedded by whatever embedded
the corpus. With one index built it is used automatically; with several the
default wins unless you name one. Naming a model you have not built is an error
listing what you have.

**Scores are only comparable within one model.** BGE spreads its cosines widely
(0.37–0.74 above); E5 compresses everything into roughly 0.73–0.93. Only the
ranking carries meaning, never the absolute number.

fastembed offers forty models; these are the ones whose prefixes are known here.
Each family expects its own — BGE prefixes only the query, E5 prefixes the
indexed passage too — and the wrong convention costs retrieval quality silently,
so a model is listed only once its prefixes have been checked. `bge-small-en` and
`e5-small` have been run end-to-end; the base and large variants inherit their
family's prefixes and have not been exercised here.

### Two indexes, built separately

`index` follows the same convention as `search`: bare it builds the **semantic**
index, `--lexical` builds the **word** index, and `--both` does the two in one
command.

```sh
org-semantic index ~/notes                 # embeddings      ~200 s / 951 notes
org-semantic index ~/notes --lexical       # BM25             1.3 s
org-semantic index ~/notes --both          # both
```

`--lang` and `--fold-diacritics` belong to the lexical index, which is the only
one that has a use for a language: they choose the stemmer. Passing them to a
semantic build is an error rather than a setting that does nothing.

They are separate artifacts with separate records of what they have seen, so each
re-run only reads the notes *that* index is behind on. Both are incremental by
default; `--full` rebuilds from scratch and `--rehash` re-reads every note,
ignoring timestamps.

Embedding takes minutes and a 129 MB model; the lexical index takes a second and
nothing but the notes. So refreshing keyword search after editing a few notes
costs a second, and changing the folding or your language list rebuilds only the
lexical index — those settings are hashed per index.

### Two rankings, never merged

Without a flag, `search` ranks by **meaning**, scoring the query's embedding
against every chunk's. With `--lexical` it ranks by **words**, using
[tantivy](https://github.com/quickwit-oss/tantivy) — BM25 over an inverted index,
with a real query language: phrases, boolean operators, field boosts. Terms are
ANDed by default, since OR would rank anything merely containing "oscillations"
for the query "Rabi oscillations"; `--any` restores OR.

One command, but never one merged result list: a phrase or a boolean means
nothing to an embedding, so a fused list would mix hits that honoured your query
with hits that could not.

The difference is not academic. Searching your notes for the surname `Gehm`:

```console
$ org-semantic search ~/notes Gehm --lexical
13.187  2024-08-27 Heating rate in optical traps      ← the note citing Gehm 1998

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
| `lang:x` | note is in language `x`; `lang:de` matches `de-DE` and `de-AT`. **`--lexical` only** |

### What gets indexed

Two things are decided by policy: which subtrees are indexed at all, and what
happens to blocks.

Subtrees tagged `:noexport:` or `:ARCHIVE:` are left out — org's own markers for
"not for consumption" and "put this away". Both inherit, so the rule covers a
whole subtree, children included.

**Blocks are treated differently by each index.** Code embedded as prose pollutes a semantic search — a shell snippet lands
near queries it has nothing to do with — but exact match is precisely what you
want when hunting a flag or a function name. So by default the body of a `src`
block is not embedded, and *is* searchable by word:

```console
$ org-semantic chunks ~/notes "smb" | tail -1
    tail: "…autofs will pick it up.\n\n[src bash]\n\nAfterwards the volume survives…"

$ org-semantic chunks ~/notes "smb" --lexical | tail -1
    tail: "…mount_smbfs //user@server/share /Volumes/share -o nobrowse\n\nAfterwards…"
```

`"placeholder"` is why the first one still reads properly. Dropping the block
outright would glue the paragraph before it to the one after — an adjacency the
note never had — and lose the fact that a snippet was there at all, which is part
of what the section is about. `[src bash]` keeps both, without forty lines of
shell drowning the prose around it.

The whole policy lives in a file you own, named with `--config`. Copy
[`config.example.json`](config.example.json) — it is exactly the defaults — and
edit it:

```json
{
  "languages": ["en-US", "de-DE"],
  "fold_diacritics": false,
  "blocks": {
    "src":     { "semantic": "placeholder", "lexical": true },
    "example": { "semantic": "placeholder", "lexical": true },
    "results": { "semantic": false,         "lexical": true },
    "quote":   { "semantic": true,          "lexical": true },
    "verse":   { "semantic": true,          "lexical": true }
  },
  "exclude_tagged": ["noexport", "ARCHIVE"]
}
```

`languages` and `fold_diacritics` configure the lexical index, which is the only
one that stems anything — see *Languages* below. `semantic` takes `true` (embed
it), `false` (drop it) or `"placeholder"`;
`lexical` is a plain boolean, since labelling something in an exact-match index
would only make `[src]` a searchable word. Babel `#+RESULTS:` and bare `: `
fixed-width lines count as output, not prose. Quote and verse stay in both —
they are prose someone chose to set off, not machine output.

Those values are the defaults, so the block above describes what you get with no
config at all.

```sh
org-semantic index ~/notes --both --config ~/notes/indexing.json
```

**The policy is sticky.** Once given it is cached, so later runs need not restate
it — forgetting the flag is safe, which is the property that makes a sticky
setting tolerable. It is compared by *meaning*, not by bytes: key order,
whitespace and duplicates all hash the same, and a file that merely restates the
defaults is indistinguishable from no file at all.

**Changing it is refused, not obeyed.** A config can change without you doing
anything — a `git pull` brings someone else's edit — and re-embedding a corpus
takes minutes, so the tool says what moved and waits:

```console
$ org-semantic index ~/notes --both --config ~/notes/indexing.json
Error: the semantic index was built under a different policy —
       exclude_tagged: was [ARCHIVE, noexport], now []
       pass --full to rebuild under the new one, or restore the previous setting
```

Unknown keys are an error rather than ignored, for the same reason unknown flags
are: a typo that does nothing looks exactly like a setting that does nothing.

`chunks --config` applies a policy **without** storing it or reindexing, so you
can see what a change would do before paying for it. `chunks` previews the
semantic index and `chunks --lexical` the word index, faithfully in each case:
only the semantic preview re-splits at 512 tokens, only the lexical one carries a
language. It says which one it is showing.

Over JSON-RPC the `index` method takes the same policy as a `config` object, so
an editor can keep its own source of truth in whatever format suits it — a
commented `.eld`, in Emacs's case — and pass it already parsed. Neither side
needs a reader for the other's syntax.

### Languages

A note declares its language the way ltex-ls-plus already asks for it:

```org
# ltex: language=de-DE
```

That takes effect from its own line onward, as ltex does, so a note may switch
part-way — the marker forces a chunk boundary, since a chunk carries exactly one
language. Placed between sections it costs nothing; placed mid-section it splits
that section in two.

The keyword is always `ltex`. You do not need ltex installed to use it —
`# ltex: language=de-DE` is an ordinary org comment — and if you do use it, the
line you already wrote for grammar checking is the one this reads.

**Language is a lexical concern.** It selects the stemmer that makes `Sprachen`
find `Sprache`, and an embedding is not stemmed — so only `index --lexical`
takes `--lang`, and `lang:` narrows only `search --lexical`. Both say so rather
than accepting a setting that would do nothing. Multilingual *semantic* search is
a question about the embedding model, not about labels, and is answered by
using a multilingual model.

Otherwise `--lang` names the languages the vault is written in, and **how many
you name decides everything else**:

```
--lang en-US                  one language: every undeclared note is English
--lang en-US,de-DE,it-IT      several: each note is classified as one of these
--lang auto                   classified with no restriction, all 176
```

`--lang` and `--fold-diacritics` are shorthand for the `languages` and
`fold_diacritics` keys in the config, and **what you pass is cached with the rest
of the policy**. Forgetting them on a later run reuses what you set; it does not
quietly revert to English alone, which is what a plain flag used to do.

Classification is per note rather than per chunk, since a chunk can be a two-line
heading. It uses fastText's `lid.176` (917 kB), downloaded to
`$XDG_CACHE_HOME/org-semantic/` on first use.

**It is accurate on prose and guesses when there is no prose.** Measured across a
951-note vault, `auto` placed English, German and Italian correctly; the 0.4% it
got wrong were notes that are almost entirely attachment links or shell snippets,
where it is classifying filenames. **Listing your languages removes those** — the
answer is the best-ranked language among the ones you named, so a note cannot
come back Portuguese because a screenshot filename looked like it. On the same
vault that takes the misclassifications to zero.

Languages are matched on their primary subtag but stored as you wrote them, so
`de-DE` stays `de-DE` rather than becoming fastText's bare `de`. The first one
you name is the vault's default.

A misclassified chunk is stemmed wrongly and becomes harder to find, with nothing
to indicate why. An explicit `# ltex: language=…` overrides the classifier — it
wins even over a language list that doesn't mention it, because the list says
what may be *guessed*, never what a note may *state*. The exception is a code the
classifier doesn't know, which is a typo far more often than a language; that
warns and falls back to the default. Such a marker is a line those notes want
anyway, so ltex doesn't grammar-check your shell
commands.

Lexical search stems each note in its own language: `oscillation` finds
`oscillations` in English notes, `Sprachen` finds `Sprache` in German ones, and
neither leaks into the other. Regional variants share a stemmer, so `de-DE` and
`de-AT` are both German.

`fold_diacritics` (or `--fold-diacritics`) folds accents, so `eleves` matches
`élèves`. It is named for the case worth having; the filter is broader, mapping
non-ASCII to ASCII generally, so `æ` becomes `ae` too. Off by default, and worth
noting it does nothing for German — that stemmer already strips umlauts, so
`Worter` finds `Wörter` regardless. It is French, Spanish and Portuguese that
need it. Changing it rebuilds the lexical index once (0.2 s).

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
| `.org-semantic/semantic/<model>/chunks.json` | 6.0 MB | every chunk: text, heading path, line, `:ID:`, tags, TODO |
| `.org-semantic/semantic/<model>/vectors.f32` | 9.7 MB | one embedding per chunk, in the same order (384–1024 floats) |
| `.org-semantic/semantic/<model>/manifest.json` | 0.2 MB | what that model's index has seen: per-note hash and `(mtime, size)` |
| `.org-semantic/tantivy/` | 4.9 MB | the lexical index, including its own copy of each chunk |
| `.org-semantic/lexical.json` | 0.2 MB | the same, for the lexical index |
| | **21 MB** | |

One directory per model, each complete in itself. The chunk table is duplicated
rather than shared because a vector is paired to its chunk **by position**: a
shared table would silently go stale for every model you did not index in that
run, and a same-count-different-content mismatch is exactly what a length check
cannot catch. 6 MB is a small price for being unable to get that wrong.

**The two indexes are independent.** Each carries everything its own hits need
and its own record of which notes it is behind on, so either can be built,
rebuilt or deleted without disturbing the other — which is what makes
`index --lexical` a one-second operation rather than a ten-minute one. The cost
is that the chunk text is stored twice; on this vault that is 4.9 MB against a
9.7 MB vector file.

The two chunk differently. The semantic index splits any section longer than 512
tokens, which is what the embedding model reads in one go; BM25 has no such
limit, so the lexical index keeps whole sections — 5757 chunks against 6328.

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

**No ANN index.** A thousand notes is 1.5M tokens and under 10 MB of `f32`, and
a brute-force dot product over all of it takes 1.4 ms and is exact. FAISS, HNSW
and quantisers like TurboQuant address corpora three orders of magnitude larger,
trading recall for memory this problem does not lack.

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

Early, and useful. It indexes an org tree, searches it by meaning and by words,
updates incrementally, and speaks JSON — over `--json` for one-shot calls and
over `serve` for an editor holding a session open.

**The editor side is the open piece.** Everything it needs exists: structured
hits with an absolute path, a line and an `:ID:`, and a resident process that
answers in ~10 ms. What is missing is the client itself — no elisp is written
yet, and jumping to a hit is still a manual `find-file`.

The rest of the roadmap is org depth rather than more formats: honouring
`:noexport:` and archived subtrees, and treating `#+begin_src` blocks distinctly,
since code embedded as prose pollutes results.

Known gaps. The two modes are not fused into a single ranking: a phrase or a
boolean means nothing to an embedding, so a combined list would mix hits that
honoured your query with hits that could not. Auto language detection is
right on prose and guesses on notes that are almost entirely attachment links or
shell snippets — 0.4% of chunks on the reference vault, and none once the
languages are named with `--lang`. Re-split pieces of a long section share their
section's line number.

## Related work

Two things separate these projects for an org user: whether they parse **org** at
all, and whether they come with an **Emacs** front-end or are a generic CLI you
wire up yourself.

| project | format | Emacs front-end | runtime |
|---|---|---|---|
| **org-semantic** | org | via `serve` (elisp UI in progress) | one static binary, in-process ONNX |
| org-supertag | org | native elisp | elisp only, no embeddings |
| org-db | org | native elisp | Python + `torch` server on a port |
| emacs-rag-libsql | org | native elisp | Python + `torch` server on a port |
| markdown-vdb | Markdown | none (Claude Code skills) | Rust CLI, external embedding API |
| mdvault | Markdown | none (MCP for Claude Code) | Python, MCP server |

### markdown-vdb

[markdown-vdb](https://github.com/geckse/markdown-vdb) reaches a very similar architecture for Markdown — one Rust binary, the index on disk, notes never modified. What sets it apart is where the embeddings come from: an external provider — OpenAI, Ollama, any OpenAI-compatible endpoint — so a search reaches out to a network or a local model server, and an API key and its cost or a running server come with it. org-semantic embeds in-process through a statically linked ONNX Runtime and calls nothing. Its front-end is aimed at AI agents — it ships Claude Code skills rather than an Emacs package. (It also offers a fused hybrid ranking, semantic + BM25 through RRF, which org-semantic leaves as separate commands for now — see Status.)

### mdvault

[mdvault](https://pypi.org/project/mdvault/) also does BM25 + semantic over a Markdown vault, but it is a Python tool (`uv tool install`) that keeps everything in one SQLite file and answers Claude Code over MCP. org-semantic is a single static binary — no interpreter, no `.db`. Both have a resident mode, aimed differently: mdvault's `serve` is MCP so Claude Code can search a vault; org-semantic's is JSON-RPC/LSP so an editor can. Neither ships an Emacs package.

All of these are Markdown-first; org-semantic exists because none of them parse org.

### org-supertag

[org-supertag](https://github.com/yibie/org-supertag) is org-native and shares the same instincts — local-first, no server, no Python, `:ID:`-addressed, incremental on `(mtime, hash)` — but solves the opposite problem. It turns `#tag` headings into a *structured database*: you define fields once (a `#paper` has `authors`, `year`, `status`), fill them through spreadsheet and Kanban views, and query with an S-expression DSL over tags and fields — `(and (tag "task") (not (field "status" "done")))`. That answers *"all unread papers rated ≥4"*, a question about structure you authored. org-semantic answers *"notes about X"* where you tagged nothing and don't know the words — a question about meaning, needing no schema. org-supertag has no embeddings; org-semantic keeps no database and never writes to your notes. They are complementary rather than competing, over the same vault: org-supertag could even maintain the `#tag` markers org-semantic reads as `tag:` predicates. Reach for org-supertag when you want structured views and a query language; reach for org-semantic when you want to find the note you can't name in a vault full of prose.

### org-db and emacs-rag-libsql

[org-db](https://github.com/jkitchin/org-db-v3) and [emacs-rag-libsql](https://github.com/jkitchin/emacs-rag-libsql), both by John Kitchin, are the closest in aim to org-semantic — semantic search over an org tree, driven from Emacs. Both split the work the same way: Emacs parses and navigates, a Python FastAPI server does the embedding and the vector search. org-db goes wide — CLIP image search, SQLite full-text, indexing linked PDF/DOCX/PPTX, gptel tools for an LLM; emacs-rag-libsql goes deep on ranking, a cross-encoder reranking the vector hits in a second stage. Both fuse vector and keyword into one hybrid result.

The difference is what you have to install and what has to be running. Each needs Python and `uv`, and pulls in `sentence-transformers` and `torch` — org-db also `transformers`, `pillow`, `pymupdf4llm`, `python-docx`, `python-pptx` — then a FastAPI process listening on a port for Emacs to reach over HTTP. org-semantic is one static binary: the embedding model runs in-process through a statically linked ONNX Runtime, nothing binds a port, and `serve` is a stdio child the editor starts and owns, not a daemon.

## Licence

MIT. Embeddings via [fastembed-rs](https://github.com/Anush008/fastembed-rs)
(Apache-2.0) over [ort](https://github.com/pykeio/ort) (Apache-2.0).
