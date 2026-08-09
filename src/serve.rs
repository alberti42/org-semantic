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
}

/// One vault's lexical index: only the analyzer needs caching, since tantivy
/// memory-maps its own segments and the documents carry their own chunks.
struct Lexical {
    analyzer: lexical::Analyzer,
}

#[derive(Default)]
struct Server {
    /// Keyed by vault and model, so several models can be served side by side
    /// exactly as they are stored.
    semantic: HashMap<(PathBuf, &'static str), Semantic>,
    lexical: HashMap<PathBuf, Lexical>,
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
            self.semantic.insert(key.clone(), Semantic { model, which: m, chunks, vectors });
        }
        Ok(self.semantic.get_mut(&key).expect("just inserted"))
    }

    fn lexical(&mut self, vault: &Path) -> Result<&Lexical> {
        let key = vault.to_path_buf();
        if !self.lexical.contains_key(&key) {
            let dir = state_dir(vault);
            let stored = lexical::stored_key(&dir)
                .ok_or_else(|| anyhow!("no lexical index — run `index --lexical`"))?;
            let analyzer = lexical::Analyzer::from_key(&stored)
                .ok_or_else(|| anyhow!("unreadable lexical index — run `index --lexical`"))?;
            self.lexical.insert(key.clone(), Lexical { analyzer });
        }
        Ok(self.lexical.get(&key).expect("just inserted"))
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
        let k = p.get("k").and_then(|v| v.as_u64()).unwrap_or(8) as usize;
        let lexical_mode = p.get("mode").and_then(|v| v.as_str()) == Some("lexical");

        let f = parse_query(query);
        if f.text.trim().is_empty() && f.is_empty() {
            // An empty query is not an error while someone is still typing.
            return Ok(serde_json::json!({ "hits": [] }));
        }

        if lexical_mode {
            let conjunction = !p.get("any").and_then(|v| v.as_bool()).unwrap_or(false);
            let a = &self.lexical(&vault)?.analyzer;
            let hits = lexical::search(&state_dir(&vault), &f, (k * 25).max(100), conjunction, a)?;
            let hits: Vec<(f32, &Chunk)> = hits.iter().map(|(s, c)| (*s, c)).collect();
            return Ok(hits_json(&vault, &hits, k));
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
        Ok(hits_json(&vault, &hits, k))
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
            "status" => self.status(params),
            // A resident process must be able to drop what it holds without
            // being restarted: an index rebuilt underneath it is otherwise
            // served stale until the editor exits.
            "reload" => {
                self.semantic.clear();
                self.lexical.clear();
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
