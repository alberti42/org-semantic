//! org-semantic — semantic search over a tree of org-mode notes.
//!
//! Prototype.  Commands:
//!
//!   org-semantic index  <vault> [--lexical|--both] [--full|--rehash]
//!                           [--lang en-US,de-DE|auto] [--fold]   (lexical only)
//!   org-semantic search  <vault> <query> [k] [--lexical]  ranked by meaning, or
//!                                                          by words with --lexical
//!   org-semantic chunks <vault> <path-substring>  show chunking, no embedding
//!   org-semantic tokens <vault> [limit]           token-length distribution
//!   org-semantic bench  <vault> [n] [config]      embedding throughput
//!
//! The index lives in `<vault>/.org-semantic/`: a JSON chunk table and a flat
//! little-endian f32 array of embeddings.  There is no ANN index and no
//! database — a vault of a thousand notes is a few megabytes of vectors, and a
//! brute-force dot product over that is exact and takes under a millisecond.

mod serve;

use anyhow::{anyhow, Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use fasttext::FastText;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

/// An embedding model, with the prefixes it was trained to see.
///
/// The prefixes are part of choosing a model, not a detail: BGE prefixes only
/// the query, E5 prefixes both sides, and using one model's convention with
/// another costs retrieval quality **silently** — no error, no warning, just
/// worse answers.  That is why this is a curated table rather than a pass-through
/// to all 40 of fastembed's models: an entry here is a claim that its prefixes
/// have been checked.  Adding one is four lines.
struct Model {
    /// What `--model` accepts and the manifest records.
    name: &'static str,
    which: EmbeddingModel,
    dim: usize,
    /// Prepended when embedding a query.
    query: &'static str,
    /// Prepended when embedding an indexed passage.
    passage: &'static str,
    /// Languages it was trained on, for `--model list`.
    about: &'static str,
}

/// BGE's query prefix, shared by the whole v1.5 English family.
const BGE_QUERY: &str = "Represent this sentence for searching relevant passages: ";

// Laid out as a table on purpose: the columns are what make a missing or
// mismatched prefix visible at a glance, and that is the mistake this list
// exists to prevent.  rustfmt would give each field its own line and lose it.
#[rustfmt::skip]
const MODELS: &[Model] = &[
    Model { name: "bge-small-en", which: EmbeddingModel::BGESmallENV15, dim: 384,
            query: BGE_QUERY, passage: "", about: "English" },
    Model { name: "bge-base-en", which: EmbeddingModel::BGEBaseENV15, dim: 768,
            query: BGE_QUERY, passage: "", about: "English" },
    Model { name: "bge-large-en", which: EmbeddingModel::BGELargeENV15, dim: 1024,
            query: BGE_QUERY, passage: "", about: "English" },
    // E5 is asymmetric: both sides carry a prefix, and omitting the passage one
    // is the quiet mistake this table exists to prevent.
    Model { name: "e5-small", which: EmbeddingModel::MultilingualE5Small, dim: 384,
            query: "query: ", passage: "passage: ", about: "100 languages" },
    Model { name: "e5-base", which: EmbeddingModel::MultilingualE5Base, dim: 768,
            query: "query: ", passage: "passage: ", about: "100 languages" },
    Model { name: "e5-large", which: EmbeddingModel::MultilingualE5Large, dim: 1024,
            query: "query: ", passage: "passage: ", about: "100 languages" },
];

const USAGE: &str = "\
usage: org-semantic <command> <vault> [options]

Two indexes are built and searched separately: a semantic one, which finds
notes by meaning, and a lexical one, which finds them by word.

  index  <vault> [--full|--rehash] [--model NAME] [--config FILE]
         Build the semantic index.  Minutes, and downloads a model once.
  index  <vault> --lexical|--both [--full|--rehash] [--config FILE]
         Build the word index (seconds), or --both in one run.
         Incremental by default; --full rebuilds, --rehash re-reads every note.

  search <vault> <query> [k] [--per-file N] [--model NAME] [--json]
         Rank by meaning: describe what you are after, not its words.
         k bounds the notes shown (default 8); --per-file bounds how many
         passages any one of them may contribute (default 3).  Keeping a
         year of meetings in one meetings.org?  Raise --per-file.
  search <vault> <query> [k] --lexical [--any] [--json]
         Rank by word (BM25, over a per-language stemmed index).  Every
         term must match; --any matches notes carrying any of them.
         Phrases, AND/OR/NOT and parentheses follow tantivy's query
         syntax.  A query may carry predicates:
           tag:x  -tag:x  dir:x  todo:x  lang:x   (lang: is lexical only)

  chunks <vault> <path-substring> [--lexical] [--config FILE] [--model NAME]
         A dry run of `index`: how notes would be split, and what a
         different --config would do, without building anything.

  tokens <vault> [limit] [--model NAME]     token lengths, and what would truncate

  models [vault]                            embedding models, and which are built

  serve                                     JSON-RPC 2.0 over stdio, for an editor

  bench  <vault> [n] [config]               embedding throughput on a slice

Everything about how a vault is indexed is policy, not flags: which languages
it is written in, whether accents are folded, which subtrees are skipped, how
large a passage may get, and what happens to src and example blocks.  It goes
in a JSON file passed with --config, remembered afterwards so later runs need
not repeat it.  Copy config.example.json and edit it.

Each model keeps its own semantic index, so several can be built side by side;
`models <vault>` shows which are.";

const DEFAULT_MODEL: &str = "bge-small-en";

fn model_named(name: &str) -> Result<&'static Model> {
    MODELS.iter().find(|m| m.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
        anyhow!(
            "unknown model `{name}`; known: {}",
            MODELS.iter().map(|m| m.name).collect::<Vec<_>>().join(", ")
        )
    })
}

const STATE_DIR: &str = ".org-semantic";

/// How much of a result list any one note may occupy.
///
/// Two caps rather than one because the two vault shapes fail in opposite
/// directions.  With a note per file, one note matching in five places would
/// spend the whole list on itself, so passages per file must be bounded.  With
/// a year of meetings in a single `meetings.org`, *every* hit comes from one
/// file, and a bound that tight returns three passages however many matched —
/// so it has to be raisable, and the file count has to be a separate number
/// rather than the only one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Limits {
    /// How many distinct notes may appear.
    files: usize,
    /// How many passages any one note may contribute.
    per_file: usize,
}

/// Chosen for a vault of one note per file, which is what most org vaults are.
/// `PER_FILE` is what someone keeping large files raises.
const DEFAULT_FILES: usize = 8;
const DEFAULT_PER_FILE: usize = 3;

impl Default for Limits {
    fn default() -> Self {
        Limits { files: DEFAULT_FILES, per_file: DEFAULT_PER_FILE }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Chunk {
    /// Relative to the vault root.  Relative rather than absolute so the index
    /// survives the vault being moved or renamed, and so `dir:` predicates are
    /// expressible without knowing where the vault lives.
    path: String,
    /// The `:ID:` of the nearest enclosing node, when it has one — this is what
    /// lets Emacs jump through `org-id` rather than by file position.
    id: Option<String>,
    /// Heading path, e.g. "Note title > Section > Subsection".
    heading: String,
    /// Where the owning heading starts: 1-based, in the real file, and the
    /// address a client jumps to.  The line counter advances over lines the
    /// parser drops, so a collapsed source block shifts nothing.
    ///
    /// Named in full because `line` was read as "the line of the hit" — which
    /// it never was.  Every passage of one section reports the same value.
    heading_line: usize,
    /// The raw-file lines this passage was built from, inclusive.
    ///
    /// Wider than the text: a `#+begin_src` collapsed to `[src bash]` still
    /// spans its whole block, so reading the file over this range shows the code
    /// itself.  Consecutive passages of one section overlap by a paragraph,
    /// because `carry_over` restarts each piece with its predecessor's last one.
    #[serde(default)]
    start_line: usize,
    #[serde(default)]
    end_line: usize,
    /// Effective org tags: `#+filetags:` plus every ancestor heading's tags plus
    /// its own.  Org inherits tags down the outline, so a chunk under
    /// `* Project :work:` carries `work` whether or not its own heading says so.
    #[serde(default)]
    tags: Vec<String>,
    /// TODO keyword of the nearest enclosing heading, if it has one.
    #[serde(default)]
    todo: Option<String>,
    /// Priority cookie (`[#A]`) of the nearest enclosing heading.
    #[serde(default)]
    priority: Option<char>,
    /// Language code in effect here, from a `# ltex: language=de-DE` comment or
    /// the configured default.
    lang: String,
    /// FNV over what was embedded, `chunk_key(heading, text)`.
    ///
    /// The identity of a passage now that the text itself is not written down:
    /// it is what the per-passage vector reuse matches on. Eight bytes where the
    /// text averaged 624 characters. There is nothing left to confirm a hit
    /// against, so a 64-bit collision would attach the wrong vector — about
    /// 1e-14 over a corpus this size, and the same risk the manifest's per-file
    /// hashes have always carried.
    #[serde(default)]
    hash: u64,
    /// The passage as it was indexed — placeholders in place of block bodies,
    /// drawers and keywords gone.
    ///
    /// **Never written to disk.** It exists while a note is being indexed,
    /// because it has to be embedded and handed to tantivy, and is empty on
    /// anything read back: a result's text is read from the file over
    /// `start_line..=end_line`, which is both smaller and *truer* — the code a
    /// `[src bash]` stands for is in the file.
    #[serde(skip)]
    text: String,
}

// ---------------------------------------------------------------- collecting

fn org_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Hidden directories hold state, not notes — including this tool's own.
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            // org-attach's store: binaries, and nothing to embed.
            if name == "data" {
                continue;
            }
            org_files(&path, out)?;
        } else if ft.is_file() && path.extension().is_some_and(|e| e == "org") {
            out.push(path);
        }
    }
    Ok(())
}

/// fastText's `lid.176`, product-quantized to 917 kB.  Fetched on first use
/// rather than vendored: it is CC BY-SA 3.0 while this is MIT, and downloading
/// keeps it out of the distribution so ShareAlike never engages.
const LID_URL: &str = "https://dl.fbaipublicfiles.com/fasttext/supervised-models/lid.176.ftz";

/// The classifier and the set of languages it knows.
///
/// The languages are read from the model rather than listed here, so the two can
/// never drift.
struct Lid {
    model: FastText,
    langs: std::collections::HashSet<String>,
}

impl Lid {
    /// Is CODE a language the classifier knows?  Compared on the primary subtag,
    /// since the model speaks `de` where a vault speaks `de-DE`.
    fn knows(&self, code: &str) -> bool {
        self.langs.contains(&lexical::primary_subtag(code))
    }
}

/// The loaded classifier, shared for the run.  `FastText` is `Sync`, and loading
/// costs ~20 ms, which is not worth paying per note.
fn classifier() -> Result<&'static Lid> {
    static LID: OnceLock<Lid> = OnceLock::new();
    if let Some(l) = LID.get() {
        return Ok(l);
    }
    let path = xdg_cache().join("org-semantic").join("lid.176.ftz");
    if !path.exists() {
        let dir = path.parent().expect("joined path has a parent");
        fs::create_dir_all(dir)?;
        eprintln!("fetching the language model from {LID_URL}");
        let bytes = ureq::get(LID_URL).call()?.body_mut().read_to_vec()?;
        // Write beside the target and rename, so an interrupted download does
        // not leave a truncated file that later runs would happily load.
        let tmp = path.with_extension("part");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &path)?;
    }
    let mut model = FastText::new();
    model
        .load_model(path.to_str().ok_or_else(|| anyhow!("model path is not UTF-8"))?)
        .map_err(|e| anyhow!("loading {}: {e}", path.display()))?;
    let langs = model
        .get_labels()
        .map_err(|e| anyhow!("reading the model's languages: {e}"))?
        .0
        .iter()
        .map(|l| l.trim_start_matches("__label__").to_ascii_lowercase())
        .collect();
    let _ = LID.set(Lid { model, langs });
    Ok(LID.get().expect("just set"))
}

/// Fetch and load the classifier before any work starts, so a failed download is
/// an error on the way in rather than a panic part-way through a long run.
///
/// Unconditional even when a single `--lang` means nothing will be classified:
/// the model is still what says whether a language exists, and `index` is
/// already downloading an embedding model two orders of magnitude larger.
fn prepare_lang(lang: &LangConfig) -> Result<()> {
    let lid = classifier()?;
    for l in lang.languages.iter().filter(|l| !lid.knows(l)) {
        eprintln!("  --lang {l}: not a language the classifier knows");
    }
    Ok(())
}

/// How many labels to ask for when the answer is restricted: all of them, so
/// the best *allowed* language is found even when it ranks last.
const LID_LABELS: i32 = 176;

/// Classify prose, returning the code fastText emits — two letters for most of
/// its 176 languages, which is what ltex and the rest of this speak.
///
/// Applied per note rather than per chunk: a chunk can be a two-line heading,
/// and a classifier given two lines guesses.  A whole note is usually enough.
///
/// With a non-empty CANDIDATES the answer is the highest-ranked language in that
/// set, which is the cure for confident nonsense on notes that are mostly
/// attachment links or shell snippets: a vault written in English and German
/// cannot be told one of its notes is Portuguese.  The winning candidate is
/// returned as the caller spelled it, so a regional variant survives.
fn detect_lang(prose: &str, candidates: &[&str]) -> String {
    // Newlines separate documents for fastText, so a multi-line note would be
    // classified by its first line alone.
    let text = prose.replace('\n', " ");
    let k = if candidates.is_empty() { 1 } else { LID_LABELS };
    let preds = classifier()
        .expect("classifier loaded by prepare_lang")
        .model
        .predict(&text, k, 0.0)
        .unwrap_or_default();
    let mut ranked = preds.iter().map(|p| p.label.trim_start_matches("__label__"));
    if candidates.is_empty() {
        return ranked.next().unwrap_or("en").to_string();
    }
    ranked
        .find_map(|code| candidates.iter().find(|c| lang_matches(c, code)))
        // Only reachable if a candidate is not one of the 176, e.g. a typo.
        .unwrap_or(&candidates[0])
        .to_string()
}

/// The value that stands for "classify this note", accepted by `--lang` and
/// spelled as ltex-ls-plus spells it.
const LANG_AUTO: &str = "auto";

/// The magic-comment keyword, spelled as ltex-ls-plus spells it.
const LTEX_KEYWORD: &str = "ltex";

/// Read a language from an ltex magic comment, e.g. `# ltex: language=de-DE`.
///
/// Deliberately ltex-ls-plus' syntax rather than something new: a note that
/// declares its language for grammar checking has already said what this needs
/// to know, and one annotation serving both is better than two that can drift.
/// Fixed at `ltex` rather than configurable.  A custom keyword would defeat the
/// reason for reusing ltex's syntax — that one annotation serves both the
/// grammar checker and this — and, unlike `--lang` and `--fold`, it is recorded
/// nowhere, so forgetting it on a later incremental run would parse the notes it
/// touches differently from the ones it does not.  `# ltex: language=de-DE` is an
/// org comment; writing one costs nothing and requires no ltex.
///
/// Applies from its own line onward, as ltex does, so a note may switch
/// part-way.
fn ltex_language(line: &str) -> Option<String> {
    let t = line.trim().trim_start_matches('#').trim();
    let rest = strip_prefix_ci(t, &format!("{LTEX_KEYWORD}:"))?;
    for part in rest.split_whitespace() {
        if let Some(v) = strip_prefix_ci(part, "language=") {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// A note's path relative to the vault root — what chunks and the manifest
/// store, so the index survives the vault being moved.
fn rel_path(vault: &Path, f: &Path) -> String {
    f.strip_prefix(vault).unwrap_or(f).to_string_lossy().into_owned()
}

/// Which index is being built.  The two apply different block policies on
/// purpose, so chunking has to know which one it is feeding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Semantic,
    Lexical,
}

/// What becomes of a block's body in the semantic index.
///
/// `"placeholder"` is the one worth having: dropping a block outright glues the
/// paragraph before it to the one after, which reads as an adjacency the note
/// never had, and loses the fact that there *was* a snippet — which is part of
/// what the section is about. Leaving `[src bash]` keeps the seam and the fact,
/// without forty lines of shell drowning the prose around it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(untagged)]
enum InSemantic {
    /// `true` embeds the body, `false` drops it.
    Body(bool),
    Marker(Marker),
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Marker {
    Placeholder,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
struct BlockPolicy {
    semantic: InSemantic,
    /// No placeholder here: labelling something in an exact-match index would
    /// only make `[src]` a searchable term.
    lexical: bool,
}

impl BlockPolicy {
    const fn new(semantic: InSemantic, lexical: bool) -> Self {
        BlockPolicy { semantic, lexical }
    }
}

/// Per block kind.  A struct rather than a map so an unknown kind is a typo
/// caught at parse time, not a rule that silently governs nothing.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields, default)]
struct Blocks {
    src: BlockPolicy,
    example: BlockPolicy,
    /// Babel output: generated, often long, and nobody looks for it by meaning.
    results: BlockPolicy,
    /// Quote and verse are prose someone chose to set off, not machine output,
    /// so they stay in both.
    quote: BlockPolicy,
    verse: BlockPolicy,
}

impl Default for Blocks {
    fn default() -> Self {
        use InSemantic::{Body, Marker as M};
        Blocks {
            src: BlockPolicy::new(M(Marker::Placeholder), true),
            example: BlockPolicy::new(M(Marker::Placeholder), true),
            results: BlockPolicy::new(Body(false), true),
            quote: BlockPolicy::new(Body(true), true),
            verse: BlockPolicy::new(Body(true), true),
        }
    }
}

impl Blocks {
    fn of(&self, kind: &str) -> BlockPolicy {
        match kind {
            "src" => self.src,
            "example" => self.example,
            "results" => self.results,
            "quote" => self.quote,
            "verse" => self.verse,
            // An unrecognised block is prose until proven otherwise: better to
            // index something unwanted than to drop a kind we never considered.
            _ => BlockPolicy::new(InSemantic::Body(true), true),
        }
    }
}

/// What to do with a heading's planning line — `DEADLINE:`, `SCHEDULED:`,
/// `CLOSED:`.
///
/// Split by index because the two want opposite things, the same way Babel
/// `#+RESULTS:` does. A date carries almost nothing an embedding can use, and in
/// a project file where nearly every heading has one it would open most chunks
/// with the same shape of noise. Looking one up by word is an ordinary thing to
/// want, though — "what was due on the first of September" is a lexical
/// question — so it stays there.
///
/// No placeholder: unlike a src block there is nothing to say a line was here
/// that the heading does not already say.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
struct PlanningLinePolicy {
    semantic: bool,
    lexical: bool,
}

impl Default for PlanningLinePolicy {
    fn default() -> Self {
        PlanningLinePolicy { semantic: false, lexical: true }
    }
}

impl PlanningLinePolicy {
    fn keeps(&self, target: Target) -> bool {
        match target {
            Target::Semantic => self.semantic,
            Target::Lexical => self.lexical,
        }
    }
}

/// How large a passage may get, in the unit each index actually cares about.
///
/// Two numbers rather than one because the two indexes are bounded by different
/// things. An embedding has a hard context limit, so the semantic budget is in
/// the model's own tokens — a character count is a proxy that drifts with the
/// content: chars-per-token runs about 2 in LaTeX-heavy notes to 4 in prose, and
/// German compounds tokenize worse than English, so one figure in characters
/// means different amounts of context in different notes of the same vault.
///
/// BM25 has no such limit, and the lexical index deliberately loads no
/// tokenizer — that is what makes `index --lexical` a second's work rather than
/// a model download. Its budget is therefore in characters, which it can measure
/// exactly, rather than an approximation of tokens it has no way to count.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
struct Chunking {
    /// Tokens per passage, heading included, as the embedding model counts them.
    semantic_tokens: usize,
    /// Characters per passage for the word index.
    lexical_chars: usize,
}

impl Default for Chunking {
    fn default() -> Self {
        // 350 keeps roughly the granularity the old 1500-character budget
        // produced — a median of 177 tokens and a p90 of 399 on a 951-note
        // vault — so moving to the right unit is not also a change in how
        // coarse a hit is.  Small enough that a hit points at a passage rather
        // than a whole note, large enough to carry its own context.
        Chunking { semantic_tokens: 350, lexical_chars: 1500 }
    }
}

impl Chunking {
    fn of(&self, target: Target) -> usize {
        match target {
            Target::Semantic => self.semantic_tokens,
            Target::Lexical => self.lexical_chars,
        }
    }
}

/// Indexing policy: what in a vault is worth indexing at all.
///
/// Kept in a file the user owns, wherever they like, and named with `--config`.
/// Once given it is cached in the state directory, so later runs need not
/// restate it — forgetting the flag must be safe, or a sticky setting is a trap.
///
/// Compared by **normalized content**, never by the file: reformatting, key
/// order, or writing it as `.eld` in Emacs and passing it over JSON-RPC must all
/// hash the same, or a cosmetic edit would cost a full re-embed.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields, default)]
struct Config {
    /// The languages the vault is written in, as `--lang` spells them: one means
    /// no classification, several restrict the classifier, empty means `auto`.
    ///
    /// Here rather than a flag because it is index-defining and must be the same
    /// on every run: as a flag, forgetting it silently rebuilt the lexical index
    /// with English alone, losing German and Italian stemming without a word.
    languages: Vec<String>,
    /// Fold non-ASCII to ASCII before indexing, so `eleves` matches `élèves`.
    ///
    /// Named for diacritics because that is the case worth having; the filter is
    /// broader (`æ` becomes `ae`).  It does nothing for German, whose stemmer
    /// already strips umlauts.
    fold_diacritics: bool,
    /// What to do with each kind of block.
    blocks: Blocks,
    /// What to do with a heading's `DEADLINE:` / `SCHEDULED:` / `CLOSED:` line.
    planning_line: PlanningLinePolicy,
    /// How large a passage may get, per index.
    chunk: Chunking,
    /// Anything tagged with one of these is not indexed — the tag names what to
    /// leave out, not what to strip from the index.  `noexport` is org's own
    /// "not for consumption" marker and `ARCHIVE` its "put this away"; both
    /// inherit down the outline, which is what makes this a subtree rule rather
    /// than a per-heading one.
    ///
    /// Unknown keys are rejected rather than ignored, for the same reason
    /// unknown flags are: a typo that does nothing looks exactly like a setting
    /// that does nothing.
    exclude_tagged: Vec<String>,
    /// The vault's TODO keywords — this file's `org-todo-keywords`.
    ///
    /// Defaults to org's own default and nothing more. Here rather than
    /// compiled in because it decides what a heading *says*: a keyword we do
    /// not know stays in the title and is embedded with it, and one we invent
    /// is cut out of a title that never had it.
    ///
    /// A file's own `#+TODO:` / `#+SEQ_TODO:` / `#+TYP_TODO:` adds to this for
    /// that file, exactly as it adds to `org-todo-keywords` in Emacs.
    ///
    /// Order is not kept: org's sequence order drives cycling, which nothing
    /// here does. It is a set.
    todo_keywords: Vec<String>,
}

/// A list read as the set it means: sorted and deduplicated, so that reordering
/// it or repeating an entry is not a change worth a re-embed.
fn as_set(items: &[String]) -> Vec<String> {
    let mut v = items.to_vec();
    v.sort();
    v.dedup();
    v
}

impl Default for Config {
    fn default() -> Self {
        Config {
            languages: vec!["en-US".into()],
            fold_diacritics: false,
            blocks: Blocks::default(),
            planning_line: PlanningLinePolicy::default(),
            chunk: Chunking::default(),
            exclude_tagged: vec!["noexport".into(), "ARCHIVE".into()],
            todo_keywords: DEFAULT_TODO_KEYWORDS.iter().map(|s| (*s).into()).collect(),
        }
    }
}

impl Config {
    /// The bytes the hash is taken over: defaults filled in, lists sorted and
    /// deduplicated, so two configs that *mean* the same thing agree.
    fn canonical(&self) -> String {
        serde_json::to_string(&Config {
            exclude_tagged: as_set(&self.exclude_tagged),
            todo_keywords: as_set(&self.todo_keywords),
            ..self.clone()
        })
        .unwrap_or_default()
    }

    const KINDS: [&'static str; 5] = ["src", "example", "results", "quote", "verse"];

    /// The policy **as one index sees it**, hashed.
    ///
    /// Per index, not global: `blocks.src.lexical` cannot affect an embedding,
    /// so changing it must not force a re-embed.  A single hash over the whole
    /// config made every lexical-only edit cost minutes on the semantic side.
    fn hash_for(&self, target: Target) -> u64 {
        // Both indexes share what is excluded, and what counts as a keyword:
        // a keyword is cut out of the heading before either one sees it.
        let mut key = format!(
            "tags={};todo={}",
            as_set(&self.exclude_tagged).join(","),
            as_set(&self.todo_keywords).join(",")
        );
        if target == Target::Lexical {
            // Language and folding pick stemmers, which only this index has.
            key.push_str(&format!(
                ";langs={};fold={}",
                self.languages.join(","),
                self.fold_diacritics
            ));
        }
        for kind in Self::KINDS {
            let p = self.blocks.of(kind);
            let v = match target {
                Target::Semantic => describe_semantic(p.semantic).to_string(),
                Target::Lexical => p.lexical.to_string(),
            };
            key.push_str(&format!(";{kind}={v}"));
        }
        key.push_str(&format!(";planning_line={}", self.planning_line.keeps(target)));
        // Only this index's own budget: the other one cannot move a boundary here.
        key.push_str(&format!(";chunk={}", self.chunk.of(target)));
        content_hash(key.as_bytes())
    }

    /// Does a chunk carrying TAGS fall outside what should be indexed?
    fn excluded(&self, tags: &[String]) -> bool {
        tags.iter().any(|t| self.exclude_tagged.iter().any(|x| x.eq_ignore_ascii_case(t)))
    }

    fn read(path: &Path) -> Result<Config> {
        let bytes = fs::read(path).with_context(|| format!("reading config {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing config {}", path.display()))
    }

    /// What changed, in words, so an error can say which setting moved rather
    /// than that something did.
    /// What changed *for this index*, in words.
    ///
    /// Compared canonically, or reordering a list would be reported as a change
    /// it is not — the cached copy is sorted and a hand-written file need not
    /// be.  Filtered by target, so an error about the semantic index never cites
    /// a lexical-only setting.
    fn differences(&self, other: &Config, target: Target) -> Vec<String> {
        let mut out = Vec::new();
        for (name, mine, theirs) in [
            ("exclude_tagged", as_set(&self.exclude_tagged), as_set(&other.exclude_tagged)),
            ("todo_keywords", as_set(&self.todo_keywords), as_set(&other.todo_keywords)),
        ] {
            if mine != theirs {
                out.push(format!("{name}: was [{}], now [{}]", theirs.join(", "), mine.join(", ")));
            }
        }
        if target == Target::Lexical {
            if self.languages != other.languages {
                out.push(format!(
                    "languages: was [{}], now [{}]",
                    other.languages.join(", "),
                    self.languages.join(", ")
                ));
            }
            if self.fold_diacritics != other.fold_diacritics {
                out.push(format!(
                    "fold_diacritics: was {}, now {}",
                    other.fold_diacritics, self.fold_diacritics
                ));
            }
        }
        for kind in Self::KINDS {
            let (a, b) = (self.blocks.of(kind), other.blocks.of(kind));
            match target {
                Target::Semantic if a.semantic != b.semantic => out.push(format!(
                    "blocks.{kind}.semantic: was {}, now {}",
                    describe_semantic(b.semantic),
                    describe_semantic(a.semantic)
                )),
                Target::Lexical if a.lexical != b.lexical => {
                    out.push(format!("blocks.{kind}.lexical: was {}, now {}", b.lexical, a.lexical))
                }
                _ => {}
            }
        }
        if self.chunk.of(target) != other.chunk.of(target) {
            let unit = match target {
                Target::Semantic => "semantic_tokens",
                Target::Lexical => "lexical_chars",
            };
            out.push(format!(
                "chunk.{unit}: was {}, now {}",
                other.chunk.of(target),
                self.chunk.of(target)
            ));
        }
        let (mine, theirs) = (self.planning_line.keeps(target), other.planning_line.keeps(target));
        if mine != theirs {
            let side = match target {
                Target::Semantic => "semantic",
                Target::Lexical => "lexical",
            };
            out.push(format!("planning_line.{side}: was {theirs}, now {mine}"));
        }
        out
    }
}

fn describe_semantic(v: InSemantic) -> &'static str {
    match v {
        InSemantic::Body(true) => "true",
        InSemantic::Body(false) => "false",
        InSemantic::Marker(_) => "\"placeholder\"",
    }
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

/// The policy to index with: what `--config` names, else what the vault was last
/// indexed with, else the defaults.
fn resolve_config(vault: &Path, given: Option<&Path>) -> Result<Config> {
    // A file the caller named is theirs: a typo in it must be an error, not a
    // setting that quietly does nothing.
    if let Some(p) = given {
        return Config::read(p);
    }
    // The cache is ours, and lives in the directory documented as disposable.
    // If a schema change makes it unreadable it must not brick every command —
    // the policy hash in each manifest still catches the real difference.
    let cached = config_path(&state_dir(vault));
    if cached.exists() {
        match Config::read(&cached) {
            Ok(c) => return Ok(c),
            Err(e) => eprintln!("  ignoring the cached policy ({e}); using the defaults"),
        }
    }
    Ok(Config::default())
}

/// Refuse to act on an index built under a different policy.
///
/// Not a silent rebuild: a config file can change without the user acting — a
/// `git pull` brings a colleague's edit — and spending minutes re-embedding on
/// something they did not do is exactly the surprise to avoid.
///
/// REMEDY is how the caller can say yes, which differs by caller: `--full` for
/// the CLI, a reindex request for an editor that has just been told its own
/// settings no longer match the index it is searching.
/// How the CLI says yes.  `serve` names its own, since an editor has no flags.
const CLI_REMEDY: &str = "pass --full to rebuild under the new one";

fn check_config(
    previous: Option<u64>,
    cfg: &Config,
    previous_cfg: Option<&Config>,
    target: Target,
    remedy: &str,
) -> Result<()> {
    let what = match target {
        Target::Semantic => "semantic",
        Target::Lexical => "lexical",
    };
    let Some(prev) = previous else { return Ok(()) };
    if prev == cfg.hash_for(target) {
        return Ok(());
    }
    // A hash that moved while every setting reads the same means the *shape* of
    // the policy changed, not its content: a setting exists now that did not
    // when this index was written, and the stored copy is silent about it
    // rather than disagreeing.  Saying "the stored policy differs" there sends
    // someone hunting for an edit they never made.
    let detail = previous_cfg
        .map(|old| cfg.differences(old, target).join("; "))
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| {
            "no setting reads differently, so this index predates one that now exists".into()
        });
    Err(anyhow!(
        "the {what} index was built under a different policy — {detail}\n\
         {remedy}, or restore the previous setting"
    ))
}

/// Where a note's language comes from: the languages the vault is written in,
/// as `--lang` spelled them.
///
/// The length of the list decides the whole policy:
///
/// - **one** — every note that does not declare its own is that language, and
///   the classifier never runs (nor is its model downloaded)
/// - **several** — the classifier runs, restricted to answering with one of them
/// - **none** — `--lang auto`: the classifier runs unrestricted, all 176
#[derive(Clone, Debug)]
struct LangConfig {
    /// Mirrors `lsp-ltex-plus-language`, whose default is "en-US".
    languages: Vec<String>,
}

impl Default for LangConfig {
    fn default() -> Self {
        LangConfig { languages: vec!["en-US".into()] }
    }
}

impl LangConfig {
    /// From a comma-separated spelling, as the tests write it.  The policy file
    /// gives an array, so nothing in production parses this.
    #[cfg(test)]
    fn parse(spec: &str) -> Self {
        let languages = if spec.trim().eq_ignore_ascii_case(LANG_AUTO) {
            Vec::new()
        } else {
            spec.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
        };
        LangConfig { languages }
    }

    /// Does a note without its own declaration need classifying?
    fn detects(&self) -> bool {
        self.languages.len() != 1
    }

    /// What the classifier may answer with; empty means anything.
    fn candidates(&self) -> Vec<&str> {
        if self.detects() {
            self.languages.iter().map(String::as_str).collect()
        } else {
            Vec::new()
        }
    }

    /// The language written on a chunk that declares nothing.  With one
    /// language that is the answer; otherwise it is a placeholder the
    /// classifier replaces once it has seen the whole note.
    fn undeclared(&self) -> &str {
        match self.languages.as_slice() {
            [only] => only,
            _ => LANG_AUTO,
        }
    }

    /// What a note's own `# ltex: language=…` is worth.
    ///
    /// A declaration wins over everything, including a `--lang` list that does
    /// not mention it — the list says what the classifier may *guess*, never
    /// what a note may *state*.  The one exception is a language the classifier
    /// has never heard of, which is a typo far more often than a real language;
    /// that falls back to the first configured language, the vault's default.
    /// Under `auto` there is no such default, so the note is classified instead.
    fn accept_declared(&self, declared: &str, note: &str) -> String {
        if classifier().is_ok_and(|lid| lid.knows(declared)) {
            return declared.to_string();
        }
        match self.languages.first() {
            Some(default) => {
                eprintln!("  {note}: unknown language `{declared}`, using `{default}`");
                default.clone()
            }
            None => {
                eprintln!("  {note}: unknown language `{declared}`, classifying instead");
                self.undeclared().to_string()
            }
        }
    }
}

// ----------------------------------------------------------------- chunking

/// TODO keywords recognised when a file declares none of its own.
///
/// Exactly org's default — `org-todo-keywords` is `((sequence "TODO" "DONE"))`
/// and nothing more. This list used to carry the familiar NEXT / WAITING /
/// SOMEDAY / CANCELLED set as well, which is Bernt Hansen's org guide and Doom
/// rather than org, and guessing at it was wrong in the direction that hides
/// the mistake: under stock settings `* NEXT Rewire the trap` really does have
/// no keyword and really is titled "NEXT Rewire the trap", so stripping NEXT
/// meant disagreeing with the user's own Emacs and then embedding our version.
///
/// A vault whose owner configured more says so in `todo_keywords`, or per file
/// with `#+TODO:`. An editor that can read `org-todo-keywords` should pass it
/// through rather than making anyone restate it.
const DEFAULT_TODO_KEYWORDS: &[&str] = &["TODO", "DONE"];

/// Org's planning keywords, from `org-planning-line-re`.
///
/// Matched case-sensitively, as org matches them: `org-element-planning-parser`
/// binds `case-fold-search` to nil, so a sentence opening "Deadline: …" is
/// prose and stays.
const PLANNING_KEYWORDS: [&str; 3] = ["DEADLINE:", "SCHEDULED:", "CLOSED:"];

/// Is this the planning line belonging to the heading just above it?
///
/// Org is strict about where one may sit — `org-at-planning-p` requires the line
/// immediately after the headline — and so is this, because the position is the
/// only thing separating a deadline from a paragraph that begins by quoting one.
/// A single line may carry more than one keyword, which is why this asks whether
/// the line *starts* with any of them rather than trying to parse the timestamps
/// after it: they are metadata either way, and org's own timestamp grammar is
/// more than is needed to decide that.
fn is_planning_line(line: &str) -> bool {
    let t = line.trim_start_matches([' ', '\t']);
    PLANNING_KEYWORDS.iter().any(|k| t.starts_with(k))
}

/// A keyword as written in `#+TODO:`, reduced to the keyword itself.
///
/// Org lets each one carry a fast-selection key and state-change logging in
/// parentheses — `WAIT(w)`, `WAIT(w!)`, `WAIT(w@/!)`, `WAIT(@/@)` — and matches
/// headings against the bare name. Keeping the suffix meant registering a token
/// no heading could ever equal, so a file declaring `WAIT(w@/!)` had its WAIT
/// headings left with the keyword sitting in their title, and in the text that
/// was then embedded.
fn todo_keyword_name(spec: &str) -> &str {
    match spec.find('(') {
        Some(i) => &spec[..i],
        None => spec,
    }
}

/// What a heading line says, once its markup is taken off.
struct Headline {
    level: usize,
    todo: Option<String>,
    priority: Option<char>,
    text: String,
    tags: Vec<String>,
}

/// Parse `** TODO [#A] Fix the laser :hardware:urgent:`.
///
/// Each part is optional and order is fixed by org: stars, keyword, priority,
/// title, tags.  Anything unrecognised stays in the title rather than being
/// dropped, so a heading is never silently truncated.
fn parse_headline(line: &str, todo_keywords: &[String]) -> Option<Headline> {
    let level = heading_level(line)?;
    let mut rest = line[level..].trim();

    let todo = rest
        .split_whitespace()
        .next()
        .filter(|w| todo_keywords.iter().any(|k| k == w))
        .map(str::to_string);
    if let Some(k) = &todo {
        rest = rest[k.len()..].trim_start();
    }

    let mut priority = None;
    if let Some(after) = rest.strip_prefix("[#") {
        let mut cs = after.chars();
        if let (Some(c), Some(']')) = (cs.next(), cs.next()) {
            priority = Some(c);
            rest = after[c.len_utf8() + 1..].trim_start();
        }
    }

    // Tags are a trailing `:a:b:` run, and org allows word characters plus
    // @#%_ inside them.  Checked rather than assumed: a heading ending in a
    // bare ratio like "2:1" must not be read as a tag block.
    let mut tags = Vec::new();
    if let Some((head, last)) = rest.rsplit_once(char::is_whitespace) {
        if last.len() > 2 && last.starts_with(':') && last.ends_with(':') {
            let parts: Vec<&str> = last.trim_matches(':').split(':').collect();
            let ok = !parts.is_empty()
                && parts.iter().all(|t| {
                    !t.is_empty() && t.chars().all(|c| c.is_alphanumeric() || "_@#%-".contains(c))
                });
            if ok {
                tags = parts.iter().map(|t| t.to_string()).collect();
                rest = head.trim_end();
            }
        }
    }

    Some(Headline { level, todo, priority, text: rest.to_string(), tags })
}

/// Split `:a:b:` as written by `#+filetags:`, tolerating a bare space-separated
/// list, which org also accepts.
fn parse_tag_list(s: &str) -> Vec<String> {
    s.split(|c: char| c == ':' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Split TEXT into chunks, one per heading, further split when a section runs
/// past the budget this index is packed to.
///
/// REL is the note's path relative to the vault root, and is what the chunk
/// stores.  PATH is only used to fall back to the filename when a note has no
/// `#+title:`.
///
/// Deliberately not a full org parser: for deciding where one passage ends and
/// the next begins, headings, property drawers and tags are the whole of what
/// matters, and the available Rust org parsers are alpha-stage.
/// Chunk one note.
///
/// LANG is `None` for the semantic index, which has no use for a language: an
/// embedding is not stemmed, so classifying a note there would be labelling for
/// its own sake.  Language belongs to the lexical index, which needs it to pick
/// a stemmer.
/// The line left where a block's body was: enough to say what stood here.
fn placeholder(kind: &str, arg: &str) -> String {
    if arg.is_empty() || arg.starts_with(':') {
        format!("[{kind}]")
    } else {
        format!("[{kind} {arg}]")
    }
}

fn chunk_file(
    path: &Path,
    rel: &str,
    text: &str,
    lang: Option<&LangConfig>,
    cfg: &Config,
    target: Target,
    measure: &dyn Fn(&str) -> usize,
) -> Vec<Chunk> {
    // The unit the caller measures in decides which budget applies: the
    // semantic index counts the model's tokens, the lexical one characters.
    let limit = cfg.chunk.of(target);
    let mut chunks = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    // Tags of each open heading, so a chunk can inherit from every ancestor.
    let mut tag_stack: Vec<Vec<String>> = Vec::new();
    let mut todo_stack: Vec<Option<String>> = Vec::new();
    let mut prio_stack: Vec<Option<char>> = Vec::new();
    let mut title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut file_tags: Vec<String> = Vec::new();
    // The vault's own, which a file may add to below.
    let mut todo_keywords: Vec<String> = cfg.todo_keywords.clone();
    let mut file_id: Option<String> = None;
    let mut cur_id: Option<String> = None;
    let mut cur_line = 1usize;
    // Kept lines, grouped into paragraphs as they arrive, each remembering the
    // raw-file lines it came from.
    let mut paras: Vec<Para> = Vec::new();
    // Whether the last paragraph is still taking lines; a blank line closes it.
    let mut open = false;
    // The paragraph a collapsed block or literal run stands in for, so its span
    // can be widened over the lines it replaced.
    let mut stands_for: Option<usize> = None;
    let mut in_drawer = false;
    // The block whose body we are inside, and whether to keep it.
    let mut in_block: Option<bool> = None;
    // `#+RESULTS:` and bare `: ` fixed-width lines are literal too, but have no
    // `#+end_`; they run until something that is not one of them.
    let mut literal_run: Option<bool> = None;
    let mut seen_heading = false;
    // Set by a heading, cleared by whatever line follows: only the line directly
    // beneath a headline can be its planning line.
    let mut at_planning = false;
    let mut cur_lang = lang.map(|l| l.undeclared().to_string()).unwrap_or_default();

    // Collected first: `#+filetags:` and `#+TODO:` may appear after content, and
    // they apply to the whole file either way.
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = strip_prefix_ci(t, "#+filetags:") {
            file_tags.extend(parse_tag_list(rest));
        } else {
            // All three are org's own in-buffer declarations and all three add
            // keywords: `#+TODO:` and `#+SEQ_TODO:` are stages, `#+TYP_TODO:`
            // is types (who or what rather than how far along).  The
            // distinction decides how `org-todo` cycles, which is not our
            // business — for reading a heading, a keyword is a keyword.
            let declared = ["#+todo:", "#+seq_todo:", "#+typ_todo:"]
                .iter()
                .find_map(|k| strip_prefix_ci(t, k));
            if let Some(rest) = declared {
                todo_keywords.extend(
                    parse_tag_list(rest)
                        .iter()
                        // `|` separates not-done from done.  Both are keywords
                        // and neither belongs in a heading's title, so the
                        // split itself carries nothing we need.
                        .filter(|w| *w != "|")
                        .map(|w| todo_keyword_name(w).to_string())
                        .filter(|w| !w.is_empty()),
                );
            }
        }
    }

    let flush = |chunks: &mut Vec<Chunk>,
                 paras: &[Para],
                 stack: &[String],
                 tag_stack: &[Vec<String>],
                 todo_stack: &[Option<String>],
                 prio_stack: &[Option<char>],
                 title: &str,
                 file_tags: &[String],
                 id: &Option<String>,
                 heading_line: usize,
                 lang: &str| {
        if paras.is_empty() {
            return;
        }
        let heading = if stack.is_empty() {
            title.to_string()
        } else {
            format!("{} > {}", title, stack.join(" > "))
        };
        // File tags, then every ancestor's, deduplicated but order-stable.
        let mut tags: Vec<String> = Vec::new();
        for t in file_tags.iter().chain(tag_stack.iter().flatten()) {
            if !tags.iter().any(|x| x == t) {
                tags.push(t.clone());
            }
        }
        // Excluded subtrees are dropped here, where inheritance has already been
        // resolved: `:noexport:` on an ancestor covers everything beneath it.
        if cfg.excluded(&tags) {
            return;
        }
        let todo = todo_stack.iter().rev().find_map(|t| t.clone());
        let priority = prio_stack.iter().rev().find_map(|p| *p);
        // The heading is prepended to every piece before it is embedded, so it
        // comes out of the budget once, here, where it is known.  The floor
        // guarantees forward progress when a heading path is itself longer than
        // the budget — see the known hole in CLAUDE.md.
        let room = limit.saturating_sub(measure(&heading) + 4).max(32);
        for piece in split_to_fit(paras, measure, room) {
            chunks.push(Chunk {
                path: rel.to_string(),
                id: id.clone(),
                heading: heading.clone(),
                heading_line,
                start_line: piece.start,
                end_line: piece.end,
                tags: tags.clone(),
                todo: todo.clone(),
                priority,
                lang: lang.to_string(),
                hash: chunk_key(&heading, &piece.text),
                text: piece.text,
            });
        }
    };

    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let trimmed = line.trim();

        // Consumed here rather than wherever a `DEADLINE:` turns up, because
        // only this position makes it org's planning line — and cleared by
        // *any* line, so it cannot reach past a property drawer.  Org parses a
        // heading as planning-then-drawer, so a deadline below the drawer is a
        // paragraph that mentions one, and stays.
        let planning = at_planning && is_planning_line(line);
        at_planning = false;
        if planning && !cfg.planning_line.keeps(target) {
            continue;
        }

        if in_drawer {
            if trimmed.eq_ignore_ascii_case(":END:") {
                in_drawer = false;
            } else if let Some(rest) = strip_prefix_ci(trimmed, ":ID:") {
                let id = Some(rest.trim().to_string());
                if seen_heading {
                    cur_id = id;
                } else {
                    file_id = id.clone();
                    cur_id = id;
                }
            }
            continue;
        }
        if trimmed.eq_ignore_ascii_case(":PROPERTIES:") {
            in_drawer = true;
            continue;
        }

        if let Some(h) = parse_headline(line, &todo_keywords) {
            flush(
                &mut chunks,
                &paras,
                &stack,
                &tag_stack,
                &todo_stack,
                &prio_stack,
                &title,
                &file_tags,
                &cur_id,
                cur_line,
                &cur_lang,
            );
            paras.clear();
            open = false;
            let depth = h.level.saturating_sub(1);
            stack.truncate(depth);
            tag_stack.truncate(depth);
            todo_stack.truncate(depth);
            prio_stack.truncate(depth);
            while stack.len() < depth {
                stack.push(String::new());
                tag_stack.push(Vec::new());
                todo_stack.push(None);
                prio_stack.push(None);
            }
            stack.push(h.text);
            tag_stack.push(h.tags);
            todo_stack.push(h.todo);
            prio_stack.push(h.priority);
            // Inherited until the node declares its own in the drawer below.
            cur_id = file_id.clone();
            cur_line = n;
            seen_heading = true;
            at_planning = true;
            continue;
        }

        if let Some(rest) = strip_prefix_ci(trimmed, "#+title:") {
            title = rest.trim().to_string();
            continue;
        }
        // Takes effect from here on, so a note may switch language part-way.
        if let Some((cfg, l)) = lang.zip(ltex_language(line)) {
            flush(
                &mut chunks,
                &paras,
                &stack,
                &tag_stack,
                &todo_stack,
                &prio_stack,
                &title,
                &file_tags,
                &cur_id,
                cur_line,
                &cur_lang,
            );
            paras.clear();
            open = false;
            cur_lang = cfg.accept_declared(&l, rel);
            continue;
        }
        // Blocks: what happens to the body is policy, not prose.
        if let Some(keep) = in_block {
            if strip_prefix_ci(trimmed, "#+end_").is_some() {
                if let Some(i) = stands_for {
                    paras[i].end = n;
                } else if keep {
                    if let Some(last) = paras.last_mut() {
                        last.end = n;
                    }
                }
                stands_for = None;
                in_block = None;
            } else if keep {
                add_line(&mut paras, &mut open, n, line);
            } else if let Some(i) = stands_for {
                // Dropped body: the placeholder standing for it grows instead.
                paras[i].end = n;
            }
            continue;
        }
        if let Some(rest) = strip_prefix_ci(trimmed, "#+begin_") {
            let mut words = rest.split_whitespace();
            let kind = words.next().unwrap_or("").to_ascii_lowercase();
            let arg = words.next().unwrap_or("");
            let p = cfg.blocks.of(&kind);
            in_block = Some(match target {
                Target::Lexical => p.lexical,
                Target::Semantic => p.semantic == InSemantic::Body(true),
            });
            stands_for = None;
            if target == Target::Semantic && matches!(p.semantic, InSemantic::Marker(_)) {
                add_line(&mut paras, &mut open, n, &placeholder(&kind, arg));
                stands_for = Some(paras.len() - 1);
            }
            continue;
        }

        // A `#+RESULTS:` header, or a run of fixed-width lines, which org treats
        // as literal exactly as an example block is.
        let fixed = trimmed == ":" || trimmed.starts_with(": ");
        if let Some(rest) = strip_prefix_ci(trimmed, "#+results:") {
            let _ = rest;
            let p = cfg.blocks.results;
            let keep = match target {
                Target::Lexical => p.lexical,
                Target::Semantic => p.semantic == InSemantic::Body(true),
            };
            stands_for = None;
            if target == Target::Semantic && matches!(p.semantic, InSemantic::Marker(_)) {
                add_line(&mut paras, &mut open, n, &placeholder("results", ""));
                stands_for = Some(paras.len() - 1);
            }
            literal_run = Some(keep);
            continue;
        }
        if fixed {
            let p = if literal_run.is_some() { cfg.blocks.results } else { cfg.blocks.example };
            let keep = literal_run.unwrap_or(match target {
                Target::Lexical => p.lexical,
                Target::Semantic => p.semantic == InSemantic::Body(true),
            });
            if literal_run.is_none() {
                stands_for = None;
                if target == Target::Semantic && matches!(p.semantic, InSemantic::Marker(_)) {
                    // Once for the run, not once per line.
                    add_line(&mut paras, &mut open, n, &placeholder("example", ""));
                    stands_for = Some(paras.len() - 1);
                }
                literal_run = Some(keep);
            }
            if keep {
                add_line(&mut paras, &mut open, n, line);
            } else if let Some(i) = stands_for {
                paras[i].end = n;
            }
            continue;
        }
        literal_run = None;
        stands_for = None;

        // Other keywords, drawer ends and comments are markup, not prose.
        if trimmed.starts_with("#+") || trimmed.starts_with("# ") || trimmed == ":END:" {
            continue;
        }

        add_line(&mut paras, &mut open, n, line);
    }
    flush(
        &mut chunks,
        &paras,
        &stack,
        &tag_stack,
        &todo_stack,
        &prio_stack,
        &title,
        &file_tags,
        &cur_id,
        cur_line,
        &cur_lang,
    );

    // Classification is deferred to here so it sees the note's prose rather than
    // its markup: drawers, keywords and `#+begin_src` are largely ASCII and
    // would pull every note towards English.  Only chunks that took the default
    // are replaced — an explicit `# ltex: language=…` always wins.
    if let Some(lang) = lang.filter(|l| l.detects()) {
        let undeclared = lang.undeclared();
        let prose: Vec<&str> =
            chunks.iter().filter(|c| c.lang == undeclared).map(|c| c.text.as_str()).collect();
        if !prose.is_empty() {
            let detected = detect_lang(&prose.join("\n"), &lang.candidates());
            for c in chunks.iter_mut().filter(|c| c.lang == undeclared) {
                c.lang = detected.clone();
            }
        }
    }
    chunks
}

fn heading_level(line: &str) -> Option<usize> {
    let stars = line.bytes().take_while(|b| *b == b'*').count();
    if stars > 0 && line.as_bytes().get(stars) == Some(&b' ') {
        Some(stars)
    } else {
        None
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // `get` rather than a slice: the notes are full of em-dashes and arrows, and
    // a byte index that lands inside one would panic on a plain `s[..n]`.
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// Add a kept line to the paragraph being built, or start a new one.
///
/// A blank line closes the current paragraph — the same boundary the old flat
/// buffer got from splitting on `\n\n`, but decided here where the line number
/// is still in hand.
fn add_line(paras: &mut Vec<Para>, open: &mut bool, n: usize, text: &str) {
    if text.trim().is_empty() {
        *open = false;
        return;
    }
    if *open {
        if let Some(p) = paras.last_mut() {
            p.text.push('\n');
            p.text.push_str(text);
            p.end = n;
            return;
        }
    }
    paras.push(Para { start: n, end: n, text: text.to_string() });
    *open = true;
}

/// A run of consecutive non-blank lines, and where it came from in the file.
///
/// The span is over the **raw** file, not over the filtered text: a source block
/// collapsed to `[src bash]` still spans its `#+begin_`…`#+end_`, so a preview
/// read from these lines shows the code the index deliberately does not carry.
/// The parser walks every line of the file and only declines to *keep* some, so
/// the numbers cost nothing to record — they are already in hand.
#[derive(Clone, Debug, PartialEq)]
struct Para {
    /// 1-based, inclusive, in the real file.
    start: usize,
    end: usize,
    text: String,
}

impl Para {
    /// A piece cut out of this paragraph — a `hard_split` remnant. It keeps the
    /// whole paragraph's span, since a cut inside one has nothing finer to name.
    fn piece(&self, text: &str) -> Piece {
        Piece { start: self.start, end: self.end, text: text.to_string() }
    }
}

/// One packed passage: the text that gets indexed, and the lines it came from.
///
/// Consecutive pieces may overlap, because `carry_over` deliberately restarts a
/// piece with its predecessor's last paragraph. The spans overlap with them, and
/// truthfully so — that text really is in both.
#[derive(Clone, Debug, PartialEq)]
struct Piece {
    start: usize,
    end: usize,
    text: String,
}

impl Piece {
    fn of(paras: &[&Para]) -> Piece {
        Piece {
            start: paras.first().map(|p| p.start).unwrap_or(1),
            end: paras.last().map(|p| p.end).unwrap_or(1),
            text: join(paras),
        }
    }
}

/// Begin each piece with the tail of the one before it, so an idea cut at a
/// boundary is embedded whole in at least one chunk.
///
/// Measured in paragraphs rather than characters.  A fixed character window —
/// org-db-v3 used 200 — would cut through the middle of a LaTeX display in
/// these notes, and half a display carries less meaning than none of it.
fn carry_over<'a, F>(prev: &[&'a Para], next: &'a Para, fits: F) -> Vec<&'a Para>
where
    F: Fn(&str) -> bool,
{
    // Not when the previous piece was a single paragraph: repeating it whole
    // would make that piece a subset of this one rather than a neighbour.
    if prev.len() > 1 {
        if let Some(&tail) = prev.last() {
            if fits(&format!("{}\n\n{}", tail.text, next.text)) {
                return vec![tail, next];
            }
        }
    }
    vec![next]
}

// --------------------------------------------------------------- measuring

/// BGE-small truncates at 512 tokens, and fastembed applies that silently
/// through `TruncationParams` — an over-long chunk simply loses its tail with
/// no error.  Characters are a poor proxy: this vault runs 3.15 chars/token
/// overall and about 2.0 in the LaTeX-heavy notes, so a 1500-char chunk can be
/// anywhere from 380 to 760 tokens.  Hence a real tokenized pass rather than a
/// character budget.
const TOKEN_LIMIT: usize = 512;

fn n_tokens(tok: &tokenizers::Tokenizer, s: &str) -> usize {
    tok.encode(s, true).map(|e| e.len()).unwrap_or(usize::MAX)
}

/// The lexical index's unit.  Characters, because it loads no tokenizer — that
/// is what keeps `index --lexical` to a second's work — and BM25 has no context
/// limit to be exact about anyway.
fn chars(s: &str) -> usize {
    s.len()
}

/// Greedily pack paragraphs up to BUDGET, in whatever unit MEASURE counts, with
/// one paragraph of overlap between consecutive pieces; hard-split any single
/// paragraph that cannot fit on its own.
///
/// The only packer.  There used to be a second one working in characters, run
/// before this one, so a section could be cut twice on rules that knew nothing
/// of each other.
fn split_to_fit(paras: &[Para], measure: &dyn Fn(&str) -> usize, budget: usize) -> Vec<Piece> {
    let fits = |s: &str| measure(s) <= budget;
    let mut out: Vec<Piece> = Vec::new();
    let mut cur: Vec<&Para> = Vec::new();

    for para in paras {
        if !fits(&para.text) {
            // Nothing to overlap on: flush, then cut this paragraph to size.
            if !cur.is_empty() {
                out.push(Piece::of(&cur));
                cur.clear();
            }
            out.extend(hard_split(para, measure, budget));
            continue;
        }
        if cur.is_empty() {
            cur.push(para);
            continue;
        }
        if fits(&format!("{}\n\n{}", join(&cur), para.text)) {
            cur.push(para);
        } else {
            out.push(Piece::of(&cur));
            cur = carry_over(&cur, para, fits);
        }
    }
    if !cur.is_empty() {
        let last = Piece::of(&cur);
        if !last.text.trim().is_empty() {
            out.push(last);
        }
    }
    out
}

fn join(paras: &[&Para]) -> String {
    paras.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n\n")
}

/// Last resort for a single paragraph over budget: cut on char boundaries,
/// sized from this text's own measured chars-per-token so the guess is close,
/// then verified and shrunk until it actually fits.
fn hard_split(para: &Para, measure: &dyn Fn(&str) -> usize, budget: usize) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut rest = para.text.as_str();
    while !rest.is_empty() {
        let toks = measure(rest);
        if toks <= budget {
            out.push(para.piece(rest));
            break;
        }
        let ratio = rest.len() as f64 / toks.max(1) as f64;
        let mut cut = ((budget as f64 * ratio) as usize).clamp(1, rest.len());
        while cut > 1 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        while cut > 1 && measure(&rest[..cut]) > budget {
            cut = (cut * 9 / 10).max(1);
            while cut > 1 && !rest.is_char_boundary(cut) {
                cut -= 1;
            }
        }
        out.push(para.piece(&rest[..cut]));
        rest = &rest[cut..];
    }
    out
}

// ----------------------------------------------------------------- embedding

fn xdg_cache() -> PathBuf {
    std::env::var("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cache")
    })
}

fn cache_dir() -> PathBuf {
    xdg_cache().join("fastembed")
}

fn model_with(
    which: EmbeddingModel,
    max_length: Option<usize>,
    coreml: bool,
) -> Result<TextEmbedding> {
    let mut opts =
        InitOptions::new(which).with_cache_dir(cache_dir()).with_show_download_progress(true);
    if let Some(n) = max_length {
        opts = opts.with_max_length(n);
    }
    if coreml {
        opts = opts.with_execution_providers(vec![ort::ep::CoreML::default().build()]);
    }
    TextEmbedding::try_new(opts).map_err(|e| anyhow!("loading model: {e}"))
}

fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// ------------------------------------------------------------------ commands

// ------------------------------------------------------------------ filters

/// Metadata predicates pulled out of a query string.
///
/// Kept separate from the free text for two reasons.  They constrain *which*
/// chunks are considered, which is meaningful for any retrieval method, whereas
/// text operators only mean something to a keyword index — so a lexical mode
/// added later applies the very same predicates.  And stripping them stops
/// query syntax reaching the embedder: `tag:work` in the embedded string would
/// have the model looking for notes *about* the words "tag" and "work".
#[derive(Default, Debug, PartialEq)]
struct Filters {
    /// All must be present: successive `tag:` terms narrow.
    tags: Vec<String>,
    not_tags: Vec<String>,
    /// Any may match: `dir:` names alternative subtrees to look in.
    dirs: Vec<String>,
    todos: Vec<String>,
    /// Language codes; any may match.  `lang:de` matches `de-DE`, so a query
    /// need not know the regional variant a note declared.
    langs: Vec<String>,
    text: String,
}

impl Filters {
    fn is_empty(&self) -> bool {
        self.tags.is_empty()
            && self.not_tags.is_empty()
            && self.dirs.is_empty()
            && self.todos.is_empty()
            && self.langs.is_empty()
    }

    fn matches(&self, c: &Chunk) -> bool {
        if !self.tags.iter().all(|t| c.tags.iter().any(|x| x.eq_ignore_ascii_case(t))) {
            return false;
        }
        if self.not_tags.iter().any(|t| c.tags.iter().any(|x| x.eq_ignore_ascii_case(t))) {
            return false;
        }
        if !self.todos.is_empty()
            && !self
                .todos
                .iter()
                .any(|t| c.todo.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(t)))
        {
            return false;
        }
        if !self.dirs.is_empty() && !self.dirs.iter().any(|d| under(&c.path, d)) {
            return false;
        }
        true
    }
}

/// Does a chunk's language answer to WANT?
///
/// Matched at subtag boundaries, so `lang:de` finds `de-DE` and `de-AT` while
/// `lang:de-DE` finds only the one.
fn lang_matches(c: &str, want: &str) -> bool {
    c.eq_ignore_ascii_case(want)
        || c.to_ascii_lowercase().starts_with(&format!("{}-", want.to_ascii_lowercase()))
}

/// Is PATH inside directory D?  Compared component-wise so that `dir:03 Lit`
/// does not match `03 Literature review/…`.
fn under(path: &str, d: &str) -> bool {
    let d = d.trim_end_matches('/');
    if d.is_empty() {
        return true;
    }
    path.strip_prefix(d).is_some_and(|r| r.starts_with('/'))
}

/// Split a query into predicates and free text.
///
/// Accepts `key:value`, `key:"value with spaces"` and a leading `-` to negate a
/// tag.  Anything unrecognised stays in the free text, so a colon inside an
/// ordinary word — a URL, a ratio — is never mistaken for a predicate.
fn parse_query(q: &str) -> Filters {
    const KEYS: [&str; 4] = ["tag", "dir", "todo", "lang"];
    let mut f = Filters::default();
    let mut text: Vec<String> = Vec::new();
    let mut rest = q.trim();

    while !rest.is_empty() {
        let (tok, tail) = next_token(rest);
        rest = tail;
        let (neg, body) = match tok.strip_prefix('-') {
            Some(b) => (true, b),
            None => (false, tok.as_str()),
        };
        match body.split_once(':') {
            Some((k, v)) if KEYS.contains(&k.to_ascii_lowercase().as_str()) && !v.is_empty() => {
                let v = v.trim_matches('"').to_string();
                match k.to_ascii_lowercase().as_str() {
                    "tag" if neg => f.not_tags.push(v),
                    "tag" => f.tags.push(v),
                    "dir" => f.dirs.push(v),
                    "lang" => f.langs.push(v),
                    _ => f.todos.push(v),
                }
            }
            _ => text.push(tok),
        }
    }
    f.text = text.join(" ");
    f
}

/// One whitespace-separated token, except that a quoted run counts as one —
/// including after a `key:`, so `dir:"03 Literature review"` survives.
fn next_token(s: &str) -> (String, &str) {
    let s = s.trim_start();
    let mut out = String::new();
    let mut in_quotes = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                out.push(c);
            }
            c if c.is_whitespace() && !in_quotes => return (out, &s[i..]),
            c => out.push(c),
        }
    }
    (out, "")
}

// ----------------------------------------------------------- lexical index

/// A tantivy index over the same chunks, for the exact-word matching
/// embeddings are poor at.
///
/// Derived, never authoritative: `chunks.json` is the source of truth, and this
/// can be discarded and rebuilt at any time.  A document therefore identifies
/// its chunk by `(path, ord)` rather than by position — chunk positions shift
/// whenever a note gains or loses a chunk, so a stored index would silently
/// point at the wrong text after any edit.
mod lexical {
    use super::{ancestor_dirs, Chunk, Filters};
    use anyhow::{anyhow, Result};
    use std::collections::HashMap;
    use std::path::Path;
    use tantivy::collector::TopDocs;
    use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
    use tantivy::schema::{
        Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,
        TEXT,
    };
    use tantivy::tokenizer::{
        AsciiFoldingFilter, Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer,
        TextAnalyzer,
    };
    use tantivy::{Index, IndexWriter, TantivyDocument, Term};

    /// How text is analysed before it is indexed.
    ///
    /// Baked into the index: querying with one analyzer against tokens produced
    /// by another fails silently, returning nothing rather than erroring.  It is
    /// therefore *derived from the chunks* — indexing and searching compute the
    /// same value independently — and stored beside the index so that a change
    /// is noticed and forces a rebuild.
    #[derive(Clone, PartialEq, Eq, Debug)]
    pub struct Analyzer {
        /// Primary subtags present in the vault, sorted: `["de", "en"]`.
        ///
        /// A field's analyzer is fixed and tantivy cannot detect a document's
        /// language, so each language gets its own body field and every chunk
        /// goes into exactly one — a partition, not a copy.  Nothing is indexed
        /// under a language it is not, and no chunk is scored twice.
        pub langs: Vec<String>,
        /// Fold accents, so `eleves` matches `élèves`.
        ///
        /// Off by default, and worth less than it looks for German: the German
        /// Snowball stemmer already strips umlauts, so `Worter` finds `Wörter`
        /// without it.  Where it earns its place is French, Spanish and
        /// Portuguese, whose stemmers keep accents — there `eleves` finds
        /// nothing until this is on.
        pub fold: bool,
    }

    /// Primary subtag of a language code: `de-DE` becomes `de`, so regional
    /// variants share one analyzer rather than splitting the index further.
    pub fn primary_subtag(code: &str) -> String {
        code.split(['-', '_']).next().unwrap_or(code).to_ascii_lowercase()
    }

    impl Analyzer {
        /// The analyzer an incremental update needs: everything PREVIOUS knew,
        /// plus any language the new chunks introduce.
        ///
        /// The set only ever grows.  A language that has left the corpus keeps
        /// an empty field, which costs nothing, whereas dropping it would change
        /// the schema and force a rebuild for no gain.
        pub fn widen(previous: Option<&Analyzer>, chunks: &[Chunk], fold: bool) -> Self {
            // No fallback to `en` on an empty chunk list: a run with nothing
            // stale would union it into an existing set, which reads as a
            // schema change and forces a rebuild that changes nothing.
            let mut langs: Vec<String> = chunks.iter().map(|c| primary_subtag(&c.lang)).collect();
            if let Some(p) = previous {
                langs.extend(p.langs.iter().cloned());
            }
            langs.sort();
            langs.dedup();
            if langs.is_empty() {
                langs.push("en".into());
            }
            Analyzer { langs, fold }
        }

        /// Identifies both the analyzer and the schema it was built for, and is
        /// stored beside the index.  `v2` is where the index became
        /// self-contained; bump it whenever the schema changes, so a stale index
        /// is discarded rather than opened against the wrong schema.
        pub fn key(&self) -> String {
            format!("v3 langs={} fold={}", self.langs.join("+"), self.fold)
        }

        /// Rebuild the analyzer from a stored key.  This is what lets the
        /// lexical index be searched without `chunks.json`: the languages come
        /// back from the index's own metadata rather than from the corpus.
        pub fn from_key(key: &str) -> Option<Self> {
            let rest = key.strip_prefix("v3 ")?;
            let (langs, fold) = rest.split_once(" fold=")?;
            Some(Analyzer {
                langs: langs.strip_prefix("langs=")?.split('+').map(String::from).collect(),
                fold: fold.trim().parse().ok()?,
            })
        }

        fn language(subtag: &str) -> Option<Language> {
            match subtag {
                "en" => Some(Language::English),
                "de" => Some(Language::German),
                "fr" => Some(Language::French),
                "it" => Some(Language::Italian),
                "es" => Some(Language::Spanish),
                "nl" => Some(Language::Dutch),
                "pt" => Some(Language::Portuguese),
                "ru" => Some(Language::Russian),
                "sv" => Some(Language::Swedish),
                "no" => Some(Language::Norwegian),
                "da" => Some(Language::Danish),
                "fi" => Some(Language::Finnish),
                _ => None,
            }
        }

        fn field_name(subtag: &str) -> String {
            format!("body_{subtag}")
        }

        /// A language Snowball does not cover still gets its own field; it is
        /// lower-cased but not stemmed, which is exact matching and perfectly
        /// usable.
        fn register(&self, index: &Index) -> Result<()> {
            for l in &self.langs {
                let mut b = TextAnalyzer::builder(SimpleTokenizer::default())
                    .filter(RemoveLongFilter::limit(40))
                    .filter(LowerCaser)
                    .dynamic();
                if self.fold {
                    b = b.filter_dynamic(AsciiFoldingFilter);
                }
                if let Some(lang) = Self::language(l) {
                    b = b.filter_dynamic(Stemmer::new(lang));
                }
                index.tokenizers().register(&Self::field_name(l), b.build());
            }
            Ok(())
        }
    }

    pub struct Fields {
        pub path: Field,
        pub ord: Field,
        /// One per language, paired with its primary subtag.
        pub bodies: Vec<(String, Field)>,
        pub title: Field,
        pub tags: Field,
        pub dirs: Field,
        pub todo: Field,
        pub lang: Field,
        /// The chunk itself, as JSON.  Stored rather than indexed: it is what
        /// makes a hit answerable without `chunks.json`, and one opaque field
        /// beats plumbing every display field through the schema twice.
        pub chunk: Field,
    }

    pub fn schema(a: &Analyzer) -> (Schema, Fields) {
        let mut b = Schema::builder();
        // STRING is indexed as one raw token; each body field goes through its
        // own language's analyzer.  Tags and directories must stay STRING, or
        // stemming would turn `physics` into `physic` and `tag:physics` would
        // stop matching.
        let path = b.add_text_field("path", STRING | STORED);
        let ord = b.add_u64_field("ord", tantivy::schema::INDEXED | STORED);
        let bodies = a
            .langs
            .iter()
            .map(|l| {
                let name = Analyzer::field_name(l);
                let opts = TextOptions::default().set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer(&name)
                        .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                );
                (l.clone(), b.add_text_field(&name, opts))
            })
            .collect();
        let f = Fields {
            path,
            ord,
            bodies,
            title: b.add_text_field("title", TEXT),
            tags: b.add_text_field("tags", STRING),
            dirs: b.add_text_field("dirs", STRING),
            todo: b.add_text_field("todo", STRING),
            lang: b.add_text_field("lang", STRING),
            chunk: b.add_text_field("chunk", STORED),
        };
        (b.build(), f)
    }

    fn dir_of(state: &Path) -> std::path::PathBuf {
        state.join("tantivy")
    }

    fn key_file(state: &Path) -> std::path::PathBuf {
        dir_of(state).join("analyzer.txt")
    }

    /// The analyzer the stored index was built with.  A plain file read, so it
    /// can be checked before opening the index — the caller must rebuild on a
    /// mismatch rather than let `open_or_create` discard an index nothing then
    /// refills.
    pub fn stored_key(state: &Path) -> Option<String> {
        std::fs::read_to_string(key_file(state)).ok()
    }

    fn open_or_create(state: &Path, a: &Analyzer) -> Result<(Index, Fields)> {
        let d = dir_of(state);
        // The schema depends on which languages exist, so a changed set means a
        // different schema.  Discard rather than try to open the old one: the
        // index is derived, and `sync` refills it.
        if d.exists() && std::fs::read_to_string(key_file(state)).ok().as_deref() != Some(&a.key())
        {
            let _ = std::fs::remove_dir_all(&d);
        }
        std::fs::create_dir_all(&d)?;
        let (schema, fields) = schema(a);
        let index = match Index::open_in_dir(&d) {
            Ok(i) => i,
            Err(_) => Index::create_in_dir(&d, schema)?,
        };
        a.register(&index)?;
        Ok((index, fields))
    }

    fn add(w: &IndexWriter, f: &Fields, c: &Chunk, ord: u64) -> Result<()> {
        let mut doc = TantivyDocument::default();
        doc.add_text(f.path, &c.path);
        doc.add_u64(f.ord, ord);
        // Exactly one body field: the chunk's own language.
        let want = primary_subtag(&c.lang);
        if let Some((_, fld)) = f.bodies.iter().find(|(l, _)| *l == want).or(f.bodies.first()) {
            doc.add_text(*fld, &c.text);
        }
        doc.add_text(f.title, c.heading.split(" > ").next().unwrap_or(&c.heading));
        for t in &c.tags {
            doc.add_text(f.tags, t);
        }
        for d in ancestor_dirs(&c.path) {
            doc.add_text(f.dirs, &d);
        }
        if let Some(t) = &c.todo {
            doc.add_text(f.todo, t);
        }
        // Both the full code and its primary subtag, so a plain term query
        // answers `lang:de-DE` and `lang:de` alike — the same trick as the
        // ancestor chain in `dirs`.
        // Lower-cased on both sides: a STRING field is indexed raw, so `de-DE`
        // and a query for `de-de` would otherwise not meet.
        let full = c.lang.to_ascii_lowercase();
        doc.add_text(f.lang, &full);
        let primary = primary_subtag(&c.lang);
        if primary != full {
            doc.add_text(f.lang, &primary);
        }
        doc.add_text(f.chunk, serde_json::to_string(c)?);
        w.add_document(doc)?;
        Ok(())
    }

    /// Ordinal of each chunk within its own note, which with the path is a
    /// stable identity across edits elsewhere in the vault.
    fn ordinals(chunks: &[Chunk]) -> Vec<u64> {
        let mut seen: HashMap<&str, u64> = HashMap::new();
        chunks
            .iter()
            .map(|c| {
                let n = seen.entry(c.path.as_str()).or_insert(0);
                let ord = *n;
                *n += 1;
                ord
            })
            .collect()
    }

    /// Replace the documents of CHANGED and DROPPED notes, or rebuild
    /// everything when FULL.
    ///
    /// CHUNKS holds only the notes being written — a note's documents are
    /// deleted by path and re-added, so nothing else has to be present.
    pub fn sync(
        state: &Path,
        chunks: &[Chunk],
        dropped: &[String],
        full: bool,
        a: &Analyzer,
    ) -> Result<()> {
        let (index, f) = open_or_create(state, a)?;
        let mut w: IndexWriter = index.writer(50_000_000)?;
        if full {
            w.delete_all_documents()?;
        } else {
            // A note is replaced wholesale: its old documents go, its new ones
            // arrive.  Deleting by path covers a note whose chunk count shrank.
            let mut paths: Vec<&str> = chunks.iter().map(|c| c.path.as_str()).collect();
            paths.extend(dropped.iter().map(String::as_str));
            paths.sort_unstable();
            paths.dedup();
            for p in paths {
                w.delete_term(Term::from_field_text(f.path, p));
            }
        }
        let ords = ordinals(chunks);
        for (i, c) in chunks.iter().enumerate() {
            add(&w, &f, c, ords[i])?;
        }
        w.commit()?;
        std::fs::write(key_file(state), a.key())?;
        Ok(())
    }

    /// Number of live documents.  Used by the tests to assert that a note's
    /// old documents really went away.
    #[cfg(test)]
    pub fn doc_count(state: &Path, a: &Analyzer) -> Result<u64> {
        let (index, _) = open_or_create(state, a)?;
        Ok(index.reader()?.searcher().num_docs())
    }

    /// Search, returning the matching chunks themselves — the index carries
    /// everything a hit needs, so no chunk table has to be loaded alongside.
    pub fn search(
        state: &Path,
        f: &Filters,
        limit: usize,
        conjunction: bool,
        a: &Analyzer,
    ) -> Result<Vec<(f32, Chunk)>> {
        let (index, fl) = open_or_create(state, a)?;
        let searcher = index.reader()?.searcher();

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let term = |field, v: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(Term::from_field_text(field, v), IndexRecordOption::Basic))
        };
        for t in &f.tags {
            clauses.push((Occur::Must, term(fl.tags, t)));
        }
        for t in &f.not_tags {
            clauses.push((Occur::MustNot, term(fl.tags, t)));
        }
        for t in &f.todos {
            clauses.push((Occur::Must, term(fl.todo, t)));
        }
        if !f.langs.is_empty() {
            let any: Vec<(Occur, Box<dyn Query>)> = f
                .langs
                .iter()
                .map(|l| (Occur::Should, term(fl.lang, &l.to_ascii_lowercase())))
                .collect();
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(any))));
        }
        if !f.dirs.is_empty() {
            let any: Vec<(Occur, Box<dyn Query>)> = f
                .dirs
                .iter()
                .map(|d| (Occur::Should, term(fl.dirs, d.trim_end_matches('/'))))
                .collect();
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(any))));
        }
        if !f.text.trim().is_empty() {
            // Every body field: a query's own language is unknown and too short
            // to classify, so each language's clause matches its own documents.
            let mut fields: Vec<Field> = fl.bodies.iter().map(|(_, f)| *f).collect();
            fields.push(fl.title);
            let mut qp = QueryParser::for_index(&index, fields);
            qp.set_field_boost(fl.title, 2.0);
            // All terms required by default.  tantivy's own default is OR,
            // which for "Rabi oscillations" ranks anything merely containing
            // "oscillations"; `--any` restores it.  There is no unset, so this
            // is a branch rather than an assignment.
            if conjunction {
                qp.set_conjunction_by_default();
            }
            clauses.push((Occur::Must, qp.parse_query(&f.text)?));
        }
        if clauses.is_empty() {
            return Err(anyhow!("nothing to search for"));
        }

        let query = BooleanQuery::new(clauses);
        let hits = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
        let mut out = Vec::with_capacity(hits.len());
        for (score, addr) in hits {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if let Some(json) = doc.get_first(fl.chunk).and_then(|v| v.as_str()) {
                out.push((score, serde_json::from_str(json)?));
            }
        }
        Ok(out)
    }
}

/// Every directory a note sits under, relative to the vault root, so `dir:x`
/// matches a whole subtree by exact token rather than needing a prefix query.
fn ancestor_dirs(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut acc = String::new();
    let parts: Vec<&str> = path.split('/').collect();
    for p in &parts[..parts.len().saturating_sub(1)] {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(p);
        out.push(acc.clone());
    }
    out
}

// ------------------------------------------------------------ index on disk

/// Bumped when the on-disk layout changes, so a stale index is rebuilt rather
/// than misread.
const INDEX_VERSION: u32 = 6;

/// Modification time and size, as a cheap pre-filter.  Deliberately not the
/// authority on whether a note changed: `git checkout`, a sync or `touch` all
/// move mtime without touching content, and re-embedding on that would be
/// wasted work.  A stamp that *matches* is trusted to mean unchanged; a stamp
/// that differs only means "read this one and hash it".
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    mtime_ns: u64,
    size: u64,
}

fn stamp_of(p: &Path) -> Option<Stamp> {
    let m = fs::metadata(p).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(Stamp { mtime_ns: t.as_nanos() as u64, size: m.len() })
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    /// Hash of the normalized `Config` this index was built under.
    #[serde(default)]
    config: u64,
    /// Recorded so that changing the embedding model invalidates every vector.
    /// Vectors from two different models are not comparable, and mixing them
    /// silently degrades every search rather than failing.
    model: String,
    dim: usize,
    /// Absolute note path to a hash of its bytes.  Content rather than mtime:
    /// this tree lives in Google Drive and under git, both of which rewrite
    /// timestamps on files whose content never changed.
    files: std::collections::BTreeMap<String, u64>,
    /// Absent from indexes written before stamps existed, in which case every
    /// file is verified by hash once and stamped on the way through — so the
    /// upgrade costs one scan rather than a rebuild.
    #[serde(default)]
    stamps: std::collections::BTreeMap<String, Stamp>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// FNV-1a over exactly what gets embedded — the heading path, a newline, and
/// the body — so that a passage can be recognised across an edit to the note
/// around it.
///
/// One pass over the two halves rather than over their concatenation: looking
/// up every chunk of a large note would otherwise allocate a copy of it.
///
/// Only ever a *lookup* key.  A hit is confirmed by comparing the strings
/// themselves, so a collision costs a needless comparison rather than the wrong
/// vector.
fn chunk_key(heading: &str, text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in heading.bytes().chain(std::iter::once(b'\n')).chain(text.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// FNV-1a. Written out rather than taken from `DefaultHasher`, whose values are
/// explicitly not stable across Rust releases — a toolchain upgrade would
/// silently invalidate every hash and force a full reindex.
fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

struct LoadedIndex {
    chunks: Vec<Chunk>,
    vectors: Vec<f32>,
    files: std::collections::BTreeMap<String, u64>,
    stamps: std::collections::BTreeMap<String, Stamp>,
    by_path: std::collections::HashMap<String, Vec<usize>>,
}

/// Read a model's chunk table, refusing one written under another layout.
///
/// `load_index` guards the *indexing* path; this guards the searching ones,
/// which used to parse `chunks.json` straight and, after a field was renamed,
/// failed with `missing field \`heading_line\`` — true, and useless. A format
/// change should say it is one.
fn read_chunks(dir: &Path, m: &Model) -> Result<Vec<Chunk>> {
    let manifest: Manifest = stored_hash(&dir.join("manifest.json"))
        .ok_or_else(|| anyhow!("no index in {} — run `index`", dir.display()))?;
    if manifest.version != INDEX_VERSION {
        return Err(anyhow!(
            "the index in {} was written under layout v{} and this is v{INDEX_VERSION} — \
             rebuild it with `index --full`",
            dir.display(),
            manifest.version
        ));
    }
    if manifest.model != m.name || manifest.dim != m.dim {
        return Err(anyhow!(
            "the index in {} belongs to {} ({}d), not {} ({}d)",
            dir.display(),
            manifest.model,
            manifest.dim,
            m.name,
            m.dim
        ));
    }
    Ok(serde_json::from_slice(&fs::read(dir.join("chunks.json"))?)?)
}

/// Read a previous index, or `None` when there is none, when it was written by
/// a different model or layout, or when its two halves disagree.
///
/// The last case is the one worth being strict about: `chunks.json` and
/// `vectors.f32` are positionally coupled, so a mismatch does not fail loudly —
/// it silently returns the wrong note for every query.
fn load_index(dir: &Path, m: &Model) -> Option<LoadedIndex> {
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(dir.join("manifest.json")).ok()?).ok()?;
    // A different model's vectors are not comparable with this one's, and a
    // different dimension cannot even be read, so either means a full rebuild.
    if manifest.version != INDEX_VERSION || manifest.model != m.name || manifest.dim != m.dim {
        eprintln!(
            "  existing index was built with {} ({}d); rebuilding for {} ({}d)",
            manifest.model, manifest.dim, m.name, m.dim
        );
        return None;
    }
    let chunks: Vec<Chunk> =
        serde_json::from_slice(&fs::read(dir.join("chunks.json")).ok()?).ok()?;
    let raw = fs::read(dir.join("vectors.f32")).ok()?;
    if raw.len() != chunks.len() * m.dim * 4 {
        eprintln!(
            "  index is inconsistent ({} chunks, {} vectors); rebuilding from scratch",
            chunks.len(),
            raw.len() / (m.dim * 4)
        );
        return None;
    }
    let vectors: Vec<f32> =
        raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let mut by_path: std::collections::HashMap<String, Vec<usize>> = Default::default();
    for (i, c) in chunks.iter().enumerate() {
        by_path.entry(c.path.clone()).or_default().push(i);
    }
    Some(LoadedIndex { chunks, vectors, files: manifest.files, stamps: manifest.stamps, by_path })
}

fn save_index(
    dir: &Path,
    m: &Model,
    cfg: &Config,
    chunks: &[Chunk],
    vectors: &[f32],
    files: std::collections::BTreeMap<String, u64>,
    stamps: std::collections::BTreeMap<String, Stamp>,
) -> Result<usize> {
    fs::create_dir_all(dir)?;
    let mut bytes = Vec::with_capacity(vectors.len() * 4);
    for x in vectors {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    fs::write(dir.join("vectors.f32"), &bytes)?;
    fs::write(dir.join("chunks.json"), serde_json::to_vec(chunks)?)?;
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&Manifest {
            version: INDEX_VERSION,
            config: cfg.hash_for(Target::Semantic),
            model: m.name.into(),
            dim: m.dim,
            files,
            stamps,
        })?,
    )?;
    Ok(bytes.len())
}

/// Where one model's semantic index lives.
///
/// A directory per model, each holding a complete `chunks.json` +
/// `vectors.f32` + `manifest.json`.  Self-contained rather than sharing one
/// chunk table, because a vector is paired to a chunk **by position**: a shared
/// table would silently go stale for every model not indexed in that run, and a
/// same-count-different-content mismatch is exactly what a length check cannot
/// catch.  The duplicated chunk table costs 6 MB against a 10–26 MB vector file,
/// and buys the ability to keep several models built and compare them without
/// re-embedding.
fn semantic_dir(vault: &Path, m: &Model) -> PathBuf {
    state_dir(vault).join("semantic").join(m.name)
}

/// Which models have a semantic index built for this vault.
fn built_models(vault: &Path) -> Vec<&'static Model> {
    MODELS.iter().filter(|m| semantic_dir(vault, m).join("manifest.json").exists()).collect()
}

fn state_dir(vault: &Path) -> PathBuf {
    vault.join(STATE_DIR)
}

/// One note as a scan found it: new or changed, with its text already read.
struct Stale {
    path: String,
    text: String,
}

/// What a pass over the vault found, relative to what an index last recorded.
///
/// Both indexes use this against **their own** record of what they have seen, so
/// either can be brought up to date without the other having run.
struct Scan {
    hashes: std::collections::BTreeMap<String, u64>,
    stamps: std::collections::BTreeMap<String, Stamp>,
    /// Unchanged since that index last saw them.
    reuse: Vec<String>,
    /// New or changed, with their text.
    stale: Vec<Stale>,
    /// Recorded before, gone from disk now.
    dropped: Vec<String>,
    by_stamp: usize,
    by_hash: usize,
    changed: usize,
    new: usize,
}

/// What a previous run recorded about the notes it saw: a content hash and a
/// `(mtime, size)` stamp, each keyed by vault-relative path.  Borrowed from
/// whichever manifest the caller loaded, since the two indexes keep their own.
type Seen<'a> =
    (&'a std::collections::BTreeMap<String, u64>, &'a std::collections::BTreeMap<String, Stamp>);

/// Three outcomes per note, cheapest first: its stamp matches, so it is not even
/// read; its stamp moved but its bytes hash the same, so it is read and reused;
/// or it is genuinely new or changed, and the caller must redo its work.
fn scan_vault(vault: &Path, files: &[PathBuf], prev: Option<Seen<'_>>, rehash: bool) -> Scan {
    let mut sc = Scan {
        hashes: Default::default(),
        stamps: Default::default(),
        reuse: Vec::new(),
        stale: Vec::new(),
        dropped: Vec::new(),
        by_stamp: 0,
        by_hash: 0,
        changed: 0,
        new: 0,
    };

    for f in files {
        let path = rel_path(vault, f);
        let stamp = stamp_of(f);

        // Fast path: same mtime and size as when we last looked.
        if !rehash {
            if let (Some((files, stamps)), Some(st)) = (prev, stamp) {
                if stamps.get(&path) == Some(&st) {
                    if let Some(h) = files.get(&path) {
                        sc.hashes.insert(path.clone(), *h);
                        sc.stamps.insert(path.clone(), st);
                        sc.reuse.push(path);
                        sc.by_stamp += 1;
                        continue;
                    }
                }
            }
        }

        let text = match fs::read_to_string(f) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping {}: {e}", f.display());
                continue;
            }
        };
        let hash = content_hash(text.as_bytes());
        sc.hashes.insert(path.clone(), hash);
        if let Some(st) = stamp {
            sc.stamps.insert(path.clone(), st);
        }

        match prev.and_then(|(files, _)| files.get(&path)) {
            // Timestamp moved, content did not: restamp and reuse the work.
            Some(h) if *h == hash => {
                sc.by_hash += 1;
                sc.reuse.push(path);
            }
            Some(_) => {
                sc.changed += 1;
                sc.stale.push(Stale { path, text });
            }
            None => {
                sc.new += 1;
                sc.stale.push(Stale { path, text });
            }
        }
    }

    if let Some((files, _)) = prev {
        sc.dropped = files.keys().filter(|p| !sc.hashes.contains_key(*p)).cloned().collect();
    }
    sc
}

/// What the lexical index has seen.  Kept apart from `manifest.json` on purpose:
/// the two indexes are updated by separate commands, so each needs its own idea
/// of which notes it is behind on.
#[derive(Serialize, Deserialize)]
struct LexManifest {
    /// The analyzer the index was built with; a change means a new schema.
    key: String,
    #[serde(default)]
    config: u64,
    files: std::collections::BTreeMap<String, u64>,
    #[serde(default)]
    stamps: std::collections::BTreeMap<String, Stamp>,
}

fn lex_manifest_path(dir: &Path) -> PathBuf {
    dir.join("lexical.json")
}

/// Build or update the lexical index, and nothing else.
///
/// Deliberately free of the embedding model: no vectors, no tokenizer, no
/// 512-token re-splitting — that limit is what BGE can read in one go, and BM25
/// has no such bound.  Chunks here are therefore whole sections, which is why
/// this needs neither a download nor a GPU-shaped wait.
fn cmd_index_lexical(
    vault: &Path,
    full: bool,
    rehash: bool,
    lang: &LangConfig,
    fold: bool,
    cfg: &Config,
    out: &mut dyn Write,
) -> Result<IndexReport> {
    let t0 = Instant::now();
    let mut files = Vec::new();
    org_files(vault, &mut files)?;
    files.sort();

    let dir = state_dir(vault);
    let old: Option<LexManifest> = (!full)
        .then(|| fs::read(lex_manifest_path(&dir)).ok())
        .flatten()
        .and_then(|b| serde_json::from_slice(&b).ok());

    let scan = scan_vault(vault, &files, old.as_ref().map(|m| (&m.files, &m.stamps)), rehash);

    // Chunk only what changed.  The languages of those notes may introduce a
    // field the schema does not have, and a schema change means the whole index
    // is rebuilt — so the language set is carried in the stored key and only
    // ever grows.
    let mut chunks: Vec<Chunk> = Vec::new();
    for st in &scan.stale {
        let f = vault.join(&st.path);
        chunks.extend(chunk_file(&f, &st.path, &st.text, Some(lang), cfg, Target::Lexical, &chars));
    }

    let previous = old.as_ref().and_then(|m| lexical::Analyzer::from_key(&m.key));
    let analyzer = lexical::Analyzer::widen(previous.as_ref(), &chunks, fold);
    let rebuilding = old.is_none() || previous.as_ref().map(|a| a.key()) != Some(analyzer.key());

    // A rebuild has to see every note, not only the changed ones.
    if rebuilding {
        chunks.clear();
        for f in &files {
            let path = rel_path(vault, f);
            match fs::read_to_string(f) {
                Ok(text) => chunks.extend(chunk_file(
                    f,
                    &path,
                    &text,
                    Some(lang),
                    cfg,
                    Target::Lexical,
                    &chars,
                )),
                Err(e) => eprintln!("skipping {}: {e}", f.display()),
            }
        }
    }

    if old.is_some() {
        writeln!(
            out,
            "{} org files · {} by stamp · {} restamped · {} changed · {} new · {} removed",
            files.len(),
            scan.by_stamp,
            scan.by_hash,
            scan.changed,
            scan.new,
            scan.dropped.len()
        )?;
    } else {
        writeln!(out, "{} org files", files.len())?;
    }

    let mut report = IndexReport {
        files: files.len(),
        chunks: chunks.len(),
        changed: scan.changed,
        new: scan.new,
        dropped: scan.dropped.len(),
        secs: t0.elapsed().as_secs_f64(),
        ..Default::default()
    };
    if !rebuilding && scan.stale.is_empty() && scan.dropped.is_empty() {
        writeln!(out, "nothing changed; lexical index left as it is")?;
        report.unchanged = true;
        return Ok(report);
    }
    if rebuilding && old.is_some() {
        writeln!(out, "  analyzer changed ({}); rebuilding", analyzer.key())?;
    }

    lexical::sync(&dir, &chunks, &scan.dropped, rebuilding, &analyzer)?;
    fs::create_dir_all(&dir)?;
    fs::write(
        lex_manifest_path(&dir),
        serde_json::to_vec(&LexManifest {
            key: analyzer.key(),
            config: cfg.hash_for(Target::Lexical),
            files: scan.hashes,
            stamps: scan.stamps,
        })?,
    )?;
    writeln!(
        out,
        "lexical index: {} chunks written in {:.2}s",
        chunks.len(),
        t0.elapsed().as_secs_f64()
    )?;
    report.chunks = chunks.len();
    report.secs = t0.elapsed().as_secs_f64();
    Ok(report)
}

/// What an index run did, as data.
///
/// Returned rather than printed because `serve` shares this code and **stdout is
/// its JSON-RPC transport** — a stray `println!` here would splice prose into the
/// protocol and desynchronise the client.  The CLI renders this; the server
/// hands it back as the reply.
#[derive(Serialize, Default)]
struct IndexReport {
    files: usize,
    /// Chunks **written by this run**, which differs by index because they
    /// update differently: the semantic index rewrites `chunks.json` wholesale,
    /// so this is the whole index; the lexical one replaces only the notes that
    /// changed, so a run that merely drops a deleted note writes none.  Neither
    /// is "the size of the index" — ask `status` for that.
    chunks: usize,
    embedded: usize,
    /// Passages inside a *changed* note whose vector was reused because the
    /// passage itself did not change.  Distinct from the notes skipped whole:
    /// this is the saving on the files that really were edited.
    #[serde(skip_serializing_if = "is_zero")]
    carried: usize,
    /// Present only for the semantic index.
    #[serde(skip_serializing_if = "Option::is_none")]
    resplit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<usize>,
    changed: usize,
    new: usize,
    dropped: usize,
    unchanged: bool,
    secs: f64,
}

fn cmd_index(
    vault: &Path,
    full: bool,
    rehash: bool,
    m: &Model,
    cfg: &Config,
    out: &mut dyn Write,
    resident: Option<&mut TextEmbedding>,
) -> Result<IndexReport> {
    let t0 = Instant::now();
    let mut files = Vec::new();
    org_files(vault, &mut files)?;
    files.sort();

    let dir = semantic_dir(vault, m);
    let old = if full { None } else { load_index(&dir, m) };

    let scan = scan_vault(vault, &files, old.as_ref().map(|ix| (&ix.files, &ix.stamps)), rehash);
    let Scan {
        hashes,
        stamps,
        reuse,
        stale,
        dropped,
        by_stamp,
        by_hash,
        changed: changed_files,
        new: new_files,
    } = scan;
    let dropped = dropped.len();

    // Loaded only if something actually needs chunking.
    let tok = if stale.is_empty() { None } else { Some(tokenizer_for(m)?) };

    // Assembled in file order with a slot per chunk.  Reused notes copy their
    // vectors straight across; stale ones leave zeroed slots that the embedding
    // pass fills, so chunks and vectors stay positionally aligned however much
    // of the corpus is carried over.
    let reused: std::collections::HashSet<&str> = reuse.iter().map(String::as_str).collect();
    let stale_text: std::collections::HashMap<&str, &str> =
        stale.iter().map(|s| (s.path.as_str(), s.text.as_str())).collect();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut vectors: Vec<f32> = Vec::new();
    let mut pending: Vec<usize> = Vec::new();
    let mut pending_len: Vec<usize> = Vec::new();
    let mut resplit = 0usize;

    // Every passage the old index holds, by what was embedded for it.
    //
    // The file-level fast path above skips notes that did not change at all;
    // this is the second chance for the notes that did.  A vector depends on
    // the heading path and the body and on nothing else in the chunk, so a note
    // edited in one place re-embeds that passage alone — which is what keeps a
    // `meetings.org` of three hundred meetings from costing three hundred
    // embeddings every time one line is added to it.  Line numbers shift for
    // everything below an insertion, but a line is metadata: the fresh chunk
    // keeps its new one and only the vector is carried over.
    //
    // Built from `chunks.json` and `vectors.f32` as they already are on disk,
    // so this costs no format change and no reindex to start working.
    let mut cached: std::collections::HashMap<u64, usize> = Default::default();
    if let Some(ix) = &old {
        for (i, c) in ix.chunks.iter().enumerate() {
            cached.entry(c.hash).or_insert(i);
        }
    }
    let mut carried = 0usize;

    for f in &files {
        let path = rel_path(vault, f);
        if reused.contains(path.as_str()) {
            if let Some(ix) = &old {
                for &j in ix.by_path.get(&path).map(Vec::as_slice).unwrap_or(&[]) {
                    chunks.push(ix.chunks[j].clone());
                    vectors.extend_from_slice(&ix.vectors[j * m.dim..(j + 1) * m.dim]);
                }
            }
            continue;
        }
        let Some(text) = stale_text.get(path.as_str()) else { continue };
        let tok = tok.as_ref().expect("tokenizer is loaded whenever anything is stale");
        let measure = |t: &str| n_tokens(tok, t);
        // One pass, in the model's own tokens.  Sections that had to be divided
        // are counted from the result — several chunks sharing a heading line —
        // rather than reported by the packer.
        let cs = chunk_file(f, &path, text, None, cfg, Target::Semantic, &measure);
        // A section that had to be divided shows up as consecutive chunks on one
        // heading line, so it is counted from the result rather than reported by
        // the packer — and counted once however many pieces it became.
        let (mut prev_line, mut counted) = (None, false);
        for c in cs {
            if prev_line == Some(c.heading_line) {
                if !counted {
                    resplit += 1;
                    counted = true;
                }
            } else {
                prev_line = Some(c.heading_line);
                counted = false;
            }
            // The hash *is* the identity now.  It used to be confirmed against
            // the strings, but the strings are no longer on disk — see the note
            // on `Chunk::hash` for what that costs.
            let hit = cached.get(&c.hash).copied();
            match hit {
                Some(j) => {
                    let ix = old.as_ref().expect("no cache without an old index");
                    vectors.extend_from_slice(&ix.vectors[j * m.dim..(j + 1) * m.dim]);
                    chunks.push(c);
                    carried += 1;
                }
                None => {
                    // Measured only for what will actually be embedded, which on
                    // an incremental run is a fraction of the corpus — the pass
                    // this replaced measured every chunk, reused ones included.
                    pending.push(chunks.len());
                    pending_len.push(measure(&format!("{}\n{}", c.heading, c.text)));
                    chunks.push(c);
                    vectors.extend(std::iter::repeat_n(0.0, m.dim));
                }
            }
        }
    }

    if old.is_some() {
        writeln!(
            out,
            "{} org files · {by_stamp} by stamp · {by_hash} restamped · \
             {changed_files} changed · {new_files} new · {dropped} removed",
            files.len()
        )?;
    } else {
        writeln!(out, "{} org files", files.len())?;
    }
    if resplit > 0 {
        eprintln!("  {resplit} sections ran past {TOKEN_LIMIT} tokens and were divided");
    }
    // The carried count is what makes a large note cheap to edit, so it is
    // worth saying out loud rather than leaving as an unexplained small number
    // next to a file the user knows they changed.
    let reused_here =
        if carried > 0 { format!(" · {carried} unchanged within them") } else { String::new() };
    writeln!(
        out,
        "{} chunks · {} to embed{reused_here} · scanned in {:.2}s",
        chunks.len(),
        pending.len(),
        t0.elapsed().as_secs_f64()
    )?;
    let mut report = IndexReport {
        files: files.len(),
        chunks: chunks.len(),
        embedded: pending.len(),
        carried,
        resplit: Some(resplit),
        changed: changed_files,
        new: new_files,
        dropped,
        secs: t0.elapsed().as_secs_f64(),
        ..Default::default()
    };

    // Only a run that changes nothing at all may skip the write.  Dropping a
    // deleted note, or merely refreshing stamps, produces no work to embed but
    // must still be persisted.
    // `stale` rather than `pending`, now that a changed note can need no
    // embedding at all: its passages may all have been carried over, but its
    // line numbers moved and its hash is new, and both belong on disk.
    let restamped = by_hash > 0 || old.as_ref().is_some_and(|ix| ix.stamps.len() != stamps.len());
    if pending.is_empty() && stale.is_empty() && dropped == 0 && !restamped && old.is_some() {
        writeln!(out, "nothing changed; index left as it is")?;
        report.unchanged = true;
        return Ok(report);
    }

    if pending.is_empty() {
        writeln!(out, "no new text to embed; rewriting the manifest")?;
    } else {
        // A resident process already holds the model; loading a second copy
        // would cost the 0.12–0.64 s this whole design exists to avoid.
        let t1 = Instant::now();
        let mut owned;
        let model: &mut TextEmbedding = match resident {
            Some(m) => m,
            None => {
                owned = Some(model_with(m.which.clone(), None, false)?);
                writeln!(out, "model loaded in {:.2}s", t1.elapsed().as_secs_f64())?;
                owned.as_mut().expect("just set")
            }
        };

        let t2 = Instant::now();
        // Heading path prepended so a passage carries the context it sits under.
        let texts: Vec<String> = pending
            .iter()
            .map(|&i| format!("{}{}\n{}", m.passage, chunks[i].heading, chunks[i].text))
            .collect();
        let total_tokens: usize = pending_len.iter().sum();

        // Sorted by tokens, not characters.  fastembed pads each batch to its
        // longest member, and chars-per-token runs from about 2.0 in the
        // LaTeX-heavy notes to 4.0 in prose, so a character sort leaves batches
        // uneven in the dimension that actually costs.  The lengths come from
        // the pass that enforced the token limit, so nothing is tokenized twice.
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_unstable_by_key(|&i| pending_len[i]);

        const BATCH: usize = 64;
        let (mut done, mut tokens_done) = (0usize, 0usize);
        for group in order.chunks(BATCH) {
            let batch: Vec<&str> = group.iter().map(|&i| texts[i].as_str()).collect();
            let vs = model.embed(&batch, Some(BATCH)).map_err(|e| anyhow!("embedding: {e}"))?;
            for (&i, mut v) in group.iter().zip(vs) {
                normalize(&mut v);
                let slot = pending[i] * m.dim;
                vectors[slot..slot + m.dim].copy_from_slice(&v);
            }
            done += group.len();
            tokens_done += group.iter().map(|&i| pending_len[i]).sum::<usize>();
            let el = t2.elapsed().as_secs_f64();
            // Tokens per second is near flat once padding is gone, so remaining
            // work divided by it is an estimate rather than an extrapolation.
            let tps = (tokens_done as f64 / el).max(1.0);
            eprint!(
                "\r  embedding {done}/{} · {:.0} chunk/s · {:.1}k tok/s · eta {:.0}s   ",
                texts.len(),
                done as f64 / el,
                tps / 1000.0,
                (total_tokens - tokens_done) as f64 / tps
            );
            io::stderr().flush().ok();
        }
        eprintln!();
        writeln!(
            out,
            "embedded {} chunks in {:.1}s ({:.0}/s)",
            texts.len(),
            t2.elapsed().as_secs_f64(),
            texts.len() as f64 / t2.elapsed().as_secs_f64()
        )?;
    }

    let written = save_index(&dir, m, cfg, &chunks, &vectors, hashes, stamps)?;
    writeln!(
        out,
        "wrote {} ({:.1} MB of vectors) in {:.2}s total",
        dir.display(),
        written as f64 / 1e6,
        t0.elapsed().as_secs_f64()
    )?;
    report.bytes = Some(written);
    report.secs = t0.elapsed().as_secs_f64();
    Ok(report)
}

/// Print hits grouped by note.
///
/// A note that matches a query tends to match it in several places, and a flat
/// top-k then spends every slot on one document.  Each note appears once, at
/// the rank of its best chunk, with its other matching sections beneath it.
/// The corpus's own noise floor under one model: the mean and spread of the
/// cosine between chunks that have nothing to do with each other.
///
/// Reported alongside the raw score because a raw cosine means nothing on its
/// own — these embeddings are strongly anisotropic, so *every* pair scores high.
/// Measured on a 951-note vault: unrelated chunks average 0.563 under
/// `bge-small-en` and 0.801 under `e5-small`, and the mean vector of the whole
/// corpus keeps 75% and 90% of unit length respectively.  Expressed in units of
/// this floor the two models agree — a top hit is ~2.2σ under either — where
/// their raw scores (0.755 and 0.883) share no scale at all.
///
/// Derived from the vectors and never stored: it costs a few milliseconds to
/// recompute, it cannot go stale that way, and storing it would mean a format
/// bump and a full re-embed for a number that is only ever displayed.
#[derive(Clone, Copy)]
struct Baseline {
    mean: f32,
    sd: f32,
}

impl Baseline {
    /// Sampled rather than exhaustive: 20k pairs settle the mean to ~3 decimal
    /// places and all-pairs would be 18M dot products.  Deterministically
    /// seeded, so the same index always reports the same figure.
    fn of(vectors: &[f32], dim: usize) -> Option<Baseline> {
        let n = vectors.len() / dim;
        if n < 3 {
            return None;
        }
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 33) as usize
        };
        let samples = 20_000.min(n * n);
        let mut cs: Vec<f32> = Vec::with_capacity(samples);
        for _ in 0..samples {
            let (a, b) = (next() % n, next() % n);
            if a == b {
                continue;
            }
            let (x, y) = (&vectors[a * dim..(a + 1) * dim], &vectors[b * dim..(b + 1) * dim]);
            cs.push(x.iter().zip(y).map(|(p, q)| p * q).sum::<f32>());
        }
        if cs.len() < 2 {
            return None;
        }
        let mean = cs.iter().sum::<f32>() / cs.len() as f32;
        let var = cs.iter().map(|c| (c - mean) * (c - mean)).sum::<f32>() / cs.len() as f32;
        Some(Baseline { mean, sd: var.sqrt().max(f32::EPSILON) })
    }

    fn z(&self, score: f32) -> f32 {
        (score - self.mean) / self.sd
    }
}

/// One outline node's passages, at the rank of its best one.
struct Group<'a> {
    /// Vault-relative path of the note the node lives in.
    path: String,
    /// Outline path of the node, e.g. `Meetings > Meeting 021 > Decisions`.
    heading: String,
    hits: Vec<(f32, &'a Chunk)>,
}

/// The hits to show, grouped by outline node and capped by `LIM`.
///
/// Grouped by node rather than by file because the file stops being the note
/// the moment someone keeps three hundred meetings in one: every hit would
/// carry the same headline, and the id printed with it would belong to
/// whichever passage happened to rank highest.  A node is the note in both
/// vault shapes — with one note per file it *is* the file.
///
/// The caps are counted per file even so, because that is where crowding
/// happens: without it a single note with fifty matching sections would fill
/// the list before the second note was reached.
///
/// Shared by the human report and the JSON output so the two can never disagree
/// about what was found — only about how it is written down.
fn select<'a>(scored: &[(f32, &'a Chunk)], lim: Limits) -> Vec<Group<'a>> {
    let mut groups: Vec<Group<'a>> = Vec::new();
    // Passages already taken from each file, in the order the files were first
    // reached, so `lim.files` counts notes and not nodes.
    let mut taken: Vec<(String, usize)> = Vec::new();
    for (score, c) in scored {
        let i = match taken.iter().position(|(p, _)| *p == c.path) {
            Some(i) => i,
            None => {
                if taken.len() == lim.files {
                    continue;
                }
                taken.push((c.path.clone(), 0));
                taken.len() - 1
            }
        };
        if taken[i].1 == lim.per_file {
            continue;
        }
        taken[i].1 += 1;
        match groups.iter_mut().find(|g| g.path == c.path && g.heading == c.heading) {
            Some(g) => g.hits.push((*score, c)),
            None => groups.push(Group {
                path: c.path.clone(),
                heading: c.heading.clone(),
                hits: vec![(*score, c)],
            }),
        }
    }
    groups
}

/// Notes opened while rendering one result list, so a file holding several hits
/// is read once.
type Notes = std::collections::HashMap<String, Option<Vec<String>>>;

/// Read a passage back out of the note it came from.
///
/// The index records *where* a passage was, not what it said, so this is where a
/// result gets its text — and what comes back is the real document. The code a
/// `[src bash]` placeholder stands for is in the file, so it appears here even
/// though it never reached the index.
///
/// A file that has moved or shrunk since indexing yields nothing rather than an
/// error or the wrong lines: an index a little behind its vault should still
/// answer, visibly missing a preview rather than inventing one.
fn passage(vault: &Path, c: &Chunk, notes: &mut Notes) -> String {
    let lines = notes.entry(c.path.clone()).or_insert_with(|| {
        fs::read_to_string(vault.join(&c.path))
            .ok()
            .map(|s| s.lines().map(str::to_string).collect())
    });
    let Some(lines) = lines else { return String::new() };
    if c.start_line == 0 || c.start_line > c.end_line || c.end_line > lines.len() {
        return String::new();
    }
    lines[c.start_line - 1..c.end_line].join("\n")
}

/// Everything an editor needs to show a hit and jump to it, without parsing
/// anything: an absolute path so it need not know where the vault is, the
/// `:ID:` when there is one so it can jump through `org-id` and survive the note
/// moving, and the line as the fallback when there is not.
fn hits_json(
    vault: &Path,
    scored: &[(f32, &Chunk)],
    lim: Limits,
    base: Option<Baseline>,
) -> serde_json::Value {
    let mut notes = Notes::new();
    let hits: Vec<serde_json::Value> = select(scored, lim)
        .iter()
        .flat_map(|g| g.hits.iter())
        .map(|(score, c)| {
            let (title, section) = match c.heading.split_once(" > ") {
                Some((t, rest)) => (t, Some(rest)),
                None => (c.heading.as_str(), None),
            };
            serde_json::json!({
                "score": score,
                // How far above the corpus's own noise floor, in its standard
                // deviations.  Comparable across models and queries where the
                // raw score is not.  Null for lexical hits: BM25 is unbounded
                // and has no such floor.
                "z": base.map(|b| b.z(*score)),
                "path": c.path,
                "file": vault.join(&c.path),
                // The heading's line, which is what a client jumps to.  Named
                // for what it is: plain `line` read as "the line of the hit",
                // which it never was.
                "headingLine": c.heading_line,
                // The lines this passage was built from, so a client can read
                // or highlight the region itself rather than trusting `text`.
                "startLine": c.start_line,
                "endLine": c.end_line,
                "id": c.id,
                "title": title,
                "section": section,
                "heading": c.heading,
                "tags": c.tags,
                "todo": c.todo,
                "priority": c.priority.map(|p| p.to_string()),
                // Null rather than "" on semantic hits, which carry no language.
                "lang": (!c.lang.is_empty()).then_some(&c.lang),
                // Read from the note, not from the index: the real passage,
                // code blocks and all.
                "text": passage(vault, c, &mut notes),
            })
        })
        .collect();
    serde_json::json!({ "hits": hits })
}

/// Print the hits.
///
/// With a BASELINE each score is annotated with how many standard deviations it
/// sits above the corpus's own noise floor.  The raw cosine alone is mostly a
/// constant offset that differs by model — unrelated chunks already score 0.56
/// under BGE and 0.80 under E5 — so it cannot be read without that context.
/// BM25 has no such floor, so lexical hits pass `None` and show their score
/// alone.
fn report(vault: &Path, scored: &[(f32, &Chunk)], lim: Limits, baseline: Option<&Baseline>) {
    let groups = select(scored, lim);
    let mut notes = Notes::new();
    // The headline figure, where notes are compared with each other.
    let rank = |s: f32| match baseline {
        Some(b) => format!("{s:.3} ({:+.1}σ)", b.z(s)),
        None => format!("{s:.3}"),
    };

    for g in &groups {
        let (best, c) = g.hits[0];
        // The whole outline path, which begins with the note's title: it reads
        // as the title itself for a note that has no headings, and locates the
        // hit inside a file that holds hundreds of them.
        println!("\n{}  {}", rank(best), g.heading);
        println!("       {}:{}", g.path, c.heading_line);
        // The node's own id, now that a group is a node: it jumps to the
        // passage rather than to whatever the file happens to start with.
        if let Some(id) = &c.id {
            println!("       id:{id}");
        }
        if !c.tags.is_empty() || c.todo.is_some() {
            let todo = c.todo.as_deref().map(|t| format!("{t} ")).unwrap_or_default();
            let tags =
                if c.tags.is_empty() { String::new() } else { format!(":{}:", c.tags.join(":")) };
            println!("       {todo}{tags}");
        }
        for (score, c) in &g.hits {
            let preview: String = passage(vault, c, &mut notes)
                .split_whitespace()
                .take(20)
                .collect::<Vec<_>>()
                .join(" ");
            // Several passages here means one section outran the budget and was
            // divided; they are numbered by line so they can be told apart.
            // Raw score: within one node every passage shares the same offset,
            // so the comparison that matters is between them.
            if g.hits.len() > 1 {
                println!("       · {score:.3} L{:<5}", c.heading_line);
                println!("               {preview}…");
            } else {
                println!("       {preview}…");
            }
        }
    }
}

fn describe_filters(f: &Filters) -> String {
    let mut parts = Vec::new();
    for t in &f.tags {
        parts.push(format!("tag:{t}"));
    }
    for t in &f.not_tags {
        parts.push(format!("-tag:{t}"));
    }
    for d in &f.dirs {
        parts.push(format!("dir:{d}"));
    }
    for t in &f.todos {
        parts.push(format!("todo:{t}"));
    }
    for l in &f.langs {
        parts.push(format!("lang:{l}"));
    }
    parts.join(" ")
}

/// Pick which model's index to search.
///
/// `--model` selects an index here rather than overriding one: the query must be
/// embedded by whatever embedded the corpus, so it can only name an index that
/// exists, never impose a model on vectors built by another.
fn choose_index(vault: &Path, want: Option<&'static Model>) -> Result<&'static Model> {
    let built = built_models(vault);
    let names = || built.iter().map(|m| m.name).collect::<Vec<_>>().join(", ");
    match want {
        Some(m) if built.iter().any(|b| b.name == m.name) => Ok(m),
        Some(m) if built.is_empty() => {
            Err(anyhow!("no semantic index; run `index --model {}`", m.name))
        }
        Some(m) => Err(anyhow!("no {} index; built: {}", m.name, names())),
        None => match built.as_slice() {
            [] => Err(anyhow!("no semantic index — run `index` first")),
            [only] => Ok(only),
            // Several to choose from: prefer the default, else make it explicit
            // rather than picking for them.
            many => many
                .iter()
                .find(|m| m.name == DEFAULT_MODEL)
                .copied()
                .ok_or_else(|| anyhow!("several indexes ({}); pass --model", names())),
        },
    }
}

fn cmd_search(
    vault: &Path,
    query: &str,
    lim: Limits,
    want: Option<&'static Model>,
    json: bool,
) -> Result<()> {
    let m = choose_index(vault, want)?;
    let dir = semantic_dir(vault, m);
    let chunks: Vec<Chunk> = read_chunks(&dir, m)?;
    let raw = fs::read(dir.join("vectors.f32"))?;
    let n = raw.len() / (m.dim * 4);
    if n != chunks.len() {
        return Err(anyhow!("index is inconsistent: {n} vectors for {} chunks", chunks.len()));
    }
    let vectors: Vec<f32> =
        raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();

    // Predicates constrain which chunks are considered; only the remaining free
    // text is embedded.
    let f = parse_query(query);
    // A language is a stemmer's business, and an embedding is not stemmed.  The
    // semantic index therefore records no language, so this predicate could only
    // ever match nothing — better said than silently returned.
    if !f.langs.is_empty() {
        return Err(anyhow!("lang: narrows the lexical index only; add --lexical"));
    }
    let candidates: Vec<usize> = (0..n).filter(|&i| f.matches(&chunks[i])).collect();
    if !f.is_empty() && !json {
        println!("filter: {} → {} of {n} chunks", describe_filters(&f), candidates.len());
    }
    if candidates.is_empty() {
        // No match is an answer, not an error: a caller reading JSON gets an
        // empty list rather than prose it would have to recognise.
        if json {
            println!("{}", hits_json(vault, &[], lim, None));
        } else {
            println!("no chunk matches those filters");
        }
        return Ok(());
    }
    if f.text.trim().is_empty() {
        return Err(anyhow!("nothing to search for: the query is only filters"));
    }

    let t0 = Instant::now();
    let mut model = model_with(m.which.clone(), None, false)?;
    let load = t0.elapsed();

    let t1 = Instant::now();
    let mut q = model
        .embed(&[format!("{}{}", m.query, f.text)], None)
        .map_err(|e| anyhow!("embedding query: {e}"))?
        .remove(0);
    normalize(&mut q);
    let embed = t1.elapsed();

    let t2 = Instant::now();
    let mut scored: Vec<(f32, usize)> = candidates
        .iter()
        .map(|&i| {
            let s = &vectors[i * m.dim..(i + 1) * m.dim];
            (s.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>(), i)
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
    let search = t2.elapsed();

    let hits: Vec<(f32, &Chunk)> = scored.iter().map(|(s, i)| (*s, &chunks[*i])).collect();
    let baseline = Baseline::of(&vectors, m.dim);
    if json {
        println!("{}", hits_json(vault, &hits, lim, baseline));
        return Ok(());
    }
    report(vault, &hits, lim, baseline.as_ref());
    eprintln!(
        "\n[model load {:.0}ms · query embed {:.0}ms · search over {} vectors {:.2}ms]",
        load.as_secs_f64() * 1000.0,
        embed.as_secs_f64() * 1000.0,
        candidates.len(),
        search.as_secs_f64() * 1000.0
    );
    Ok(())
}

/// Time embedding on a slice of the vault, to get a rate before committing to a
/// full run.  Reports the chunk-length distribution too, since throughput on
/// this workload is set by tokens rather than by chunk count.
fn cmd_bench(vault: &Path, n: usize, which_config: &str) -> Result<()> {
    let mut files = Vec::new();
    org_files(vault, &mut files)?;
    files.sort();
    // Packed with the real tokenizer, or this measures chunks the indexer would
    // never produce.
    let tok = tokenizer_for(model_named(DEFAULT_MODEL)?)?;
    let measure = |t: &str| n_tokens(&tok, t);
    let mut chunks = Vec::new();
    for f in &files {
        if let Ok(text) = fs::read_to_string(f) {
            chunks.extend(chunk_file(
                f,
                &rel_path(vault, f),
                &text,
                None,
                &Config::default(),
                Target::Semantic,
                &measure,
            ));
        }
        if chunks.len() >= n {
            break;
        }
    }
    chunks.truncate(n);

    let mut lens: Vec<usize> = chunks.iter().map(|c| c.heading.len() + c.text.len()).collect();
    lens.sort_unstable();
    let total: usize = lens.iter().sum();
    println!(
        "{} chunks · chars: mean {} · median {} · p90 {} · max {}",
        lens.len(),
        total / lens.len().max(1),
        lens[lens.len() / 2],
        lens[lens.len() * 9 / 10],
        lens[lens.len() - 1]
    );

    let texts: Vec<String> = chunks.iter().map(|c| format!("{}\n{}", c.heading, c.text)).collect();

    // One configuration per process: an ORT session that is merely dropped does
    // not necessarily return its arena, and running four in a row was enough to
    // get the process OOM-killed.
    let configs: Vec<(&str, EmbeddingModel, Option<usize>, bool)> = match which_config {
        "cpu512" => vec![("CPU  f32 max_len 512", EmbeddingModel::BGESmallENV15, Some(512), false)],
        "cpu256" => vec![("CPU  f32 max_len 256", EmbeddingModel::BGESmallENV15, Some(256), false)],
        "coreml512" => {
            vec![("CoreML f32 max_len 512", EmbeddingModel::BGESmallENV15, Some(512), true)]
        }
        "coreml256" => {
            vec![("CoreML f32 max_len 256", EmbeddingModel::BGESmallENV15, Some(256), true)]
        }
        other => return Err(anyhow!("unknown config {other}")),
    };
    for (label, which, max_len, coreml) in configs {
        let t0 = Instant::now();
        let mut m = match model_with(which, max_len, coreml) {
            Ok(m) => m,
            Err(e) => {
                println!("  {label}: unavailable ({e})");
                continue;
            }
        };
        let load = t0.elapsed();
        let t1 = Instant::now();
        match m.embed(&texts, Some(64)) {
            Ok(v) => {
                let el = t1.elapsed().as_secs_f64();
                println!(
                    "  {label}: load {:.2}s · {} chunks in {:.1}s = {:.0}/s",
                    load.as_secs_f64(),
                    v.len(),
                    el,
                    v.len() as f64 / el
                );
            }
            Err(e) => println!("  {label}: failed ({e})"),
        }
    }
    Ok(())
}

/// Report the token-length distribution of every chunk, and how many would be
/// silently truncated.  fastembed sets `TruncationParams { max_length }` on the
/// tokenizer, so a chunk over the limit loses its tail with no error — and
/// characters are a poor proxy for tokens in notes full of LaTeX and German.
fn cmd_tokens(vault: &Path, limit: usize, m: &Model) -> Result<()> {
    let tok = tokenizer_for(m)?;

    let mut files = Vec::new();
    org_files(vault, &mut files)?;
    files.sort();
    // The same packing the index applies — one pass, in tokens — so this reports
    // what is actually embedded rather than the raw sections.
    let measure = |s: &str| n_tokens(&tok, s);
    let mut chunks = Vec::new();
    for f in &files {
        if let Ok(text) = fs::read_to_string(f) {
            chunks.extend(chunk_file(
                f,
                &rel_path(vault, f),
                &text,
                None,
                &Config::default(),
                Target::Semantic,
                &measure,
            ));
        }
    }
    println!("{} chunks packed to the policy's budget", chunks.len());
    let texts: Vec<String> = chunks.iter().map(|c| format!("{}\n{}", c.heading, c.text)).collect();

    let mut lens: Vec<(usize, usize)> = Vec::with_capacity(texts.len());
    for (i, t) in texts.iter().enumerate() {
        let enc = tok.encode(t.as_str(), true).map_err(|e| anyhow!("tokenizing: {e}"))?;
        lens.push((enc.len(), i));
    }
    let mut sorted: Vec<usize> = lens.iter().map(|(n, _)| *n).collect();
    sorted.sort_unstable();
    let n = sorted.len();
    let over: Vec<&(usize, usize)> = lens.iter().filter(|(t, _)| *t > limit).collect();
    let lost: usize = over.iter().map(|(t, _)| t - limit).sum();
    let total: usize = sorted.iter().sum();

    println!(
        "{n} chunks · tokens: median {} · p90 {} · p99 {} · max {}",
        sorted[n / 2],
        sorted[n * 9 / 10],
        sorted[n * 99 / 100],
        sorted[n - 1]
    );
    println!("total {total} tokens · {} chunks over {limit} ({:.1}%) · {lost} tokens truncated ({:.2}% of corpus)",
             over.len(), 100.0 * over.len() as f64 / n as f64,
             100.0 * lost as f64 / total as f64);
    println!(
        "chars-per-token overall: {:.2}",
        texts.iter().map(|t| t.len()).sum::<usize>() as f64 / total as f64
    );
    for (t, i) in over.iter().take(8) {
        println!("  {t} tokens · {} chars · {}", texts[*i].len(), chunks[*i].heading);
    }
    Ok(())
}

/// Where fastembed caches a model's files.
fn model_cache_root(m: &Model) -> Result<PathBuf> {
    let code = TextEmbedding::list_supported_models()
        .into_iter()
        .find(|i| i.model == m.which)
        .map(|i| i.model_code)
        .ok_or_else(|| anyhow!("fastembed does not know {}", m.name))?;
    Ok(cache_dir().join(format!("models--{}", code.replace('/', "--"))))
}

fn find_tokenizer(m: &Model) -> Result<PathBuf> {
    let snaps = model_cache_root(m)?.join("snapshots");
    for e in fs::read_dir(&snaps).with_context(|| format!("reading {}", snaps.display()))? {
        let p = e?.path().join("tokenizer.json");
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!("no tokenizer.json under {}", snaps.display()))
}

/// The tokenizer belonging to M, fetching the model if it is not cached yet.
///
/// It must be **that** model's tokenizer: the 512-token limit is enforced with
/// it, and BGE's WordPiece and E5's XLM-RoBERTa disagree wildly on how many
/// tokens a German sentence is.  This used to be hardcoded to BGE, which counted
/// every other model's text with the wrong vocabulary and never said so.
fn tokenizer_for(m: &Model) -> Result<tokenizers::Tokenizer> {
    let path = match find_tokenizer(m) {
        Ok(p) => p,
        // Not cached: chunking runs before embedding, so the download that would
        // have happened later has to happen now.
        Err(_) => {
            eprintln!("  fetching {} …", m.name);
            let _ = model_with(m.which.clone(), None, false)?;
            find_tokenizer(m)?
        }
    };
    tokenizers::Tokenizer::from_file(&path)
        .map_err(|e| anyhow!("loading tokenizer {}: {e}", path.display()))
}

/// The vault argument, rejecting a flag standing in its place.
///
/// Without this a forgotten path makes the *flag* the vault: `index --model
/// e5-small` walked a directory called `--model` and failed several layers down
/// with "No such file or directory".
/// `--model NAME` from the arguments, or the default.  FROM is where this
/// subcommand's flags start.
/// Reject a flag this subcommand does not take, rather than ignoring it.
///
/// A flag that is silently dropped looks exactly like a flag that had no effect,
/// which has already cost this project twice: `--fold` was accepted and ignored
/// for a whole session, and `--lang-keyword` outlived its own removal. Values
/// are never scanned, only tokens starting with `--`.
fn reject_unknown_flags(args: &[String], from: usize, allowed: &[&str]) -> Result<()> {
    for a in args.iter().skip(from) {
        if a.starts_with("--") && !allowed.contains(&a.as_str()) {
            return Err(match allowed {
                [] => anyhow!("unknown option `{a}`; this command takes no options"),
                _ => anyhow!("unknown option `{a}`; this command takes {}", allowed.join(", ")),
            });
        }
    }
    Ok(())
}

/// The value following FLAG, if it is there.
fn flag_value<'a>(args: &'a [String], from: usize, flag: &str) -> Option<&'a str> {
    let i = args.iter().skip(from).position(|a| a == flag)?;
    args.get(from + i + 1).map(String::as_str)
}

/// Read a manifest just far enough to learn what policy it was written under.
fn stored_hash<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn model_arg(args: &[String], from: usize) -> Result<&'static Model> {
    match args.iter().skip(from).position(|a| a == "--model") {
        Some(i) => model_named(args.get(from + i + 1).map(String::as_str).unwrap_or("")),
        None => model_named(DEFAULT_MODEL),
    }
}

fn vault_arg<'a>(args: &'a [String], usage: &str) -> Result<&'a Path> {
    match args.get(2).map(String::as_str) {
        Some(v) if !v.starts_with('-') => Ok(Path::new(v)),
        Some(v) => Err(anyhow!("expected a vault directory, got `{v}`\nusage: {usage}")),
        None => Err(anyhow!("usage: {usage}")),
    }
}

/// Print the chunks a file produces, without embedding — for checking chunking
/// decisions (boundaries, overlap, headings) without paying for a full index.
/// Preview what an index will actually store, for the index you name.
///
/// The two differ in more than one way now — the semantic side re-splits at 512
/// tokens and drops block bodies, the lexical side does neither — so a preview
/// that showed one of them while claiming to show "the chunking" would be worse
/// than none.  Bare is semantic, `--lexical` is the word index, as everywhere
/// else.
fn cmd_chunks(
    vault: &Path,
    needle: &str,
    lang: &LangConfig,
    m: &Model,
    cfg: &Config,
    target: Target,
) -> Result<()> {
    let tok = tokenizer_for(m)?;
    println!(
        "previewing the {} index",
        match target {
            Target::Semantic => "semantic",
            Target::Lexical => "lexical",
        }
    );
    let mut files = Vec::new();
    org_files(vault, &mut files)?;
    files.sort();
    for f in files.iter().filter(|f| f.to_string_lossy().contains(needle)) {
        let text = fs::read_to_string(f)?;
        let measure = |s: &str| n_tokens(&tok, s);
        // Each index is previewed in its own unit, because each is packed in
        // its own: showing a lexical preview cut to token boundaries would be
        // showing something that never reaches disk.
        let unit: &dyn Fn(&str) -> usize = match target {
            Target::Semantic => &measure,
            Target::Lexical => &chars,
        };
        let chunks = chunk_file(
            f,
            &rel_path(vault, f),
            &text,
            (target == Target::Lexical).then_some(lang),
            cfg,
            target,
            unit,
        );
        println!("\n=== {} — {} chunks", f.display(), chunks.len());
        for (i, c) in chunks.iter().enumerate() {
            let full = format!("{}\n{}", c.heading, c.text);
            println!(
                "\n--- [{i}] L{} · {} tok · {} chars · lang={} · {}",
                c.heading_line,
                n_tokens(&tok, &full),
                c.text.len(),
                c.lang,
                c.heading.split(" > ").last().unwrap_or("")
            );
            println!("    head: {:?}", &c.text.chars().take(60).collect::<String>());
            println!(
                "    tail: {:?}",
                &c.text
                    .chars()
                    .rev()
                    .take(60)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            );
        }
    }
    Ok(())
}

/// Lexical search: exact words, phrases and boolean operators, over the same
/// chunks the semantic index describes.
///
/// A separate command rather than a mode of `search`, deliberately.  The two
/// honour the same `tag:`/`dir:`/`todo:` predicates, but a phrase or a boolean
/// means nothing to an embedding, so a fused ranking would mix results that
/// honoured the query with results that could not.  Fusing them is a later
/// decision, to be made once there is evidence it helps.
fn cmd_lexical(
    vault: &Path,
    query: &str,
    lim: Limits,
    conjunction: bool,
    json: bool,
) -> Result<()> {
    let dir = state_dir(vault);
    // The analyzer comes from the index's own metadata, not from the corpus:
    // tokens produced by one analyzer cannot be queried with another, and the
    // stored key is the only record of which one built this index.
    let stored = lexical::stored_key(&dir)
        .ok_or_else(|| anyhow!("no lexical index in {} — run `index --lexical`", dir.display()))?;
    let analyzer = lexical::Analyzer::from_key(&stored)
        .ok_or_else(|| anyhow!("unreadable lexical index — run `index --lexical`"))?;
    let f = parse_query(query);
    if !f.is_empty() && !json {
        println!("filter: {}", describe_filters(&f));
    }
    let t = Instant::now();
    // Generous, because grouping collapses many chunks into one node: a single
    // well-matching note can otherwise fill the whole candidate pool and hide
    // every other note.  Scaled by both caps, since raising either one asks for
    // more of the corpus to be considered before the grouping thins it out.
    let pool = lim.files.saturating_mul(lim.per_file).saturating_mul(25).max(100);
    let hits = lexical::search(&dir, &f, pool, conjunction, &analyzer)?;
    let el = t.elapsed();
    let hits: Vec<(f32, &Chunk)> = hits.iter().map(|(s, c)| (*s, c)).collect();
    if json {
        // No baseline: BM25 scores are unbounded, so there is nothing to
        // standardise them against.
        println!("{}", hits_json(vault, &hits, lim, None));
        return Ok(());
    }
    if hits.is_empty() {
        println!("no match");
        return Ok(());
    }
    report(vault, &hits, lim, None);
    eprintln!("\n[lexical search {:.1}ms]", el.as_secs_f64() * 1000.0);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("index") => {
            let vault = vault_arg(&args, "index <vault> [--lexical|--both] [--model NAME]")?;
            // `--incremental` is the default; accepted so a script can say so.
            let full = args.iter().skip(3).any(|a| a == "--full");
            // `--rehash` reads and hashes every note, ignoring stamps: the
            // backstop for a change that left mtime untouched.
            let rehash = args.iter().skip(3).any(|a| a == "--rehash");
            reject_unknown_flags(
                &args,
                3,
                &["--full", "--rehash", "--lexical", "--both", "--model", "--config"],
            )?;
            // Same convention as `search`: bare is semantic, `--lexical` is the
            // word index, and the two are separate artifacts built separately.
            // `--both` is one command for the Emacs side to call.
            let lexical = args.iter().skip(3).any(|a| a == "--lexical");
            let both = args.iter().skip(3).any(|a| a == "--both");
            let model = match args.iter().skip(3).position(|a| a == "--model") {
                Some(i) => model_named(args.get(i + 4).map(String::as_str).unwrap_or(""))?,
                None => model_named(DEFAULT_MODEL)?,
            };
            let given = flag_value(&args, 3, "--config").map(PathBuf::from);
            let cfg = resolve_config(vault, given.as_deref())?;
            let lang = LangConfig { languages: cfg.languages.clone() };
            // The policy last indexed under, kept only so the error can say
            // which setting moved rather than that one did.
            let previous = Config::read(&config_path(&state_dir(vault))).ok();
            if !full {
                if both || !lexical {
                    check_config(
                        stored_hash::<Manifest>(&semantic_dir(vault, model).join("manifest.json"))
                            .map(|m| m.config),
                        &cfg,
                        previous.as_ref(),
                        Target::Semantic,
                        CLI_REMEDY,
                    )?;
                }
                if both || lexical {
                    check_config(
                        stored_hash::<LexManifest>(&lex_manifest_path(&state_dir(vault)))
                            .map(|m| m.config),
                        &cfg,
                        previous.as_ref(),
                        Target::Lexical,
                        CLI_REMEDY,
                    )?;
                }
            }
            let mut out = io::stdout();
            if both || !lexical {
                cmd_index(vault, full, rehash, model, &cfg, &mut out, None)?;
            }
            if both || lexical {
                prepare_lang(&lang)?;
                cmd_index_lexical(vault, full, rehash, &lang, cfg.fold_diacritics, &cfg, &mut out)?;
            }
            // Cached so a later run need not restate it.
            fs::create_dir_all(state_dir(vault))?;
            fs::write(config_path(&state_dir(vault)), cfg.canonical())?;
            Ok(())
        }
        Some("search") => {
            let vault = vault_arg(&args, "search <vault> <query> [k] [--lexical] [--json]")?;
            let query = args
                .get(3)
                .ok_or_else(|| anyhow!("usage: search <vault> <query> [k] [--lexical]"))?;
            // One command, two rankings, never mixed.  A shared entry point is
            // not the same as a fused result list: `--lexical` returns purely
            // word-ranked hits, `search` alone purely meaning-ranked ones.
            reject_unknown_flags(
                &args,
                4,
                &["--lexical", "--any", "--json", "--model", "--per-file"],
            )?;
            // `k` bounds the notes; `--per-file` bounds how much of the list any
            // one of them may take.  A vault kept in a few large files wants the
            // second raised — see the manual.
            let per_file = if args.iter().skip(4).any(|a| a == "--per-file") {
                let v = flag_value(&args, 4, "--per-file")
                    .ok_or_else(|| anyhow!("--per-file wants a number after it"))?;
                match v.parse::<usize>() {
                    // Zero would return an empty list rather than an error,
                    // which reads as "nothing matched".
                    Ok(0) => return Err(anyhow!("--per-file 0 would show nothing")),
                    Ok(n) => n,
                    Err(_) => return Err(anyhow!("--per-file wants a number, not `{v}`")),
                }
            } else {
                DEFAULT_PER_FILE
            };
            let lim = Limits {
                files: args.get(4).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_FILES),
                per_file,
            };
            let lexical = args.iter().skip(4).any(|a| a == "--lexical");
            // Structured output for an editor: the same hits, without prose to
            // parse back out.
            let json = args.iter().skip(4).any(|a| a == "--json");
            // Selects which index to search when several models are built.
            let want = match args.iter().skip(4).position(|a| a == "--model") {
                Some(i) => Some(model_named(args.get(i + 5).map(String::as_str).unwrap_or(""))?),
                None => None,
            };
            if lexical {
                let conjunction = !args.iter().skip(4).any(|a| a == "--any");
                cmd_lexical(vault, query, lim, conjunction, json)
            } else {
                cmd_search(vault, query, lim, want, json)
            }
        }
        Some("chunks") => {
            let vault = vault_arg(&args, "chunks <vault> <path-substring>")?;
            let needle = args.get(3).map(String::as_str).unwrap_or("");
            reject_unknown_flags(&args, 3, &["--model", "--config", "--lexical"])?;
            // `--config` here is a dry run: try a policy without storing it or
            // paying for a reindex to find out what it would do.
            let given = flag_value(&args, 3, "--config").map(PathBuf::from);
            let cfg = resolve_config(vault, given.as_deref())?;
            let lang = LangConfig { languages: cfg.languages.clone() };
            prepare_lang(&lang)?;
            let target = if args.iter().skip(3).any(|a| a == "--lexical") {
                Target::Lexical
            } else {
                Target::Semantic
            };
            cmd_chunks(vault, needle, &lang, model_arg(&args, 3)?, &cfg, target)
        }
        Some("serve") => serve::serve(),
        Some("models") => {
            reject_unknown_flags(&args, 2, &[])?;
            // With a vault, say which of them are actually built for it.
            let built = args
                .get(2)
                .map(|v| built_models(Path::new(v)).iter().map(|m| m.name).collect::<Vec<_>>());
            // `status` inline, as in the row below, so the two formats match.
            println!("{:<14} {:>5}  {:<14}  status", "name", "dim", "trained on");
            for m in MODELS {
                let status = match &built {
                    Some(b) if b.contains(&m.name) => "built",
                    Some(_) => "",
                    None => "",
                };
                let dflt = if m.name == DEFAULT_MODEL { "default" } else { "" };
                println!("{:<14} {:>5}  {:<14}  {status} {dflt}", m.name, m.dim, m.about);
            }
            if built.is_none() {
                println!("\nPass a vault to see which are built for it.");
            }
            println!(
                "\nEach model keeps its own index under .org-semantic/semantic/<model>/, so\n\
                 several can be built side by side and compared without re-embedding.\n\
                 `search --model NAME` picks between them."
            );
            Ok(())
        }
        Some("tokens") => {
            let vault = vault_arg(&args, "tokens <vault> [limit]")?;
            reject_unknown_flags(&args, 3, &["--model"])?;
            let limit = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(512);
            cmd_tokens(vault, limit, model_arg(&args, 3)?)
        }
        Some("bench") => {
            let vault = vault_arg(&args, "bench <vault> [n] [config]")?;
            reject_unknown_flags(&args, 3, &[])?;
            let n = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(500);
            let cfg = args.get(4).map(String::as_str).unwrap_or("cpu512");
            cmd_bench(Path::new(vault), n, cfg)
        }
        // Asking for help is not an error: it goes to stdout and exits 0.
        Some("-h") | Some("--help") | Some("help") => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(anyhow!("{USAGE}")),
    }
}

// ═══════════════════════════════════════════════════════════════════ tests

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the tokenizer: one "token" per whitespace-separated word.
    /// Keeps these tests independent of a 129 MB model download, and the
    /// invariants under test are about the packing logic, not about BGE's
    /// vocabulary.
    fn words(s: &str) -> usize {
        s.split_whitespace().count()
    }

    /// Paragraphs from a blank-line-separated string, numbered from line 1.
    /// The packing tests are about packing, so the numbers only need to exist.
    fn as_paras(text: &str) -> Vec<Para> {
        let mut out = Vec::new();
        let mut open = false;
        for (i, line) in text.lines().enumerate() {
            add_line(&mut out, &mut open, i + 1, line);
        }
        out
    }

    /// The pieces' text run together, for asserting a hard split lost nothing.
    fn joined(pieces: &[Piece]) -> String {
        pieces.iter().map(|p| p.text.as_str()).collect()
    }

    /// One paragraph, for the `hard_split` tests.
    fn one_para(text: &str) -> Para {
        Para { start: 1, end: 1, text: text.to_string() }
    }

    fn para(word: &str, n: usize) -> String {
        std::iter::repeat_n(word, n).collect::<Vec<_>>().join(" ")
    }

    // ---------------------------------------------------------- primitives

    #[test]
    fn heading_level_counts_stars_and_needs_a_space() {
        assert_eq!(heading_level("* Top"), Some(1));
        assert_eq!(heading_level("*** Deep"), Some(3));
        assert_eq!(heading_level("*bold* not a heading"), None);
        assert_eq!(heading_level("no stars"), None);
        assert_eq!(heading_level("*"), None);
    }

    /// Regression: the first full run panicked here on `start — =modified-stamp=`,
    /// because byte 8 of that line falls inside the em-dash.
    #[test]
    fn strip_prefix_ci_survives_multibyte_at_the_boundary() {
        assert_eq!(strip_prefix_ci("start — dash", "#+title:"), None);
        assert_eq!(strip_prefix_ci("→→→", ":ID:"), None);
        assert_eq!(strip_prefix_ci("#+TITLE: X", "#+title:"), Some(" X"));
        assert_eq!(strip_prefix_ci("ab", "#+title:"), None);
    }

    // ------------------------------------------------------------ chunking

    fn chunks_of(text: &str) -> Vec<Chunk> {
        chunk_file(
            Path::new("/vault/Note.org"),
            "Note.org",
            text,
            None,
            &Config::default(),
            Target::Semantic,
            &unsplit,
        )
    }

    #[test]
    fn file_id_is_inherited_and_overridden_per_heading() {
        let c = chunks_of(
            ":PROPERTIES:\n:ID: file-uuid\n:END:\n#+title: T\n\nintro\n\n\
             * One\nbody one\n\n* Two\n:PROPERTIES:\n:ID: two-uuid\n:END:\nbody two\n",
        );
        assert_eq!(c[0].id.as_deref(), Some("file-uuid"), "preamble takes the file ID");
        assert_eq!(c[1].id.as_deref(), Some("file-uuid"), "heading without its own inherits");
        assert_eq!(c[2].id.as_deref(), Some("two-uuid"), "heading with a drawer overrides");
    }

    #[test]
    fn heading_path_uses_title_and_nesting_and_drops_tags() {
        let c = chunks_of("#+title: My Note\n* Section :work:urgent:\nbody\n\n** Sub\ndeeper\n");
        assert_eq!(c[0].heading, "My Note > Section");
        assert_eq!(c[1].heading, "My Note > Section > Sub");
    }

    #[test]
    fn markup_is_not_embedded() {
        let c = chunks_of("#+title: T\n:PROPERTIES:\n:ID: x\n:END:\n#+filetags: :a:\nreal text\n");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text.trim(), "real text");
        assert!(!c[0].text.contains(":ID:"), "drawer contents must not reach the embedder");
        assert!(!c[0].text.contains("#+filetags"));
    }

    #[test]
    fn line_points_at_the_heading_that_owns_the_chunk() {
        let c = chunks_of("#+title: T\n\npreamble\n\n* First\nalpha\n\n* Second\nbeta\n");
        assert_eq!(c[1].heading, "T > First");
        assert_eq!(c[1].heading_line, 5);
        assert_eq!(c[2].heading_line, 8);
    }

    // --------------------------------------------------------------- spans

    /// Every construct whose span could be wrong, in one note.  The last
    /// section's paragraphs are long enough to divide under a real budget —
    /// `room` has a floor of 32, so a toy budget would never split anything.
    fn spanned_note() -> String {
        format!(
            "#+title: Spans\n\nPreamble prose.\n\n\
             * First\nAbove the block.\n\n\
             #+begin_src bash\necho hello\necho world\n#+end_src\n\n\
             Below the block.\n\n\
             * Second\n{}\n\n{}\n\n{}\n\n{}\n",
            para("alpha", 20),
            para("beta", 20),
            para("gamma", 20),
            para("delta", 20)
        )
    }

    fn spanned(budget: usize) -> Vec<Chunk> {
        let cfg = Config {
            chunk: Chunking { semantic_tokens: budget, ..Chunking::default() },
            ..Config::default()
        };
        chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            &spanned_note(),
            None,
            &cfg,
            Target::Semantic,
            &words,
        )
    }

    /// The span has to cover the passage: whatever the chunk kept must be found
    /// in the raw lines it points at. This is the property the whole design
    /// rests on — a preview is read from these lines, so if they do not contain
    /// the passage, the preview is of something else.
    #[test]
    fn a_span_covers_the_lines_its_text_came_from() {
        let note = spanned_note();
        let lines: Vec<&str> = note.lines().collect();
        // Both the undivided case and the divided one, since dividing is where
        // a span could start pointing at the wrong paragraph.
        for c in spanned(200).into_iter().chain(spanned(45)) {
            assert!(c.start_line >= 1 && c.end_line >= c.start_line, "{c:?}");
            assert!(c.end_line <= lines.len(), "span past the end of the file: {c:?}");
            let raw = lines[c.start_line - 1..c.end_line].join("\n");
            for line in c.text.lines() {
                // A placeholder stands for text it replaced; by construction it
                // is the one thing not found in the file.
                if line.trim().is_empty() || line.trim_start().starts_with('[') {
                    continue;
                }
                assert!(
                    raw.contains(line.trim()),
                    "line {line:?} is not inside lines {}..{} of the file:\n{raw}",
                    c.start_line,
                    c.end_line
                );
            }
        }
    }

    /// The point of a span being wider than its text: a collapsed block still
    /// spans the block, so the code the index dropped can be read back.
    #[test]
    fn a_collapsed_block_still_spans_the_code_it_replaced() {
        let note = spanned_note();
        let lines: Vec<&str> = note.lines().collect();
        let c = spanned(200)
            .into_iter()
            .find(|c| c.text.contains("[src bash]"))
            .expect("the block should have left a placeholder");
        let raw = lines[c.start_line - 1..c.end_line].join("\n");
        assert!(raw.contains("echo hello") && raw.contains("echo world"), "{raw}");
        assert!(!c.text.contains("echo hello"), "the text itself must not carry the code");
    }

    /// Text above every heading belongs to no section, and points at the top of
    /// the file rather than at nothing.
    #[test]
    fn a_preamble_passage_spans_its_own_lines() {
        let c = spanned(200).into_iter().next().expect("a preamble chunk");
        assert_eq!(c.text.trim(), "Preamble prose.");
        assert_eq!((c.start_line, c.end_line), (3, 3));
        assert_eq!(c.heading_line, 1, "no heading owns it, so it jumps to the top");
    }

    /// A divided section's pieces advance through the file, and overlap where
    /// `carry_over` repeats a paragraph — the spans say so rather than hiding it.
    #[test]
    fn a_divided_section_yields_advancing_spans() {
        let cs: Vec<Chunk> =
            spanned(45).into_iter().filter(|c| c.heading.ends_with("Second")).collect();
        assert!(cs.len() > 1, "four 20-word paragraphs must divide under a 45-word budget");
        for w in cs.windows(2) {
            assert!(w[1].start_line >= w[0].start_line, "pieces must advance: {w:?}");
            assert!(w[1].start_line <= w[0].end_line + 2, "and not skip lines: {w:?}");
        }
    }

    // ------------------------------------------------------------- packing

    #[test]
    fn packs_groups_of_paragraphs_not_single_ones() {
        // Ten 10-word paragraphs against a budget of 35: must split, and each
        // piece should still hold three paragraphs rather than one.
        let body = (0..10).map(|_| para("w", 10)).collect::<Vec<_>>().join("\n\n");
        let pieces = split_to_fit(&as_paras(&body), &words, 35);
        assert!(pieces.len() > 1, "should have split");
        assert!(
            pieces.iter().any(|p| p.text.split("\n\n").count() > 1),
            "pieces must group paragraphs, not isolate them"
        );
    }

    #[test]
    fn overlap_repeats_the_previous_paragraph() {
        let paras: Vec<String> = (0..6).map(|i| format!("p{i} {}", para("w", 20))).collect();
        let pieces = split_to_fit(&as_paras(&paras.join("\n\n")), &words, 60);
        assert!(pieces.len() >= 2);
        // The second piece must begin with the last paragraph of the first.
        let tail_of_first = pieces[0].text.split("\n\n").last().unwrap();
        assert!(
            pieces[1].text.starts_with(tail_of_first),
            "piece 2 should open with piece 1's final paragraph\n1: {:?}\n2: {:?}",
            pieces[0],
            pieces[1]
        );
    }

    #[test]
    fn overlap_is_skipped_when_the_previous_piece_was_one_paragraph() {
        // Two paragraphs, each nearly the whole budget: no overlap is possible
        // without making piece 1 a subset of piece 2.
        let body = format!("{}\n\n{}", para("a", 50), para("b", 50));
        let pieces = split_to_fit(&as_paras(&body), &words, 60);
        assert_eq!(pieces.len(), 2);
        assert!(
            !pieces[1].text.contains("a a a"),
            "must not repeat a whole single-paragraph piece"
        );
    }

    /// The invariant the overlap could plausibly have broken.
    #[test]
    fn overlap_never_pushes_a_piece_over_budget() {
        for budget in [20usize, 37, 60, 100] {
            for sizes in [vec![5; 40], vec![19; 12], vec![3, 30, 4, 28, 5, 25]] {
                let body = sizes
                    .iter()
                    .enumerate()
                    .map(|(i, n)| format!("p{i} {}", para("w", *n)))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                for piece in split_to_fit(&as_paras(&body), &words, budget) {
                    assert!(
                        words(&piece.text) <= budget,
                        "piece of {} words exceeds budget {budget}",
                        words(&piece.text)
                    );
                }
            }
        }
    }

    /// Pack a note under a budget counted in whatever `words` counts.
    fn packed(note: &str, budget: usize) -> Vec<Chunk> {
        let cfg = Config {
            chunk: Chunking { semantic_tokens: budget, ..Chunking::default() },
            ..Config::default()
        };
        chunk_file(Path::new("/v/n.org"), "n.org", note, None, &cfg, Target::Semantic, &words)
    }

    /// The heading is prepended to every piece before it is embedded, so it has
    /// to come out of the budget.  Packing a body to the whole budget and then
    /// adding the heading would hand the model more than it takes, and fastembed
    /// truncates that in silence.
    #[test]
    fn the_heading_comes_out_of_the_budget() {
        let note = format!("#+title: T\n* {}\n{}\n", para("h", 10), para("w", 200));
        let cs = packed(&note, 50);
        assert!(cs.len() > 1, "200 words cannot fit a budget of 50");
        for c in &cs {
            let full = format!("{}\n{}", c.heading, c.text);
            assert!(words(&full) <= 50, "heading + text must fit: {}", words(&full));
        }
    }

    /// And a section already within budget is left whole, with its metadata.
    #[test]
    fn a_section_within_budget_is_left_whole() {
        let cs = packed("#+title: T\n* H\nshort body\n", 50);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].text.trim(), "short body");
        assert_eq!(cs[0].heading_line, 2, "the heading's own line");
    }

    /// Each index is packed in its own unit, so one budget cannot govern both:
    /// the same note divides differently for words than for characters.
    #[test]
    fn each_index_is_packed_in_its_own_unit() {
        let note = format!("#+title: T\n* H\n{}\n", para("word", 100));
        let cfg = Config {
            chunk: Chunking { semantic_tokens: 20, lexical_chars: 10_000 },
            ..Config::default()
        };
        let at = |target, m: &dyn Fn(&str) -> usize| {
            chunk_file(
                Path::new("/v/n.org"),
                "n.org",
                &note,
                Some(&LangConfig::default()),
                &cfg,
                target,
                m,
            )
            .len()
        };
        assert!(at(Target::Semantic, &words) > 1, "100 words exceed a 20-word budget");
        assert_eq!(at(Target::Lexical, &chars), 1, "but not 10,000 characters");
    }

    #[test]
    fn hard_split_is_lossless_and_within_budget() {
        let para = para("word", 500);
        let pieces = hard_split(&one_para(&para), &words, 40);
        assert!(pieces.len() > 1);
        for p in &pieces {
            assert!(words(&p.text) <= 40, "{} words > 40", words(&p.text));
        }
        assert_eq!(joined(&pieces), para, "hard split must not lose or reorder text");
    }

    #[test]
    fn hard_split_never_cuts_inside_a_character() {
        // All multi-byte, so a naive byte cut would panic or corrupt.
        let para = "é→ü ".repeat(400);
        let pieces = hard_split(&one_para(&para), &words, 30);
        assert_eq!(joined(&pieces), para);
    }

    // ------------------------------------------------- index round-trip / prune

    /// The default model's index directory, which is where the semantic tests
    /// seed and assert.
    fn sem(v: &Path) -> PathBuf {
        semantic_dir(v, model_named(DEFAULT_MODEL).unwrap())
    }

    /// A measure for tests that are about parsing, not packing: nothing ever
    /// exceeds a budget, so a note comes back as the sections it has.  Packing
    /// itself is exercised directly against `split_to_fit`.
    fn unsplit(_: &str) -> usize {
        0
    }

    fn scratch(name: &str) -> PathBuf {
        // No tempfile dependency: a per-test directory under the system temp,
        // removed first so a previous failed run cannot leak into this one.
        let d = std::env::temp_dir().join(format!("org-semantic-test-{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write a note and return its vault-relative path, which is what the
    /// index stores.
    fn note(dir: &Path, name: &str) -> String {
        let body = format!(
            ":PROPERTIES:\n:ID: id-{name}\n:END:\n#+title: {name}\n\n* S\nText about {name}.\n"
        );
        let rel = format!("{name}.org");
        fs::write(dir.join(&rel), &body).unwrap();
        rel
    }

    /// Seed an index without embedding anything: vectors are zeros, which is
    /// all the reuse and prune paths inspect.  PATHS are vault-relative.
    fn seed(dir: &Path, paths: &[&str]) {
        let mut chunks = Vec::new();
        let mut files = std::collections::BTreeMap::new();
        for p in paths {
            chunks.push(Chunk {
                path: (*p).into(),
                id: None,
                heading: "H".into(),
                heading_line: 1,
                start_line: 1,
                end_line: 1,
                tags: Vec::new(),
                todo: None,
                priority: None,
                lang: "en-US".into(),
                hash: 0,
                text: "body".into(),
            });
            files.insert((*p).to_string(), content_hash(&fs::read(dir.join(p)).unwrap()));
        }
        let stamps =
            paths.iter().map(|p| ((*p).to_string(), stamp_of(&dir.join(p)).unwrap())).collect();
        let m = model_named(DEFAULT_MODEL).unwrap();
        let vectors = vec![0.0f32; chunks.len() * m.dim];
        save_index(&semantic_dir(dir, m), m, &Config::default(), &chunks, &vectors, files, stamps)
            .unwrap();
    }

    /// Seed an index holding the chunks the vault really produces, so that the
    /// per-passage reuse has something it can recognise.  `seed` cannot serve
    /// here: its chunks are placeholders that no re-chunking would ever match,
    /// which is exactly what the reuse looks for.
    ///
    /// Vectors are distinct per chunk rather than zeroed, so a carried vector
    /// can be told from a freshly written one.
    fn seed_real(dir: &Path, paths: &[&str]) {
        let m = model_named(DEFAULT_MODEL).unwrap();
        let mut chunks = Vec::new();
        let mut files = std::collections::BTreeMap::new();
        for p in paths {
            let abs = dir.join(p);
            let text = fs::read_to_string(&abs).unwrap();
            chunks.extend(chunk_file(
                &abs,
                p,
                &text,
                None,
                &Config::default(),
                Target::Semantic,
                &unsplit,
            ));
            files.insert((*p).to_string(), content_hash(text.as_bytes()));
        }
        let stamps =
            paths.iter().map(|p| ((*p).to_string(), stamp_of(&dir.join(p)).unwrap())).collect();
        let vectors: Vec<f32> =
            (0..chunks.len()).flat_map(|i| std::iter::repeat_n(i as f32 + 1.0, m.dim)).collect();
        save_index(&semantic_dir(dir, m), m, &Config::default(), &chunks, &vectors, files, stamps)
            .unwrap();
    }

    /// A note whose bytes changed but whose passages did not: every vector is
    /// carried across and nothing is embedded.
    ///
    /// Regression on the write, not only on the saving: the early return used to
    /// ask whether anything needed embedding, and a note like this needs none —
    /// so its new hash was never recorded and it was read as changed on every
    /// later run, forever.
    #[test]
    fn a_changed_note_carries_over_the_passages_that_did_not_change() {
        let v = scratch("carry");
        let a = note(&v, "alpha");
        seed_real(&v, &[a.as_str()]);
        let m = model_named(DEFAULT_MODEL).unwrap();
        let before = fs::read(sem(&v).join("vectors.f32")).unwrap();
        let old_hash = load_index(&sem(&v), m).unwrap().files[&a];

        // Trailing blank lines: the file's bytes differ, its chunking does not.
        let abs = v.join(&a);
        let body = fs::read_to_string(&abs).unwrap();
        fs::write(&abs, format!("{body}\n\n")).unwrap();

        let r = cmd_index(&v, false, false, m, &Config::default(), &mut io::sink(), None).unwrap();

        assert_eq!(r.embedded, 0, "no passage changed, so none may be embedded");
        assert!(r.carried > 0, "its passages must be carried over, not rebuilt");
        assert_eq!(
            fs::read(sem(&v).join("vectors.f32")).unwrap(),
            before,
            "carried vectors must be copied verbatim"
        );
        assert_ne!(
            load_index(&sem(&v), m).unwrap().files[&a],
            old_hash,
            "the new content hash must reach the manifest, or this repeats every run"
        );
    }

    /// The saving that makes a large note cheap to edit: one new section costs
    /// one embedding, however many sections stand around it.
    #[test]
    fn adding_a_section_re_embeds_only_that_section() {
        let v = scratch("carry-one");
        let a = note(&v, "alpha");
        // A note of several sections, so there is something to carry.
        let abs = v.join(&a);
        let mut body = fs::read_to_string(&abs).unwrap();
        for i in 0..5 {
            body.push_str(&format!("\n* Section {i}\nA paragraph about topic {i}.\n"));
        }
        fs::write(&abs, &body).unwrap();
        seed_real(&v, &[a.as_str()]);
        let seeded =
            load_index(&sem(&v), model_named(DEFAULT_MODEL).unwrap()).unwrap().chunks.len();

        // Insert at the top: every line number below it moves, but a line is
        // metadata and no passage's text changed.
        fs::write(&abs, format!("* Section new\nSomething else entirely.\n{body}")).unwrap();

        // `chunk_file` rather than `cmd_index`, which would load the model to
        // embed the one new passage.
        let text = fs::read_to_string(&abs).unwrap();
        let fresh =
            chunk_file(&abs, &a, &text, None, &Config::default(), Target::Semantic, &unsplit);
        let old = load_index(&sem(&v), model_named(DEFAULT_MODEL).unwrap()).unwrap();
        // Matched on the stored hash, as the indexer does — the old index no
        // longer carries the text to compare against.
        let cached: std::collections::HashMap<u64, usize> =
            old.chunks.iter().enumerate().map(|(i, c)| (c.hash, i)).collect();
        let missing = fresh.iter().filter(|c| !cached.contains_key(&c.hash)).count();

        assert_eq!(fresh.len(), seeded + 1, "the note gained exactly one passage");
        assert_eq!(missing, 1, "only the new passage may need embedding");
    }

    /// Regression: a run whose only change is a deleted note produced nothing to
    /// embed, hit the early return, and never wrote the pruned index — so the
    /// note stayed searchable until something unrelated changed.
    #[test]
    fn deleting_a_note_is_persisted_even_though_nothing_needs_embedding() {
        let v = scratch("prune");
        let a = note(&v, "alpha");
        let b = note(&v, "beta");
        seed(&v, &[a.as_str(), b.as_str()]);

        fs::remove_file(v.join(&b)).unwrap();
        cmd_index(
            &v,
            false,
            false,
            model_named(DEFAULT_MODEL).unwrap(),
            &Config::default(),
            &mut io::sink(),
            None,
        )
        .unwrap();

        let ix = load_index(&sem(&v), model_named(DEFAULT_MODEL).unwrap())
            .expect("index should still load");
        assert_eq!(ix.chunks.len(), 1, "beta's chunk must be gone");
        assert_eq!(ix.chunks[0].path, a);
        assert!(!ix.files.contains_key(&b), "beta must be gone from the manifest");
        assert_eq!(
            ix.vectors.len(),
            ix.chunks.len() * model_named(DEFAULT_MODEL).unwrap().dim,
            "halves stay aligned"
        );
    }

    #[test]
    fn an_unchanged_vault_is_left_alone() {
        let v = scratch("unchanged");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);
        let before = fs::read(sem(&v).join("vectors.f32")).unwrap();
        cmd_index(
            &v,
            false,
            false,
            model_named(DEFAULT_MODEL).unwrap(),
            &Config::default(),
            &mut io::sink(),
            None,
        )
        .unwrap();
        assert_eq!(fs::read(sem(&v).join("vectors.f32")).unwrap(), before);
    }

    /// Regression: `cmd_search` embedded the query with a hardcoded BGE while the
    /// corpus had been embedded with another model.  Nothing failed — the
    /// vectors were simply near-orthogonal, so every score collapsed and the
    /// ranking was noise.  A model belongs to an index, and the manifest is
    /// where that is written down.
    #[test]
    fn an_index_belongs_to_the_model_that_built_it() {
        let v = scratch("model-identity");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);

        let manifest: Manifest =
            serde_json::from_slice(&fs::read(sem(&v).join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.model, DEFAULT_MODEL, "the index records its model");
        assert!(model_named(&manifest.model).is_ok(), "and search can resolve it back");

        // Asked for under a different model, the index must be refused rather
        // than read: its vectors answer a different question.
        let other = model_named("e5-large").unwrap();
        assert_ne!(other.dim, model_named(DEFAULT_MODEL).unwrap().dim);
        assert!(load_index(&state_dir(&v), other).is_none());
    }

    #[test]
    fn an_excluded_subtree_takes_its_children_with_it() {
        let text = "#+title: T\n* Public\nordinary prose here\n\
                    * Private :noexport:\nsecret prose here\n\
                    ** Deeper\nstill under the excluded parent\n";
        let c = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            text,
            None,
            &Config::default(),
            Target::Semantic,
            &unsplit,
        );
        let texts: Vec<&str> = c.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts.len(), 1, "only the public section survives: {texts:?}");
        assert!(texts[0].contains("ordinary"));

        // The exclusion is inherited, so it is the child that proves it works:
        // a per-heading rule would have kept "Deeper".
        let keep = Config { exclude_tagged: vec![], ..Config::default() };
        let all = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            text,
            None,
            &keep,
            Target::Semantic,
            &unsplit,
        );
        assert_eq!(all.len(), 3, "and nothing is dropped when nothing is excluded");
    }

    #[test]
    fn a_block_body_is_embedded_or_labelled_by_policy() {
        let text = "#+title: T\n* S\nbefore the snippet\n\n\
                    #+begin_src bash\nrm -rf /tmp/x\n#+end_src\n\n\
                    after the snippet\n";
        let cfg = Config::default();
        let sem = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            text,
            None,
            &cfg,
            Target::Semantic,
            &unsplit,
        );
        let lex =
            chunk_file(Path::new("/v/n.org"), "n.org", text, None, &cfg, Target::Lexical, &unsplit);

        // The body is not embedded, but the seam and the fact survive: without
        // the placeholder the two paragraphs would read as adjacent.
        assert!(!sem[0].text.contains("rm -rf"), "code is not embedded: {:?}", sem[0].text);
        assert!(sem[0].text.contains("[src bash]"), "labelled: {:?}", sem[0].text);
        assert!(sem[0].text.contains("before") && sem[0].text.contains("after"));

        // Exact match is what lexical is for, so the body stays whole there.
        assert!(lex[0].text.contains("rm -rf /tmp/x"), "{:?}", lex[0].text);
        assert!(!lex[0].text.contains("[src"), "no label in an exact-match index");
    }

    #[test]
    fn results_go_but_quotations_stay() {
        let text = "#+title: T\n* S\nprose\n\n#+RESULTS:\n: mounted ok\n: 0 errors\n\n\
                    #+begin_quote\nA quotation is prose set off.\n#+end_quote\n";
        let cfg = Config::default();
        let sem = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            text,
            None,
            &cfg,
            Target::Semantic,
            &unsplit,
        );
        // Babel output is generated and nobody looks for it by meaning; a
        // quotation is prose someone chose to set off.
        assert!(!sem[0].text.contains("mounted ok"), "{:?}", sem[0].text);
        assert!(sem[0].text.contains("quotation is prose"), "{:?}", sem[0].text);

        let lex =
            chunk_file(Path::new("/v/n.org"), "n.org", text, None, &cfg, Target::Lexical, &unsplit);
        assert!(lex[0].text.contains("mounted ok"), "still findable by word");
    }

    /// The example is the first thing a user edits, so it must parse — and it
    /// must be the defaults, or it silently changes their index the moment they
    /// pass it.  `include_str!` means a stale example fails the build, not a
    /// user's run.
    /// README.md is the GitHub landing page; `docs/manual.org` is the manual the
    /// site is built from. They share a tagline and the worked example, and
    /// those drifted apart once already — so they are compared here rather than
    /// remembered.
    #[test]
    fn the_landing_page_agrees_with_the_manual() {
        let md = include_str!("../README.md");
        let org = include_str!("../docs/manual.org");
        let strip = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");

        let tagline = "Search a tree of org-mode notes by meaning or by words.";
        assert!(md.contains(tagline) && org.contains(tagline), "the tagline moved");

        // The demo is the part carrying real numbers, so it rots fastest.
        let block = |s: &str, open: &str| {
            let start = s.find(open).expect("no console block") + open.len();
            strip(&s[start..][..s[start..].find("```").or(s[start..].find("#+end_src")).unwrap()])
        };
        assert_eq!(
            block(md, "```console\n"),
            block(org, "#+begin_src console\n"),
            "the worked example differs between the two READMEs"
        );
    }

    #[test]
    fn the_example_config_is_exactly_the_defaults() {
        let text = include_str!("../config.example.json");
        let cfg: Config = serde_json::from_str(text).expect("config.example.json must parse");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn a_policy_is_compared_by_meaning_not_by_bytes() {
        let a: Config =
            serde_json::from_str(r#"{"exclude_tagged":["noexport","ARCHIVE"]}"#).unwrap();
        let b: Config =
            serde_json::from_str(r#"{"exclude_tagged":["ARCHIVE","noexport","ARCHIVE"]}"#).unwrap();
        for t in [Target::Semantic, Target::Lexical] {
            assert_eq!(a.hash_for(t), b.hash_for(t), "order and duplicates are not changes");
            assert_eq!(a.hash_for(t), Config::default().hash_for(t), "restating defaults is free");
            assert_ne!(
                a.hash_for(t),
                Config { exclude_tagged: vec![], ..Config::default() }.hash_for(t),
                "a real change is seen"
            );
        }

        // A typo must not be a setting that silently does nothing.
        assert!(serde_json::from_str::<Config>(r#"{"exclude_tag":["x"]}"#).is_err());
    }

    #[test]
    fn an_unreadable_cached_policy_falls_back_rather_than_bricking() {
        let v = scratch("stale-policy");
        fs::create_dir_all(state_dir(&v)).unwrap();
        // Whatever a schema change leaves behind: our own file, no longer
        // parseable.  The key is not a former name — nothing knows about those.
        fs::write(config_path(&state_dir(&v)), r#"{"from_an_older_schema":1}"#).unwrap();
        let cfg = resolve_config(&v, None).expect("a stale cache must not brick every command");
        assert_eq!(cfg, Config::default());

        // A file the caller named is a different matter: that is their typo.
        let named = v.join("theirs.json");
        fs::write(&named, r#"{"from_an_older_schema":1}"#).unwrap();
        assert!(resolve_config(&v, Some(&named)).is_err());
    }

    #[test]
    fn languages_and_folding_are_lexical_policy_and_stick() {
        let a = Config::default();
        let b = Config { languages: vec!["en-US".into(), "de-DE".into()], ..Config::default() };
        // Adding a language cannot change an embedding, so it must not cost one.
        assert_eq!(a.hash_for(Target::Semantic), b.hash_for(Target::Semantic));
        assert_ne!(a.hash_for(Target::Lexical), b.hash_for(Target::Lexical));

        // They are set only in the policy file, so there is one place to look
        // and one thing to forget — which the cache then remembers for you.
        let from_file: Config =
            serde_json::from_str(r#"{"languages":["en-US","de-DE"],"fold_diacritics":true}"#)
                .unwrap();
        assert_eq!(from_file.languages, vec!["en-US", "de-DE"]);
        assert!(from_file.fold_diacritics);
        assert_eq!(from_file.hash_for(Target::Semantic), a.hash_for(Target::Semantic));
    }

    #[test]
    fn a_changed_policy_is_refused_and_names_what_moved() {
        let old = Config::default();
        let new = Config { exclude_tagged: vec![], ..Config::default() };
        let t = Target::Semantic;
        assert!(check_config(Some(old.hash_for(t)), &old, Some(&old), t, CLI_REMEDY).is_ok());
        let err = check_config(Some(old.hash_for(t)), &new, Some(&old), t, CLI_REMEDY)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exclude_tagged"), "names the setting: {err}");
        assert!(err.contains("--full"), "and how to proceed: {err}");
        // Nothing stored yet is not a mismatch.
        assert!(check_config(None, &new, None, t, CLI_REMEDY).is_ok());

        // A lexical-only change must not invalidate the semantic index: an
        // embedding cannot be affected by what BM25 indexes.
        let mut lex_only = Config::default();
        lex_only.blocks.src.lexical = false;
        assert_eq!(
            lex_only.hash_for(Target::Semantic),
            old.hash_for(Target::Semantic),
            "a lexical-only edit must not cost a re-embed"
        );
        assert_ne!(lex_only.hash_for(Target::Lexical), old.hash_for(Target::Lexical));

        // Reordering a list is not a change, and must not be reported as one.
        let reordered = Config {
            exclude_tagged: vec!["ARCHIVE".into(), "noexport".into()],
            ..Config::default()
        };
        assert!(
            reordered.differences(&old, t).is_empty(),
            "order is not a difference: {:?}",
            reordered.differences(&old, t)
        );
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        let args: Vec<String> =
            ["org-semantic", "index", "/v", "--fulll"].iter().map(|s| s.to_string()).collect();
        let err = reject_unknown_flags(&args, 3, &["--full", "--model"]).unwrap_err().to_string();
        assert!(err.contains("--fulll"), "names the offender: {err}");
        assert!(err.contains("--full"), "and lists what is accepted: {err}");

        // A command with no options at all still says so, rather than trailing
        // off with an empty list.
        let none = reject_unknown_flags(&args, 3, &[]).unwrap_err().to_string();
        assert!(none.contains("takes no options"), "{none}");

        // Values are not flags, and must not be mistaken for them.
        let ok: Vec<String> = ["org-semantic", "index", "/v", "--model", "e5-small"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(reject_unknown_flags(&ok, 3, &["--full", "--model"]).is_ok());
    }

    #[test]
    fn a_flag_is_not_a_vault() {
        // `index --model e5-small` used to take `--model` as the vault and fail
        // several layers down with "No such file or directory".
        let args: Vec<String> = ["org-semantic", "index", "--model", "e5-small"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = vault_arg(&args, "index <vault>").unwrap_err().to_string();
        assert!(err.contains("--model"), "names what it got: {err}");
        assert!(err.contains("usage"), "and how to fix it: {err}");

        let ok: Vec<String> =
            ["org-semantic", "index", "/tmp/v"].iter().map(|s| s.to_string()).collect();
        assert_eq!(vault_arg(&ok, "index <vault>").unwrap(), Path::new("/tmp/v"));
    }

    /// Each model's tokenizer must be its own: the 512-token limit is enforced
    /// with it, and BGE's WordPiece counts a German sentence ~50% higher than
    /// E5's XLM-RoBERTa (66 tokens against 44 on the same paragraph).  This was
    /// hardcoded to BGE and silently mismeasured every other model.
    #[test]
    fn a_tokenizer_belongs_to_its_model() {
        let bge = model_cache_root(model_named("bge-small-en").unwrap()).unwrap();
        let e5 = model_cache_root(model_named("e5-small").unwrap()).unwrap();
        assert_ne!(bge, e5);
        assert!(bge.ends_with("models--Xenova--bge-small-en-v1.5"), "{bge:?}");
        assert!(e5.ends_with("models--intfloat--multilingual-e5-small"), "{e5:?}");
    }

    #[test]
    fn the_baseline_is_the_corpus_noise_floor() {
        // Orthonormal vectors: every unrelated pair scores exactly 0, so the
        // floor is 0 and a perfect match stands far above it.
        let dim = 8;
        let mut v = vec![0.0f32; dim * dim];
        for i in 0..dim {
            v[i * dim + i] = 1.0;
        }
        let b = Baseline::of(&v, dim).unwrap();
        assert!(b.mean.abs() < 1e-6, "an orthogonal corpus has a zero floor: {}", b.mean);
        assert!(b.z(1.0) > 100.0, "and a perfect match towers over it");

        // Every vector pointing the same way is anisotropy in miniature: the
        // floor sits at 1.0, so a perfect match is worth nothing above it —
        // which is why the raw score alone says so little.
        let mut same = Vec::new();
        for _ in 0..dim {
            same.push(1.0f32);
            same.extend(std::iter::repeat_n(0.0, dim - 1));
        }
        let b2 = Baseline::of(&same, dim).unwrap();
        assert!((b2.mean - 1.0).abs() < 1e-6, "floor at 1.0: {}", b2.mean);
        assert!(b2.z(1.0).abs() < 1.0, "nothing stands out: {}", b2.z(1.0));
    }

    #[test]
    fn several_model_indexes_coexist_and_are_chosen_between() {
        let v = scratch("choose-model");
        let a = note(&v, "alpha");
        assert!(choose_index(&v, None).is_err(), "nothing is built yet");

        seed(&v, &[a.as_str()]);
        let dflt = model_named(DEFAULT_MODEL).unwrap();
        assert_eq!(choose_index(&v, None).unwrap().name, dflt.name);

        // A second model is built *beside* the first, not over it — which is the
        // point: comparing two models must not cost re-embedding for the one you
        // already had.
        let other = model_named("e5-small").unwrap();
        save_index(
            &semantic_dir(&v, other),
            other,
            &Config::default(),
            &[],
            &[],
            Default::default(),
            Default::default(),
        )
        .unwrap();
        assert_eq!(built_models(&v).len(), 2, "both survive");
        assert!(sem(&v).join("vectors.f32").exists(), "the first is untouched");

        assert_eq!(choose_index(&v, Some(other)).unwrap().name, "e5-small");
        assert_eq!(choose_index(&v, None).unwrap().name, dflt.name, "default breaks the tie");
        // Naming a model with no index is an error, not a silent fallback: it
        // would otherwise answer from vectors the caller did not ask for.
        assert!(choose_index(&v, Some(model_named("e5-large").unwrap())).is_err());
    }

    #[test]
    fn save_and_load_round_trip() {
        let v = scratch("roundtrip");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);
        let ix = load_index(&sem(&v), model_named(DEFAULT_MODEL).unwrap()).unwrap();
        assert_eq!(ix.chunks.len(), 1);
        assert_eq!(ix.vectors.len(), model_named(DEFAULT_MODEL).unwrap().dim);
        assert_eq!(ix.by_path.get(&a).map(Vec::len), Some(1));
    }

    #[test]
    fn a_truncated_vector_file_is_rejected_rather_than_trusted() {
        let v = scratch("mismatch");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);
        let f = sem(&v).join("vectors.f32");
        let mut bytes = fs::read(&f).unwrap();
        bytes.truncate(bytes.len() - 4);
        fs::write(&f, bytes).unwrap();
        assert!(
            load_index(&sem(&v), model_named(DEFAULT_MODEL).unwrap()).is_none(),
            "positional coupling means a mismatch returns wrong answers, not errors"
        );
    }

    #[test]
    fn an_index_from_another_model_is_rejected() {
        let v = scratch("model");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);
        let f = sem(&v).join("manifest.json");
        let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
        m["model"] = serde_json::Value::String("SomeOtherModel".into());
        fs::write(&f, serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(load_index(&sem(&v), model_named(DEFAULT_MODEL).unwrap()).is_none());
    }

    #[test]
    fn content_hash_is_stable_and_discriminating() {
        assert_eq!(content_hash(b"alpha"), content_hash(b"alpha"));
        assert_ne!(content_hash(b"alpha"), content_hash(b"alphb"));
        assert_ne!(content_hash(b""), content_hash(b"x"));
    }

    /// mtime is a filter, not the answer: a note whose timestamp moved but whose
    /// bytes are identical must be restamped and reused, never re-embedded.
    #[test]
    fn a_touched_note_is_restamped_without_re_embedding() {
        let v = scratch("restamp");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);
        let before = fs::read(sem(&v).join("vectors.f32")).unwrap();
        let old_stamp =
            load_index(&sem(&v), model_named(DEFAULT_MODEL).unwrap()).unwrap().stamps[&a];
        let abs = v.join(&a);

        // Move mtime without touching content.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let body = fs::read(&abs).unwrap();
        fs::write(&abs, &body).unwrap();

        cmd_index(
            &v,
            false,
            false,
            model_named(DEFAULT_MODEL).unwrap(),
            &Config::default(),
            &mut io::sink(),
            None,
        )
        .unwrap();

        let ix = load_index(&sem(&v), model_named(DEFAULT_MODEL).unwrap()).unwrap();
        assert_eq!(
            fs::read(sem(&v).join("vectors.f32")).unwrap(),
            before,
            "identical content must not be re-embedded"
        );
        assert_ne!(ix.stamps[&a], old_stamp, "the new stamp must be recorded");
    }

    // -------------------------------------------------------------- grouping

    fn at(path: &str, heading: &str, line: usize) -> Chunk {
        Chunk {
            path: path.into(),
            id: None,
            heading: heading.into(),
            heading_line: line,
            start_line: line,
            end_line: line,
            tags: Vec::new(),
            todo: None,
            priority: None,
            lang: String::new(),
            hash: 0,
            text: format!("body at {line}"),
        }
    }

    /// Descending scores in the order given, which is the order `select` sees.
    fn ranked(cs: &[Chunk]) -> Vec<(f32, &Chunk)> {
        cs.iter().enumerate().map(|(i, c)| (1.0 - i as f32 / 1000.0, c)).collect()
    }

    /// Two meetings inside one `meetings.org` are two results, not one file
    /// wearing the title of whichever passage ranked highest.
    #[test]
    fn hits_are_grouped_by_node_rather_than_by_file() {
        let cs = vec![
            at("meetings.org", "Meetings > Meeting 021", 315),
            at("meetings.org", "Meetings > Meeting 030", 450),
        ];
        let g = select(&ranked(&cs), Limits::default());
        assert_eq!(g.len(), 2, "two nodes, two results");
        assert_eq!(g[0].heading, "Meetings > Meeting 021");
        assert_eq!(g[1].heading, "Meetings > Meeting 030");
    }

    /// The other direction: one section divided because it outran the budget
    /// is still one place in the vault, so it stays a single result.
    #[test]
    fn a_divided_section_stays_one_result() {
        let cs = vec![at("n.org", "N > S", 1), at("n.org", "N > S", 40)];
        let g = select(&ranked(&cs), Limits::default());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].hits.len(), 2, "both passages hang under the one node");
    }

    /// Regression: `k` counted files, so a vault kept in three of them returned
    /// nine hits however large `k` was, with no way to ask for more.  The
    /// passage cap is what a large-file vault raises.
    #[test]
    fn the_per_file_cap_is_what_bounds_a_large_note() {
        let cs: Vec<Chunk> =
            (0..30).map(|i| at("meetings.org", &format!("M > Meeting {i}"), i * 10 + 1)).collect();

        let d = select(&ranked(&cs), Limits::default());
        assert_eq!(d.len(), DEFAULT_PER_FILE, "one file may not fill the whole list");

        let wide = select(&ranked(&cs), Limits { files: 8, per_file: 25 });
        assert_eq!(wide.len(), 25, "raising the passage cap reaches deeper into it");

        // Raising the *file* cap cannot help here: there is only one file.
        let more_files = select(&ranked(&cs), Limits { files: 50, per_file: DEFAULT_PER_FILE });
        assert_eq!(more_files.len(), DEFAULT_PER_FILE, "the two caps bound different things");
    }

    /// The property the file cap exists for, kept: one crowded note may not
    /// spend the whole list before a second note is reached.
    #[test]
    fn a_crowded_note_still_leaves_room_for_the_others() {
        let mut cs: Vec<Chunk> =
            (0..20).map(|i| at("big.org", &format!("B > Node {i}"), i + 1)).collect();
        cs.push(at("small.org", "Small", 1));
        let g = select(&ranked(&cs), Limits::default());
        assert_eq!(g.len(), DEFAULT_PER_FILE + 1);
        assert_eq!(g.last().unwrap().path, "small.org", "the runner-up note survives");
    }

    /// `files` counts notes, not nodes: a file already at the cap keeps
    /// contributing nodes, and a file beyond it contributes none.
    #[test]
    fn the_file_cap_counts_notes() {
        let cs =
            vec![at("a.org", "A > One", 1), at("b.org", "B > One", 1), at("a.org", "A > Two", 9)];
        let g = select(&ranked(&cs), Limits { files: 1, per_file: 5 });
        assert_eq!(g.len(), 2, "both of a.org's nodes, none of b.org's");
        assert!(g.iter().all(|x| x.path == "a.org"));
    }

    // ------------------------------------------------------------ org parsing

    #[test]
    fn headline_parts_are_separated() {
        let kw: Vec<String> = DEFAULT_TODO_KEYWORDS.iter().map(|s| s.to_string()).collect();
        let h = parse_headline("** TODO [#A] Fix the laser :hardware:urgent:", &kw).unwrap();
        assert_eq!(h.level, 2);
        assert_eq!(h.todo.as_deref(), Some("TODO"));
        assert_eq!(h.priority, Some('A'));
        assert_eq!(h.text, "Fix the laser");
        assert_eq!(h.tags, vec!["hardware", "urgent"]);
    }

    #[test]
    fn a_heading_without_markup_keeps_all_its_words() {
        let kw: Vec<String> = DEFAULT_TODO_KEYWORDS.iter().map(|s| s.to_string()).collect();
        let h = parse_headline("* GPU benchmarks", &kw).unwrap();
        assert_eq!(h.todo, None, "an all-caps word is not a keyword unless declared");
        assert_eq!(h.text, "GPU benchmarks");
        assert!(h.tags.is_empty());
    }

    /// A trailing `2:1` is a ratio, not a tag block.
    #[test]
    fn a_trailing_colon_run_that_is_not_tags_is_left_alone() {
        let kw: Vec<String> = DEFAULT_TODO_KEYWORDS.iter().map(|s| s.to_string()).collect();
        let h = parse_headline("* Duty cycle 2:1", &kw).unwrap();
        assert_eq!(h.text, "Duty cycle 2:1");
        assert!(h.tags.is_empty());
    }

    #[test]
    fn tags_are_inherited_from_ancestors_and_filetags() {
        let c = chunks_of(
            "#+title: T\n#+filetags: :physics:\n\n* Project :work:\nalpha\n\n** Sub :urgent:\nbeta\n\n* Other\ngamma\n",
        );
        let by = |needle: &str| c.iter().find(|x| x.text.trim() == needle).unwrap();
        assert_eq!(by("alpha").tags, vec!["physics", "work"]);
        assert_eq!(by("beta").tags, vec!["physics", "work", "urgent"], "inherits the ancestor");
        assert_eq!(by("gamma").tags, vec!["physics"], "sibling does not inherit");
    }

    #[test]
    fn todo_and_priority_are_inherited_by_the_body_beneath_them() {
        let c = chunks_of("#+title: T\n* TODO [#B] Task\nalpha\n\n** Detail\nbeta\n");
        let by = |needle: &str| c.iter().find(|x| x.text.trim() == needle).unwrap();
        assert_eq!(by("alpha").todo.as_deref(), Some("TODO"));
        assert_eq!(by("alpha").priority, Some('B'));
        assert_eq!(by("beta").todo.as_deref(), Some("TODO"), "nearest enclosing heading wins");
    }

    #[test]
    fn a_file_can_declare_its_own_todo_keywords() {
        let c = chunks_of("#+title: T\n#+TODO: LATER | SHIPPED\n* LATER Rewire\nalpha\n");
        assert_eq!(c[0].todo.as_deref(), Some("LATER"));
        assert_eq!(c[0].heading, "T > Rewire");
    }

    /// A planning line is metadata, like the tags and the TODO keyword beside
    /// it, and must not reach the text that gets embedded: in a project file
    /// nearly every heading carries one, so leaving them in would open most
    /// chunks with a date that says nothing about what the passage is.
    #[test]
    fn a_planning_line_is_metadata_and_is_not_embedded() {
        let c = chunks_of(
            "#+title: T\n* Task\nDEADLINE: <2026-09-01 Tue>\n:PROPERTIES:\n:ID: abc\n:END:\n\
             The mounts couple to the pulse tube.\n",
        );
        assert_eq!(c[0].text.trim(), "The mounts couple to the pulse tube.");
        assert_eq!(c[0].id.as_deref(), Some("abc"), "the drawer below it still resolves");
    }

    /// One line may carry several, which is what `org-element-planning-parser`
    /// loops over.
    #[test]
    fn several_planning_keywords_may_share_a_line() {
        let c = chunks_of(
            "#+title: T\n* Task\nSCHEDULED: <2026-08-20 Thu> DEADLINE: <2026-09-01 Tue>\nBody.\n",
        );
        assert_eq!(c[0].text.trim(), "Body.");
    }

    /// Position is the whole of what separates a deadline from a paragraph
    /// about one, so both of these stay: org requires the line immediately
    /// after the headline, and parses planning *before* the property drawer.
    #[test]
    fn a_deadline_anywhere_else_is_prose() {
        let below = chunks_of("#+title: T\n* Task\nSome prose.\nDEADLINE: <2026-09-01 Tue>\n");
        assert!(below[0].text.contains("DEADLINE:"), "not the line after the heading");

        let after_drawer = chunks_of(
            "#+title: T\n* Task\n:PROPERTIES:\n:ID: abc\n:END:\nDEADLINE: <2026-09-01 Tue>\n",
        );
        assert!(after_drawer[0].text.contains("DEADLINE:"), "planning precedes the drawer");
    }

    /// Org matches these case-sensitively — `org-element-planning-parser` binds
    /// `case-fold-search` to nil — so a sentence that opens with the word is
    /// prose.
    #[test]
    fn planning_keywords_are_matched_case_sensitively() {
        let c = chunks_of("#+title: T\n* Task\nDeadline: we agreed on the first of September.\n");
        assert!(c[0].text.contains("Deadline: we agreed"));
    }

    /// The two indexes want opposite things from a date, so the policy is split
    /// the way `#+RESULTS:` is: no use to an embedding, an ordinary thing to
    /// look up by word.
    #[test]
    fn planning_is_kept_for_words_and_dropped_for_meaning_by_default() {
        let note = "#+title: T\n* Task\nDEADLINE: <2026-09-01 Tue>\nBody.\n";
        let of = |target| {
            chunk_file(
                Path::new("/v/n.org"),
                "n.org",
                note,
                None,
                &Config::default(),
                target,
                &unsplit,
            )
        };
        assert!(!of(Target::Semantic)[0].text.contains("DEADLINE"), "noise to an embedding");
        assert!(of(Target::Lexical)[0].text.contains("DEADLINE: <2026-09-01 Tue>"), "searchable");
    }

    /// And either side can be turned round, which is the point of it being
    /// policy rather than a decision we made for everyone.
    #[test]
    fn planning_can_be_kept_or_dropped_on_either_side() {
        let note = "#+title: T\n* Task\nDEADLINE: <2026-09-01 Tue>\nBody.\n";
        let cfg = Config {
            planning_line: PlanningLinePolicy { semantic: true, lexical: false },
            ..Config::default()
        };
        let of =
            |target| chunk_file(Path::new("/v/n.org"), "n.org", note, None, &cfg, target, &unsplit);
        assert!(of(Target::Semantic)[0].text.contains("DEADLINE"));
        assert!(!of(Target::Lexical)[0].text.contains("DEADLINE"));

        // And it reaches the hash of the index it governs, and only that one.
        let d = Config::default();
        assert_ne!(cfg.hash_for(Target::Semantic), d.hash_for(Target::Semantic));
        assert_ne!(cfg.hash_for(Target::Lexical), d.hash_for(Target::Lexical));
        let semantic_only = Config {
            planning_line: PlanningLinePolicy { semantic: true, ..d.planning_line },
            ..d.clone()
        };
        assert_eq!(
            semantic_only.hash_for(Target::Lexical),
            d.hash_for(Target::Lexical),
            "a semantic-only change must not cost a lexical rebuild"
        );
        assert!(semantic_only
            .differences(&d, Target::Semantic)
            .iter()
            .any(|s| s.starts_with("planning_line.semantic:")));
    }

    /// Regression: org lets a keyword carry a fast-selection key and logging
    /// spec — `WAIT(w@/!)` — and matches headings against the bare name.  We
    /// registered the whole token, which no heading can equal, so the keyword
    /// stayed in the title and was embedded with it.
    #[test]
    fn a_declared_keyword_may_carry_its_selection_key_and_logging_spec() {
        let c = chunks_of(
            "#+title: T\n#+TODO: TODO(t) WAIT(w@/!) NEXT(n) | DONE(d) CANCELLED(c@)\n\
             * WAIT Waiting on the vendor\nalpha\n\n* CANCELLED Dropped\nbeta\n",
        );
        let by = |needle: &str| c.iter().find(|x| x.text.trim() == needle).unwrap();
        assert_eq!(by("alpha").todo.as_deref(), Some("WAIT"));
        assert_eq!(by("alpha").heading, "T > Waiting on the vendor");
        assert_eq!(by("beta").todo.as_deref(), Some("CANCELLED"));
        assert_eq!(by("beta").heading, "T > Dropped");
    }

    /// `#+TYP_TODO:` declares keywords that name a who or a what rather than a
    /// stage.  Org treats it alongside `#+TODO:` and `#+SEQ_TODO:`; we used to
    /// ignore it, leaving "Sara" in the heading.
    #[test]
    fn typ_todo_declares_keywords_too() {
        let c =
            chunks_of("#+title: T\n#+TYP_TODO: Fred Sara Lucy | DONE\n* Sara Chase it\nalpha\n");
        assert_eq!(c[0].todo.as_deref(), Some("Sara"));
        assert_eq!(c[0].heading, "T > Chase it");
    }

    /// Org's default is `((sequence "TODO" "DONE"))` and nothing else, so a
    /// heading starting with NEXT is titled "NEXT …" until the vault says
    /// otherwise — which is what the user's own Emacs shows them.
    #[test]
    fn the_default_keywords_are_orgs_own_and_the_vault_may_widen_them() {
        let c = chunks_of("#+title: T\n* NEXT Rewire\nalpha\n");
        assert_eq!(c[0].todo, None, "NEXT is not an org keyword by default");
        assert_eq!(c[0].heading, "T > NEXT Rewire");

        let cfg = Config {
            todo_keywords: vec!["TODO".into(), "NEXT".into(), "DONE".into()],
            ..Config::default()
        };
        let c = chunk_file(
            Path::new("/vault/Note.org"),
            "Note.org",
            "#+title: T\n* NEXT Rewire\nalpha\n",
            None,
            &cfg,
            Target::Semantic,
            &unsplit,
        );
        assert_eq!(c[0].todo.as_deref(), Some("NEXT"));
        assert_eq!(c[0].heading, "T > Rewire");
    }

    /// The keyword list decides what a heading *says*, so it must reach the
    /// embedded text — and therefore both indexes' policy hashes.
    #[test]
    fn changing_the_keywords_invalidates_both_indexes() {
        let a = Config::default();
        let b = Config { todo_keywords: vec!["TODO".into(), "NEXT".into()], ..a.clone() };
        for t in [Target::Semantic, Target::Lexical] {
            assert_ne!(a.hash_for(t), b.hash_for(t), "a keyword change is a real change");
        }
        // Still a set: restating it in another order is not a change.
        let reordered = Config {
            todo_keywords: vec!["DONE".into(), "TODO".into(), "DONE".into()],
            ..a.clone()
        };
        for t in [Target::Semantic, Target::Lexical] {
            assert_eq!(a.hash_for(t), reordered.hash_for(t), "order and repeats are not changes");
        }
        assert!(b
            .differences(&a, Target::Semantic)
            .iter()
            .any(|d| d.starts_with("todo_keywords:")));
    }

    #[test]
    fn tags_do_not_reach_the_embedded_text_or_the_heading() {
        let c = chunks_of("#+title: T\n#+filetags: :physics:\n* Section :work:\nbody\n");
        assert_eq!(c[0].heading, "T > Section");
        assert!(!c[0].text.contains("work"));
        assert_eq!(c[0].tags, vec!["physics", "work"]);
    }

    #[test]
    fn chunks_store_a_vault_relative_path() {
        let c = chunk_file(
            Path::new("/vault/sub/Note.org"),
            "sub/Note.org",
            "#+title: T\nbody\n",
            None,
            &Config::default(),
            Target::Semantic,
            &unsplit,
        );
        assert_eq!(c[0].path, "sub/Note.org", "relative, so the vault can move");
    }

    // --------------------------------------------------------------- filters

    fn chunk_with(path: &str, tags: &[&str], todo: Option<&str>) -> Chunk {
        Chunk {
            path: path.into(),
            id: None,
            heading: "H".into(),
            heading_line: 1,
            start_line: 1,
            end_line: 1,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            todo: todo.map(str::to_string),
            priority: None,
            lang: "en-US".into(),
            hash: 0,
            text: "body".into(),
        }
    }

    #[test]
    fn predicates_are_split_from_free_text() {
        let f = parse_query("tag:work dir:\"03 Literature review\" -tag:draft atom heating");
        assert_eq!(f.tags, vec!["work"]);
        assert_eq!(f.not_tags, vec!["draft"]);
        assert_eq!(f.dirs, vec!["03 Literature review"]);
        assert_eq!(f.text, "atom heating", "only the prose reaches the embedder");
    }

    /// A colon inside an ordinary word is not a predicate.
    #[test]
    fn unknown_keys_and_bare_colons_stay_in_the_text() {
        let f = parse_query("see https://example.com/a ratio 2:1 note:kept rabi");
        assert!(f.is_empty());
        assert_eq!(f.text, "see https://example.com/a ratio 2:1 note:kept rabi");
    }

    #[test]
    fn tags_narrow_and_dirs_widen() {
        let both = chunk_with("a.org", &["work", "urgent"], None);
        let one = chunk_with("a.org", &["work"], None);
        let f = parse_query("tag:work tag:urgent x");
        assert!(f.matches(&both), "all tags must be present");
        assert!(!f.matches(&one));

        let f = parse_query("dir:alpha dir:beta x");
        assert!(f.matches(&chunk_with("alpha/n.org", &[], None)), "any dir may match");
        assert!(f.matches(&chunk_with("beta/n.org", &[], None)));
        assert!(!f.matches(&chunk_with("gamma/n.org", &[], None)));
    }

    #[test]
    fn negation_and_todo_filter() {
        let f = parse_query("-tag:draft x");
        assert!(!f.matches(&chunk_with("a.org", &["draft"], None)));
        assert!(f.matches(&chunk_with("a.org", &["final"], None)));

        let f = parse_query("todo:next x");
        assert!(f.matches(&chunk_with("a.org", &[], Some("NEXT"))), "case-insensitive");
        assert!(!f.matches(&chunk_with("a.org", &[], Some("DONE"))));
        assert!(!f.matches(&chunk_with("a.org", &[], None)));
    }

    /// `dir:03 Lit` must not match `03 Literature review/…`.
    #[test]
    fn dir_matches_whole_components_only() {
        assert!(under("03 Literature review/x.org", "03 Literature review"));
        assert!(under("03 Literature review/2025/x.org", "03 Literature review"));
        assert!(!under("03 Literature review/x.org", "03 Lit"));
        assert!(!under("other/x.org", "03 Literature review"));
    }

    // --------------------------------------------------------------- lexical

    #[test]
    fn ancestor_dirs_names_every_enclosing_directory() {
        assert_eq!(
            ancestor_dirs("03 Literature review/Reviewed in 2025/x.org"),
            vec!["03 Literature review", "03 Literature review/Reviewed in 2025"]
        );
        assert!(ancestor_dirs("top.org").is_empty(), "a note at the root has none");
    }

    /// The lexical index identifies a chunk by (path, ordinal), never by
    /// position: positions shift whenever any earlier note gains or loses a
    /// chunk, and a stored position would then point at the wrong text.
    #[test]
    fn lexical_round_trips_through_a_real_index() {
        let v = scratch("lexical");
        let a = note(&v, "alpha");
        let b = note(&v, "beta");
        let chunks = vec![
            Chunk {
                path: a.clone(),
                id: None,
                heading: "alpha".into(),
                heading_line: 1,
                start_line: 1,
                end_line: 1,
                tags: vec!["physics".into()],
                todo: None,
                priority: None,
                lang: "en-US".into(),
                hash: 0,
                text: "the quick brown fox".into(),
            },
            Chunk {
                path: b.clone(),
                id: None,
                heading: "beta".into(),
                heading_line: 1,
                start_line: 1,
                end_line: 1,
                tags: vec!["german".into()],
                todo: None,
                priority: None,
                lang: "en-US".into(),
                hash: 0,
                text: "der schnelle braune Fuchs".into(),
            },
        ];
        let dir = state_dir(&v);
        fs::create_dir_all(&dir).unwrap();
        let an = lexical::Analyzer::widen(None, &chunks, false);
        lexical::sync(&dir, &chunks, &[], true, &an).unwrap();
        assert_eq!(lexical::doc_count(&dir, &an).unwrap(), 2);

        // The hit carries the chunk itself: nothing else has to be loaded to
        // know *which* passage matched, which is what lets the two indexes stand
        // apart.  Not its text — that is stored nowhere and read from the note,
        // so what comes back is where to look rather than what was said.
        let hits = lexical::search(&dir, &parse_query("brown"), 10, true, &an).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1.path, a, "the hit carries its own chunk");
        assert_eq!(hits[0].1.heading, "alpha");
        assert!(hits[0].1.text.is_empty(), "text is never read back out of the index");

        // A predicate must constrain the lexical side exactly as it does the
        // semantic one, or the two modes disagree about what was searched.
        let hits = lexical::search(&dir, &parse_query("tag:german Fuchs"), 10, true, &an).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1.path, b);
        let hits = lexical::search(&dir, &parse_query("tag:physics Fuchs"), 10, true, &an).unwrap();
        assert!(hits.is_empty(), "predicate excludes the only textual match");
    }

    #[test]
    fn lexical_sync_replaces_a_changed_note_and_drops_a_deleted_one() {
        let v = scratch("lexical-sync");
        let a = note(&v, "alpha");
        let b = note(&v, "beta");
        let mk = |p: &str, t: &str| Chunk {
            path: p.into(),
            id: None,
            heading: p.into(),
            heading_line: 1,
            start_line: 1,
            end_line: 1,
            tags: vec![],
            todo: None,
            priority: None,
            lang: "en-US".into(),
            hash: 0,
            text: t.into(),
        };
        let dir = state_dir(&v);
        fs::create_dir_all(&dir).unwrap();
        let chunks = vec![mk(&a, "brown fox"), mk(&b, "brown bear")];
        let an = lexical::Analyzer::widen(None, &chunks, false);
        lexical::sync(&dir, &chunks, &[], true, &an).unwrap();
        assert_eq!(lexical::search(&dir, &parse_query("brown"), 10, true, &an).unwrap().len(), 2);

        // beta changes, alpha is deleted.  Only beta's chunks are passed: a
        // note is replaced by path, so an incremental update never has to hold
        // the notes it is not touching.
        let changed = vec![mk(&b, "crimson bear")];
        let an2 = lexical::Analyzer::widen(Some(&an), &changed, false);
        lexical::sync(&dir, &changed, std::slice::from_ref(&a), false, &an2).unwrap();
        assert!(lexical::search(&dir, &parse_query("brown"), 10, true, &an2).unwrap().is_empty());
        assert_eq!(
            lexical::search(&dir, &parse_query("crimson"), 10, true, &an2).unwrap().len(),
            1
        );
        assert_eq!(lexical::doc_count(&dir, &an2).unwrap(), 1);
    }

    // ------------------------------------------------------------- languages

    #[test]
    fn ltex_magic_comment_is_read() {
        assert_eq!(ltex_language("# ltex: language=de-DE").as_deref(), Some("de-DE"));
        assert_eq!(ltex_language("#ltex: language=fr").as_deref(), Some("fr"));
        assert_eq!(
            ltex_language("# ltex: language=de-DE enabled=false").as_deref(),
            Some("de-DE"),
            "other ltex settings on the line are ignored, not tripped over"
        );
        assert_eq!(ltex_language("# ltex: enabled=false"), None);
        assert_eq!(ltex_language("# just a comment"), None);
        assert_eq!(
            ltex_language("# spell: language=it"),
            None,
            "only ltex's keyword: a custom one would be an annotation serving \
             nothing but this tool, and is recorded nowhere"
        );
    }

    /// ltex applies a magic comment from its own line onward, so a note may
    /// switch part-way; chunks before it keep the default.
    #[test]
    fn language_applies_from_its_line_onward() {
        let cfg = LangConfig::default();
        let c = chunk_file(
            Path::new("/v/N.org"),
            "N.org",
            "#+title: T\n* English part\nalpha\n\n# ltex: language=de-DE\n* Deutscher Teil\nbeta\n",
            Some(&cfg),
            &Config::default(),
            Target::Lexical,
            &unsplit,
        );
        let by = |n: &str| c.iter().find(|x| x.text.trim() == n).unwrap();
        assert_eq!(by("alpha").lang, "en-US", "the configured default");
        assert_eq!(by("beta").lang, "de-DE");
    }

    #[test]
    fn the_default_language_is_configurable() {
        let cfg = LangConfig::parse("it-IT");
        let c = chunk_file(
            Path::new("/v/N.org"),
            "N.org",
            "#+title: T\nciao\n",
            Some(&cfg),
            &Config::default(),
            Target::Lexical,
            &unsplit,
        );
        assert_eq!(c[0].lang, "it-IT");
    }

    #[test]
    fn lang_predicate_matches_at_subtag_boundaries() {
        assert!(lang_matches("de-DE", "de"), "lang:de finds de-DE");
        assert!(lang_matches("de-DE", "de-DE"));
        assert!(lang_matches("de-AT", "de"));
        assert!(!lang_matches("de-DE", "de-AT"));
        assert!(!lang_matches("en-US", "de"));
    }

    #[test]
    fn lang_is_a_lexical_predicate_only() {
        // A language picks a stemmer, and an embedding is not stemmed, so the
        // semantic index records none.  `matches` — which is the semantic-side
        // filter — therefore ignores `langs`, and `cmd_search` rejects the query
        // outright rather than quietly returning nothing.
        let mut en = chunk_with("b.org", &[], None);
        en.lang = "en-US".into();
        let f = parse_query("lang:de Wörter");
        assert_eq!(f.langs, vec!["de"], "still parsed, for the lexical side");
        assert!(f.matches(&en), "and not applied on the semantic side");
        assert_eq!(f.text, "Wörter", "the predicate does not reach the embedder");
    }

    /// Each chunk is stemmed in its own language, and `lang:` constrains the
    /// lexical side exactly as it constrains the semantic one.
    ///
    /// Regression: `langs` was added to `Filters` and honoured by the semantic
    /// path but never given a clause here, so `lang:en` returned German hits —
    /// the one invariant this project cannot afford to break quietly.
    #[test]
    fn each_language_is_stemmed_in_its_own_field_and_lang_filters_apply() {
        let v = scratch("langs");
        let en = note(&v, "en");
        let de = note(&v, "de");
        let mk = |p: &str, lang: &str, t: &str| Chunk {
            path: p.into(),
            id: None,
            heading: p.into(),
            heading_line: 1,
            start_line: 1,
            end_line: 1,
            tags: vec![],
            todo: None,
            priority: None,
            lang: lang.into(),
            hash: 0,
            text: t.into(),
        };
        let chunks = vec![
            mk(&en, "en-US", "the damped oscillations of a trapped atom"),
            mk(&de, "de-DE", "Die Wörter der Sprache sind lang"),
        ];
        let dir = state_dir(&v);
        fs::create_dir_all(&dir).unwrap();
        let an = lexical::Analyzer::widen(None, &chunks, false);
        assert_eq!(an.langs, vec!["de", "en"], "derived from the corpus");
        lexical::sync(&dir, &chunks, &[], true, &an).unwrap();

        let hits = |q: &str| lexical::search(&dir, &parse_query(q), 10, true, &an).unwrap().len();
        assert_eq!(hits("oscillation"), 1, "English stemming: singular finds plural");
        assert_eq!(hits("Sprachen"), 1, "German stemming: plural finds singular");
        assert_eq!(hits("lang:de Sprachen"), 1);
        assert_eq!(hits("lang:de-DE Sprachen"), 1, "full code matches");
        assert_eq!(hits("lang:DE-de Sprachen"), 1, "and is case-insensitive");
        assert_eq!(hits("lang:en Sprachen"), 0, "the predicate must exclude it");
        assert_eq!(hits("lang:de oscillation"), 0);
    }

    // ------------------------------------------------------ auto-detection

    #[test]
    fn detect_lang_returns_two_letter_codes() {
        assert_eq!(
            detect_lang("The quick brown fox jumps over the lazy dog again and again", &[]),
            "en"
        );
        assert_eq!(
            detect_lang(
                "Die Wörter der deutschen Sprache sind manchmal sehr lang und kompliziert",
                &[]
            ),
            "de"
        );
        assert_eq!(
            detect_lang(
                "Les élèves de la classe ont étudié la théorie pendant toute la semaine",
                &[]
            ),
            "fr"
        );
    }

    /// The regression that pinned `fasttext` to 0.7: the 0.8 rewrite answered
    /// `ar` at p = 0.999 here.  Multi-line because newlines separate documents
    /// for fastText, and a note classified by its first line alone is the other
    /// way this silently goes wrong.  See docs/fasttext-lid.md.
    #[test]
    fn a_quantized_model_classifies_multi_line_prose() {
        assert_eq!(
            detect_lang(
                "Die Wörter der deutschen Sprache sind manchmal sehr lang\n\
                 und kompliziert, aber man gewöhnt sich daran mit der Zeit.",
                &[]
            ),
            "de"
        );
    }

    #[test]
    fn candidates_confine_the_answer_and_keep_their_own_spelling() {
        // Unrestricted this is Portuguese; the vault says it is written in
        // English and German, so the best allowed language wins instead.
        let prose = "[[attachment:Bildschirmfoto 2024-07-08 um 14.43.48.jpg]]";
        assert_eq!(detect_lang(prose, &[]), "pt");
        assert!(matches!(detect_lang(prose, &["en-US", "de-DE"]).as_str(), "en-US" | "de-DE"));

        // A candidate is matched on its primary subtag but returned as written,
        // so the vault keeps the regional variant it thinks in.
        assert_eq!(
            detect_lang("Die Wörter der deutschen Sprache sind sehr lang", &["en-US", "de-DE"]),
            "de-DE"
        );
    }

    #[test]
    fn the_length_of_the_language_list_decides_the_policy() {
        // One language: no classifier, and it is what undeclared notes get.
        let one = LangConfig::parse("en-US");
        assert!(!one.detects());
        assert_eq!(one.undeclared(), "en-US");
        assert!(one.candidates().is_empty());

        // Several: classify, restricted to them.
        let some = LangConfig::parse("en-US, de-DE");
        assert!(some.detects());
        assert_eq!(some.candidates(), vec!["en-US", "de-DE"]);

        // `auto`: classify, unrestricted.
        let auto = LangConfig::parse("AUTO");
        assert!(auto.detects());
        assert!(auto.candidates().is_empty());

        // A language merely named `autopilot` is not `auto`.
        assert!(!LangConfig::parse("autopilot").detects());
    }

    #[test]
    fn auto_classifies_a_note_from_its_prose() {
        let cfg = LangConfig::parse("auto");
        let de = chunk_file(
            Path::new("/v/de.org"),
            "de.org",
            "#+title: Notiz\n* Abschnitt\nDie Wörter der deutschen Sprache sind manchmal sehr lang\n",
            Some(&cfg),
            &Config::default(),
            Target::Lexical,
                &unsplit,
);
        assert_eq!(de[0].lang, "de");
        let en = chunk_file(
            Path::new("/v/en.org"),
            "en.org",
            "#+title: Note\n* Section\nThe damped oscillations of a trapped atom in the tweezer\n",
            Some(&cfg),
            &Config::default(),
            Target::Lexical,
            &unsplit,
        );
        assert_eq!(en[0].lang, "en");
    }

    /// An explicit declaration always beats the classifier — otherwise a note
    /// could not correct a wrong guess.
    #[test]
    fn an_explicit_language_wins_over_auto() {
        let cfg = LangConfig::parse("auto");
        let c = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            "#+title: N\n* One\nThe damped oscillations of a trapped atom in the tweezer\n\n             # ltex: language=it-IT\n* Two\nThe text here is still English but declared Italian\n",
            Some(&cfg),
            &Config::default(),
            Target::Lexical,
                &unsplit,
);
        let by = |n: &str| c.iter().find(|x| x.text.contains(n)).unwrap();
        assert_eq!(by("damped").lang, "en", "classified");
        assert_eq!(by("declared").lang, "it-IT", "declared, and not overruled");
    }

    #[test]
    fn an_explicit_language_wins_even_from_outside_the_candidate_set() {
        // The vault says it is written in English and German.  A note that
        // declares Italian is Italian: the list constrains what the classifier
        // may guess, never what a note states about itself.
        let cfg = LangConfig::parse("en-US,de-DE");
        let c = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            "#+title: N\n* One\nThe damped oscillations of a trapped atom in the tweezer\n\n# ltex: language=it-IT\n* Two\nQuesto paragrafo dichiara la propria lingua\n",
            Some(&cfg),
            &Config::default(),
            Target::Lexical,
                &unsplit,
);
        let by = |n: &str| c.iter().find(|x| x.text.contains(n)).unwrap();
        assert_eq!(by("damped").lang, "en-US", "classified, from the candidates");
        assert_eq!(by("dichiara").lang, "it-IT", "declared, outside the candidates");
    }

    #[test]
    fn one_language_classifies_nothing_but_still_honours_a_declaration() {
        let cfg = LangConfig::parse("en-US");
        assert!(!cfg.detects(), "a single language must not run the classifier");
        let c = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            "#+title: N\n* One\nPlain English body text here\n\n# ltex: language=de-DE\n* Two\nDieser Abschnitt ist auf Deutsch geschrieben\n",
            Some(&cfg),
            &Config::default(),
            Target::Lexical,
                &unsplit,
);
        let by = |n: &str| c.iter().find(|x| x.text.contains(n)).unwrap();
        assert_eq!(by("Plain").lang, "en-US");
        assert_eq!(by("Abschnitt").lang, "de-DE");
    }

    #[test]
    fn a_declaration_the_classifier_does_not_know_falls_back_to_the_default() {
        // `klingon` is a typo far more often than a language, and honouring it
        // would file the chunk under a language nothing ever searches.  The
        // first configured language is the vault's default and takes over.
        let body =
            "#+title: N\n* One\n# ltex: language=klingon\nBody text under a bogus declaration\n";
        let listed = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            body,
            Some(&LangConfig::parse("de-DE,en-US")),
            &Config::default(),
            Target::Lexical,
            &unsplit,
        );
        assert_eq!(listed[0].lang, "de-DE", "first configured language wins");

        let single = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            body,
            Some(&LangConfig::parse("en-US")),
            &Config::default(),
            Target::Lexical,
            &unsplit,
        );
        assert_eq!(single[0].lang, "en-US");

        // Under `auto` there is no configured default, so it is classified.
        let auto = chunk_file(
            Path::new("/v/n.org"),
            "n.org",
            body,
            Some(&LangConfig::parse("auto")),
            &Config::default(),
            Target::Lexical,
            &unsplit,
        );
        assert_eq!(auto[0].lang, "en");
    }
}

#[cfg(test)]
mod prefix_check {
    use super::*;

    const PASSAGES: [&str; 6] = [
        "Atoms escape the optical tweezer when the trap depth drops below the recoil energy.",
        "Die Woerter der deutschen Sprache sind manchmal sehr lang und zusammengesetzt.",
        "La pasta va cotta in acqua salata per circa otto minuti.",
        "Rabi oscillations between two hyperfine ground states are driven by a microwave field.",
        "Der Zug faehrt jeden Morgen um sieben Uhr vom Hauptbahnhof ab.",
        "Compiling the kernel requires a working C toolchain and about two gigabytes of disk.",
    ];
    /// (query, index of the passage that should win)
    const QUERIES: [(&str, usize); 5] = [
        ("why do atoms get lost from the trap", 0),
        ("how long should I boil pasta", 2),
        ("driving transitions with microwaves", 3),
        ("wann faehrt der Zug ab", 4),
        ("building software from source", 5),
    ];

    fn score(which: EmbeddingModel, q_prefix: &str, p_prefix: &str) -> (usize, f32) {
        let mut model = model_with(which, None, false).unwrap();
        let docs: Vec<String> = PASSAGES.iter().map(|p| format!("{p_prefix}{p}")).collect();
        let mut dv = model.embed(&docs, None).unwrap();
        dv.iter_mut().for_each(|v| normalize(v));
        let (mut hits, mut margin) = (0usize, 0.0f32);
        for (q, want) in QUERIES {
            let mut qv = model.embed(&[format!("{q_prefix}{q}")], None).unwrap();
            normalize(&mut qv[0]);
            let mut s: Vec<(f32, usize)> = dv
                .iter()
                .enumerate()
                .map(|(i, d)| (d.iter().zip(qv[0].iter()).map(|(a, b)| a * b).sum::<f32>(), i))
                .collect();
            s.sort_by(|a, b| b.0.total_cmp(&a.0));
            if s[0].1 == want {
                hits += 1;
            }
            // How far the intended passage sits above the best distractor.
            let got = s.iter().find(|(_, i)| *i == want).unwrap().0;
            let best_other = s.iter().find(|(_, i)| *i != want).unwrap().0;
            margin += got - best_other;
        }
        (hits, margin / QUERIES.len() as f32)
    }

    #[test]
    #[ignore]
    fn prefixes_earn_their_place() {
        // Only models already in the cache: this must not quietly pull gigabytes.
        let cached: Vec<String> = fs::read_dir(cache_dir())
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        for m in MODELS {
            let code = TextEmbedding::list_supported_models()
                .into_iter()
                .find(|i| i.model == m.which)
                .map(|i| i.model_code.replace('/', "--"))
                .unwrap_or_default();
            if !cached.iter().any(|c| c == &format!("models--{code}")) {
                println!("{:<14} not cached, skipped", m.name);
                continue;
            }
            let with = score(m.which.clone(), m.query, m.passage);
            let without = score(m.which.clone(), "", "");
            println!(
                "{:<14} prefixed {}/{} margin {:+.3}   bare {}/{} margin {:+.3}",
                m.name,
                with.0,
                QUERIES.len(),
                with.1,
                without.0,
                QUERIES.len(),
                without.1
            );
        }
    }
}
