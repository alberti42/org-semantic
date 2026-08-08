//! org-semantic — semantic search over a tree of org notes.
//!
//! Prototype.  Commands:
//!
//!   org-semantic index  <vault> [--full|--rehash] refresh (incremental by default)
//!   org-semantic search <vault> <query> [k]       query it, grouped by note
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

#[derive(Serialize, Deserialize, Clone)]
struct Chunk {
    path: String,
    /// The `:ID:` of the nearest enclosing node, when it has one — this is what
    /// lets Emacs jump through `org-id` rather than by file position.
    id: Option<String>,
    /// Heading path, e.g. "Note title > Section > Subsection".
    heading: String,
    /// 1-based, for the case where there is no `:ID:` to jump by.
    line: usize,
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

// ----------------------------------------------------------------- chunking

/// Split TEXT into chunks, one per heading, further split when a section runs
/// past `MAX_CHARS`.
///
/// Deliberately not a full org parser: for deciding where one passage ends and
/// the next begins, headings and property drawers are the whole of what
/// matters, and the available Rust org parsers are alpha-stage.
fn chunk_file(path: &Path, text: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut title = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut file_id: Option<String> = None;
    let mut cur_id: Option<String> = None;
    let mut cur_line = 1usize;
    let mut buf = String::new();
    let mut in_drawer = false;
    let mut seen_heading = false;

    let flush = |chunks: &mut Vec<Chunk>,
                 buf: &str,
                 stack: &[String],
                 title: &str,
                 id: &Option<String>,
                 line: usize| {
        let body = buf.trim();
        if body.is_empty() {
            return;
        }
        let heading = if stack.is_empty() {
            title.to_string()
        } else {
            format!("{} > {}", title, stack.join(" > "))
        };
        for piece in split_long(body) {
            chunks.push(Chunk {
                path: path.to_string_lossy().into_owned(),
                id: id.clone(),
                heading: heading.clone(),
                line,
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

        // Heading: one or more stars followed by a space.
        if let Some(level) = heading_level(line) {
            flush(&mut chunks, &buf, &stack, &title, &cur_id, cur_line);
            buf.clear();
            let text = line[level..].trim();
            // Drop a trailing tag block, ":tag1:tag2:", which is markup rather
            // than prose and would otherwise skew the embedding.
            let text = text.rsplit_once(char::is_whitespace).map_or(text, |(l, r)| {
                if r.starts_with(':') && r.ends_with(':') && r.len() > 2 {
                    l.trim_end()
                } else {
                    text
                }
            });
            stack.truncate(level.saturating_sub(1));
            while stack.len() < level - 1 {
                stack.push(String::new());
            }
            stack.push(text.to_string());
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
        // Other keywords, drawer ends and comments are markup, not prose.
        if trimmed.starts_with("#+") || trimmed.starts_with("# ") || trimmed == ":END:" {
            continue;
        }

        buf.push_str(line);
        buf.push('\n');
    }
    flush(&mut chunks, &buf, &stack, &title, &cur_id, cur_line);
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

// ------------------------------------------------------------ index on disk

/// Bumped when the on-disk layout changes, so a stale index is rebuilt rather
/// than misread.
const INDEX_VERSION: u32 = 1;

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

fn cmd_index(vault: &Path, full: bool, rehash: bool) -> Result<()> {
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
        let path = f.to_string_lossy().into_owned();
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
        let path = f.to_string_lossy().into_owned();
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
            enforce_token_limit(chunk_file(f, text), &measure, TOKEN_LIMIT);
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

    let written = save_index(&dir, &chunks, &vectors, hashes, stamps)?;
    println!(
        "wrote {} ({:.1} MB of vectors) in {:.2}s total",
        dir.display(),
        written as f64 / 1e6,
        t0.elapsed().as_secs_f64()
    );
    Ok(())
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

    let t0 = Instant::now();
    let mut model = model()?;
    let load = t0.elapsed();

    let t1 = Instant::now();
    let mut q = model
        .embed(&[format!("{QUERY_PREFIX}{query}")], None)
        .map_err(|e| anyhow!("embedding query: {e}"))?
        .remove(0);
    normalize(&mut q);
    let embed = t1.elapsed();

    let t2 = Instant::now();
    let mut scored: Vec<(f32, usize)> = (0..n)
        .map(|i| {
            let s = &vectors[i * DIM..(i + 1) * DIM];
            (s.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>(), i)
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
    let search = t2.elapsed();

    // Grouped by note.  A note that matches a query tends to match it in several
    // places, and a flat top-k then spends every slot on one document — which is
    // what happened on the phase-estimation query.  Each note appears once, at
    // the rank of its best chunk, with its other matching sections listed under
    // it.
    const SECTIONS_PER_NOTE: usize = 3;
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
    eprintln!(
        "\n[model load {:.0}ms · query embed {:.0}ms · search over {n} vectors {:.2}ms]",
        load.as_secs_f64() * 1000.0,
        embed.as_secs_f64() * 1000.0,
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
            chunks.extend(chunk_file(f, &text));
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
            chunks.extend(chunk_file(f, &text));
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
        let (chunks, _, _) = enforce_token_limit(chunk_file(f, &text), &measure, TOKEN_LIMIT);
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("index") => {
            let vault = args.get(2).ok_or_else(|| anyhow!("usage: index <vault>"))?;
            // `--incremental` is the default; accepted so a script can say so.
            let full = args.iter().any(|a| a == "--full");
            // `--rehash` reads and hashes every note, ignoring stamps: the
            // backstop for a change that left mtime untouched.
            let rehash = args.iter().any(|a| a == "--rehash");
            cmd_index(Path::new(vault), full, rehash)
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
            "usage:\n  org-semantic index  <vault>\n  org-semantic search <vault> <query> [k]\n  org-semantic bench  <vault> [n]"
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
        chunk_file(Path::new("/vault/Note.org"), text)
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
            path: "/v/n.org".into(),
            id: Some("id".into()),
            heading: para("h", 10),
            line: 1,
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
            path: "/v/n.org".into(),
            id: None,
            heading: "H".into(),
            line: 3,
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

    fn note(dir: &Path, name: &str) -> String {
        let body = format!(
            ":PROPERTIES:\n:ID: id-{name}\n:END:\n#+title: {name}\n\n* S\nText about {name}.\n"
        );
        let p = dir.join(format!("{name}.org"));
        fs::write(&p, &body).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// Seed an index without embedding anything: vectors are zeros, which is
    /// all the reuse and prune paths care about.
    fn seed(dir: &Path, paths: &[&str]) {
        let mut chunks = Vec::new();
        let mut files = std::collections::BTreeMap::new();
        for p in paths {
            chunks.push(Chunk {
                path: (*p).into(),
                id: None,
                heading: "H".into(),
                line: 1,
                text: "body".into(),
            });
            files.insert((*p).to_string(), content_hash(&fs::read(p).unwrap()));
        }
        let stamps = paths
            .iter()
            .map(|p| ((*p).to_string(), stamp_of(Path::new(p)).unwrap()))
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

        fs::remove_file(&b).unwrap();
        cmd_index(&v, false, false).unwrap();

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
        cmd_index(&v, false, false).unwrap();
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

        // Move mtime without touching content.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let body = fs::read(&a).unwrap();
        fs::write(&a, &body).unwrap();

        cmd_index(&v, false, false).unwrap();

        let ix = load_index(&state_dir(&v)).unwrap();
        assert_eq!(
            fs::read(state_dir(&v).join("vectors.f32")).unwrap(),
            before,
            "identical content must not be re-embedded"
        );
        assert_ne!(ix.stamps[&a], old_stamp, "the new stamp must be recorded");
    }
}
