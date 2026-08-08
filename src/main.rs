//! org-semantic — semantic search over a tree of org notes.
//!
//! Prototype.  Commands:
//!
//!   org-semantic index  <vault>                   build the index
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

fn state_dir(vault: &Path) -> PathBuf {
    vault.join(STATE_DIR)
}

fn cmd_index(vault: &Path) -> Result<()> {
    let t0 = Instant::now();
    let mut files = Vec::new();
    org_files(vault, &mut files)?;
    files.sort();
    println!("{} org files", files.len());

    let mut chunks = Vec::new();
    for f in &files {
        match fs::read_to_string(f) {
            Ok(text) => chunks.extend(chunk_file(f, &text)),
            Err(e) => eprintln!("skipping {}: {e}", f.display()),
        }
    }
    let raw = chunks.len();
    eprint!("  tokenizing {raw} chunks against the {TOKEN_LIMIT}-token limit... ");
    io::stderr().flush().ok();
    let t_tok = Instant::now();
    let tok = tokenizers::Tokenizer::from_file(find_tokenizer()?)
        .map_err(|e| anyhow!("loading tokenizer: {e}"))?;
    let measure = |s: &str| n_tokens(&tok, s);
    let (chunks, resplit, tok_len) = enforce_token_limit(chunks, &measure, TOKEN_LIMIT);
    eprintln!("{:.1}s", t_tok.elapsed().as_secs_f64());
    if resplit > 0 {
        eprintln!(
            "  {resplit} of {raw} sections ran past {TOKEN_LIMIT} tokens and were divided \
             ({} chunks total)",
            chunks.len()
        );
    }
    println!("{} chunks in {:.1}s", chunks.len(), t0.elapsed().as_secs_f64());

    let t1 = Instant::now();
    let mut model = model()?;
    println!("model loaded in {:.2}s", t1.elapsed().as_secs_f64());

    let t2 = Instant::now();
    // Heading path prepended so a passage carries the context it sits under.
    let texts: Vec<String> = chunks
        .iter()
        .map(|c| format!("{}\n{}", c.heading, c.text))
        .collect();

    // Length-sorted batching.  fastembed pads each batch to its longest member
    // (`PaddingStrategy::BatchLongest`), so one 400-token chunk in a batch of 64
    // makes every short chunk beside it cost 400 tokens too.  In file order
    // almost every batch catches a long one; sorted, each batch is nearly
    // uniform and the padding it pays for is only its own spread.  Nothing is
    // truncated and no text is altered — only the order in which it is fed.
    // Sorted and estimated by tokens, not characters.  Chars-per-token runs
    // from about 2.0 in the LaTeX-heavy notes to 4.0 in prose, so a character
    // sort leaves batches uneven in the dimension that actually costs, and a
    // chunk-count ETA is wrong by the same factor.  One tokenizer pass over the
    // corpus takes a couple of seconds and pays for both.
    let total_tokens: usize = tok_len.iter().sum();
    debug_assert_eq!(tok_len.len(), texts.len());
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_unstable_by_key(|&i| tok_len[i]);

    const BATCH: usize = 64;
    let mut slots: Vec<Option<Vec<f32>>> = (0..texts.len()).map(|_| None).collect();
    let mut done = 0usize;
    let mut tokens_done = 0usize;
    for group in order.chunks(BATCH) {
        let batch: Vec<&str> = group.iter().map(|&i| texts[i].as_str()).collect();
        let vs = model
            .embed(&batch, Some(BATCH))
            .map_err(|e| anyhow!("embedding: {e}"))?;
        for (&i, v) in group.iter().zip(vs) {
            slots[i] = Some(v);
        }
        done += group.len();
        tokens_done += group.iter().map(|&i| tok_len[i]).sum::<usize>();
        let el = t2.elapsed().as_secs_f64();
        // Tokens per second is near flat once padding is gone, so remaining
        // work divided by it is an estimate rather than an extrapolation.
        let tps = (tokens_done as f64 / el).max(1.0);
        eprint!(
            "\r  embedding {done}/{} · {:.0} chunk/s · {:.0}k tok/s · eta {:.0}s   ",
            texts.len(),
            done as f64 / el,
            tps / 1000.0,
            (total_tokens - tokens_done) as f64 / tps
        );
        io::stderr().flush().ok();
    }
    eprintln!();
    let mut vectors: Vec<Vec<f32>> = slots.into_iter().map(Option::unwrap).collect();
    println!(
        "embedded {} chunks in {:.1}s ({:.0}/s)",
        vectors.len(),
        t2.elapsed().as_secs_f64(),
        vectors.len() as f64 / t2.elapsed().as_secs_f64()
    );

    let dir = state_dir(vault);
    fs::create_dir_all(&dir)?;
    let mut bytes = Vec::with_capacity(vectors.len() * DIM * 4);
    for v in vectors.iter_mut() {
        normalize(v);
        for x in v.iter() {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
    }
    fs::write(dir.join("vectors.f32"), &bytes)?;
    fs::write(dir.join("chunks.json"), serde_json::to_vec(&chunks)?)?;
    println!(
        "wrote {} ({:.1} MB of vectors) in {:.1}s total",
        dir.display(),
        bytes.len() as f64 / 1e6,
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
            cmd_index(Path::new(vault))
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
}
