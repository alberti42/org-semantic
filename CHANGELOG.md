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

Cutting a release: move everything under `[Unreleased]` into a new version
heading with today's date, and push the tag. The release workflow reads that
section for the GitHub release body and refuses a tag it cannot find one for.

## [Unreleased]

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
- An Emacs client: `org-semantic-find` draws a results buffer with `next-error`
  navigation, and `org-semantic-reindex` reports progress. No dependency on org
  itself, and none on consult or vertico.
- `ORG_SEMANTIC_CACHE_HOME`, and `org-semantic-cache-home` on the Emacs side,
  for putting the downloaded models somewhere other than `$XDG_CACHE_HOME`.
  A model is 128 MB to 2.24 GB, which is worth being able to move.
- Signed and notarized macOS binaries, built by CI on a version tag.

### Known limitations

- **Apple Silicon only on macOS.** `ort` publishes no prebuilt ONNX Runtime for
  `x86_64-apple-darwin`, so an Intel build does not link. Rosetta does not help:
  there is no x86_64 binary for it to translate.
- The Windows binary is built by CI and has not been run by anyone.
- Nothing is interruptible while a model downloads — that wait has no unit
  boundaries to check a cancellation between.

[Unreleased]: https://github.com/alberti42/org-semantic/commits/main
