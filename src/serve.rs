//! A resident process speaking JSON-RPC 2.0 over stdio.
//!
//! The point is the model: embedding a query takes ~7 ms and scanning the
//! vectors ~1.4 ms, but *loading* the model costs 120 ms for BGE and 640 ms for
//! E5.  Paying that per keystroke is the difference between search-as-you-type
//! and a command you press RET on, so the process stays alive and keeps both the
//! model and the vault's vectors in memory.
//!
//! The transport is `lsp_server` — LSP's `Content-Length` framing, chosen
//! because Emacs ships `jsonrpc.el` (what Eglot runs on), so the editor side
//! needs no protocol code at all.  It was hand-rolled once, and the framing was
//! never the reason to stop: `Connection::sender` is a **cloneable, `Send`
//! channel**, which is what lets an indexing worker answer and report without
//! two threads racing for the descriptor.  Everything that used to guard that
//! race — `O_NONBLOCK`, a `PIPE_BUF` argument, a policy for dropping reports —
//! is gone, because the run no longer writes anything itself.
//!
//! **An `index` runs on a worker; everything else is answered on the loop.**
//! That is what makes search-during-reindex possible, and it is also what
//! retired the rest of the workarounds: cancellation is `$/cancelRequest`, which
//! now arrives while there is still something to stop *and* carries the id; and
//! the session has an `initialize` to answer `serverInfo.version` at, so there
//! is no `version` method.  Each of those had been written the other way for one
//! reason — the loop used to sit inside `index` and read nothing until it
//! answered.  When that went, so did the reasons.
//!
//! Searches stay on the loop, serial with each other.  Each is ~10 ms and they
//! would serialize on the one model regardless, so a second thread would buy
//! nothing.
//!
//! What a query *does* wait for during a rebuild is one embedding batch —
//! `BATCH` divided by chunks per second, which is a p90 of ~2 s where a warm
//! query is 9 ms.  It answers instead of blocking for the whole run; it is not
//! fast enough to type into.  Lexical search touches no model and is unaffected.

use crate::*;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response, ResponseError};
use std::collections::HashMap;
use std::sync::Arc;

/// How an editor says yes: it has no flags, so it calls `index` with `full`.
/// Phrased as something a client can put in front of a user verbatim.
const SERVE_REMEDY: &str = "reindex with `full` to rebuild under the new one";

/// One vault's index and the model that reads it, shared between the request
/// loop and an indexing run.
///
/// The two are separate fields because they have different lifetimes: the
/// **model** is the expensive thing and the reason this process is resident,
/// while the **index** is replaced whole by every run that writes one.
///
/// **Hold at most one of these locks at a time.**  Every path here obeys it — a
/// search clones the `Arc<Index>` out and drops the guard before scanning, and
/// takes the model only to embed its query.  Nothing ever holds one lock while
/// reaching for another, so no cycle exists and deadlock is ruled out by
/// construction rather than by inspection.  Keep it that way.
struct Semantic {
    which: &'static Model,
    /// The model, **shared with every other vault using the same one**.  Held for
    /// a single query, or a single batch of an index, and never for longer — see
    /// the batch loop in `cmd_index`.
    ///
    /// Shared because inference is stateless: nothing about embedding a query
    /// depends on which vault asked, so a second copy of the weights buys
    /// nothing. It used to be one per `(vault, model)` — measured at **+143 MB**
    /// for the second vault on one model, which was pure waste.
    model: Arc<Mutex<TextEmbedding>>,
    /// The version committed last.  Swapped whole; a reader clones the `Arc`.
    ///
    /// This is the part that really is per vault: the chunk table and the vectors
    /// every dot product runs against, ~10 MB on the reference vault.
    index: Mutex<Arc<Index>>,
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
    semantic: Mutex<HashMap<(PathBuf, &'static str), Arc<Semantic>>>,
    /// One loaded model per model *name*, shared by every vault using it.
    ///
    /// **`Weak`, deliberately.** Holding `Arc`s here would keep every model that
    /// has ever been used alive for the life of the process, and `close` could
    /// never release one. With `Weak`, the owners are the `Semantic` entries and a
    /// model is dropped exactly when the last vault using it goes — no reference
    /// counting of our own, and no eviction policy to get wrong.
    models: Mutex<HashMap<&'static str, std::sync::Weak<Mutex<TextEmbedding>>>>,
    /// The runs in flight, **one slot per vault**.
    ///
    /// It was a single global slot, which meant rebuilding one vault refused a
    /// one-note reindex of an unrelated one — a vault made to wait on work that
    /// had nothing to do with it. Per vault, that cannot happen.
    ///
    /// Still *refused* rather than queued or cancelled within a vault. Queueing
    /// would answer minutes later, past the client's timeout; cancelling would be
    /// worse, because a stopped run writes nothing at all (chunks and vectors are
    /// positionally paired, so there is no partial index to save), so a rebuild
    /// restarted by every save could make no progress at all.
    ///
    /// A thread that has finished is reaped here rather than announcing itself:
    /// `JoinHandle::is_finished` is the whole of the bookkeeping.
    run: Mutex<HashMap<PathBuf, Run>>,
}

/// One indexing run, and the two things needed to reach it from outside.
struct Run {
    /// What it will answer under, so `$/cancelRequest` can find it.
    id: RequestId,
    /// **Its own** stop flag. A global one would mean cancelling a run on one
    /// vault silently stopped every other vault's.
    stop: Arc<Cancel>,
    handle: std::thread::JoinHandle<()>,
}

/// What a run needs, resolved on the loop thread before anything is spawned.
///
/// Everything a caller can get wrong — a missing vault, an unknown mode or
/// model, a policy that will not parse or has drifted — is settled here, so the
/// answer comes back at once instead of in a reply minutes away.
struct Plan {
    vault: PathBuf,
    semantic: bool,
    lexical: bool,
    full: bool,
    rehash: bool,
    want: &'static Model,
    cfg: Config,
    /// Never hold a second set of weights, whatever it costs a search.
    ///
    /// A **parameter and not policy**: it changes nothing about what the index
    /// contains, so putting it in `Config` would hash it into the manifest and
    /// demand a reindex the first time someone toggled a memory setting.
    conserve: bool,
}

impl Server {
    /// The loaded model for M, shared with whoever else is using it.
    ///
    /// The load (0.12–0.64 s) happens with this map locked, which is the one slow
    /// thing under a lock here. Releasing it around the load would buy a race in
    /// which two copies load for one name — the very waste this exists to remove.
    fn model(&self, m: &'static Model) -> Result<Arc<Mutex<TextEmbedding>>> {
        let mut models = lock(&self.models);
        if let Some(loaded) = models.get(m.name).and_then(std::sync::Weak::upgrade) {
            return Ok(loaded);
        }
        // **A search may not fetch a model.** This is the only place `serve`
        // loads one — indexing takes `Lend::Own` and loads its own — so the rule
        // is enforced by the one function that could break it.
        //
        // Not a timeout problem, though that is how it shows up: a search is
        // answered *on the message loop*, and `model_with` would block it for
        // the length of the download — 487 MB for `e5-small`, over 2 GB for the
        // large multilingual ones. Nothing else would be read meanwhile: not a
        // lexical search, not `status`, not `$/cancelRequest`, not `shutdown`.
        // And a download has no unit boundaries to check a cancel flag between,
        // so the only way out would be killing the process. It is silent as
        // well, since `download` progress comes from the callers holding a
        // `Journal` and this path holds none.
        //
        // So it is refused in about a millisecond, labelled, and `remedy` sends
        // the client to `index` — which runs on a worker thread, reports the
        // download with its size, and has a budget measured in hours. Fetching
        // is a thing you ask for, not a thing a query does to you.
        //
        // **`weights_cached` and not `weight_bytes`, and the difference is a
        // download already in flight.** Summing the `.onnx*` bytes answers yes to
        // *any* of them, and `e5-large` is a 546 kB `model.onnx` beside a 2.2 GB
        // `model.onnx_data` fetched after it. For the minutes between the two, a
        // byte count calls that cached, this check passes, and the query walks
        // into `model_with` — where hf-hub finds the second file missing, cannot
        // take the flock the running index holds, retries five times a second
        // apart and fails (hf-hub sync.rs:86-100, 810-818). No second download,
        // because that lock is exactly what prevents one — but five seconds of
        // the loop, spent to arrive at an error with no `kind` on it.
        //
        // It still cannot be exact, and the residue is the tokenizer: `ModelInfo`
        // names the ONNX artifacts alone, while the four files fastembed also
        // fetches are hardcoded in its loader (common.rs:44-48) and appear in no
        // metadata. A cache holding the weights but not `tokenizer.json`
        // therefore passes and fetches ~17 MB on the loop. Seconds, not minutes,
        // and closing it would mean curating those four names — the one thing
        // this path refuses, since they are fastembed's to change.
        //
        // Whatever is done here, **keep the bias**: this must fail towards
        // "cached". The refusal is not self-correcting — `index` takes
        // `Lend::Own` and never populates `Server::models` — so a search that
        // says "missing" about a model that is present says it again after the
        // very run it asked for, and for ever.
        if !weights_cached(m) {
            return Err(fault(
                "model-missing",
                serde_json::json!({ "target": "semantic", "model": m.name,
                                    "remedy": "index" }),
                format!(
                    "the {} model is not downloaded yet — index this vault to fetch it",
                    m.name
                ),
            ));
        }
        let loaded = Arc::new(Mutex::new(model_with(m.which.clone(), None, false)?));
        // Overwrites a dead `Weak` if one is there, which is how they are reaped.
        models.insert(m.name, Arc::downgrade(&loaded));
        Ok(loaded)
    }

    /// Load a vault's semantic index once, then keep it.
    ///
    /// Two critical sections rather than one, and never nested: the model comes
    /// from `models`, then the index is read under `semantic`. The second lookup
    /// is not redundant — the lock is released in between, so another thread may
    /// have inserted this key meanwhile, and without the re-check both would
    /// install an entry and one would be orphaned along with any index a run
    /// later adopted into it.
    fn semantic(&self, vault: &Path, want: Option<&'static Model>) -> Result<Arc<Semantic>> {
        let m = choose_index(vault, want)?;
        let key = (vault.to_path_buf(), m.name);
        if let Some(s) = lock(&self.semantic).get(&key) {
            return Ok(Arc::clone(s));
        }
        let model = self.model(m)?;
        let mut cache = lock(&self.semantic);
        if let Some(s) = cache.get(&key) {
            return Ok(Arc::clone(s));
        }
        let index = Index::read(&semantic_dir(vault, m), m)?;
        let s = Arc::new(Semantic { which: m, model, index: Mutex::new(Arc::new(index)) });
        cache.insert(key, Arc::clone(&s));
        Ok(s)
    }

    /// `close` — forget a vault, so what it was holding can be reclaimed.
    ///
    /// The client's counterpart to sharing models: an editor that has closed the
    /// last buffer in a vault says so, and the chunk table and vectors go. The
    /// model goes with them **only if no other vault was using it**, which falls
    /// out of `Weak` rather than being decided here.
    ///
    /// Its own method rather than an optional `vault` on `reload`: that would make
    /// one method answer two questions, which is the mistake `status` already made
    /// once. `reload` means "I rebuilt something behind your back"; this means "I
    /// am done with this one".
    ///
    /// Expect only part of the memory back. Measured with `reload`: dropping two
    /// vaults and their models returned ~112 MB of ~390 MB, in two steps a few
    /// seconds apart, because freed pages go to the allocator and only some of
    /// them onward to the OS. A process exit is still the only thing that returns
    /// everything.
    fn close(&self, p: &serde_json::Value) -> Result<serde_json::Value> {
        let vault = PathBuf::from(
            p.get("vault").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing `vault`"))?,
        );
        let mut cache = lock(&self.semantic);
        let before = cache.len();
        cache.retain(|(v, _), _| v != &vault);
        Ok(serde_json::json!({ "dropped": before - cache.len() }))
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

    /// Whether **this vault** is being indexed, which is what refuses a second run
    /// on it and what tells a searcher its answer is a version behind.
    ///
    /// Per vault, so a search on one vault no longer reports itself stale because
    /// an unrelated vault is rebuilding — which the single global slot did.
    fn indexing(&self, vault: &Path) -> bool {
        lock(&self.run).get(vault).is_some_and(|r| !r.handle.is_finished())
    }

    /// `search` — both modalities, one shape.
    ///
    /// `mode` selects the ranking; everything else is identical, so an editor
    /// can offer them as one command with a toggle and never branch on the
    /// reply.
    fn search(&self, p: &serde_json::Value) -> Result<serde_json::Value> {
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

        // Said on every reply, because a hit list answered mid-rebuild is a
        // version behind and the client is the one that decides whether to say
        // so.  It costs a boolean; asking `status` per keystroke would not.
        let moving = self.indexing(&vault);
        let answer = |mut v: serde_json::Value| {
            v["indexing"] = moving.into();
            v
        };

        let f = parse_query(query);
        if f.text.trim().is_empty() && f.is_empty() {
            // An empty query is not an error while someone is still typing.
            return Ok(answer(serde_json::json!({ "hits": [] })));
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
            return Ok(answer(hits_json(&vault, &hits, lim, merge, None)));
        }

        if !f.langs.is_empty() {
            return Err(anyhow!("lang: narrows the lexical index only"));
        }
        let s = self.semantic(&vault, want)?;
        let dim = s.which.dim;
        // Cloned out from under the lock: an index committed while this query is
        // being answered replaces the `Arc` in the cache and leaves this one
        // alone, so a search reads one version throughout and never a seam.
        let ix = Arc::clone(&lock(&s.index));
        let candidates: Vec<usize> =
            (0..ix.chunks.len()).filter(|&i| f.matches(&ix.chunks[i])).collect();
        if candidates.is_empty() || f.text.trim().is_empty() {
            return Ok(answer(serde_json::json!({ "hits": [] })));
        }
        // The one place a query waits on an indexing run: the batch in flight,
        // which is `BATCH` divided by chunks per second — a p90 of ~2 s on a real
        // rebuild, against 9 ms warm.  Not a median: `scan` and `chunk` use no
        // model, so most queries during a run are answered at once and a median
        // measures the phase mix rather than the wait.  It answers rather than
        // blocking for the whole run, which is the trade, and that is not the
        // same thing as staying fast.
        let mut q = lock(&s.model)
            .embed(&[format!("{}{}", s.which.query, f.text)], None)
            .map_err(|e| anyhow!("embedding query: {e}"))?
            .remove(0);
        normalize(&mut q);
        let mut scored: Vec<(f32, usize)> = candidates
            .iter()
            .map(|&i| {
                let v = &ix.vectors[i * dim..(i + 1) * dim];
                (v.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>(), i)
            })
            .collect();
        scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        let hits: Vec<(f32, &Chunk)> = scored.iter().map(|(sc, i)| (*sc, &ix.chunks[*i])).collect();
        Ok(answer(hits_json(&vault, &hits, lim, merge, ix.baseline)))
    }

    /// Everything an `index` request can be refused for, settled before a thread
    /// is spawned.
    fn plan(&self, p: &serde_json::Value, j: &mut Journal) -> Result<Plan> {
        let vault = PathBuf::from(
            p.get("vault").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing `vault`"))?,
        );
        let mode = p.get("mode").and_then(|v| v.as_str()).unwrap_or("semantic");
        let (semantic, lexical) = match mode {
            "semantic" => (true, false),
            "lexical" => (false, true),
            "both" => (true, true),
            // Ahead of the work rather than after it: this used to be discovered
            // once both branches had declined to run, which on a worker would
            // mean answering a typo minutes later.
            _ => return Err(anyhow!("unknown mode `{mode}`; use semantic, lexical or both")),
        };
        let full = p.get("full").and_then(|v| v.as_bool()).unwrap_or(false);
        let rehash = p.get("rehash").and_then(|v| v.as_bool()).unwrap_or(false);
        // For a client whose user is short of memory rather than of patience.
        // Off by default: the second model is transient and a rebuild is rare,
        // so most people would rather have the responsive search.
        let conserve = p.get("conserveMemory").and_then(|v| v.as_bool()).unwrap_or(false);
        // Emacs keeps its policy in whatever format it likes — a commented
        // `.eld`, say — and passes it here already parsed, so neither side
        // needs a reader for the other's syntax.
        let cfg: Config = match p.get("config") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| anyhow!("config: {e}"))?,
            None => resolve_config(&vault, None, j)?,
        };
        // Deserializing does not validate; `Config::read` would have, so a
        // policy arriving over the wire must be held to the same bar.
        cfg.check()?;
        let previous = Config::read(&config_path(&state_dir(&vault))).ok();

        let want = match p.get("model").and_then(|v| v.as_str()) {
            Some(name) => model_named(name)?,
            // Not `choose_index`: the first index for a vault has to be
            // creatable, and nothing is built yet to choose among.
            None => built_models(&vault).first().copied().unwrap_or(model_named(DEFAULT_MODEL)?),
        };
        // Both indexes are checked before either is written, so a changed policy
        // can never leave lexical describing one corpus and semantic another.
        if !full {
            if semantic {
                check_config(
                    stored_hash::<Manifest>(&semantic_dir(&vault, want).join("manifest.json"))
                        .map(|m| m.config),
                    &cfg,
                    previous.as_ref(),
                    Target::Semantic,
                    SERVE_REMEDY,
                )?;
            }
            if lexical {
                check_config(
                    stored_hash::<LexManifest>(&lex_manifest_path(&state_dir(&vault)))
                        .map(|m| m.config),
                    &cfg,
                    previous.as_ref(),
                    Target::Lexical,
                    SERVE_REMEDY,
                )?;
            }
        }
        Ok(Plan { vault, semantic, lexical, full, rehash, want, cfg, conserve })
    }

    /// Start an `index` and return to the loop.
    ///
    /// Spawning a CLI for this would pay the model load again, which is the one
    /// cost this whole design exists to avoid: the resident model is lent to the
    /// indexer, so re-embedding a note the editor just saved costs the embedding
    /// and nothing else.
    ///
    /// The worker answers for itself, through a clone of the sender.  Nothing on
    /// this thread waits for it, which is why there is no second event source to
    /// select over.
    fn start(
        self: &Arc<Self>,
        req: &Request,
        sender: &crossbeam_channel::Sender<Message>,
    ) -> Result<()> {
        let mut j = Journal::quiet();
        let plan = self.plan(&req.params, &mut j)?;

        let mut runs = lock(&self.run);
        // Reap whatever has finished, on any vault.  Dropping a handle whose
        // thread has already ended detaches nothing that is still running.
        runs.retain(|_, r| !r.handle.is_finished());
        if runs.contains_key(&plan.vault) {
            return Err(fault(
                "indexing",
                serde_json::json!({ "remedy": "wait" }),
                "an index of this vault is already running; wait for it to finish".into(),
            ));
        }
        // And the same claim across processes, since the in-process slot above
        // says nothing about a CLI run writing this vault.  Taken here rather than
        // on the worker so the refusal is synchronous, like every other reason a
        // request is turned down.  Moved into the thread, and released when it ends.
        let claim = Claim::on(&plan.vault)?;
        // Fresh, so there is no stale cancellation to clear — which is what the
        // old global flag needed a `rearm` step for.
        let stop = Arc::new(Cancel::default());

        let reports = watch(&mut j, sender, &req.id);
        let me = Arc::clone(self);
        let sender = sender.clone();
        let id = req.id.clone();
        let vault = plan.vault.clone();
        let mine = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let done = me.run(plan, &mut j, &mine);
            // Explicit, so that moving this line is a visible decision: the vault
            // stays claimed until the run is finished with it.
            drop(claim);
            // The queue is closed and drained *before* the reply, in that order.
            // Reports go out on their own thread, so a reply sent straight to the
            // transport could otherwise overtake ones still queued — and "the
            // response ends the last run of reports" is the contract a client
            // renders against.  Waiting here costs nothing that matters: the work
            // is done, and a reply has always been delivered blocking.
            j.watch = None;
            let _ = reports.join();
            let _ = sender.send(replied(id, done, &mut j).into());
        });
        runs.insert(vault, Run { id: req.id.clone(), stop, handle });
        Ok(())
    }

    /// The run itself, on the worker.
    fn run(&self, plan: Plan, j: &mut Journal, stop: &Cancel) -> Result<serde_json::Value> {
        let Plan { vault, semantic, lexical, full, rehash, want, cfg, conserve } = plan;
        let mut done = serde_json::Map::new();

        if semantic {
            let key = (vault.clone(), want.name);
            // Stamped at the boundary rather than carried as state on the
            // journal: a flag is something you can forget to clear, a mark is
            // not.
            let mark = j.remarks.len();
            // Offer the resident model if this vault's index is already loaded.
            // A vault nobody has searched has no entry and no model to offer, so
            // the run loads one of its own and drops it; the first search after
            // it reads the committed index from disk, as it always did.
            //
            // Whether the offer is *taken* for a long run is `cmd_index`'s call,
            // since it is the one that knows how many chunks there are — unless
            // the client has asked us never to hold two sets of weights, which is
            // an answer no chunk count overrides.
            let loaded = lock(&self.semantic).get(&key).cloned();
            let lend = match (loaded.as_ref(), conserve) {
                (Some(s), true) => Lend::Always(&s.model),
                (Some(s), false) => Lend::IfShort(&s.model),
                (None, _) => Lend::Own,
            };
            let out = cmd_index(&vault, full, rehash, want, &cfg, j, lend, stop)?;
            for r in &mut j.remarks[mark..] {
                r.target = Some("semantic");
            }
            // What is held in memory is now a version behind, so the new one
            // takes its place — in one assignment, from what the run built,
            // rather than by reading back the file it just wrote.  Nothing is
            // part-updated at any point, and the model is untouched.
            //
            // `built` is `None` when the run wrote nothing, and the version in
            // memory is then still the committed one.
            //
            // `reload` may have emptied the cache while this ran, in which case
            // this installs into an entry nothing can reach and it is dropped
            // with the `Arc`.  Harmless: the next search reads from disk.
            if let (Some(b), Some(s)) = (out.built, loaded) {
                *lock(&s.index) = Arc::new(Index::of(b, want.dim));
            }
            done.insert("semantic".into(), serde_json::to_value(out.report)?);
            done.insert("model".into(), want.name.into());
        }

        if lexical {
            // Languages and folding come from the policy, not from separate
            // parameters: they are part of what the index *is*, and a second
            // channel for them is a second thing that can disagree.
            let lang = LangConfig { languages: cfg.languages.clone() };
            let mark = j.remarks.len();
            prepare_lang(&lang, j)?;
            let report =
                cmd_index_lexical(&vault, full, rehash, &lang, cfg.fold_diacritics, &cfg, j, stop)?;
            for r in &mut j.remarks[mark..] {
                r.target = Some("lexical");
            }
            done.insert("lexical".into(), serde_json::to_value(report)?);
        }

        fs::create_dir_all(state_dir(&vault))?;
        fs::write(config_path(&state_dir(&vault)), cfg.canonical())?;
        Ok(serde_json::Value::Object(done))
    }

    /// `$/cancelRequest` — stop the run answering under this id.
    ///
    /// Matched against the id rather than taken as "whatever is running": the
    /// protocol carries one, and honouring it is what makes a late cancellation
    /// — arriving after its run already answered — do nothing instead of
    /// stopping the next one.
    /// Found by id across every vault, and it stops **that** run: each carries its
    /// own flag, so a cancellation on one vault leaves the others running.
    fn cancel(&self, params: &serde_json::Value) {
        let Some(asked) = params.get("id") else { return };
        let Ok(asked) = serde_json::from_value::<RequestId>(asked.clone()) else { return };
        if let Some(r) = lock(&self.run).values().find(|r| r.id == asked && !r.handle.is_finished())
        {
            r.stop.request();
        }
    }

    /// Wait for every run in flight, so their replies go out before we do.
    ///
    /// Drained before joining rather than joined under the lock: a worker takes
    /// the `semantic` lock and this thread would be holding `run`, which is the
    /// one pairing the "at most one lock at a time" rule exists to prevent.
    fn join(&self) {
        let runs: Vec<Run> = lock(&self.run).drain().map(|(_, r)| r).collect();
        for r in runs {
            let _ = r.handle.join();
        }
    }

    /// `memory` — what this process is holding, in raw bytes.
    ///
    /// About the **process**, so its own method: `status` answers about a vault,
    /// and mixing the two subjects is the mistake recorded twice in this file.
    ///
    /// **Honest rather than complete.** `rss` is the total. `vaults` and `models`
    /// are what can be counted exactly — vectors are `len * 4`, chunk tables are
    /// summed, and the loaded models are named. What is *not* here is a figure for
    /// ONNX itself: `ort` exposes no usage reporting, so the runtime's arenas and
    /// the resident weights cannot be asked about from inside. Reporting
    /// `rss` minus the rest under a name like "onnx" would be inventing a
    /// measurement — the remainder also holds the allocator's retained pages and
    /// this process's own working set.
    ///
    /// `weightFile` is the model file's size on disk, and says so in its name: it
    /// is what the weights *are*, not what they occupy once ORT has loaded them.
    ///
    /// Raw counts only, and nothing derived. A client that wants "unaccounted for"
    /// subtracts; a client that wants "MB" formats. Both are display decisions,
    /// and `Progress` already follows that rule — two renderers of one fact drift.
    fn memory(&self) -> Result<serde_json::Value> {
        // One lock at a time, as everywhere here: the map is cloned out and
        // released before any `Index` is looked at.
        let loaded: Vec<((PathBuf, &'static str), Arc<Semantic>)> =
            lock(&self.semantic).iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect();

        let mut vaults = Vec::new();
        for ((vault, model), s) in &loaded {
            let ix = Arc::clone(&lock(&s.index));
            vaults.push(serde_json::json!({
                "vault": vault,
                "model": model,
                "chunks": ix.chunks.len(),
                "vectors": (ix.vectors.len() * std::mem::size_of::<f32>()) as u64,
                "table": table_bytes(&ix.chunks),
            }));
        }

        // By model rather than by vault, matching how they are actually held: one
        // model serves every vault using it.
        let mut names: Vec<&'static str> = loaded.iter().map(|((_, m), _)| *m).collect();
        names.sort_unstable();
        names.dedup();
        let models: Vec<serde_json::Value> = names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "weightFile": model_named(name).ok().and_then(weight_bytes),
                })
            })
            .collect();

        Ok(serde_json::json!({ "rss": rss(), "vaults": vaults, "models": models }))
    }

    /// What a vault has, so an editor can offer the right commands and say why
    /// one is unavailable rather than failing when it is used.
    fn status(&self, p: &serde_json::Value) -> Result<serde_json::Value> {
        let vault = PathBuf::from(
            p.get("vault").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing `vault`"))?,
        );
        let models: Vec<serde_json::Value> = built_models(&vault)
            .iter()
            .map(|m| {
                serde_json::json!({ "name": m.name, "dim": m.dim, "about": m.about,
                                    // Whether a search of this index can be
                                    // answered at all: an index can outlive the
                                    // weights that built it — a vault copied to
                                    // another machine, a cleared cache — and a
                                    // search will then refuse rather than fetch.
                                    // Said here so a client can offer to fetch
                                    // before someone asks and is turned down.
                                    "cached": weights_cached(m) })
            })
            .collect();
        let lexical = lexical::stored_key(&state_dir(&vault)).is_some();
        Ok(serde_json::json!({
            "vault": vault,
            "semantic": models,
            "lexical": lexical,
            // About *this* vault, like every other field here: whether its index
            // is resident, so a client knows the next search is warm (~10 ms)
            // rather than a model load (~150–300 ms).  It was the size of the
            // whole cache — a process-wide number in a reply about one vault,
            // which is the third time that particular mistake has been made here.
            "loaded": lock(&self.semantic).keys().any(|(v, _)| v == &vault),
            "indexing": self.indexing(&vault),
        }))
    }
}

/// Send at most one report per this long, for the phases whose unit is a file.
///
/// Not a display decision — that is the client's. This is about not flooding the
/// client's event loop: a scan and a chunking pass over a thousand notes are
/// ~2,000 reports in a few seconds, and an editor that redraws on each is an
/// editor doing nothing else. At this rate the stream is a report every tenth of
/// a second, which is faster than anyone can read and slow enough to render.
const FLOOR: std::time::Duration = std::time::Duration::from_millis(100);

/// Whether a report is worth sending at all.
///
/// About repetition and nothing else. A phase's **opening** is news — it is what
/// the protocol says ends the previous run of reports — and a phase's **last**
/// is what lets a client stop rendering 6,400 of 6,522. Neither can be rebuilt
/// from its neighbours, so neither is ever held back. Everything between them is
/// superseded a tenth of a second later.
fn deliver(opening: bool, last: bool, since: std::time::Duration) -> bool {
    opening || last || since >= FLOOR
}

/// Report the work as it happens, under the id the caller is already waiting on.
///
/// The token is that id rather than one negotiated through
/// `window/workDoneProgress/create`: the client holds it already, so there is
/// nothing to negotiate. (That request is *possible* now the loop is free to
/// receive its response — it is simply not needed.)
///
/// There is no `begin` or `end`. An `index` that fails answers with an error and
/// would skip its `end`, leaving a client holding a token for ever. The contract
/// has no such hole: one report per completed unit, a change of `target` or
/// `phase` ends the previous run of them, and the response — result or error —
/// ends the last.
/// **A report is worth less than the work delivering it would hold up**, so
/// reports go through a buffer of this depth and are dropped when it is full.
/// That is the promise; the depth only decides when it starts costing anything.
///
/// Every channel `lsp-server` hands out is `bounded(0)` — a rendezvous — and its
/// writer thread writes each frame before taking the next, so sending straight to
/// it blocks as soon as the client stops reading and stdout fills. Which means
/// the run stops until the editor breathes. This is what stands between those two
/// facts.
///
/// 64 because `FLOOR` caps the stream at ten reports a second, so this is ~6 s of
/// slack on top of stdout's own 16–64 kB, and ~13 kB if it ever does fill.
const BACKLOG: usize = 64;

/// Report the work as it happens, under the id the caller is already waiting on.
///
/// The token is that id rather than one negotiated through
/// `window/workDoneProgress/create`: the client holds it already, so there is
/// nothing to negotiate. (That request is *possible* now the loop is free to
/// receive its response — it is simply not needed.)
///
/// There is no `begin` or `end`. An `index` that fails answers with an error and
/// would skip its `end`, leaving a client holding a token for ever. The contract
/// has no such hole: one report per completed unit, a change of `target` or
/// `phase` ends the previous run of them, and the response — result or error —
/// ends the last.
///
/// Returns the forwarding thread. Drop `Journal::watch` to close the queue, then
/// join it: that drains what is left *before* the reply goes out, which is what
/// keeps the response last. See the caller.
fn watch(
    j: &mut Journal,
    out: &crossbeam_channel::Sender<Message>,
    token: &RequestId,
) -> std::thread::JoinHandle<()> {
    let (tx, rx) = crossbeam_channel::bounded::<Message>(BACKLOG);
    let out = out.clone();
    // The only thread here allowed to wait on the client.  It has nothing else to
    // do, which is the entire point.
    let forwarder = std::thread::spawn(move || {
        for msg in rx {
            // Failing means the transport is gone and the session with it.
            // Stopping now also latches that: `try_send` below then fails on a
            // disconnected queue rather than collecting errors nobody reads.
            if out.send(msg).is_err() {
                break;
            }
        }
    });

    let token = serde_json::to_value(token).unwrap_or(serde_json::Value::Null);
    let mut sent = Instant::now() - FLOOR;
    let mut running: Option<(&'static str, &'static str)> = None;
    j.watch = Some(Box::new(move |p: &Progress| {
        let opening = running != Some((p.target, p.phase));
        running = Some((p.target, p.phase));
        if !deliver(opening, p.last, sent.elapsed()) {
            return;
        }
        sent = Instant::now();
        // `try_send`, never `send`: this runs on the thread doing the work.  A
        // full queue means the client is not reading, and the next report
        // supersedes this one a tenth of a second later anyway.  So **any**
        // report can be lost, an opening and a `last` included — a client learns
        // where the run got to from the next phase, or from the reply.
        let _ = tx.try_send(
            Notification::new(
                "$/progress".into(),
                serde_json::json!({ "token": token, "value": p }),
            )
            .into(),
        );
    }));
    forwarder
}

/// A run's answer, with whatever it found worth saying attached.
///
/// Warnings ride the reply because stderr does not reach the client: a bare
/// `eprintln!` in the indexer goes to a pipe nobody has correlated with a
/// request. One list for the whole run, not a field on each report — two of the
/// kinds belong to neither index, and an unreadable note under `mode: "both"` is
/// one problem seen twice.
fn replied(id: RequestId, done: Result<serde_json::Value>, j: &mut Journal) -> Response {
    match done {
        Ok(mut v) => {
            let remarks = j.drain();
            if !remarks.is_empty() {
                if let Ok(rs) = serde_json::to_value(remarks) {
                    v["remarks"] = rs;
                }
            }
            Response::new_ok(id, v)
        }
        Err(e) => failed(id, &e),
    }
}

/// An application error as a JSON-RPC one, never a process exit: a mistyped
/// vault must not end the session.
///
/// A `Fault` adds LSP's third member, `data`, carrying the label and whatever
/// that label promises. Its absence is meaningful: an error with no `data` is
/// one to show, not one to act on.
fn failed(id: RequestId, e: &anyhow::Error) -> Response {
    let labelled = e.downcast_ref::<Fault>();
    Response {
        id,
        response_result: Err(ResponseError {
            code: labelled.map_or(-32000, Fault::code),
            message: e.to_string(),
            data: labelled.and_then(|f| serde_json::to_value(f).ok()),
        }),
    }
}

pub fn serve() -> Result<()> {
    let (conn, io) = Connection::stdio();
    // The handshake is where a client learns which binary it actually reached —
    // a different answer from `--version` the moment a new one has been
    // installed under a server that is still running.
    let (id, _params) = conn.initialize_start()?;
    conn.initialize_finish(
        id,
        serde_json::json!({
            "capabilities": {},
            "serverInfo": { "name": "org-semantic", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;

    let server = Arc::new(Server::default());
    // LSP's two steps, and they are two for a reason: `shutdown` says stop
    // accepting work, `exit` says end the process.
    let mut closing = false;
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if req.method == "shutdown" {
                    // Waited on, so a run in flight still answers under its own
                    // id before we say we are done.  `exit` is the one that does
                    // not wait.
                    server.join();
                    closing = true;
                    let _ = conn.sender.send(Response::new_ok(req.id, ()).into());
                    // Deliberately not a `break`.  Ending the loop here means
                    // `io.join()` below, which waits on a reader thread still
                    // blocked on a stdin the client has not closed — so a clean
                    // `shutdown` hung the process until the pipe went. Found by
                    // driving it by hand; the tests close stdin and never saw it.
                    continue;
                }
                if closing {
                    // The spec's answer for anything asked after `shutdown`.
                    let _ = conn.sender.send(
                        Response::new_err(req.id, -32600, "the server is shutting down".into())
                            .into(),
                    );
                    continue;
                }
                // `index` answers from its own thread; everything else here.
                let answer = match req.method.as_str() {
                    "search" => server.search(&req.params).map(Some),
                    "status" => server.status(&req.params).map(Some),
                    "memory" => server.memory().map(Some),
                    // A resident process must be able to drop what it holds
                    // without being restarted: an index rebuilt underneath it is
                    // otherwise served stale until the editor exits.
                    "reload" => {
                        lock(&server.semantic).clear();
                        Ok(Some(serde_json::json!({ "ok": true })))
                    }
                    "close" => server.close(&req.params).map(Some),
                    "index" => server.start(&req, &conn.sender).map(|()| None),
                    _ => Err(anyhow!("unknown method `{}`", req.method)),
                };
                match answer {
                    Ok(Some(v)) => {
                        let _ = conn.sender.send(Response::new_ok(req.id, v).into());
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = conn.sender.send(failed(req.id, &e).into());
                    }
                }
            }
            Message::Notification(n) => match n.method.as_str() {
                // Abandons the run rather than waiting for it.  Safe: the index
                // is committed by a single rename, so an abandoned run leaves
                // the previous one exactly as it was.
                "exit" => break,
                "$/cancelRequest" => server.cancel(&n.params),
                // A notification is owed nothing at all, including on failure —
                // and an `index` sent as one could report nothing and be
                // cancelled by nothing, so it is not run either.
                _ => {}
            },
            Message::Response(_) => {}
        }
    }
    drop(conn);
    io.join()?;
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

    /// The shape the whole design rests on: the loop hands `Server` to a worker
    /// and both touch it. If this stops holding, it stops compiling here rather
    /// than somewhere less obvious.
    #[test]
    fn the_server_can_be_shared_with_the_thread_that_indexes() {
        fn shareable<T: Send + Sync>() {}
        shareable::<Server>();
        shareable::<Semantic>();
    }

    /// A client that has stopped reading costs reports, never the run.
    ///
    /// The transport is `bounded(0)`, so a sender nobody receives from *is* a
    /// wedged editor: sending straight to it blocks for ever. Reporting must come
    /// straight back regardless, which is what `BACKLOG` plus `try_send` buys.
    ///
    /// Every report here opens a phase, so none of them is thinned by `FLOOR` —
    /// otherwise a fast loop would send one report and prove nothing.
    ///
    /// On a thread with a deadline, because the failure mode is a hang: a guard
    /// that wedges the suite reports nothing and costs whatever patience is
    /// watching it.
    #[test]
    fn a_client_that_stops_reading_costs_reports_and_not_the_run() {
        let (out, wedged) = crossbeam_channel::bounded::<Message>(0);
        let (tx, done) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut j = Journal::quiet();
            let forwarder = watch(&mut j, &out, &RequestId::from(7));
            for i in 0..BACKLOG * 20 {
                let phase = if i % 2 == 0 { "chunk" } else { "embed" };
                j.progress(&Progress::new("semantic", phase, "files", i, 0.0).of(BACKLOG * 20));
            }
            let _ = tx.send(());
            // Closing the queue lets the forwarder go even though nobody ever
            // read: `rx` disconnects, and its blocked `out.send` is the last thing
            // it will ever try.
            j.watch = None;
            drop(wedged);
            let _ = forwarder.join();
        });
        assert_eq!(
            done.recv_timeout(Duration::from_secs(5)),
            Ok(()),
            "the run waited on a client instead of dropping a report"
        );
        worker.join().unwrap();
    }
}
