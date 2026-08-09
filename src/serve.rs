//! A resident process speaking JSON-RPC 2.0 over stdio.
//!
//! The point is the model: embedding a query takes ~7 ms and scanning the
//! vectors ~1.4 ms, but *loading* the model costs 120 ms for BGE and 640 ms for
//! E5.  Paying that per keystroke is the difference between search-as-you-type
//! and a command you press RET on, so the process stays alive and keeps both the
//! model and the vault's vectors in memory.
//!
//! Framing is LSP's — `Content-Length: N\r\n\r\n<body>` — chosen because Emacs
//! ships `jsonrpc.el` (what Eglot runs on), so the editor side needs no protocol
//! code at all: request/response correlation, async notifications and
//! cancellation come for free over a plain `make-process` pipe.  No socket, no
//! port, no authentication, and the server's lifetime is the editor's.
//!
//! Requests are served one at a time.  At ~10 ms each that is not a queue worth
//! managing, and it keeps every borrow of the cached indexes trivially sound.

use crate::*;
use std::collections::HashMap;
use std::io::BufRead;

/// One vault's semantic index, with the model that reads it, kept loaded.
struct Semantic {
    model: TextEmbedding,
    which: &'static Model,
    chunks: Vec<Chunk>,
    vectors: Vec<f32>,
    /// Computed once when the index is loaded — a few milliseconds there rather
    /// than on every keystroke.
    baseline: Option<Baseline>,
}

#[derive(Default)]
struct Server {
    /// Keyed by vault and model, so several models can be served side by side
    /// exactly as they are stored.
    ///
    /// Only the semantic side is cached.  The lexical one was too, until it was
    /// measured: a warm lexical query is 2–5 ms *including* opening the index,
    /// because tantivy memory-maps its segments and the pages stay resident.
    /// Caching its analyzer saved a 30-byte file read and bought a stale-state
    /// bug to invalidate.
    semantic: HashMap<(PathBuf, &'static str), Semantic>,
}

impl Server {
    /// Load a vault's semantic index once, then keep it.
    fn semantic(&mut self, vault: &Path, want: Option<&'static Model>) -> Result<&mut Semantic> {
        let m = choose_index(vault, want)?;
        let key = (vault.to_path_buf(), m.name);
        if !self.semantic.contains_key(&key) {
            let dir = semantic_dir(vault, m);
            let chunks: Vec<Chunk> = serde_json::from_slice(&fs::read(dir.join("chunks.json"))?)?;
            let raw = fs::read(dir.join("vectors.f32"))?;
            if raw.len() != chunks.len() * m.dim * 4 {
                return Err(anyhow!(
                    "index is inconsistent: {} vectors for {} chunks",
                    raw.len() / (m.dim * 4),
                    chunks.len()
                ));
            }
            let vectors: Vec<f32> = raw
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            let model = model_with(m.which.clone(), None, false)?;
            let baseline = Baseline::of(&vectors, m.dim);
            self.semantic
                .insert(key.clone(), Semantic { model, which: m, chunks, vectors, baseline });
        }
        Ok(self.semantic.get_mut(&key).expect("just inserted"))
    }

    /// The analyzer the lexical index was built with, read from beside it.
    ///
    /// Read per query rather than cached: it costs a 30-byte file read, and
    /// reading it fresh means a rebuild under different languages or folding
    /// can never be answered with the previous one.
    fn analyzer(vault: &Path) -> Result<lexical::Analyzer> {
        let stored = lexical::stored_key(&state_dir(vault))
            .ok_or_else(|| anyhow!("no lexical index — run `index --lexical`"))?;
        lexical::Analyzer::from_key(&stored)
            .ok_or_else(|| anyhow!("unreadable lexical index — run `index --lexical`"))
    }

    /// `search` — both modalities, one shape.
    ///
    /// `mode` selects the ranking; everything else is identical, so an editor
    /// can offer them as one command with a toggle and never branch on the
    /// reply.
    fn search(&mut self, p: &serde_json::Value) -> Result<serde_json::Value> {
        let vault = PathBuf::from(
            p.get("vault").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing `vault`"))?,
        );
        let query = p.get("query").and_then(|v| v.as_str()).unwrap_or("");
        // `k` bounds the notes, `perFile` how much of the list any one of them
        // may take.  An editor showing a vault kept in a few large files raises
        // the second; the names match the CLI's `k` and `--per-file`.
        let lim = Limits {
            files: p.get("k").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_FILES as u64) as usize,
            per_file: p
                .get("perFile")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_PER_FILE as u64)
                .max(1) as usize,
        };
        let lexical_mode = p.get("mode").and_then(|v| v.as_str()) == Some("lexical");

        let f = parse_query(query);
        if f.text.trim().is_empty() && f.is_empty() {
            // An empty query is not an error while someone is still typing.
            return Ok(serde_json::json!({ "hits": [] }));
        }

        if lexical_mode {
            let conjunction = !p.get("any").and_then(|v| v.as_bool()).unwrap_or(false);
            let a = Self::analyzer(&vault)?;
            let pool = lim.files.saturating_mul(lim.per_file).saturating_mul(25).max(100);
            let hits = lexical::search(&state_dir(&vault), &f, pool, conjunction, &a)?;
            let hits: Vec<(f32, &Chunk)> = hits.iter().map(|(s, c)| (*s, c)).collect();
            // BM25 has no noise floor to standardise against.
            return Ok(hits_json(&vault, &hits, lim, None));
        }

        if !f.langs.is_empty() {
            return Err(anyhow!("lang: narrows the lexical index only"));
        }
        let want = match p.get("model").and_then(|v| v.as_str()) {
            Some(name) => Some(model_named(name)?),
            None => None,
        };
        let s = self.semantic(&vault, want)?;
        let dim = s.which.dim;
        let candidates: Vec<usize> =
            (0..s.chunks.len()).filter(|&i| f.matches(&s.chunks[i])).collect();
        if candidates.is_empty() || f.text.trim().is_empty() {
            return Ok(serde_json::json!({ "hits": [] }));
        }
        let mut q = s
            .model
            .embed(&[format!("{}{}", s.which.query, f.text)], None)
            .map_err(|e| anyhow!("embedding query: {e}"))?
            .remove(0);
        normalize(&mut q);
        let mut scored: Vec<(f32, usize)> = candidates
            .iter()
            .map(|&i| {
                let v = &s.vectors[i * dim..(i + 1) * dim];
                (v.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>(), i)
            })
            .collect();
        scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        let hits: Vec<(f32, &Chunk)> = scored.iter().map(|(sc, i)| (*sc, &s.chunks[*i])).collect();
        Ok(hits_json(&vault, &hits, lim, s.baseline))
    }

    /// `index` — rebuild either index, or both, without leaving the process.
    ///
    /// Spawning a CLI for this would pay the model load again, which is the one
    /// cost this whole design exists to avoid: the resident model is lent to the
    /// indexer, so re-embedding a note the editor just saved costs the embedding
    /// and nothing else.
    ///
    /// The human-readable report goes to a sink — the caller gets the numbers.
    fn index(&mut self, p: &serde_json::Value) -> Result<serde_json::Value> {
        let vault = PathBuf::from(
            p.get("vault").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing `vault`"))?,
        );
        let mode = p.get("mode").and_then(|v| v.as_str()).unwrap_or("semantic");
        let full = p.get("full").and_then(|v| v.as_bool()).unwrap_or(false);
        let rehash = p.get("rehash").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut out = io::sink();
        let mut done = serde_json::Map::new();
        // Emacs keeps its policy in whatever format it likes — a commented
        // `.eld`, say — and passes it here already parsed, so neither side
        // needs a reader for the other's syntax.
        let cfg = match p.get("config") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| anyhow!("config: {e}"))?,
            None => resolve_config(&vault, None)?,
        };
        let previous = Config::read(&config_path(&state_dir(&vault))).ok();

        if mode == "semantic" || mode == "both" {
            let want = match p.get("model").and_then(|v| v.as_str()) {
                Some(name) => model_named(name)?,
                // Not `choose_index`: the first index for a vault has to be
                // creatable, and nothing is built yet to choose among.
                None => built_models(&vault)
                    .first()
                    .copied()
                    .unwrap_or(model_named(DEFAULT_MODEL)?),
            };
            if !full {
                check_config(
                    stored_hash::<Manifest>(&semantic_dir(&vault, want).join("manifest.json"))
                        .map(|m| m.config),
                    &cfg,
                    previous.as_ref(),
                    Target::Semantic,
                )?;
            }
            let key = (vault.clone(), want.name);
            // Lend the resident model if this vault's index is already loaded.
            let report = match self.semantic.get_mut(&key) {
                Some(s) => {
                    cmd_index(&vault, full, rehash, want, &cfg, &mut out, Some(&mut s.model))?
                }
                None => cmd_index(&vault, full, rehash, want, &cfg, &mut out, None)?,
            };
            // The vectors on disk have moved, so what is held in memory is now
            // wrong — including the baseline, which is derived from them.
            self.refresh(&key)?;
            done.insert("semantic".into(), serde_json::to_value(report)?);
            done.insert("model".into(), want.name.into());
        }

        if mode == "lexical" || mode == "both" {
            // Languages and folding come from the policy, not from separate
            // parameters: they are part of what the index *is*, and a second
            // channel for them is a second thing that can disagree.
            let lang = LangConfig { languages: cfg.languages.clone() };
            prepare_lang(&lang)?;
            if !full {
                check_config(
                    stored_hash::<LexManifest>(&lex_manifest_path(&state_dir(&vault)))
                        .map(|m| m.config),
                    &cfg,
                    previous.as_ref(),
                    Target::Lexical,
                )?;
            }
            let report =
                cmd_index_lexical(&vault, full, rehash, &lang, cfg.fold_diacritics, &cfg, &mut out)?;
            done.insert("lexical".into(), serde_json::to_value(report)?);
        }

        if done.is_empty() {
            return Err(anyhow!("unknown mode `{mode}`; use semantic, lexical or both"));
        }
        fs::create_dir_all(state_dir(&vault))?;
        fs::write(config_path(&state_dir(&vault)), cfg.canonical())?;
        Ok(serde_json::Value::Object(done))
    }

    /// Re-read a vault's vectors and recompute its baseline, keeping the loaded
    /// model.  Dropping the entry instead would be simpler and would throw away
    /// the very thing worth keeping.
    fn refresh(&mut self, key: &(PathBuf, &'static str)) -> Result<()> {
        let Some(s) = self.semantic.get_mut(key) else { return Ok(()) };
        let dir = semantic_dir(&key.0, s.which);
        let chunks: Vec<Chunk> = serde_json::from_slice(&fs::read(dir.join("chunks.json"))?)?;
        let raw = fs::read(dir.join("vectors.f32"))?;
        let vectors: Vec<f32> = raw
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        s.baseline = Baseline::of(&vectors, s.which.dim);
        s.chunks = chunks;
        s.vectors = vectors;
        Ok(())
    }

    /// What a vault has, so an editor can offer the right commands and say why
    /// one is unavailable rather than failing when it is used.
    fn status(&mut self, p: &serde_json::Value) -> Result<serde_json::Value> {
        let vault = PathBuf::from(
            p.get("vault").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing `vault`"))?,
        );
        let models: Vec<serde_json::Value> = built_models(&vault)
            .iter()
            .map(|m| serde_json::json!({ "name": m.name, "dim": m.dim, "about": m.about }))
            .collect();
        let lexical = lexical::stored_key(&state_dir(&vault)).is_some();
        Ok(serde_json::json!({
            "vault": vault,
            "semantic": models,
            "lexical": lexical,
            "loaded": self.semantic.len(),
        }))
    }

    fn dispatch(&mut self, method: &str, params: &serde_json::Value) -> Result<serde_json::Value> {
        match method {
            "search" => self.search(params),
            "index" => self.index(params),
            "status" => self.status(params),
            // A resident process must be able to drop what it holds without
            // being restarted: an index rebuilt underneath it is otherwise
            // served stale until the editor exits.
            "reload" => {
                self.semantic.clear();
                Ok(serde_json::json!({ "ok": true }))
            }
            _ => Err(anyhow!("unknown method `{method}`")),
        }
    }
}

/// Read one LSP-framed message: headers, blank line, then exactly
/// `Content-Length` bytes.
fn read_message(r: &mut impl BufRead) -> Result<Option<String>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None); // the editor closed the pipe
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = strip_prefix_ci(line, "content-length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let len = len.ok_or_else(|| anyhow!("message without Content-Length"))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(String::from_utf8(buf)?))
}

fn write_message(v: &serde_json::Value) -> Result<()> {
    let body = serde_json::to_string(v)?;
    let mut out = io::stdout().lock();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    out.flush()?;
    Ok(())
}

pub fn serve() -> Result<()> {
    let mut server = Server::default();
    let stdin = io::stdin();
    let mut r = stdin.lock();

    while let Some(body) = read_message(&mut r)? {
        let msg: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                write_message(&serde_json::json!({
                    "jsonrpc": "2.0", "id": serde_json::Value::Null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                }))?;
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default();
        if method == "exit" || method == "shutdown" {
            if let Some(id) = id {
                write_message(&serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": null }))?;
            }
            return Ok(());
        }
        let params = msg.get("params").cloned().unwrap_or(serde_json::Value::Null);
        let result = server.dispatch(method, &params);
        // A notification (no id) expects no reply, not even for an error.
        let Some(id) = id else { continue };
        write_message(&match result {
            Ok(v) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            // Application errors go back as JSON-RPC errors rather than killing
            // the process: a mistyped vault must not end the session.
            Err(e) => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32000, "message": e.to_string() }
            }),
        })?;
    }
    Ok(())
}
