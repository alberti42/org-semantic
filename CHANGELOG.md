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

## [0.5.0] — 2026-08-16

Binary version 0.4.0, and this release needs it. The floor moves to 0.4.0 with
it, because an older binary gets the first two entries below **silently** wrong:
it still loads a second model on a long rebuild, and it still loads a model to
discover that an index it cannot read is unreadable. Nothing about either is
visible from Emacs, which is what the floor is for.

One setting is removed. If `org-semantic-conserve-memory` is in your
configuration, delete it — see below for what it did and why nothing replaces
it.

### Added

- `org-semantic-wait-for-index`. Off by default. Set it and a search for a vault
  this Emacs is indexing is held, and sent when the run replies, so what you read
  is never a version behind. The buffer says it is waiting.

  A run this Emacs started needs nothing from the server: the run answers the
  request that started it, and that reply is the notification.
  `org-semantic-index-finished-functions` is the new hook it runs, on success and
  on failure alike. A run in another process is **refused** rather than waited
  for — the buffer says the vault is being indexed elsewhere and asks you to
  search again. Nothing polls: this Emacs can neither hear that run end nor stop
  it, so a timer would wait on a process that might stall.

### Fixed

- `indexing` now reports a run in **any** process, not only one this server
  started. A `serve` is spawned per editor, so an index run by a shell, a cron
  job or another Emacs was invisible: a search during one answered
  `indexing: false`, and told the client its results were current while the index
  underneath was being rewritten. The vault's lock file is the one thing those
  processes share, so that is what is read, using the same staleness rule
  `Claim` already applies — a lock whose owner has died is not a run.

### Changed

- The server never holds two copies of the embedding model. An indexing run
  shares the resident one, however long the run is. A search during a rebuild
  waits for at most one embedding batch, which is a p90 of about 1.7 seconds.
- A search that cannot be answered no longer loads a model to find that out. A
  vault left behind by an index-layout change refused each search in about
  190 ms, and did it again on every search. It is now about 1 ms.
- `index` reports a steady speed. Chunks are still grouped by length, which is
  what keeps the batches efficient, but the batches now run in a shuffled order
  instead of shortest first. The rate on the progress line used to start at about
  four times the true speed and fall for the whole run, which made the estimated
  time wrong from the first report. The index itself is unchanged: the vectors are
  identical byte for byte, and the run takes the same time.

### Removed

- `conserveMemory` on the `index` request, and `org-semantic-conserve-memory` in
  Emacs. The option chose between one model and two; there is only one now.
  Sending it does nothing rather than failing, so an older client is not broken.

  It bought a p90 of 41 ms instead of 1.7 seconds for a search during a long
  rebuild. It cost 229 MB on the small English model and more on the larger ones,
  and that memory was never returned — the process stayed at its high mark until
  restarted. It also could not apply at all unless the vault's index was both
  readable and already searched, so the cases where a rebuild is most likely — no
  index yet, a layout change, a new model — never reached it.

## [0.4.1] — 2026-08-15

Binary version 0.3.0, unchanged — there is nothing to download, and an existing
binary keeps working. This release is the Emacs package alone.

**Two commands were renamed or removed**, which a patch release would not
normally do. It is here rather than in 0.5.0 because the package is days old and
both names shipped in 0.4.0: `M-x org-semantic-install` becomes `M-x
org-semantic-binary-install`, and `M-x org-semantic-show-install-manual` is gone.
If either is in your configuration or a keybinding, update it — there is no
alias, so an old name fails as an unknown command rather than silently.

### Changed

- A missing binary is now a question rather than a message. It offers the
  download and the build instructions, and the download runs from the answer —
  so the search or index that asked for a binary carries on once it lands,
  instead of ending in a sentence telling you what to type. The save hook is
  the exception: it reports and never asks, since nobody is waiting on it.
- `M-x org-semantic-install` is now `M-x org-semantic-binary-install`. It
  installs the binary, not the package, and the old name left which of the two
  to be guessed. **There is no alias**: the old name is gone.
- `M-x org-semantic-show-install-manual` is gone. Opening the build
  instructions is one of the answers to the question above, which is where it
  was ever wanted — it did not need to be a command of its own.

## [0.4.0] — 2026-08-15

Binary version 0.3.0, unchanged — there is nothing new to download, and an
existing binary keeps working. This release is the Emacs package and the
release machinery around it.

Getting a binary stops being something you do outside Emacs. `M-x
org-semantic-install` fetches the signed one for your platform, checks it
against the release's own `SHA256SUMS`, and puts it where the package already
looks — so a package-manager install is now the whole setup. For the two people
that does not serve — a platform with no published build, and anyone who would
rather compile what they run — `M-x org-semantic-show-install-manual` opens the
instructions, and every release now carries a source archive with a published
checksum.

### Added

- `M-x org-semantic-install` downloads the binary for this platform into
  `org-semantic-install-directory`, verifies it against the release's
  `SHA256SUMS`, and asks it for its version before reporting success. It takes
  the release matching the package rather than the newest one, so what arrives
  is the binary this elisp was written against.
- `M-x org-semantic-show-install-manual` opens the manual on building it
  yourself.
- Every release now carries `org-semantic-<version>-src.tar.gz`, built with
  `git archive` and `gzip -n` so a given tag is the same bytes for ever, with
  its hash in `SHA256SUMS`. Pin it rather than the `Source code (tar.gz)`
  GitHub attaches by itself, whose checksum GitHub has moved before. It holds
  what builds and tests the project — not the screenshots or the vendored HTML
  theme — and both test suites run from it unchanged.

### Changed

- **Release assets are renamed, and a script pinning the old names will need
  updating.** They now carry the version and say what is inside:
  `org-semantic-0.4.0-bin-aarch64-macos.tar.gz` rather than
  `org-semantic-aarch64-macos.tar.gz`. Previously one filename served every
  release, so a downloaded file could not say what it was and a `SHA256SUMS`
  line could not be told from another release's. `bin` rather than `cli`
  because the same binary is the server the Emacs package drives.
- The manual gained *What it touches*, which answers what indexing does with
  your notes before you point it at them: read and never written, nothing about
  them leaves the machine, and the network is reached only to fetch the model
  and the language classifier. It also lists public vaults to try it on.
- The README leads with the Emacs interface, and its worked example is now a
  public vault anyone can clone, so the whole thing is reproducible.

### Fixed

- A hit's address no longer wraps to column 0. It carried no `wrap-prefix`
  where the passage lines below it always had one, so an outline path too long
  for the window continued further left than anything else in the buffer and
  read as a broken line. Deep breadcrumbs with a long tag string are what
  reach it.

## [0.3.0] — 2026-08-14

Binary version 0.3.0, and this release needs it: the package calls a `download`
method 0.2.0 has no answer for.

**Rebuild the semantic index once, and searching by meaning will not work until
you do**: `org-semantic index <vault> --full`, or `C-u C-u M-x
org-semantic-reindex`. The index now records a language per passage, so its
layout version moved and an older index is refused rather than answering every
`lang:` query with nothing. The word index is unaffected, and rebuilding it is
seconds in any case.

Two things this release is for. `lang:` now narrows a search by meaning as well
as by word, which is the one predicate that used to be honoured on one side and
refused on the other. And the Emacs client stops needing to be driven: it keeps a
vault's indexes current as notes are saved, takes a function for a vault whose
identity is worked out rather than written down, and asks what to do about a
failure instead of drawing one.

### Fixed

- **Previewing a hit could replace the list it came from.** `display-buffer` is
  free to choose the selected window, and the selected window is the results
  buffer — so walking down with `n` reached the last hit and the list was gone,
  with a note in its place. A preview now refuses that window; `RET`, which means
  take me there, still selects.
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

- **A kept block's `#+begin_` line is inside the passage's span.** It never was:
  the marker is not part of a chunk's text while its body is, so a passage
  beginning inside a long block started at the first line of code and ran to an
  `#+end_` with no beginning. Read back, that is a block that never opened — org
  fontified the code as prose and a reader saw a stray end marker. Measured at
  6..96 on a 90-line block whose marker was line 5; it is 5..96 now.

  And **a blank line inside a block no longer divides it**, which was the other
  half: a blank line ends a paragraph, so a quote of two paragraphs became two
  chunks and each kept only the marker on its own side — one opening a block it
  never closed, the other closing one that never opened. Org treats a block as one
  element and its blank lines as body; so does this now, which means a block is
  whole in every passage of it.

  Every block kind, since the marker rule does not vary with the policy: `src`,
  `example`, `quote`, `verse` and anything unrecognised. Needs the semantic or
  word index rebuilding to take effect on notes already indexed, and costs
  nothing if you do not.

### Added

- **`org-semantic-vault-root` may be a function**, for a vault whose identity has
  to be worked out rather than written down — a package that already tracks which
  collection of notes is current, and switches between them during a session. It
  is asked on every question about a vault, returns a directory or nil, and nil
  means "no vault here", which is a complete answer rather than a failure to
  recover from.

  This exists because the alternative was advice. Something like vulpea had no way
  to say "the vault is whichever one is open now": the setting held a fixed string,
  so answering that meant `advice-add` on `org-semantic-vault` from outside. Now
  the setting can express it, and any package — org-roam, `project.el`, your own
  init — can answer without reaching into this one.

  A function is legal only as the **global** value. `safe-local-variable` refuses
  one from a `.dir-locals.el`, since a directory you merely visit could otherwise
  run whatever it liked, and a declared function is ignored rather than obeyed if
  it is marked safe by hand. A declaration says which directory a vault is; how to
  work one out belongs to your configuration.
- **`lang:` narrows a search by meaning too.** It answered `search --lexical`
  alone and was refused on the other side, because only the word index recorded a
  language: a language picks a stemmer, and an embedding is not stemmed. That is
  the right answer to *retrieving* across languages — which is a question about the
  embedding model, and still is — and the wrong answer to "show me only the German
  notes", which no model can do and which a label can. Both indexes now write a
  language onto every chunk, from the same `# ltex:` declarations and the same
  classifier, and both honour `lang:` and `-lang:`. Needs binary 0.3.0.

  **Rebuild the semantic index once to get it: `index --full`.** An index built
  before this parses perfectly and has the field empty, so every `lang:` query
  would answer with nothing and say nothing — the layout version is raised so that
  it asks for the rebuild instead of quietly doing that. Nothing else about the
  format moves, and the word index is unaffected.

  **And `languages` now defines the semantic index as well**, so changing the list
  — the order included, since the first entry is the vault's default — is refused
  until the index is rebuilt under it. That is minutes of re-embedding where the
  word index is seconds, which is the price of the label being trustworthy;
  applying it silently would leave every note you had not edited answering under
  the language it was given last time. Settle the list when you first index.

- **A vault's index need not sit beside its notes.** A `vault.json` in
  `.org-semantic/` names where the notes are, and then the vault directory holds
  nothing but the index — for notes in a synced folder that should not have
  vectors rewritten under it, or for several vaults keeping their indexes together
  under one cache directory. Needs binary 0.3.0.

  Every command still takes the one path it always took, and nothing about the
  index format changes, so an existing index is read by the new binary as it
  stands. `models <vault>` prints both roots, which is the only way to see what an
  index describes once the two can differ. Both keys are optional and merged over
  the defaults; an unknown key or a version this binary does not know is refused
  rather than half-read; and a vault named by `notes` may not name notes of its
  own — one hop, not a chain.

  Two things are now said rather than left to be found: a vault with no notes at
  all, instead of an index of nothing that answers every search with nothing, and
  `.org` files left in the vault directory when the notes are elsewhere, which
  otherwise look exactly like a chunking bug.

  From Emacs, `org-semantic-vault-root` names the vault, as it always did — a hit
  already carries the absolute file it is in, and `org-semantic-auto-reindex-mode`
  asks whether a saved file is in the *notes*, so saving one still reindexes.
  `status` carries `notes` for any client that needs it, and
  `M-x org-semantic-show-status` names both.
- **`org-semantic-auto-reindex-mode`** keeps a vault's indexes current as its
  notes are saved: two seconds after saving stops, whatever changed is reindexed.
  The wait is a debounce (`org-semantic-auto-reindex-delay`), so
  `save-some-buffers` over fifty notes costs one run rather than fifty, and a save
  landing during a run waits for it instead of being refused. A run of one changed
  note is about 70 ms.

  It **will not build** an index that does not exist — that is minutes of
  embedding and a decision — so a vault with nothing built is named once, with
  `M-x org-semantic-reindex` as the thing to press. Successes are silent
  (`org-semantic-auto-reindex-quietly`); failures are said once per vault, because
  an automatic feature that has stopped working looks exactly like one that is
  working.

  It hears about a note through `after-save-hook`, which is every change made by
  editing and none of the others: a rename or a delete in Dired, a `git pull`, a
  folder arriving from a sync. Something that *does* watch the tree — a file
  watcher, another package's index of the same notes — can say so with
  **`org-semantic-auto-reindex-touch`**, and the change is then picked up like a
  save. It takes a vault and not a file, because a run is a vault-wide
  incremental scan: a rename is caught by the arrival of the new name alone,
  since the same scan finds the old one gone.

  The touch does **not** require the mode. The mode is one trigger and not the
  policy: a watcher that reports saves too makes the save hook redundant, so
  that configuration uses the touch alone. Both together cost one run, since
  they share the debounce, and the touch keeps the mode's manners either way —
  the same delay, the same silence, the same refusal to build an index that does
  not exist. Failing both,
  `org-semantic-reindex` still catches up, and being behind costs nothing — a
  search says so when the index is a version old.
- **`org-semantic-results-connector`** and `l` — for logic — in the results buffer:
  join a word query's terms with `and` or `or`. Named for the logic rather than for the wire,
  which spells it as a boolean called `any` — the server's vocabulary, and no
  reason for a reader of Emacs to learn it. `AND`, `OR`, `NOT` and parentheses are
  writable in the query itself, so this is the default rather than the only way to
  say it.
- **Passages are shown with org's own faces** — emphasis, verbatim, headings,
  block markers, links — by fontifying each in a hidden `org-mode` buffer and
  copying the faces back, which is the trick `magit` uses for diffs.
  `org-semantic-results-fontify` turns it off.

  **Only faces are copied and no character moves**, so the nth line of a passage
  is still line `startLine` + n of the note: org's `keymap`, `invisible` and
  `display` are left behind. About 0.8 ms a passage.
- **A link shows its description**, brackets hidden, when `org-link-descriptive`
  is on — the default, and what `org-toggle-link-display` toggles. Hidden by us
  rather than by org, which does it through `org-fold-core` and would need that
  initialised in a buffer that is not an org buffer. A link split across two lines
  is left alone.
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

- **A vault is declared, and one setting is the whole of it.**
  `org-semantic-vault-root` is now a `defcustom`: set it to a directory and that
  is your vault, from any buffer at all — a note, Dired, `*scratch*`, the agenda.
  A vault's own `.dir-locals.el` still overrides it for the notes inside, which is
  the several-vaults answer, and Emacs applies that when the file is opened so
  nothing here searches for anything.

  **The `.org-semantic` directory is no longer consulted**, which is the breaking
  half: a vault that was found only because it had been indexed now needs
  declaring. It held derived data, its place is not the vault's to promise, and
  a vault discovered that way stops being discoverable the moment those indexes
  are allowed to live elsewhere — silently answering with a *different* vault
  rather than with none. Reading a directory's `.dir-locals.el` by hand went with
  it: Emacs decides when directory-local variables apply, and Dired already
  applies them.
- **A failed search asks what to do in the minibuffer** rather than drawing a row
  of buttons in the results buffer. One keystroke per offer — each its label's own
  initial, so `d` for "Download it", `b` for "Build it", `l` for "Search by word
  (lexical)", `q` or `C-g` to leave it — and each says what it costs, since one of them is
  minutes and the other seconds. The buffer keeps the sentence, so there is still
  an account of the empty list once the question is gone. Declining costs nothing:
  `R` indexes the vault whenever you like, which is the same call the offer would
  have made.
- **`org-semantic-canonical-vault` is public**, because an integration naming a
  vault of its own needs it: the server keys everything it holds by the string it
  was given, so a `close` or a `status` spelled another way — a trailing slash, a
  symlinked path — finds nothing and says so cheerfully. It was private, and the
  one integration that needed it carried a copy that would have fallen out of step
  the moment ours changed. `org-semantic--canonical` remains as an obsolete alias.
- **`org-semantic-close` says nothing unless it is called as a command**, and
  returns how many entries were dropped. The caller with a reason to send it is
  one that knows a vault has been left — a vault switch, the last buffer of one
  being killed — and neither is an occasion for a line in the echo area, least of
  all "0 entry/entries dropped" for a vault the server never held. A client that
  wants to report it has the number.
- The message behind a search that has no model reads *"indexing this vault will
  fetch it"* rather than *"index this vault to fetch it"*. It only ever reaches a
  client, which has a keystroke rather than a command line.
- **The query prompt names the ranking**: `Semantic search for:` or `Lexical
  search for:`, with `M-s` and `M-l` changing it while the query is being typed,
  carrying across whatever has been typed. Which index answers is half of what a
  query means, and it used to be a second question — asked before the query and
  only under a prefix argument, so it was settled blind or not at all. `C-u` still
  asks, now for the ranking to start from.
- **The ranking is chosen, not toggled**: `M-s` for semantic, `M-l` for lexical,
  the same two keys in the results buffer as in the prompt, so the gesture is
  learned once. Named as the header and the setting name them, rather than after
  the by-meaning/by-word gloss, so the keys and the screen say one word for one
  thing. A toggle could not be pressed without first knowing which ranking was in
  force, so one key meant two things depending on state you had to go and read.
  Meta rather than control because `C-s` is worth more where it is — a list of
  passages is prose somebody may want to isearch.
- **No letter does two jobs**, in the buffer or in a question. A full rebuild is
  `[b]` whether it is offered as "Rebuild fully" or "Rebuild from scratch", since
  no failure ever offers both and `[r]` would have implied the letter told them
  apart.

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

[Unreleased]: https://github.com/alberti42/org-semantic/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/alberti42/org-semantic/releases/tag/v0.5.0
[0.4.1]: https://github.com/alberti42/org-semantic/releases/tag/v0.4.1
[0.4.0]: https://github.com/alberti42/org-semantic/releases/tag/v0.4.0
[0.3.0]: https://github.com/alberti42/org-semantic/releases/tag/v0.3.0
[0.2.0]: https://github.com/alberti42/org-semantic/releases/tag/v0.2.0
[0.1.0]: https://github.com/alberti42/org-semantic/releases/tag/v0.1.0
