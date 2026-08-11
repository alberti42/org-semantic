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
//! code at all: request/response correlation and async notifications come for
//! free over a plain `make-process` pipe.  No socket, no port, no
//! authentication, and the server's lifetime is the editor's.  Cancellation is
//! the one thing that does not ride the pipe — see `mod interrupt`.
//!
//! Requests are served one at a time.  At ~10 ms each that is not a queue worth
//! managing, and it keeps every borrow of the cached indexes trivially sound.

use crate::*;
use std::collections::HashMap;

/// How an editor says yes: it has no flags, so it calls `index` with `full`.
/// Phrased as something a client can put in front of a user verbatim.
const SERVE_REMEDY: &str = "reindex with `full` to rebuild under the new one";
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
            let chunks: Vec<Chunk> = read_chunks(&dir, m)?;
            let raw = fs::read(dir.join("vectors.f32"))?;
            if raw.len() != chunks.len() * m.dim * 4 {
                return Err(corrupt_index(chunks.len(), raw.len() / (m.dim * 4)));
            }
            let vectors: Vec<f32> =
                raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
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
        // Spelled for the caller that is actually here: an editor driving this
        // has an `index` method with a `mode`, not a `--lexical` flag.
        let missing = |what: &str| {
            fault(
                "no-index",
                serde_json::json!({ "target": "lexical", "remedy": "index" }),
                format!("{what} lexical index — build one with `index` in lexical mode"),
            )
        };
        let stored = lexical::stored_key(&state_dir(vault)).ok_or_else(|| missing("no"))?;
        lexical::Analyzer::from_key(&stored).ok_or_else(|| missing("unreadable"))
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
        // A section divided by the budget answers as several passages, each with
        // its own span.  `mergeBySection` folds them back into one result — off
        // by default, since the spans make the pieces individually reachable.
        let merge = p.get("mergeBySection").and_then(|v| v.as_bool()).unwrap_or(false);
        let want = match p.get("model").and_then(|v| v.as_str()) {
            Some(name) => Some(model_named(name)?),
            None => None,
        };

        let f = parse_query(query);
        if f.text.trim().is_empty() && f.is_empty() {
            // An empty query is not an error while someone is still typing.
            return Ok(serde_json::json!({ "hits": [] }));
        }

        // An editor that derives its policy from its own settings — Emacs
        // reading `org-todo-keywords` — can send it with every query, and learn
        // here that those settings have drifted from what the index was built
        // under.  Answering anyway would answer from chunks split by rules the
        // caller no longer holds.
        //
        // Checked, never applied: `search` writes nothing, so the remedy is a
        // reindex the user has to agree to.  A client sending this per keystroke
        // must latch the resulting error into one prompt rather than one per
        // keypress — the condition holds until they act on it.
        if let Some(v) = p.get("config") {
            let cfg: Config =
                serde_json::from_value(v.clone()).map_err(|e| anyhow!("config: {e}"))?;
            // Deserializing does not validate: without this a client can send a
            // chunk budget larger than the model reads and have it accepted.
            cfg.check()?;
            let stored = Config::read(&config_path(&state_dir(&vault))).ok();
            let (previous, target) = if lexical_mode {
                let h = stored_hash::<LexManifest>(&lex_manifest_path(&state_dir(&vault)))
                    .map(|m| m.config);
                (h, Target::Lexical)
            } else {
                let dir = semantic_dir(&vault, choose_index(&vault, want)?);
                let h = stored_hash::<Manifest>(&dir.join("manifest.json")).map(|m| m.config);
                (h, Target::Semantic)
            };
            check_config(previous, &cfg, stored.as_ref(), target, SERVE_REMEDY)?;
        }

        if lexical_mode {
            let conjunction = !p.get("any").and_then(|v| v.as_bool()).unwrap_or(false);
            let a = Self::analyzer(&vault)?;
            let pool = lim.files.saturating_mul(lim.per_file).saturating_mul(25).max(100);
            let hits = lexical::search(&state_dir(&vault), &f, pool, conjunction, &a)?;
            let hits: Vec<(f32, &Chunk)> = hits.iter().map(|(s, c)| (*s, c)).collect();
            // BM25 has no noise floor to standardise against.
            return Ok(hits_json(&vault, &hits, lim, merge, None));
        }

        if !f.langs.is_empty() {
            return Err(anyhow!("lang: narrows the lexical index only"));
        }
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
        Ok(hits_json(&vault, &hits, lim, merge, s.baseline))
    }

    /// `index` — rebuild either index, or both, without leaving the process.
    ///
    /// Spawning a CLI for this would pay the model load again, which is the one
    /// cost this whole design exists to avoid: the resident model is lent to the
    /// indexer, so re-embedding a note the editor just saved costs the embedding
    /// and nothing else.
    ///
    /// The human-readable report goes to a sink — the caller gets the numbers,
    /// and, while the work runs, `$/progress` notifications under TOKEN.
    fn index(
        &mut self,
        p: &serde_json::Value,
        token: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let vault = PathBuf::from(
            p.get("vault").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing `vault`"))?,
        );
        let mode = p.get("mode").and_then(|v| v.as_str()).unwrap_or("semantic");
        let full = p.get("full").and_then(|v| v.as_bool()).unwrap_or(false);
        let rehash = p.get("rehash").and_then(|v| v.as_bool()).unwrap_or(false);
        // Both streams sunk: stdout here *is* the JSON-RPC transport, and
        // stderr is a pipe nobody has correlated with this request.  What the
        // CLI would print is carried back as `remarks` instead.
        let mut j = Journal::quiet();
        watch(&mut j, token);
        let mut done = serde_json::Map::new();
        // Emacs keeps its policy in whatever format it likes — a commented
        // `.eld`, say — and passes it here already parsed, so neither side
        // needs a reader for the other's syntax.
        let cfg: Config = match p.get("config") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| anyhow!("config: {e}"))?,
            None => resolve_config(&vault, None, &mut j)?,
        };
        // Deserializing does not validate; `Config::read` would have, so a
        // policy arriving over the wire must be held to the same bar.
        cfg.check()?;
        let previous = Config::read(&config_path(&state_dir(&vault))).ok();

        if mode == "semantic" || mode == "both" {
            let want = match p.get("model").and_then(|v| v.as_str()) {
                Some(name) => model_named(name)?,
                // Not `choose_index`: the first index for a vault has to be
                // creatable, and nothing is built yet to choose among.
                None => {
                    built_models(&vault).first().copied().unwrap_or(model_named(DEFAULT_MODEL)?)
                }
            };
            if !full {
                check_config(
                    stored_hash::<Manifest>(&semantic_dir(&vault, want).join("manifest.json"))
                        .map(|m| m.config),
                    &cfg,
                    previous.as_ref(),
                    Target::Semantic,
                    SERVE_REMEDY,
                )?;
            }
            let key = (vault.clone(), want.name);
            // Stamped at the boundary rather than carried as state on the
            // journal: a flag is something you can forget to clear, a mark is
            // not.
            let mark = j.remarks.len();
            // Lend the resident model if this vault's index is already loaded.
            let report = match self.semantic.get_mut(&key) {
                Some(s) => cmd_index(&vault, full, rehash, want, &cfg, &mut j, Some(&mut s.model))?,
                None => cmd_index(&vault, full, rehash, want, &cfg, &mut j, None)?,
            };
            for r in &mut j.remarks[mark..] {
                r.target = Some("semantic");
            }
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
            let mark = j.remarks.len();
            prepare_lang(&lang, &mut j)?;
            if !full {
                check_config(
                    stored_hash::<LexManifest>(&lex_manifest_path(&state_dir(&vault)))
                        .map(|m| m.config),
                    &cfg,
                    previous.as_ref(),
                    Target::Lexical,
                    SERVE_REMEDY,
                )?;
            }
            let report =
                cmd_index_lexical(&vault, full, rehash, &lang, cfg.fold_diacritics, &cfg, &mut j)?;
            for r in &mut j.remarks[mark..] {
                r.target = Some("lexical");
            }
            done.insert("lexical".into(), serde_json::to_value(report)?);
        }

        if done.is_empty() {
            return Err(anyhow!("unknown mode `{mode}`; use semantic, lexical or both"));
        }
        fs::create_dir_all(state_dir(&vault))?;
        fs::write(config_path(&state_dir(&vault)), cfg.canonical())?;
        // One list for the whole run, not a field on each report: two of the
        // kinds belong to neither index, and an unreadable note under
        // `mode: "both"` is one problem seen twice.
        let remarks = j.drain();
        if !remarks.is_empty() {
            done.insert("remarks".into(), serde_json::to_value(remarks)?);
        }
        Ok(serde_json::Value::Object(done))
    }

    /// Re-read a vault's vectors and recompute its baseline, keeping the loaded
    /// model.  Dropping the entry instead would be simpler and would throw away
    /// the very thing worth keeping.
    fn refresh(&mut self, key: &(PathBuf, &'static str)) -> Result<()> {
        let Some(s) = self.semantic.get_mut(key) else { return Ok(()) };
        let dir = semantic_dir(&key.0, s.which);
        let chunks: Vec<Chunk> = read_chunks(&dir, s.which)?;
        let raw = fs::read(dir.join("vectors.f32"))?;
        let vectors: Vec<f32> =
            raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
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

    /// TOKEN is the request's id, which doubles as the progress token: a
    /// notification has none, and nothing to correlate a report with.
    fn dispatch(
        &mut self,
        method: &str,
        params: &serde_json::Value,
        token: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        match method {
            "search" => self.search(params),
            "index" => self.index(params, token),
            "status" => self.status(params),
            // Its own method rather than a field on `status`, which answers
            // about a *vault*.  This answers about the process, and the two
            // have neither the same subject nor the same lifetime.
            //
            // Worth asking even though `--version` exists: a client that has
            // just installed a new binary finds the old one still serving, and
            // the file on disk no longer says what this process is.
            "version" => Ok(serde_json::json!({ "version": env!("CARGO_PKG_VERSION") })),
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

/// Send at most one report per this long, for the phases whose unit is a file.
///
/// Not a display decision — that is the client's. This is flow control: a scan
/// and a chunking pass over a thousand notes are ~2,000 reports of ~200 bytes in
/// a few seconds, against a pipe of 16 kB that only the editor can drain. At
/// this rate the stream is under 2 kB/s and the buffer takes most of a minute to
/// fill rather than half a second.
const FLOOR: std::time::Duration = std::time::Duration::from_millis(100);

/// What became of a message that the sender was not willing to wait on.
#[derive(PartialEq, Debug)]
enum Sent {
    Yes,
    /// The client has stopped reading and the buffer is full.  The report is
    /// discarded: it is worth less than the work delivering it would hold up,
    /// and the next one supersedes it a tenth of a second later.
    NoRoom,
    /// The far end is closed.  Nothing more will be read, ever.
    Gone,
}

/// Write a framed message, or give up rather than wait for the client.
///
/// `O_NONBLOCK` is set for the duration of this write only.  Every other write
/// on the descriptor — the reply above all — keeps the blocking behaviour it
/// needs, and no caller can be surprised by an `EAGAIN` it did not ask for.
///
/// The flag is what makes "did it fit?" a question the write itself answers.  A
/// pipe write of at most `PIPE_BUF` bytes is all-or-nothing under it: ours are
/// ~202 bytes against a floor of 512.  A stdout that is *not* a pipe — a socket
/// — can still write partially, and half a frame would desynchronise the client
/// permanently, so a short write is finished blocking rather than abandoned.
/// The choice is whether to begin a write; once begun, it is always finished.
#[cfg(unix)]
fn write_without_waiting(fd: std::os::fd::RawFd, v: &serde_json::Value) -> Sent {
    let Ok(body) = serde_json::to_vec(v) else { return Sent::Gone };
    let mut buf = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    buf.extend_from_slice(&body);

    // Rust's stdout is line-buffered and every other writer here flushes, but
    // raw bytes must not overtake anything still held back.
    let _ = io::stdout().flush();

    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Sent::Gone;
    }
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    let err = std::io::Error::last_os_error();
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };

    if n < 0 {
        return match err.raw_os_error() {
            // EWOULDBLOCK is the same number here; naming both is a pattern
            // the compiler rejects as unreachable.
            Some(libc::EAGAIN) => Sent::NoRoom,
            _ => Sent::Gone,
        };
    }
    let n = n as usize;
    if n < buf.len() {
        let mut out = io::stdout().lock();
        if out.write_all(&buf[n..]).is_err() || out.flush().is_err() {
            return Sent::Gone;
        }
    }
    Sent::Yes
}

#[cfg(not(unix))]
fn write_without_waiting(_fd: i32, v: &serde_json::Value) -> Sent {
    if write_message(v).is_ok() {
        Sent::Yes
    } else {
        Sent::Gone
    }
}

/// Whether a report is worth sending at all.
///
/// About repetition and nothing else. A phase's **opening** is news — it is what
/// the protocol says ends the previous run of reports — and a phase's **last**
/// is what lets a client stop rendering 6,400 of 6,522. Neither can be rebuilt
/// from its neighbours, so neither is ever held back. Everything between them is
/// superseded a tenth of a second later.
///
/// Whether it then *fits* is a separate question, and one the write answers.
fn deliver(opening: bool, last: bool, since: std::time::Duration) -> bool {
    opening || last || since >= FLOOR
}

/// Report the work as it happens, under the id the caller is already waiting on.
///
/// The token is that id rather than one negotiated first: LSP's
/// `window/workDoneProgress/create` is a server→**client** request, and
/// answering it would mean re-entering `read_message` from inside `index`, which
/// this one-request-at-a-time loop forbids. There is nothing to negotiate — the
/// client holds the id already.
///
/// There is no `begin` or `end` either. An `index` that fails answers with an
/// error and would skip its `end`, leaving a client holding a token for ever.
/// The contract has no such hole: one report per completed unit, a change of
/// `target` or `phase` ends the previous run of them, and the response — result
/// or error — ends the last.
fn watch(j: &mut Journal, token: Option<&serde_json::Value>) {
    // A notification has no id, and an explicit `"id": null` is not one either.
    // Whether *that* deserves a reply is a separate question this must not
    // silently answer, which is why the test is here and not in the loop.
    let Some(tok) = token.filter(|t| !t.is_null()).cloned() else { return };
    let mut gone = false;
    let mut sent = Instant::now() - FLOOR;
    let mut running: Option<(&'static str, &'static str)> = None;
    let fd = {
        use std::os::fd::AsRawFd;
        io::stdout().as_raw_fd()
    };
    j.watch = Some(Box::new(move |p: &Progress| {
        let opening = running != Some((p.target, p.phase));
        running = Some((p.target, p.phase));
        if gone || !deliver(opening, p.last, sent.elapsed()) {
            return;
        }
        sent = Instant::now();
        let msg = serde_json::json!({
            "jsonrpc": "2.0", "method": "$/progress",
            "params": { "token": tok, "value": p },
        });
        // Nothing here is worth waiting on.  A client that has stopped reading
        // loses reports; it does not stop the work, and it learns where the run
        // got to from the reply, which *is* waited on.
        match write_without_waiting(fd, &msg) {
            Sent::Yes | Sent::NoRoom => {}
            // Rust leaves SIGPIPE ignored, so a client that has gone away would
            // otherwise mean minutes of write errors nobody will ever read.
            Sent::Gone => gone = true,
        }
    }));
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
    // Only here.  On the CLI, Ctrl-C must keep ending the program rather than
    // politely finishing the phase it is in.
    interrupt::listen();
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
        // A signal that landed while nothing was running belongs to nothing, and
        // must not cancel whatever is asked for next.
        interrupt::rearm();
        // Borrowed, not cloned: the borrow ends when `dispatch` returns, so
        // `id` is still movable into the reply below.  An `index` sent as a
        // notification therefore reports nothing, with no branch to write —
        // correct, since there is no token to report under.
        let result = server.dispatch(method, &params, id.as_ref());
        // A notification (no id) expects no reply, not even for an error.
        let Some(id) = id else { continue };
        write_message(&match result {
            Ok(v) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            // Application errors go back as JSON-RPC errors rather than killing
            // the process: a mistyped vault must not end the session.
            //
            // A `Fault` adds LSP's third member, `data`, carrying the label and
            // whatever that label promises.  Its absence is meaningful: an error
            // with no `data` is one to show, not one to act on.
            Err(e) => {
                let labelled = e.downcast_ref::<Fault>();
                let code = labelled.map_or(-32000, Fault::code);
                let mut err = serde_json::json!({ "code": code, "message": e.to_string() });
                if let Some(f) = labelled {
                    err["data"] = serde_json::to_value(f).unwrap_or(serde_json::Value::Null);
                }
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": err })
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The rate rule, which is about repetition and nothing else.
    #[test]
    fn a_phase_is_always_seen_to_begin_and_to_end() {
        assert!(deliver(true, false, Duration::ZERO), "a phase must be seen to begin");
        assert!(deliver(false, true, Duration::ZERO), "and to end");
        assert!(deliver(false, false, FLOOR), "otherwise, once the floor has passed");
        assert!(!deliver(false, false, Duration::ZERO), "and not before");
    }

    /// The size the all-or-nothing guarantee depends on.
    ///
    /// A pipe write of at most `PIPE_BUF` either goes entirely or not at all, so
    /// a report is delivered or dropped and never half-sent. Above that limit a
    /// write can go short, and half a frame desynchronises the client for good —
    /// the tail is finished blocking for exactly that reason, but on a pipe that
    /// branch should be unreachable.
    ///
    /// Nothing in `Progress` is user text, so the message is small and of fixed
    /// shape. This is here so that stays true: add a path or a message to it and
    /// this fails rather than the guarantee quietly evaporating.
    #[test]
    fn a_report_fits_in_one_atomic_write() {
        // Every optional field present, every number at its widest.
        let p = Progress::new("semantic", "download", "chunks", usize::MAX, 99999.999999)
            .of(usize::MAX)
            .tokens(usize::MAX, usize::MAX)
            .maybe_sized(Some(u64::MAX))
            .last();
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "method": "$/progress",
            "params": { "token": u64::MAX, "value": p },
        }))
        .unwrap();
        let framed = format!("Content-Length: {}\r\n\r\n", body.len()).len() + body.len();
        // 512 is the POSIX floor for `PIPE_BUF`; macOS is exactly that, Linux
        // more. Hold to the floor so this is not a claim about one platform.
        assert!(framed < 512, "a report of {framed} bytes can be written in halves");
    }

    /// A pipe nobody is reading, which is what a wedged editor looks like from
    /// this side. The write must come back rather than wait, and must say which
    /// of the two things happened — no room, or nobody there.
    #[test]
    #[cfg(unix)]
    fn a_full_pipe_costs_a_report_and_not_the_run() {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read, write) = (fds[0], fds[1]);
        let msg = serde_json::json!({ "jsonrpc": "2.0", "method": "$/progress" });

        // Room to begin with.
        assert_eq!(write_without_waiting(write, &msg), Sent::Yes);

        // Fill it. The descriptor is left blocking between calls, so this also
        // shows the flag is not leaking out of `write_without_waiting`.
        let flags = unsafe { libc::fcntl(write, libc::F_GETFL) };
        unsafe { libc::fcntl(write, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        let junk = [b'x'; 4096];
        while unsafe { libc::write(write, junk.as_ptr() as *const libc::c_void, junk.len()) } > 0 {}
        unsafe { libc::fcntl(write, libc::F_SETFL, flags) };

        // The call that would otherwise wait for the editor to breathe. On
        // another thread, so a write that *does* block fails this test in half a
        // second instead of hanging it — a guard that wedges the run reports
        // nothing and costs whatever patience is watching.
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = msg.clone();
        std::thread::spawn(move || tx.send(write_without_waiting(write, &sent)));
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(s) => assert_eq!(s, Sent::NoRoom, "a full pipe costs the report"),
            Err(_) => panic!("the write blocked on a full pipe instead of giving up"),
        }

        // Drained, and it goes again — the report was dropped, not the channel.
        let mut sink = [0u8; 65536];
        unsafe { libc::read(read, sink.as_mut_ptr() as *mut libc::c_void, sink.len()) };
        assert_eq!(write_without_waiting(write, &msg), Sent::Yes);

        // A reader that has gone is not the same as one that is busy.
        unsafe { libc::close(read) };
        assert_eq!(write_without_waiting(write, &msg), Sent::Gone);
        unsafe { libc::close(write) };
    }
}
