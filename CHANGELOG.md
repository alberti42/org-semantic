# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
— with the usual `0.x` licence to break things in a minor release, which is
where it currently is.

**Two versions are released together and do not move together.** The heading is
the *release*, which is the Emacs package's; each section says which *binary*
version it carries. An entry that needs a newer binary says so, because
otherwise the only way to find out is to update and see. A release that changes
the elisp alone leaves the binary where it was, and there is nothing to
download — the client checks a version floor rather than a match.

### Three version numbers, and when each moves

| number | where | moves when |
|---|---|---|
| release | `org-semantic-version`, and the tag | anything ships at all |
| binary | `version` in `Cargo.toml` | the Rust changes |
| floor | `org-semantic-minimum-binary-version` | the elisp starts needing something the server did not have |

The floor is the one users feel: raising it tells everyone with an older binary
to update. So raise it only when the elisp actually depends on something new — a
new method, a new field, a changed reply shape — or when a release *documents*
behaviour that only the newer binary provides and the older one gets **silently
wrong**. Raise it in the same commit, to the binary version that introduced the
thing. Not to "the current version", and never merely to be tidy.

That second case is why 0.2.0 raised it: nothing in the elisp calls a new method,
but 0.1.0 has no negated predicates at all, so it reads `-dir:archive` as
`dir:archive` and answers with the opposite of the request. The floor exists to
prevent silence, not merely to gate method calls.

The binary version is what the floor names, so it has to move whenever the Rust
does, whether or not the elisp cares yet. Skip it and two different binaries
report the same version, after which no floor can tell them apart — the release
job refuses a tag whose Rust changed without it.

### Cutting a release

1. Set `org-semantic-version` to the new release, and the `;; Version:` header of
   each file in `lisp/` with it.
2. Bump `Cargo.toml` if the Rust changed since the last tag; leave it alone if it
   did not.
3. Raise `org-semantic-minimum-binary-version` if this release needs the new
   binary — by the rule above, which includes a release documenting behaviour the
   old binary gets silently wrong.
4. Move everything under `[Unreleased]` into a new version heading with today's
   date, and say in it which binary version the release carries.
5. Commit, tag `vX.Y.Z`, push the tag.

The workflow refuses the tag unless it matches `org-semantic-version`, the
changelog has a section for it, and the floor is not above the binary being
built. It then publishes a **draft** release for review.

## [Unreleased]

Needs binary version **0.2.1**: the package calls a `download` method 0.2.0 does
not have.

### Fixed

- **Offering to download a missing model downloaded nothing.** The offer ran an
  index, and an incremental index of a vault whose notes have not changed embeds
  nothing — so it loaded no model, fetched nothing, reported success, and the next
  search refused in exactly the same words. There is a `download` method now: it
  fetches the weights and builds nothing, and the search that follows asks about a
  missing index separately, as its own question.
- **Searching while the model downloaded asked you to try again.** A search sent
  mid-fetch cannot be answered, so it came back as the same refusal with an offer
  to retry — a poll loop by hand for someone already waiting. When the fetch is
  this buffer's own, it now says it is waiting; the download's reply re-runs the
  search by itself.
- **Searching by word out of a refusal stuck.** `[l]` set the buffer's ranking for
  good, so every later query in it was answered by word with nothing saying why.
  It searches once now and leaves the buffer's own ranking alone —
  `org-semantic-results-ranking` is where a preference belongs, and it takes
  `"ask"` if you have no usual answer.
- **A cold first run could silently ignore a note's `# ltex: language=…`.** The
  language classifier was fetched and loaded by every thread that wanted it at
  once, through one shared staging file, so a note declaring Italian in a German
  vault could come back as German. One loader at a time now, and the staging file
  carries the process id, which also covers two vaults indexing at once.

### Added

- **`org-semantic-results-connector`** and `l` — for logic — in the results buffer:
  join a word query's terms with `and` or `or`. Named for the logic rather than for the wire,
  which spells it as a boolean called `any` — the server's vocabulary, and no
  reason for a reader of Emacs to learn it. `AND`, `OR`, `NOT` and parentheses are
  writable in the query itself, so this is the default rather than the only way to
  say it.

### Removed

- **The "Try again" offer**, which only re-ran the search — something `g` and any
  new search already do, so it was a manual poll offered as a decision. A refusal
  that says a download is already running now offers the word index and nothing
  else, and the `indexing` refusal asks nothing at all: its message is the whole of
  what is known.
- **Four keys in the results buffer**: `=` (describe the hit), `P` (per-file cap),
  `a` (the same question, in the wire's vocabulary) and `M` (fold a split section
  into one hit). Each
  turned a command-line flag into a keystroke because the flag existed. `=` in
  particular reported the heading's line, the tags and the `:ID:` to the echo
  area, where the first two are already in the block's head.

  `mergeBySection` and `any` are still server options and still parameters of
  `org-semantic-search-async`; what is gone is a key for them.

### Changed

- **A failed search asks what to do in the minibuffer** rather than drawing a row
  of buttons in the results buffer. One keystroke per offer — each its label's own
  initial, so `d` for "Download it", `b` for "Build it", `l` for "Search by word
  (lexical)", `q` or `C-g` to leave it — and each says what it costs, since one of them is
  minutes and the other seconds. The buffer keeps the sentence, so there is still
  an account of the empty list once the question is gone. Declining costs nothing:
  `R` indexes the vault whenever you like, which is the same call the offer would
  have made.
- The message behind a search that has no model reads *"indexing this vault will
  fetch it"* rather than *"index this vault to fetch it"*. It only ever reaches a
  client, which has a keystroke rather than a command line.

## [0.2.0] — 2026-08-12

Binary version 0.2.0. Update the binary as well as the package: 0.1.0 reads
every negated predicate as its opposite, so `-dir:archive` searches the archive
rather than excluding it.

The Emacs client is what this release is for. In 0.1.0 it was unfinished; it is
now how org-semantic is meant to be used from Emacs.

### Added

- **A hit's address is four links**, and each goes where it names: the directory
  opens in Dired (`org-semantic-results-reveal-function` replaces that), the note
  opens at its top, the section goes to its heading, and the two line numbers go
  to where the passage starts and ends.
- **Search history**, shared by every prompt and saved by `savehist-mode` with no
  configuration — so `M-p` from the results buffer reaches the query that made it.
- **`dir:` may be absolute**, by Emacs's own rule: `/` or `~` means absolute,
  anything else is relative to the vault. A path pasted out of Dired works as it
  stands, and one outside the vault is an error rather than a search that finds
  nothing.
- **Every predicate negates** — `-dir:`, `-todo:` and `-lang:` join `-tag:` — and
  a query may be nothing but exclusions, so `-todo:DONE` on its own is every
  passage that is not done.
- `org-semantic-install-directory`: unpack a release there and nothing needs
  configuring. Searched before `exec-path`, so a `cargo install` for shell use
  cannot quietly move Emacs onto a different build.
- `org-semantic-cache-home` and `ORG_SEMANTIC_CACHE_HOME`, for putting the
  downloaded models somewhere other than `$XDG_CACHE_HOME`.
- `org-semantic-results-display-action`, a *default* for where the buffer appears.
  `display-buffer-alist` is consulted first, so anything you set there still wins.

### Fixed

- **`-dir:` and `-todo:` were read as their positives** — the opposite of the
  request, silently. A leading `-` was stripped from every key and consulted by
  only one of them.
- **A query of only exclusions returned nothing** from the word index: tantivy has
  no implicit universe, so a boolean of `MustNot` alone matched no document where
  the semantic side matched everything not excluded.
- **`-lang:` was accepted and ignored** by the semantic side through the server,
  which had its own copy of a check that had learned about negation only in the
  command line's copy.
- **The ranking prompt offered one of its two rankings.** The current one was
  passed as initial input, which a completion UI treats as a filter.
- **A passage could not be selected with the mouse**: every line carried
  `follow-link`, so a click jumped instead of placing point. Only the address
  links answer the mouse now.
- The gutter no longer paints a background, and a repeated line keeps it, so the
  left edge stays straight.
- `org-semantic-find` had lost its autoload cookie, so a fresh Emacs could not
  find the command.

### Changed

- **Prebuilt binaries ship as compressed archives** — 13 MB rather than 40, and
  the executable bit survives, which a bare release asset does not carry.
- **A kept block's `#+begin_` line is inside the passage's span.** It never was:
  the marker is not part of a chunk's text while its body is, so a passage
  beginning inside a long block started at the first line of code and ran to an
  `#+end_` with no beginning. Read back, that is a block that never opened — org
  fontified the code as prose and a reader saw a stray end marker. Measured at
  6..96 on a 90-line block whose marker was line 5; it is 5..96 now.

  Every block kind, since the marker rule does not vary with the policy: `src`,
  `example`, `quote`, `verse` and anything unrecognised. Needs the semantic or
  word index rebuilding to take effect on notes already indexed, and costs
  nothing if you do not.
- **Passages are shown with org's own faces** — emphasis, verbatim, headings,
  block markers, links — by fontifying each in a hidden `org-mode` buffer and
  copying the faces back, which is the trick `magit` uses for diffs.
  `org-semantic-results-fontify` turns it off.

  **Only faces are copied and no character moves**, so the nth line of a passage
  is still line `startLine` + n of the note: org's `keymap`, `invisible` and
  `display` are left behind, which is also why a link still shows its brackets.
  About 0.8 ms a passage.
- **`f` follows point**, previewing each passage in its note as you reach it —
  `next-error-follow-minor-mode`, which was reachable only as `C-c C-f` and so
  went unnoticed. That spelling still works, being the one `occur` and `grep` use,
  and `org-semantic-results-mode-hook` turns it on for good.
- **A pair of keys for each cap**: `k`/`K` for how many notes may appear, `+`/`-`
  for how many passages one note may contribute. `+`/`-` moved off the note cap,
  which had no mnemonic and left the passage cap reachable only by re-running the
  search. Both double and halve, and both stop at one — a cap of zero answers with
  an empty list and reads exactly like a query that matched nothing. `C-k` and `=`
  set exact values, each sharing a key with the pair it belongs to — `=` its place
  with `+`, `C-k` its letter with `k`.
- **The ranking is chosen, not toggled**: `m` for meaning, `w` for word. A toggle
  could not be pressed without first knowing which ranking was in force, so one key
  meant two things depending on state you had to go and read. `w` is also the
  prompt's key for the same choice — though the prompt's version searches by word
  once, where the key is a statement about the buffer.
- **Keys moved so that no letter does two jobs.** The offer that searches by word
  is `[w]`, freeing `l` for the logical connector. `retry` lost its second label with it — "Wait for it" is
  "Try again" everywhere now, one action with one name. A full rebuild is `[b]`
  like a first build, since no failure ever offers both and `[r]` would have
  implied the letter told them apart.
- `C-u` asks which ranking and nothing else; `C-u C-u` asks about the length of
  the list as well. It used to take three answers to change one thing.
- Searching for the thing at point offers it as a *default* rather than as text
  already typed, so `RET` takes it and typing replaces it.

## [0.1.0] — 2026-08-12

Binary version 0.1.0. **The command line is the release; the Emacs client is
early.** Everything below about `index`, `search` and `serve` is in use and
measured. The elisp searches, draws results and reindexes, but it is still being
built — there is no minibuffer interface, no installer, and the results buffer
is read-only by design for now.

### Added

- Initial release. A CLI that indexes a tree of org-mode notes and searches it
  two ways, by meaning over embeddings and by word over BM25, keeping the two
  rankings separate. One static binary: no database, no Python, no service.
- `index`, `search`, `chunks`, `tokens`, `models`, `bench` and `serve`.
  Indexing is incremental at the passage, not the file, so appending one meeting
  to a year of them costs one embedding rather than nine hundred.
- `serve` — JSON-RPC 2.0 over stdio with LSP framing, keeping the embedding
  model resident. A warm semantic query is ~9 ms against ~309 ms cold, which is
  what makes search-as-you-type viable. Indexing runs on a worker thread, so
  searches are answered while a rebuild is in flight.
- An Emacs client, unfinished: `org-semantic-find` draws a results buffer with
  `next-error` navigation, and `org-semantic-reindex` reports progress. No
  dependency on org itself, and none on consult or vertico. You will need to put
  the binary on `exec-path` yourself, or set `org-semantic-executable`. See 0.2.0,
  which is where it became usable.
- `ORG_SEMANTIC_CACHE_HOME`, and `org-semantic-cache-home` on the Emacs side,
  for putting the downloaded models somewhere other than `$XDG_CACHE_HOME`.
  A model is 128 MB to 2.24 GB, which is worth being able to move.
- Signed and notarized macOS binaries, built by CI on a version tag.

### Known limitations

- **Apple Silicon only on macOS.** `ort` publishes no prebuilt ONNX Runtime for
  `x86_64-apple-darwin`, so an Intel build does not link. Rosetta does not help:
  there is no x86_64 binary for it to translate. The manual sketches an untested
  workaround — link against a Homebrew ONNX Runtime — at the cost of the binary
  no longer being self-contained.
- The Windows binary starts and answers `--version` and `models` in CI, and
  nothing beyond that has been tried on it — no indexing, no searching.
- Nothing is interruptible while a model downloads — that wait has no unit
  boundaries to check a cancellation between.
- The Emacs package does not install or update the binary yet. It checks the
  version in both directions and warns; there is nothing to fetch with.

[Unreleased]: https://github.com/alberti42/org-semantic/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/alberti42/org-semantic/releases/tag/v0.2.0
[0.1.0]: https://github.com/alberti42/org-semantic/releases/tag/v0.1.0
