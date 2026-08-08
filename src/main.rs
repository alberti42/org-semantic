//! org-semantic — semantic search over a tree of org-mode notes.
//!
//! Prototype.  Commands:
//!
//!   org-semantic index  <vault> [--full|--rehash] [--lang en-US]  refresh the index
//!   org-semantic search  <vault> <query> [k]      semantic search, grouped by note
//!   org-semantic keyword <vault> <query> [k] [--any]  lexical search, same predicates
//!   org-semantic chunks <vault> <path-substring>  show chunking, no embedding
//!   org-semantic tokens <vault> [limit]           token-length distribution
//!   org-semantic bench  <vault> [n] [config]      embedding throughput
//!
//! The index lives in `<vault>/.org-semantic/`: a JSON chunk table and a flat
//! little-endian f32 array of embeddings.  There is no ANN index and no
//! database — a vault of a thousand notes is a few megabytes of vectors, and a
//! brute-force dot product over that is exact and takes under a millisecond.

use anyhow::{anyhow, Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// BGE-small-en-v1.5.
const DIM: usize = 384;

/// What BGE was trained to see in front of a query, and only a query — the
/// indexed side is embedded bare.  Leaving it off costs retrieval quality.
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// Roughly a screenful.  Small enough that a hit points at a passage rather
/// than at a whole note, large enough to carry its own context.
const MAX_CHARS: usize = 1500;

const STATE_DIR: &str = ".org-semantic";

/// How many matching sections to show beneath a note before collapsing.
const SECTIONS_PER_NOTE: usize = 3;

#[derive(Serialize, Deserialize, Clone)]
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
    /// 1-based, for the case where there is no `:ID:` to jump by.
    line: usize,
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

/// Read a language from an ltex magic comment, e.g. `# ltex: language=de-DE`.
///
/// Deliberately ltex-ls-plus' syntax rather than something new: a note that
/// declares its language for grammar checking has already said what this needs
/// to know, and one annotation serving both is better than two that can drift.
/// The keyword is configurable for anyone not using ltex.
///
/// Applies from its own line onward, as ltex does, so a note may switch
/// part-way.
fn ltex_language(line: &str, keyword: &str) -> Option<String> {
    let t = line.trim().trim_start_matches('#').trim();
    let rest = strip_prefix_ci(t, &format!("{keyword}:"))?;
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

/// Where a note's language comes from.
#[derive(Clone, Debug)]
struct LangConfig {
    /// Used when a note says nothing.  Mirrors `lsp-ltex-plus-language`, whose
    /// default is "en-US".
    default: String,
    /// Magic-comment keyword, `ltex` unless someone wants their own.
    keyword: String,
}

impl Default for LangConfig {
    fn default() -> Self {
        LangConfig { default: "en-US".into(), keyword: "ltex".into() }
    }
}

// ----------------------------------------------------------------- chunking

/// TODO keywords recognised when a file does not declare its own with
/// `#+TODO:`.  An explicit set rather than an all-caps heuristic: headings like
/// "GPU benchmarks" or "PID loop" would otherwise lose their first word.
const DEFAULT_TODO_KEYWORDS: &[&str] = &[
    "TODO", "NEXT", "STARTED", "WAITING", "HOLD", "SOMEDAY", "PROJ",
    "DONE", "CANCELLED", "CANCELED",
];

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
                    !t.is_empty()
                        && t.chars().all(|c| c.is_alphanumeric() || "_@#%-".contains(c))
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
/// past `MAX_CHARS`.
///
/// REL is the note's path relative to the vault root, and is what the chunk
/// stores.  PATH is only used to fall back to the filename when a note has no
/// `#+title:`.
///
/// Deliberately not a full org parser: for deciding where one passage ends and
/// the next begins, headings, property drawers and tags are the whole of what
/// matters, and the available Rust org parsers are alpha-stage.
fn chunk_file(path: &Path, rel: &str, text: &str, lang: &LangConfig) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    // Tags of each open heading, so a chunk can inherit from every ancestor.
    let mut tag_stack: Vec<Vec<String>> = Vec::new();
    let mut todo_stack: Vec<Option<String>> = Vec::new();
    let mut prio_stack: Vec<Option<char>> = Vec::new();
    let mut title = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut file_tags: Vec<String> = Vec::new();
    let mut todo_keywords: Vec<String> =
        DEFAULT_TODO_KEYWORDS.iter().map(|s| s.to_string()).collect();
    let mut file_id: Option<String> = None;
    let mut cur_id: Option<String> = None;
    let mut cur_line = 1usize;
    let mut buf = String::new();
    let mut in_drawer = false;
    let mut seen_heading = false;
    let mut cur_lang = lang.default.clone();

    // Collected first: `#+filetags:` and `#+TODO:` may appear after content, and
    // they apply to the whole file either way.
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = strip_prefix_ci(t, "#+filetags:") {
            file_tags.extend(parse_tag_list(rest));
        } else if let Some(rest) = strip_prefix_ci(t, "#+todo:") {
            todo_keywords.extend(parse_tag_list(rest).into_iter().filter(|w| w != "|"));
        } else if let Some(rest) = strip_prefix_ci(t, "#+seq_todo:") {
            todo_keywords.extend(parse_tag_list(rest).into_iter().filter(|w| w != "|"));
        }
    }

    let flush = |chunks: &mut Vec<Chunk>,
                 buf: &str,
                 stack: &[String],
                 tag_stack: &[Vec<String>],
                 todo_stack: &[Option<String>],
                 prio_stack: &[Option<char>],
                 title: &str,
                 file_tags: &[String],
                 id: &Option<String>,
                 line: usize,
                 lang: &str| {
        let body = buf.trim();
        if body.is_empty() {
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
        let todo = todo_stack.iter().rev().find_map(|t| t.clone());
        let priority = prio_stack.iter().rev().find_map(|p| *p);
        for piece in split_long(body) {
            chunks.push(Chunk {
                path: rel.to_string(),
                id: id.clone(),
                heading: heading.clone(),
                line,
                tags: tags.clone(),
                todo: todo.clone(),
                priority,
                lang: lang.to_string(),
                text: piece,
            });
        }
    };

    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let trimmed = line.trim();

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
            flush(&mut chunks, &buf, &stack, &tag_stack, &todo_stack, &prio_stack,
                  &title, &file_tags, &cur_id, cur_line, &cur_lang);
            buf.clear();
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
            continue;
        }

        if let Some(rest) = strip_prefix_ci(trimmed, "#+title:") {
            title = rest.trim().to_string();
            continue;
        }
        // Takes effect from here on, so a note may switch language part-way.
        if let Some(l) = ltex_language(line, &lang.keyword) {
            flush(&mut chunks, &buf, &stack, &tag_stack, &todo_stack, &prio_stack,
                  &title, &file_tags, &cur_id, cur_line, &cur_lang);
            buf.clear();
            cur_lang = l;
            continue;
        }
        // Other keywords, drawer ends and comments are markup, not prose.
        if trimmed.starts_with("#+") || trimmed.starts_with("# ") || trimmed == ":END:" {
            continue;
        }

        buf.push_str(line);
        buf.push('\n');
    }
    flush(&mut chunks, &buf, &stack, &tag_stack, &todo_stack, &prio_stack,
          &title, &file_tags, &cur_id, cur_line, &cur_lang);
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
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Begin each piece with the tail of the one before it, so an idea cut at a
/// boundary is embedded whole in at least one chunk.
///
/// Measured in paragraphs rather than characters.  A fixed character window —
/// org-db-v3 used 200 — would cut through the middle of a LaTeX display in
/// these notes, and half a display carries less meaning than none of it.
fn carry_over<'a, F>(prev: &[&'a str], next: &'a str, fits: F) -> Vec<&'a str>
where
    F: Fn(&str) -> bool,
{
    // Not when the previous piece was a single paragraph: repeating it whole
    // would make that piece a subset of this one rather than a neighbour.
    if prev.len() > 1 {
        if let Some(&tail) = prev.last() {
            if fits(&format!("{tail}\n\n{next}")) {
                return vec![tail, next];
            }
        }
    }
    vec![next]
}

/// Break a section longer than `MAX_CHARS` on paragraph boundaries, with one
/// paragraph of overlap between consecutive pieces.
fn split_long(body: &str) -> Vec<String> {
    if body.len() <= MAX_CHARS {
        return vec![body.to_string()];
    }
    let fits = |s: &str| s.len() <= MAX_CHARS;
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();

    for para in body.split("\n\n") {
        if para.len() > MAX_CHARS {
            // A single paragraph over the limit has no boundary to overlap on;
            // flush what is pending and hard-split it on char boundaries.
            if !cur.is_empty() {
                out.push(cur.join("\n\n"));
                cur.clear();
            }
            let mut rest = para;
            while rest.len() > MAX_CHARS {
                let mut cut = MAX_CHARS;
                while cut > 0 && !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            if !rest.trim().is_empty() {
                cur.push(rest);
            }
            continue;
        }
        if cur.is_empty() {
            cur.push(para);
            continue;
        }
        let cand = format!("{}\n\n{}", cur.join("\n\n"), para);
        if fits(&cand) {
            cur.push(para);
        } else {
            out.push(cur.join("\n\n"));
            cur = carry_over(&cur, para, fits);
        }
    }
    let last = cur.join("\n\n");
    if !last.trim().is_empty() {
        out.push(last);
    }
    out
}


// ------------------------------------------------------- token-limit enforcement

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

/// Re-split any chunk whose heading+body exceeds LIMIT tokens.  Returns the new
/// chunks and how many originals had to be divided.
fn enforce_token_limit(
    chunks: Vec<Chunk>,
    measure: &dyn Fn(&str) -> usize,
    limit: usize,
) -> (Vec<Chunk>, usize, Vec<usize>) {
    let mut out = Vec::with_capacity(chunks.len());
    let mut lens = Vec::with_capacity(chunks.len());
    let mut resplit = 0usize;
    for c in chunks {
        // Measured once, here, and handed back: the caller needs these lengths
        // to sort batches and to estimate remaining work, and re-tokenizing the
        // corpus to recover them would double the startup cost.
        let n = measure(&format!("{}\n{}", c.heading, c.text));
        if n <= limit {
            out.push(c);
            lens.push(n);
            continue;
        }
        resplit += 1;
        // The heading rides on every piece, so it comes out of the budget once.
        let budget = limit.saturating_sub(measure(&c.heading) + 4).max(32);
        for piece in split_to_fit(&c.text, measure, budget) {
            lens.push(measure(&format!("{}\n{}", c.heading, piece)));
            out.push(Chunk { text: piece, ..c.clone() });
        }
    }
    (out, resplit, lens)
}

/// Greedily pack paragraphs up to BUDGET tokens, with one paragraph of overlap
/// between consecutive pieces; hard-split any single paragraph that cannot fit
/// on its own.
fn split_to_fit(text: &str, measure: &dyn Fn(&str) -> usize, budget: usize) -> Vec<String> {
    let fits = |s: &str| measure(s) <= budget;
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();

    for para in text.split("\n\n") {
        if !fits(para) {
            // Nothing to overlap on: flush, then cut this paragraph to size.
            if !cur.is_empty() {
                out.push(cur.join("\n\n"));
                cur.clear();
            }
            out.extend(hard_split(para, measure, budget));
            continue;
        }
        if cur.is_empty() {
            cur.push(para);
            continue;
        }
        if fits(&format!("{}\n\n{}", cur.join("\n\n"), para)) {
            cur.push(para);
        } else {
            out.push(cur.join("\n\n"));
            cur = carry_over(&cur, para, fits);
        }
    }
    let last = cur.join("\n\n");
    if !last.trim().is_empty() {
        out.push(last);
    }
    out
}

/// Last resort for a single paragraph over budget: cut on char boundaries,
/// sized from this text's own measured chars-per-token so the guess is close,
/// then verified and shrunk until it actually fits.
fn hard_split(para: &str, measure: &dyn Fn(&str) -> usize, budget: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = para;
    while !rest.is_empty() {
        let toks = measure(rest);
        if toks <= budget {
            out.push(rest.to_string());
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
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    out
}

// ----------------------------------------------------------------- embedding

fn cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cache")
        });
    base.join("fastembed")
}

fn model() -> Result<TextEmbedding> {
    model_with(EmbeddingModel::BGESmallENV15, None, false)
}

fn model_with(
    which: EmbeddingModel,
    max_length: Option<usize>,
    coreml: bool,
) -> Result<TextEmbedding> {
    let mut opts = InitOptions::new(which)
        .with_cache_dir(cache_dir())
        .with_show_download_progress(true);
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
        if !self.langs.is_empty()
            && !self.langs.iter().any(|l| lang_matches(&c.lang, l))
        {
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
        || c.to_ascii_lowercase()
            .starts_with(&format!("{}-", want.to_ascii_lowercase()))
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
        Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED,
        STRING, TEXT,
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
        /// Fold accents, so `Worter` matches `Wörter`.  Off by default: it maps
        /// `ö` to `o`, and `Worter` is not a German word — the transliteration
        /// someone would actually type is `Woerter`, which this does not do.
        pub fold: bool,
    }

    /// Primary subtag of a language code: `de-DE` becomes `de`, so regional
    /// variants share one analyzer rather than splitting the index further.
    pub fn primary_subtag(code: &str) -> String {
        code.split(['-', '_']).next().unwrap_or(code).to_ascii_lowercase()
    }

    impl Analyzer {
        /// Derived from the corpus, so indexing and searching always agree.
        pub fn from_chunks(chunks: &[Chunk], fold: bool) -> Self {
            let mut langs: Vec<String> =
                chunks.iter().map(|c| primary_subtag(&c.lang)).collect();
            langs.sort();
            langs.dedup();
            if langs.is_empty() {
                langs.push("en".into());
            }
            Analyzer { langs, fold }
        }

        pub fn key(&self) -> String {
            format!("langs={} fold={}", self.langs.join("+"), self.fold)
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
        };
        (b.build(), f)
    }

    fn dir_of(state: &Path) -> std::path::PathBuf {
        state.join("tantivy")
    }

    fn key_file(state: &Path) -> std::path::PathBuf {
        dir_of(state).join("analyzer.txt")
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

    /// Apply CHANGED and DROPPED paths, or rebuild everything when FULL.
    pub fn sync(
        state: &Path,
        chunks: &[Chunk],
        changed: &[String],
        dropped: &[String],
        full: bool,
        a: &Analyzer,
    ) -> Result<()> {
        // A changed language set means a new schema, so partial updates against
        // the old one are meaningless.
        let rebuild =
            full || std::fs::read_to_string(key_file(state)).ok().as_deref() != Some(&a.key());
        let (index, f) = open_or_create(state, a)?;
        let mut w: IndexWriter = index.writer(50_000_000)?;
        let ords = ordinals(chunks);
        if rebuild {
            w.delete_all_documents()?;
            for (i, c) in chunks.iter().enumerate() {
                add(&w, &f, c, ords[i])?;
            }
        } else {
            for p in changed.iter().chain(dropped.iter()) {
                w.delete_term(Term::from_field_text(f.path, p));
            }
            let touched: std::collections::HashSet<&str> =
                changed.iter().map(String::as_str).collect();
            for (i, c) in chunks.iter().enumerate() {
                if touched.contains(c.path.as_str()) {
                    add(&w, &f, c, ords[i])?;
                }
            }
        }
        w.commit()?;
        std::fs::write(key_file(state), a.key())?;
        Ok(())
    }

    /// Number of live documents, for the consistency check.
    pub fn doc_count(state: &Path, a: &Analyzer) -> Result<u64> {
        let (index, _) = open_or_create(state, a)?;
        Ok(index.reader()?.searcher().num_docs())
    }

    /// Search, returning `(score, chunk index)` against CHUNKS.
    pub fn search(
        state: &Path,
        chunks: &[Chunk],
        f: &Filters,
        limit: usize,
        conjunction: bool,
        a: &Analyzer,
    ) -> Result<Vec<(f32, usize)>> {
        let (index, fl) = open_or_create(state, a)?;
        let searcher = index.reader()?.searcher();

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let term = |field, v: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, v),
                IndexRecordOption::Basic,
            ))
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

        // (path, ord) back to a position in CHUNKS.
        let ords = ordinals(chunks);
        let mut by_key: HashMap<(&str, u64), usize> = HashMap::new();
        for (i, c) in chunks.iter().enumerate() {
            by_key.insert((c.path.as_str(), ords[i]), i);
        }

        let query = BooleanQuery::new(clauses);
        let hits = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
        let mut out = Vec::with_capacity(hits.len());
        for (score, addr) in hits {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let path = doc.get_first(fl.path).and_then(|v| v.as_str()).unwrap_or("");
            let ord = doc.get_first(fl.ord).and_then(|v| v.as_u64()).unwrap_or(0);
            if let Some(&i) = by_key.get(&(path, ord)) {
                out.push((score, i));
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
const INDEX_VERSION: u32 = 3;

/// Recorded so that changing the embedding model invalidates every vector.
/// Vectors from two different models are not comparable, and mixing them
/// silently degrades every search rather than failing.
const MODEL_NAME: &str = "BGESmallENV15";

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

/// Read a previous index, or `None` when there is none, when it was written by
/// a different model or layout, or when its two halves disagree.
///
/// The last case is the one worth being strict about: `chunks.json` and
/// `vectors.f32` are positionally coupled, so a mismatch does not fail loudly —
/// it silently returns the wrong note for every query.
fn load_index(dir: &Path) -> Option<LoadedIndex> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(dir.join("manifest.json")).ok()?).ok()?;
    if manifest.version != INDEX_VERSION || manifest.model != MODEL_NAME || manifest.dim != DIM {
        eprintln!("  existing index was built differently; rebuilding from scratch");
        return None;
    }
    let chunks: Vec<Chunk> = serde_json::from_slice(&fs::read(dir.join("chunks.json")).ok()?).ok()?;
    let raw = fs::read(dir.join("vectors.f32")).ok()?;
    if raw.len() != chunks.len() * DIM * 4 {
        eprintln!(
            "  index is inconsistent ({} chunks, {} vectors); rebuilding from scratch",
            chunks.len(),
            raw.len() / (DIM * 4)
        );
        return None;
    }
    let vectors: Vec<f32> = raw
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let mut by_path: std::collections::HashMap<String, Vec<usize>> = Default::default();
    for (i, c) in chunks.iter().enumerate() {
        by_path.entry(c.path.clone()).or_default().push(i);
    }
    Some(LoadedIndex { chunks, vectors, files: manifest.files, stamps: manifest.stamps, by_path })
}

fn save_index(
    dir: &Path,
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
            model: MODEL_NAME.into(),
            dim: DIM,
            files,
            stamps,
        })?,
    )?;
    Ok(bytes.len())
}

fn state_dir(vault: &Path) -> PathBuf {
    vault.join(STATE_DIR)
}

fn cmd_index(vault: &Path, full: bool, rehash: bool, lang: &LangConfig) -> Result<()> {
    let t0 = Instant::now();
    let mut files = Vec::new();
    org_files(vault, &mut files)?;
    files.sort();

    let dir = state_dir(vault);
    let old = if full { None } else { load_index(&dir) };

    // Three outcomes per note, cheapest first: its stamp matches, so it is not
    // even read; its stamp moved but its bytes hash the same, so it is read and
    // reused; or it is genuinely new or changed, and must be re-embedded.
    struct Stale {
        path: String,
        text: String,
    }
    let mut hashes: std::collections::BTreeMap<String, u64> = Default::default();
    let mut stamps: std::collections::BTreeMap<String, Stamp> = Default::default();
    let mut reuse: Vec<String> = Vec::new();
    let mut stale: Vec<Stale> = Vec::new();
    let (mut by_stamp, mut by_hash, mut changed_files, mut new_files) = (0usize, 0, 0, 0);

    for f in &files {
        let path = rel_path(vault, f);
        let stamp = stamp_of(f);

        // Fast path: same mtime and size as when we last looked.
        if !rehash {
            if let (Some(ix), Some(st)) = (&old, stamp) {
                if ix.stamps.get(&path) == Some(&st) {
                    if let Some(h) = ix.files.get(&path) {
                        hashes.insert(path.clone(), *h);
                        stamps.insert(path.clone(), st);
                        reuse.push(path);
                        by_stamp += 1;
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
        hashes.insert(path.clone(), hash);
        if let Some(st) = stamp {
            stamps.insert(path.clone(), st);
        }

        match old.as_ref().and_then(|ix| ix.files.get(&path)) {
            // Timestamp moved, content did not: restamp and reuse the vectors.
            Some(h) if *h == hash => {
                by_hash += 1;
                reuse.push(path);
            }
            Some(_) => {
                changed_files += 1;
                stale.push(Stale { path, text });
            }
            None => {
                new_files += 1;
                stale.push(Stale { path, text });
            }
        }
    }

    let dropped = old
        .as_ref()
        .map(|ix| ix.files.keys().filter(|p| !hashes.contains_key(*p)).count())
        .unwrap_or(0);

    // Loaded only if something actually needs chunking.
    let tok = if stale.is_empty() {
        None
    } else {
        Some(
            tokenizers::Tokenizer::from_file(find_tokenizer()?)
                .map_err(|e| anyhow!("loading tokenizer: {e}"))?,
        )
    };

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

    for f in &files {
        let path = rel_path(vault, f);
        if reused.contains(path.as_str()) {
            if let Some(ix) = &old {
                for &j in ix.by_path.get(&path).map(Vec::as_slice).unwrap_or(&[]) {
                    chunks.push(ix.chunks[j].clone());
                    vectors.extend_from_slice(&ix.vectors[j * DIM..(j + 1) * DIM]);
                }
            }
            continue;
        }
        let Some(text) = stale_text.get(path.as_str()) else { continue };
        let tok = tok.as_ref().expect("tokenizer is loaded whenever anything is stale");
        let measure = |t: &str| n_tokens(tok, t);
        let (cs, n, lens) =
            enforce_token_limit(chunk_file(f, &path, text, lang), &measure, TOKEN_LIMIT);
        resplit += n;
        for (c, len) in cs.into_iter().zip(lens) {
            pending.push(chunks.len());
            pending_len.push(len);
            chunks.push(c);
            vectors.extend(std::iter::repeat(0.0).take(DIM));
        }
    }

    if old.is_some() {
        println!(
            "{} org files · {by_stamp} by stamp · {by_hash} restamped · \
             {changed_files} changed · {new_files} new · {dropped} removed",
            files.len()
        );
    } else {
        println!("{} org files", files.len());
    }
    if resplit > 0 {
        eprintln!("  {resplit} sections ran past {TOKEN_LIMIT} tokens and were divided");
    }
    println!(
        "{} chunks · {} to embed · scanned in {:.2}s",
        chunks.len(),
        pending.len(),
        t0.elapsed().as_secs_f64()
    );

    // Only a run that changes nothing at all may skip the write.  Dropping a
    // deleted note, or merely refreshing stamps, produces no work to embed but
    // must still be persisted.
    let restamped = by_hash > 0 || old.as_ref().is_some_and(|ix| ix.stamps.len() != stamps.len());
    if pending.is_empty() && dropped == 0 && !restamped && old.is_some() {
        println!("nothing changed; index left as it is");
        return Ok(());
    }

    if pending.is_empty() {
        println!("no new text to embed; rewriting the manifest");
    } else {
        let t1 = Instant::now();
        let mut model = model()?;
        println!("model loaded in {:.2}s", t1.elapsed().as_secs_f64());

        let t2 = Instant::now();
        // Heading path prepended so a passage carries the context it sits under.
        let texts: Vec<String> = pending
            .iter()
            .map(|&i| format!("{}\n{}", chunks[i].heading, chunks[i].text))
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
            let vs = model
                .embed(&batch, Some(BATCH))
                .map_err(|e| anyhow!("embedding: {e}"))?;
            for (&i, mut v) in group.iter().zip(vs) {
                normalize(&mut v);
                let slot = pending[i] * DIM;
                vectors[slot..slot + DIM].copy_from_slice(&v);
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
        println!(
            "embedded {} chunks in {:.1}s ({:.0}/s)",
            texts.len(),
            t2.elapsed().as_secs_f64(),
            texts.len() as f64 / t2.elapsed().as_secs_f64()
        );
    }

    let hashes_snapshot: std::collections::BTreeSet<String> =
        hashes.keys().cloned().collect();
    let written = save_index(&dir, &chunks, &vectors, hashes, stamps)?;
    // The lexical index follows the same deltas.  Failing to update it must not
    // fail the run: it is derived, and `keyword` rebuilds it when the counts
    // disagree.
    let changed_paths: Vec<String> = stale.iter().map(|s| s.path.clone()).collect();
    let dropped_paths: Vec<String> = old
        .as_ref()
        .map(|ix| {
            ix.files
                .keys()
                .filter(|p| !hashes_snapshot.contains(*p))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let analyzer = lexical::Analyzer::from_chunks(&chunks, false);
    if let Err(e) = lexical::sync(
        &dir,
        &chunks,
        &changed_paths,
        &dropped_paths,
        old.is_none(),
        &analyzer,
    ) {
        eprintln!("  lexical index not updated ({e}); `keyword` will rebuild it");
    }
    println!(
        "wrote {} ({:.1} MB of vectors) in {:.2}s total",
        dir.display(),
        written as f64 / 1e6,
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Print hits grouped by note.
///
/// A note that matches a query tends to match it in several places, and a flat
/// top-k then spends every slot on one document.  Each note appears once, at
/// the rank of its best chunk, with its other matching sections beneath it.
fn report(chunks: &[Chunk], scored: Vec<(f32, usize)>, k: usize) {
    let mut notes: Vec<(String, Vec<(f32, usize)>)> = Vec::new();
    for (score, i) in scored {
        let path = &chunks[i].path;
        match notes.iter_mut().find(|(p, _)| p == path) {
            Some((_, hits)) => {
                if hits.len() < SECTIONS_PER_NOTE {
                    hits.push((score, i));
                }
            }
            None => {
                if notes.len() < k {
                    notes.push((path.clone(), vec![(score, i)]));
                }
            }
        }
    }

    for (path, hits) in &notes {
        let (best, bi) = hits[0];
        let c = &chunks[bi];
        let title = c.heading.split(" > ").next().unwrap_or(&c.heading);
        println!("\n{best:.3}  {title}");
        println!("       {}:{}", path, c.line);
        if let Some(id) = &c.id {
            println!("       id:{id}");
        }
        if !c.tags.is_empty() || c.todo.is_some() {
            let todo = c.todo.as_deref().map(|t| format!("{t} ")).unwrap_or_default();
            let tags = if c.tags.is_empty() {
                String::new()
            } else {
                format!(":{}:", c.tags.join(":"))
            };
            println!("       {todo}{tags}");
        }
        for (score, i) in hits {
            let c = &chunks[*i];
            let section = c
                .heading
                .split_once(" > ")
                .map(|(_, rest)| rest)
                .unwrap_or("(top)");
            let preview: String =
                c.text.split_whitespace().take(20).collect::<Vec<_>>().join(" ");
            println!("       · {score:.3} L{:<5} {section}", c.line);
            println!("               {preview}…");
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

fn cmd_search(vault: &Path, query: &str, k: usize) -> Result<()> {
    let dir = state_dir(vault);
    let chunks: Vec<Chunk> = serde_json::from_slice(
        &fs::read(dir.join("chunks.json"))
            .with_context(|| format!("no index in {} — run `index` first", dir.display()))?,
    )?;
    let raw = fs::read(dir.join("vectors.f32"))?;
    let n = raw.len() / (DIM * 4);
    if n != chunks.len() {
        return Err(anyhow!(
            "index is inconsistent: {n} vectors for {} chunks",
            chunks.len()
        ));
    }
    let vectors: Vec<f32> = raw
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    // Predicates constrain which chunks are considered; only the remaining free
    // text is embedded.
    let f = parse_query(query);
    let candidates: Vec<usize> = (0..n).filter(|&i| f.matches(&chunks[i])).collect();
    if !f.is_empty() {
        println!(
            "filter: {} → {} of {n} chunks",
            describe_filters(&f),
            candidates.len()
        );
    }
    if candidates.is_empty() {
        println!("no chunk matches those filters");
        return Ok(());
    }
    if f.text.trim().is_empty() {
        return Err(anyhow!("nothing to search for: the query is only filters"));
    }

    let t0 = Instant::now();
    let mut model = model()?;
    let load = t0.elapsed();

    let t1 = Instant::now();
    let mut q = model
        .embed(&[format!("{QUERY_PREFIX}{}", f.text)], None)
        .map_err(|e| anyhow!("embedding query: {e}"))?
        .remove(0);
    normalize(&mut q);
    let embed = t1.elapsed();

    let t2 = Instant::now();
    let mut scored: Vec<(f32, usize)> = candidates
        .iter()
        .map(|&i| {
            let s = &vectors[i * DIM..(i + 1) * DIM];
            (s.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>(), i)
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
    let search = t2.elapsed();

    report(&chunks, scored, k);
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
    let mut chunks = Vec::new();
    for f in &files {
        if let Ok(text) = fs::read_to_string(f) {
            chunks.extend(chunk_file(f, &rel_path(vault, f), &text, &LangConfig::default()));
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

    let texts: Vec<String> = chunks
        .iter()
        .map(|c| format!("{}\n{}", c.heading, c.text))
        .collect();

    // One configuration per process: an ORT session that is merely dropped does
    // not necessarily return its arena, and running four in a row was enough to
    // get the process OOM-killed.
    let configs: Vec<(&str, EmbeddingModel, Option<usize>, bool)> = match which_config {
        "cpu512" => vec![("CPU  f32 max_len 512", EmbeddingModel::BGESmallENV15, Some(512), false)],
        "cpu256" => vec![("CPU  f32 max_len 256", EmbeddingModel::BGESmallENV15, Some(256), false)],
        "coreml512" => vec![("CoreML f32 max_len 512", EmbeddingModel::BGESmallENV15, Some(512), true)],
        "coreml256" => vec![("CoreML f32 max_len 256", EmbeddingModel::BGESmallENV15, Some(256), true)],
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
fn cmd_tokens(vault: &Path, limit: usize) -> Result<()> {
    let tok_path = find_tokenizer()?;
    let tok = tokenizers::Tokenizer::from_file(&tok_path)
        .map_err(|e| anyhow!("loading tokenizer {}: {e}", tok_path.display()))?;

    let mut files = Vec::new();
    org_files(vault, &mut files)?;
    files.sort();
    let mut chunks = Vec::new();
    for f in &files {
        if let Ok(text) = fs::read_to_string(f) {
            chunks.extend(chunk_file(f, &rel_path(vault, f), &text, &LangConfig::default()));
        }
    }
    // Same splitting the index applies, so this reports what is actually
    // embedded rather than the raw sections.
    let raw = chunks.len();
    let measure = |s: &str| n_tokens(&tok, s);
    let (chunks, resplit, _) = enforce_token_limit(chunks, &measure, limit);
    println!("{raw} raw chunks · {resplit} re-split · {} embedded", chunks.len());
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

    println!("{n} chunks · tokens: median {} · p90 {} · p99 {} · max {}",
             sorted[n / 2], sorted[n * 9 / 10], sorted[n * 99 / 100], sorted[n - 1]);
    println!("total {total} tokens · {} chunks over {limit} ({:.1}%) · {lost} tokens truncated ({:.2}% of corpus)",
             over.len(), 100.0 * over.len() as f64 / n as f64,
             100.0 * lost as f64 / total as f64);
    println!("chars-per-token overall: {:.2}",
             texts.iter().map(|t| t.len()).sum::<usize>() as f64 / total as f64);
    for (t, i) in over.iter().take(8) {
        println!("  {t} tokens · {} chars · {}", texts[*i].len(), chunks[*i].heading);
    }
    Ok(())
}

fn find_tokenizer() -> Result<PathBuf> {
    let root = cache_dir().join("models--Xenova--bge-small-en-v1.5");
    let snaps = root.join("snapshots");
    for e in fs::read_dir(&snaps).with_context(|| format!("reading {}", snaps.display()))? {
        let p = e?.path().join("tokenizer.json");
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!("no tokenizer.json under {}", snaps.display()))
}

/// Print the chunks a file produces, without embedding — for checking chunking
/// decisions (boundaries, overlap, headings) without paying for a full index.
fn cmd_chunks(vault: &Path, needle: &str) -> Result<()> {
    let tok = tokenizers::Tokenizer::from_file(find_tokenizer()?)
        .map_err(|e| anyhow!("loading tokenizer: {e}"))?;
    let mut files = Vec::new();
    org_files(vault, &mut files)?;
    files.sort();
    for f in files.iter().filter(|f| f.to_string_lossy().contains(needle)) {
        let text = fs::read_to_string(f)?;
        let measure = |s: &str| n_tokens(&tok, s);
        let (chunks, _, _) =
            enforce_token_limit(
                chunk_file(f, &rel_path(vault, f), &text, &LangConfig::default()),
                &measure,
                TOKEN_LIMIT,
            );
        println!("\n=== {} — {} chunks", f.display(), chunks.len());
        for (i, c) in chunks.iter().enumerate() {
            let full = format!("{}\n{}", c.heading, c.text);
            println!(
                "\n--- [{i}] L{} · {} tok · {} chars · {}",
                c.line,
                n_tokens(&tok, &full),
                c.text.len(),
                c.heading.split(" > ").last().unwrap_or("")
            );
            println!("    head: {:?}", &c.text.chars().take(60).collect::<String>());
            println!("    tail: {:?}", &c.text.chars().rev().take(60).collect::<String>()
                                          .chars().rev().collect::<String>());
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
fn cmd_keyword(vault: &Path, query: &str, k: usize, conjunction: bool) -> Result<()> {
    let dir = state_dir(vault);
    let chunks: Vec<Chunk> = serde_json::from_slice(
        &fs::read(dir.join("chunks.json"))
            .with_context(|| format!("no index in {} — run `index` first", dir.display()))?,
    )?;

    // The lexical index is derived, so it can be rebuilt rather than trusted.
    // A count that disagrees with chunks.json means it missed an update, and a
    // stale keyword index returns confidently wrong answers.
    let analyzer = lexical::Analyzer::from_chunks(&chunks, false);
    let have = lexical::doc_count(&dir, &analyzer).unwrap_or(0);
    if have != chunks.len() as u64 {
        eprint!(
            "  lexical index has {have} docs for {} chunks; rebuilding... ",
            chunks.len()
        );
        io::stderr().flush().ok();
        let t = Instant::now();
        lexical::sync(&dir, &chunks, &[], &[], true, &analyzer)?;
        eprintln!("{:.1}s", t.elapsed().as_secs_f64());
    }

    let f = parse_query(query);
    if !f.is_empty() {
        println!("filter: {}", describe_filters(&f));
    }
    let t = Instant::now();
    // Generous, because grouping collapses many chunks into one note: a single
    // well-matching note can otherwise fill the whole candidate pool and hide
    // every other note.
    let hits = lexical::search(&dir, &chunks, &f, (k * 25).max(100), conjunction, &analyzer)?;
    let el = t.elapsed();
    if hits.is_empty() {
        println!("no match");
        return Ok(());
    }
    report(&chunks, hits, k);
    eprintln!(
        "\n[lexical search over {} chunks {:.1}ms]",
        chunks.len(),
        el.as_secs_f64() * 1000.0
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("index") => {
            let vault = args.get(2).ok_or_else(|| anyhow!("usage: index <vault>"))?;
            // `--incremental` is the default; accepted so a script can say so.
            let full = args.iter().skip(3).any(|a| a == "--full");
            // `--rehash` reads and hashes every note, ignoring stamps: the
            // backstop for a change that left mtime untouched.
            let rehash = args.iter().skip(3).any(|a| a == "--rehash");
            let mut lang = LangConfig::default();
            for (i, a) in args.iter().enumerate().skip(3) {
                match a.as_str() {
                    "--lang" => {
                        if let Some(v) = args.get(i + 1) {
                            lang.default = v.clone();
                        }
                    }
                    "--lang-keyword" => {
                        if let Some(v) = args.get(i + 1) {
                            lang.keyword = v.clone();
                        }
                    }
                    _ => {}
                }
            }
            if lang.default.eq_ignore_ascii_case("auto") {
                return Err(anyhow!(
                    "--lang auto needs a language classifier, which is not built yet"
                ));
            }
            cmd_index(Path::new(vault), full, rehash, &lang)
        }
        Some("search") => {
            let vault = args
                .get(2)
                .ok_or_else(|| anyhow!("usage: search <vault> <query> [k]"))?;
            let query = args
                .get(3)
                .ok_or_else(|| anyhow!("usage: search <vault> <query> [k]"))?;
            let k = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
            cmd_search(Path::new(vault), query, k)
        }
        Some("keyword") => {
            let vault = args
                .get(2)
                .ok_or_else(|| anyhow!("usage: keyword <vault> <query> [k]"))?;
            let query = args
                .get(3)
                .ok_or_else(|| anyhow!("usage: keyword <vault> <query> [k]"))?;
            let k = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
            // Flags are looked for only past the positional arguments, so a
            // query whose text happens to be `--any` stays a query.  Emacs will
            // be building argv programmatically, where that is easy to hit.
            let conjunction = !args.iter().skip(4).any(|a| a == "--any");
            cmd_keyword(Path::new(vault), query, k, conjunction)
        }
        Some("chunks") => {
            let vault = args.get(2).ok_or_else(|| anyhow!("usage: chunks <vault> <path-substring>"))?;
            let needle = args.get(3).map(String::as_str).unwrap_or("");
            cmd_chunks(Path::new(vault), needle)
        }
        Some("tokens") => {
            let vault = args.get(2).ok_or_else(|| anyhow!("usage: tokens <vault> [limit]"))?;
            let limit = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(512);
            cmd_tokens(Path::new(vault), limit)
        }
        Some("bench") => {
            let vault = args.get(2).ok_or_else(|| anyhow!("usage: bench <vault> [n]"))?;
            let n = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(500);
            let cfg = args.get(4).map(String::as_str).unwrap_or("cpu512");
            cmd_bench(Path::new(vault), n, cfg)
        }
        _ => Err(anyhow!(
            "usage:\n  org-semantic index   <vault>\n  org-semantic search <vault> <query> [k]\n  org-semantic bench  <vault> [n]"
        )),
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

    fn para(word: &str, n: usize) -> String {
        std::iter::repeat(word).take(n).collect::<Vec<_>>().join(" ")
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
        chunk_file(Path::new("/vault/Note.org"), "Note.org", text, &LangConfig::default())
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
        assert_eq!(c[1].line, 5);
        assert_eq!(c[2].line, 8);
    }

    // ------------------------------------------------------------- packing

    #[test]
    fn packs_groups_of_paragraphs_not_single_ones() {
        // Ten 10-word paragraphs against a budget of 35: must split, and each
        // piece should still hold three paragraphs rather than one.
        let body = (0..10).map(|_| para("w", 10)).collect::<Vec<_>>().join("\n\n");
        let pieces = split_to_fit(&body, &words, 35);
        assert!(pieces.len() > 1, "should have split");
        assert!(
            pieces.iter().any(|p| p.split("\n\n").count() > 1),
            "pieces must group paragraphs, not isolate them"
        );
    }

    #[test]
    fn overlap_repeats_the_previous_paragraph() {
        let paras: Vec<String> = (0..6).map(|i| format!("p{i} {}", para("w", 20))).collect();
        let pieces = split_to_fit(&paras.join("\n\n"), &words, 60);
        assert!(pieces.len() >= 2);
        // The second piece must begin with the last paragraph of the first.
        let tail_of_first = pieces[0].split("\n\n").last().unwrap();
        assert!(
            pieces[1].starts_with(tail_of_first),
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
        let pieces = split_to_fit(&body, &words, 60);
        assert_eq!(pieces.len(), 2);
        assert!(!pieces[1].contains("a a a"), "must not repeat a whole single-paragraph piece");
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
                for piece in split_to_fit(&body, &words, budget) {
                    assert!(
                        words(&piece) <= budget,
                        "piece of {} words exceeds budget {budget}",
                        words(&piece)
                    );
                }
            }
        }
    }

    #[test]
    fn enforce_token_limit_accounts_for_the_heading() {
        let long = para("w", 200);
        let c = vec![Chunk {
            path: "n.org".into(),
            id: Some("id".into()),
            heading: para("h", 10),
            line: 1,
            tags: Vec::new(),
            todo: None,
            priority: None,
            lang: "en-US".into(),
            text: long,
        }];
        let (out, resplit, _) = enforce_token_limit(c, &words, 50);
        assert_eq!(resplit, 1);
        for ch in &out {
            let full = format!("{}\n{}", ch.heading, ch.text);
            assert!(words(&full) <= 50, "heading + text must fit: {}", words(&full));
        }
    }

    #[test]
    fn enforce_token_limit_leaves_conforming_chunks_alone() {
        let c = vec![Chunk {
            path: "n.org".into(),
            id: None,
            heading: "H".into(),
            line: 3,
            tags: Vec::new(),
            todo: None,
            priority: None,
            lang: "en-US".into(),
            text: "short body".into(),
        }];
        let (out, resplit, _) = enforce_token_limit(c, &words, 50);
        assert_eq!(resplit, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 3, "metadata must survive the pass");
    }

    #[test]
    fn hard_split_is_lossless_and_within_budget() {
        let para = para("word", 500);
        let pieces = hard_split(&para, &words, 40);
        assert!(pieces.len() > 1);
        for p in &pieces {
            assert!(words(p) <= 40, "{} words > 40", words(p));
        }
        assert_eq!(pieces.concat(), para, "hard split must not lose or reorder text");
    }

    #[test]
    fn hard_split_never_cuts_inside_a_character() {
        // All multi-byte, so a naive byte cut would panic or corrupt.
        let para = std::iter::repeat("é→ü ").take(400).collect::<String>();
        let pieces = hard_split(&para, &words, 30);
        assert_eq!(pieces.concat(), para);
    }

    // ------------------------------------------------- index round-trip / prune

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
                line: 1,
                tags: Vec::new(),
                todo: None,
                priority: None,
                lang: "en-US".into(),
                text: "body".into(),
            });
            files.insert((*p).to_string(), content_hash(&fs::read(dir.join(p)).unwrap()));
        }
        let stamps = paths
            .iter()
            .map(|p| ((*p).to_string(), stamp_of(&dir.join(p)).unwrap()))
            .collect();
        let vectors = vec![0.0f32; chunks.len() * DIM];
        save_index(&state_dir(dir), &chunks, &vectors, files, stamps).unwrap();
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
        cmd_index(&v, false, false, &LangConfig::default()).unwrap();

        let ix = load_index(&state_dir(&v)).expect("index should still load");
        assert_eq!(ix.chunks.len(), 1, "beta's chunk must be gone");
        assert_eq!(ix.chunks[0].path, a);
        assert!(!ix.files.contains_key(&b), "beta must be gone from the manifest");
        assert_eq!(ix.vectors.len(), ix.chunks.len() * DIM, "halves stay aligned");
    }

    #[test]
    fn an_unchanged_vault_is_left_alone() {
        let v = scratch("unchanged");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);
        let before = fs::read(state_dir(&v).join("vectors.f32")).unwrap();
        cmd_index(&v, false, false, &LangConfig::default()).unwrap();
        assert_eq!(fs::read(state_dir(&v).join("vectors.f32")).unwrap(), before);
    }

    #[test]
    fn save_and_load_round_trip() {
        let v = scratch("roundtrip");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);
        let ix = load_index(&state_dir(&v)).unwrap();
        assert_eq!(ix.chunks.len(), 1);
        assert_eq!(ix.vectors.len(), DIM);
        assert_eq!(ix.by_path.get(&a).map(Vec::len), Some(1));
    }

    #[test]
    fn a_truncated_vector_file_is_rejected_rather_than_trusted() {
        let v = scratch("mismatch");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);
        let f = state_dir(&v).join("vectors.f32");
        let mut bytes = fs::read(&f).unwrap();
        bytes.truncate(bytes.len() - 4);
        fs::write(&f, bytes).unwrap();
        assert!(
            load_index(&state_dir(&v)).is_none(),
            "positional coupling means a mismatch returns wrong answers, not errors"
        );
    }

    #[test]
    fn an_index_from_another_model_is_rejected() {
        let v = scratch("model");
        let a = note(&v, "alpha");
        seed(&v, &[a.as_str()]);
        let f = state_dir(&v).join("manifest.json");
        let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
        m["model"] = serde_json::Value::String("SomeOtherModel".into());
        fs::write(&f, serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(load_index(&state_dir(&v)).is_none());
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
        let before = fs::read(state_dir(&v).join("vectors.f32")).unwrap();
        let old_stamp = load_index(&state_dir(&v)).unwrap().stamps[&a];
        let abs = v.join(&a);

        // Move mtime without touching content.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let body = fs::read(&abs).unwrap();
        fs::write(&abs, &body).unwrap();

        cmd_index(&v, false, false, &LangConfig::default()).unwrap();

        let ix = load_index(&state_dir(&v)).unwrap();
        assert_eq!(
            fs::read(state_dir(&v).join("vectors.f32")).unwrap(),
            before,
            "identical content must not be re-embedded"
        );
        assert_ne!(ix.stamps[&a], old_stamp, "the new stamp must be recorded");
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

    #[test]
    fn tags_do_not_reach_the_embedded_text_or_the_heading() {
        let c = chunks_of("#+title: T\n#+filetags: :physics:\n* Section :work:\nbody\n");
        assert_eq!(c[0].heading, "T > Section");
        assert!(!c[0].text.contains("work"));
        assert_eq!(c[0].tags, vec!["physics", "work"]);
    }

    #[test]
    fn chunks_store_a_vault_relative_path() {
        let c = chunk_file(Path::new("/vault/sub/Note.org"), "sub/Note.org", "#+title: T\nbody\n", &LangConfig::default());
        assert_eq!(c[0].path, "sub/Note.org", "relative, so the vault can move");
    }

    // --------------------------------------------------------------- filters

    fn chunk_with(path: &str, tags: &[&str], todo: Option<&str>) -> Chunk {
        Chunk {
            path: path.into(),
            id: None,
            heading: "H".into(),
            line: 1,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            todo: todo.map(str::to_string),
            priority: None,
            lang: "en-US".into(),
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
            Chunk { path: a.clone(), id: None, heading: "alpha".into(), line: 1,
                    tags: vec!["physics".into()], todo: None, priority: None, lang: "en-US".into(),
                    text: "the quick brown fox".into() },
            Chunk { path: b.clone(), id: None, heading: "beta".into(), line: 1,
                    tags: vec!["german".into()], todo: None, priority: None, lang: "en-US".into(),
                    text: "der schnelle braune Fuchs".into() },
        ];
        let dir = state_dir(&v);
        fs::create_dir_all(&dir).unwrap();
        let an = lexical::Analyzer::from_chunks(&chunks, false);
        lexical::sync(&dir, &chunks, &[], &[], true, &an).unwrap();
        assert_eq!(lexical::doc_count(&dir, &an).unwrap(), 2);

        let hits = lexical::search(&dir, &chunks, &parse_query("brown"), 10, true, &an).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(chunks[hits[0].1].path, a, "resolves back to the right chunk");

        // A predicate must constrain the lexical side exactly as it does the
        // semantic one, or the two modes disagree about what was searched.
        let hits = lexical::search(&dir, &chunks, &parse_query("tag:german Fuchs"), 10, true, &an).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(chunks[hits[0].1].path, b);
        let hits = lexical::search(&dir, &chunks, &parse_query("tag:physics Fuchs"), 10, true, &an).unwrap();
        assert!(hits.is_empty(), "predicate excludes the only textual match");
    }

    #[test]
    fn lexical_sync_replaces_a_changed_note_and_drops_a_deleted_one() {
        let v = scratch("lexical-sync");
        let a = note(&v, "alpha");
        let b = note(&v, "beta");
        let mk = |p: &str, t: &str| Chunk {
            path: p.into(), id: None, heading: p.into(), line: 1,
            tags: vec![], todo: None, priority: None, lang: "en-US".into(), text: t.into(),
        };
        let dir = state_dir(&v);
        fs::create_dir_all(&dir).unwrap();
        let chunks = vec![mk(&a, "brown fox"), mk(&b, "brown bear")];
        let an = lexical::Analyzer::from_chunks(&chunks, false);
        lexical::sync(&dir, &chunks, &[], &[], true, &an).unwrap();
        assert_eq!(lexical::search(&dir, &chunks, &parse_query("brown"), 10, true, &an).unwrap().len(), 2);

        // beta changes, alpha is deleted.
        let chunks = vec![mk(&b, "crimson bear")];
        let an2 = lexical::Analyzer::from_chunks(&chunks, false);
        lexical::sync(&dir, &chunks, &[b.clone()], &[a.clone()], false, &an2).unwrap();
        assert!(lexical::search(&dir, &chunks, &parse_query("brown"), 10, true, &an2).unwrap().is_empty());
        assert_eq!(lexical::search(&dir, &chunks, &parse_query("crimson"), 10, true, &an2).unwrap().len(), 1);
        assert_eq!(lexical::doc_count(&dir, &an2).unwrap(), 1);
    }

    // ------------------------------------------------------------- languages

    #[test]
    fn ltex_magic_comment_is_read() {
        assert_eq!(ltex_language("# ltex: language=de-DE", "ltex").as_deref(), Some("de-DE"));
        assert_eq!(ltex_language("#ltex: language=fr", "ltex").as_deref(), Some("fr"));
        assert_eq!(
            ltex_language("# ltex: language=de-DE enabled=false", "ltex").as_deref(),
            Some("de-DE"),
            "other ltex settings on the line are ignored, not tripped over"
        );
        assert_eq!(ltex_language("# ltex: enabled=false", "ltex"), None);
        assert_eq!(ltex_language("# just a comment", "ltex"), None);
        assert_eq!(
            ltex_language("# spell: language=it", "spell").as_deref(),
            Some("it"),
            "the keyword is configurable"
        );
    }

    /// ltex applies a magic comment from its own line onward, so a note may
    /// switch part-way; chunks before it keep the default.
    #[test]
    fn language_applies_from_its_line_onward() {
        let c = chunks_of(
            "#+title: T\n* English part\nalpha\n\n# ltex: language=de-DE\n* Deutscher Teil\nbeta\n",
        );
        let by = |n: &str| c.iter().find(|x| x.text.trim() == n).unwrap();
        assert_eq!(by("alpha").lang, "en-US", "the configured default");
        assert_eq!(by("beta").lang, "de-DE");
    }

    #[test]
    fn the_default_language_is_configurable() {
        let cfg = LangConfig { default: "it-IT".into(), keyword: "ltex".into() };
        let c = chunk_file(Path::new("/v/N.org"), "N.org", "#+title: T\nciao\n", &cfg);
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
    fn lang_filters_the_candidate_set() {
        let mut de = chunk_with("a.org", &[], None);
        de.lang = "de-DE".into();
        let mut en = chunk_with("b.org", &[], None);
        en.lang = "en-US".into();
        let f = parse_query("lang:de Wörter");
        assert!(f.matches(&de));
        assert!(!f.matches(&en));
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
            path: p.into(), id: None, heading: p.into(), line: 1,
            tags: vec![], todo: None, priority: None,
            lang: lang.into(), text: t.into(),
        };
        let chunks = vec![
            mk(&en, "en-US", "the damped oscillations of a trapped atom"),
            mk(&de, "de-DE", "Die Wörter der Sprache sind lang"),
        ];
        let dir = state_dir(&v);
        fs::create_dir_all(&dir).unwrap();
        let an = lexical::Analyzer::from_chunks(&chunks, false);
        assert_eq!(an.langs, vec!["de", "en"], "derived from the corpus");
        lexical::sync(&dir, &chunks, &[], &[], true, &an).unwrap();

        let hits = |q: &str| {
            lexical::search(&dir, &chunks, &parse_query(q), 10, true, &an).unwrap().len()
        };
        assert_eq!(hits("oscillation"), 1, "English stemming: singular finds plural");
        assert_eq!(hits("Sprachen"), 1, "German stemming: plural finds singular");
        assert_eq!(hits("lang:de Sprachen"), 1);
        assert_eq!(hits("lang:de-DE Sprachen"), 1, "full code matches");
        assert_eq!(hits("lang:DE-de Sprachen"), 1, "and is case-insensitive");
        assert_eq!(hits("lang:en Sprachen"), 0, "the predicate must exclude it");
        assert_eq!(hits("lang:de oscillation"), 0);
    }
}
