//! The server, driven the way an editor drives it.
//!
//! Everything here spawns the real binary and speaks JSON-RPC 2.0 over its
//! stdio with LSP framing, because that is the half of `serve.rs` no unit test
//! reaches: the handshake, framing, the request id doubling as a progress token,
//! the send-rate floor, and what a notification with no id is owed. Those were
//! checked by hand from a scratch script until this file existed, which is how
//! the floor came to be dropping the one notification that matters.
//!
//! **Two harnesses, and the difference matters.** `talk` writes every request at
//! once and reads after the process exits — deterministic, and blind to anything
//! the server does *while* it is busy. `Session` keeps the pipe open and reads
//! one reply at a time, which is the only way to see a search answered during a
//! reindex. Reach for `talk` unless the test is about concurrency.
//!
//! The concurrency tests use `mode: "lexical"` on both sides so they need no
//! embedding model and run offline; what they prove is the *loop*. The model
//! lock needs both sides semantic and so is `#[ignore]`d.
//!
//! An **integration** test rather than a module in `main.rs` for one practical
//! reason: Cargo builds the binary for these and hands over its path in
//! `CARGO_BIN_EXE_org-semantic`. A unit test would have to guess at
//! `target/<profile>/` and would fail confusingly on a tree that had only ever
//! been `cargo test`ed. The cost is that nothing internal is reachable — this
//! sees the tool exactly as a client does, which for these tests is the point.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn frame(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Split a stream strictly by its framing.
///
/// Deliberately strict: a stray byte — a progress bar written to a stderr the
/// client merged into stdout, say — desynchronises this and fails the test,
/// which is the behaviour being guarded.
fn messages(raw: &[u8]) -> Vec<Value> {
    let (mut out, mut off) = (Vec::new(), 0usize);
    while off < raw.len() {
        let hdr =
            off + raw[off..].windows(4).position(|w| w == b"\r\n\r\n").expect("a framed message");
        let head = std::str::from_utf8(&raw[off..hdr]).expect("ASCII headers");
        let n: usize = head.rsplit(':').next().unwrap().trim().parse().expect("a length");
        out.push(serde_json::from_slice(&raw[hdr + 4..hdr + 4 + n]).expect("a JSON body"));
        off = hdr + 4 + n;
    }
    out
}

/// One framed message, read as it arrives, or `None` at end of stream.
///
/// A byte at a time, which is wasteful and exactly right here: it must not read
/// past the message it returns, because the caller acts on that message while
/// the server is still working.
fn read_one(r: &mut impl std::io::Read) -> Option<Value> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        if r.read(&mut b).ok()? == 0 {
            return None;
        }
        head.push(b[0]);
    }
    let n: usize = std::str::from_utf8(&head).ok()?.rsplit(':').next()?.trim().parse().ok()?;
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// The handshake every session opens with, id 0 so it collides with nothing a
/// test asks. Its reply is left in what `talk` returns, since it is where the
/// release now comes from.
fn handshake() -> Vec<u8> {
    let mut b = frame(&json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                               "params": { "capabilities": {} } }));
    b.extend(frame(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} })));
    b
}

/// Send REQUESTS to a fresh server and return everything it said, notifications
/// included. CACHE, when given, becomes its `XDG_CACHE_HOME` — which is how a
/// first run on a bare machine is staged.
///
/// Everything is written in one go and read after the process exits, which is
/// what makes these deterministic. It also means `shutdown` arrives while an
/// index is still running — and must wait for it, or every assertion about what
/// a run reported would be racing it.
fn talk(requests: &[Value], cache: Option<&Path>) -> Vec<Value> {
    match cache {
        Some(c) => talk_with(requests, &[("XDG_CACHE_HOME", c)]),
        None => talk_with(requests, &[]),
    }
}

/// `talk`, with the environment spelled out — for the tests about *which*
/// variable decides where a download lands, which have to set two.
fn talk_with(requests: &[Value], env: &[(&str, &Path)]) -> Vec<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_org-semantic"));
    cmd.arg("serve").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawning the binary Cargo just built");
    let mut input = handshake();
    input.extend(requests.iter().flat_map(frame));
    input.extend(frame(&json!({ "jsonrpc": "2.0", "id": 999, "method": "shutdown" })));
    child.stdin.take().unwrap().write_all(&input).unwrap();
    messages(&child.wait_with_output().expect("the server exited").stdout)
}

fn index(vault: &Path, mode: &str) -> Vec<Value> {
    talk(
        &[json!({ "jsonrpc": "2.0", "id": 7, "method": "index",
                  "params": { "vault": vault, "mode": mode, "full": true } })],
        None,
    )
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("org-semantic-serve-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn vault(name: &str, notes: usize) -> PathBuf {
    let d = scratch(name);
    for i in 0..notes {
        std::fs::write(
            d.join(format!("n{i:04}.org")),
            format!("#+title: Note {i}\n* Section {i}\nProse about trapped atoms, number {i}.\n"),
        )
        .unwrap();
    }
    d
}

/// Every `$/progress` value, in the order it arrived.
fn reports(msgs: &[Value]) -> Vec<&Value> {
    msgs.iter().filter(|m| m["method"] == "$/progress").map(|m| &m["params"]["value"]).collect()
}

/// The `(target, phase)` pairs a run moved through, deduplicated in sequence.
fn phases(values: &[&Value]) -> Vec<(String, String)> {
    values.iter().fold(Vec::new(), |mut acc, v| {
        let key = (v["target"].as_str().unwrap().into(), v["phase"].as_str().unwrap().into());
        if acc.last() != Some(&key) {
            acc.push(key);
        }
        acc
    })
}

/// Bytes of real files under DIR, ignoring the symlinks hf-hub threads from
/// `snapshots/` to `blobs/` — counting those would double everything.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match std::fs::symlink_metadata(e.path()) {
            Ok(md) if md.is_dir() => dir_bytes(&e.path()),
            Ok(md) if md.is_file() => md.len(),
            _ => 0,
        })
        .sum()
}

// ------------------------------------------------------------ offline checks

/// The report a client correlates against, and the guarantee it renders against.
#[test]
fn progress_is_tagged_with_the_request_it_belongs_to() {
    let v = vault("token", 4);
    let msgs = index(&v, "lexical");
    let progress: Vec<&Value> = msgs.iter().filter(|m| m["method"] == "$/progress").collect();
    assert!(!progress.is_empty(), "a run says something while it runs");
    for p in &progress {
        assert_eq!(p["params"]["token"], 7, "the token is the id being waited on");
        assert_eq!(p["params"]["value"]["kind"], "report");
    }
    assert!(msgs.iter().any(|m| m["id"] == 7 && m.get("result").is_some()), "and it replies");

    // "The response ends the last run of reports" — so no report may arrive after
    // it. Reports go out on a thread of their own now, so this holds only because
    // the run closes and drains that queue before sending the reply.
    let reply = msgs.iter().position(|m| m["id"] == 7).expect("a reply");
    let last_report = msgs.iter().rposition(|m| m["method"] == "$/progress").expect("a report");
    assert!(last_report < reply, "a report overtook the reply it was meant to precede");
}

/// Phases run in the order the work happens, and each is closed exactly once —
/// there is no `end` message, so `last` is the only thing that closes one.
#[test]
fn every_phase_is_opened_in_order_and_closed_once() {
    let v = vault("phases", 4);
    let msgs = index(&v, "lexical");
    let values = reports(&msgs);

    assert_eq!(
        phases(&values),
        [("lexical".to_string(), "scan".to_string()), ("lexical".into(), "chunk".into())],
        "scan before chunk, each naming its index"
    );
    for phase in ["scan", "chunk"] {
        let ends: Vec<&&Value> =
            values.iter().filter(|v| v["phase"] == phase && v["last"] == true).collect();
        assert_eq!(ends.len(), 1, "{phase} closes once: {values:?}");
        assert_eq!(ends[0]["done"], 4, "having counted every note");
        assert_eq!(ends[0]["done"], ends[0]["total"]);
    }
}

/// `mode: "both"` scans and chunks once per index, over the same notes, because
/// the two split them differently. Without `target` a client would watch the
/// count reach the total and start again, which reads as a crash.
#[test]
#[ignore = "the semantic half needs an embedding model in the cache"]
fn each_index_reports_its_own_pass_over_the_same_notes() {
    let v = vault("both", 4);
    let msgs = index(&v, "both");
    let values = reports(&msgs);
    let seq = phases(&values);
    let chunks: Vec<&(String, String)> = seq.iter().filter(|(_, p)| p == "chunk").collect();
    assert_eq!(
        chunks,
        [&("semantic".to_string(), "chunk".to_string()), &("lexical".into(), "chunk".into())],
        "one chunking pass each, distinguishable: {seq:?}"
    );
    assert!(
        seq.contains(&("semantic".into(), "embed".into())),
        "and only the semantic index embeds: {seq:?}"
    );

    // Every phase obeys the same send rate, embedding included — a batch takes
    // far longer than the floor, so nothing is withheld without an exemption
    // for it.  If this ever drops reports, batches have become faster than
    // 100 ms and the question is whether a client wants them all.
    let embed: Vec<&&Value> = values.iter().filter(|v| v["phase"] == "embed").collect();
    let chunks = embed.last().expect("something was embedded")["total"].as_u64().unwrap();
    assert_eq!(embed.len() as u64, chunks.div_ceil(64), "one report per batch of 64: {embed:?}");
}

/// The send rate is flow control, not display policy. Files are far too fine a
/// unit to send one at a time — a thousand of them is ~2,000 reports of ~200
/// bytes against a pipe of 16 kB that only the editor can drain.
#[test]
fn reports_counted_in_files_are_thinned_on_the_way_out() {
    let v = vault("floor", 400);
    let msgs = index(&v, "lexical");
    let values = reports(&msgs);
    let scans = values.iter().filter(|v| v["phase"] == "scan").count();
    assert!(scans < 400, "400 notes must not mean 400 notifications, got {scans}");
    // Thinned, never silenced: the phase still opens and still closes.
    assert!(values.iter().any(|v| v["phase"] == "scan" && v["last"] == true));
    assert!(values.iter().any(|v| v["phase"] == "chunk" && v["last"] == true));
}

/// A notification carries no id, so there is no token to report under and
/// nothing to correlate a report with. Reporting anyway would be noise a client
/// could not attribute.
#[test]
fn an_index_sent_as_a_notification_says_nothing() {
    let v = vault("notification", 4);
    let msgs = talk(
        &[json!({ "jsonrpc": "2.0", "method": "index",
                  "params": { "vault": v, "mode": "lexical", "full": true } })],
        None,
    );
    assert!(
        !msgs.iter().any(|m| m["method"] == "$/progress"),
        "no id, no token, no reports: {msgs:?}"
    );
    assert!(!msgs.iter().any(|m| m["id"] == 7), "and no reply either");
}

/// A failing index answers with an error and owes nothing further. This is why
/// there is no `begin`/`end` pair: an `end` would be skipped here, and a client
/// would hold that token for ever.
#[test]
fn a_failing_index_leaves_no_progress_owed() {
    let msgs = talk(
        &[json!({ "jsonrpc": "2.0", "id": 7, "method": "index",
                  "params": { "vault": "/nonesuch", "mode": "lexical" } })],
        None,
    );
    let reply: Vec<&Value> = msgs.iter().filter(|m| m["id"] == 7).collect();
    assert_eq!(reply.len(), 1);
    assert!(reply[0].get("error").is_some(), "it failed, and said so: {reply:?}");
}

/// Errors a client must act on carry a label; the rest carry none, and that
/// absence is what says "show this, there is nothing to decide".
#[test]
fn a_condition_worth_acting_on_arrives_labelled() {
    let v = vault("drift", 2);
    // Built in a session of its own, and deliberately not by sending `index`
    // ahead of the searches in this one: an index runs on a worker now, so a
    // request behind it is answered *while* it runs rather than after. The
    // search would find no lexical index yet and say so — correctly, and not
    // what this test is about.
    index(&v, "lexical");
    let msgs = talk(
        &[
            json!({ "jsonrpc": "2.0", "id": 2, "method": "search",
                    "params": { "vault": v, "query": "atoms", "mode": "lexical",
                                "config": { "languages": ["en-US"],
                                            "todo_keywords": ["TODO", "DONE", "WAITING"] } } }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "search",
                    "params": { "vault": "/nonesuch", "query": "atoms" } }),
        ],
        None,
    );
    let err = |id: i64| msgs.iter().find(|m| m["id"] == id).expect("a reply")["error"].clone();

    let drift = err(2);
    assert_eq!(drift["data"]["kind"], "config-drift");
    assert_eq!(drift["data"]["changed"], json!(["todo_keywords"]));
    assert_eq!(drift["data"]["remedy"], "reindex-full");
    assert!(
        drift["message"].as_str().unwrap().contains("todo_keywords"),
        "and still reads as a sentence"
    );

    assert_eq!(err(3)["data"]["kind"], "no-index", "a missing vault has no semantic index");
}

/// What the indexer found but survived, carried back with the reply rather than
/// written to a stderr nobody correlates with a request.
#[test]
fn warnings_ride_the_reply() {
    let v = vault("remarks", 2);
    std::fs::write(v.join("broken.org"), [0xffu8, 0xfe, 0x00]).unwrap();
    let msgs = index(&v, "lexical");
    let reply = msgs.iter().find(|m| m["id"] == 7).expect("a reply");
    let remarks = reply["result"]["remarks"].as_array().expect("remarks");
    let unreadable: Vec<&Value> =
        remarks.iter().filter(|r| r["kind"] == "unreadable-file").collect();
    assert_eq!(unreadable.len(), 1, "named once, not once per pass: {remarks:?}");
    assert_eq!(unreadable[0]["path"], "broken.org", "vault-relative, as a hit is");
}

/// The Emacs package ships from this repo and moves with it, so the two are one
/// release and one version. That is the whole of the compatibility story: the
/// package checks the binary is its own, and fetches the right one if not.
///
/// Asked two ways because a client needs it at two moments. `--version` reads
/// the file on disk, which is what to check before spawning anything.
/// `initialize` answers for the *process*, which is a different thing the moment
/// a new binary has been installed under a server that is still running — and it
/// answers at the one moment a client is guaranteed to be listening.
#[test]
fn the_binary_says_which_release_it_is() {
    let flag = Command::new(env!("CARGO_BIN_EXE_org-semantic")).arg("--version").output().unwrap();
    let printed = String::from_utf8(flag.stdout).unwrap().trim().to_string();
    assert_eq!(printed, env!("CARGO_PKG_VERSION"), "`--version` is the crate's own");

    // No vault, and no request: this is about the process, and nothing else.
    let msgs = talk(&[], None);
    let hello = &msgs.iter().find(|m| m["id"] == 0).expect("the handshake is answered")["result"];
    assert_eq!(hello["serverInfo"]["version"], printed, "the same release the flag prints");
}

/// `status` answers about a vault and nothing else: which indexes it has, so a
/// client can offer the commands that will work and explain the ones that will
/// not. The release is a separate question with a separate method.
#[test]
fn status_answers_about_a_vault() {
    let v = vault("status", 2);
    let msgs = talk(
        &[json!({ "jsonrpc": "2.0", "id": 7, "method": "status", "params": { "vault": v } })],
        None,
    );
    let result = &msgs.iter().find(|m| m["id"] == 7).expect("a reply")["result"];
    assert_eq!(result["lexical"], false, "nothing built here yet");
    assert_eq!(result["semantic"], json!([]));
    assert!(result.get("version").is_none(), "not its question to answer: {result:?}");
}

/// `close` is how a client says it is done with a vault, so the chunk table and
/// vectors can go — and the shared model with them, if nothing else holds it.
///
/// Only the bookkeeping is checked here, because anything more needs an embedding
/// model. That the weights are genuinely shared is a *measurement*, not an
/// assertion: three vaults on one model cost 255.7 MB against 253.5 MB for one,
/// and a second vault's first search skips the 150 ms load.
#[test]
fn closing_a_vault_is_asked_for_by_name() {
    let v = vault("close", 2);
    let msgs = talk(
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "close",
                    "params": { "vault": v } }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "close", "params": {} }),
        ],
        None,
    );
    let reply = |id: i64| msgs.iter().find(|m| m["id"] == id).expect("a reply");

    // Nothing was loaded, so nothing is dropped — and that is an answer, not an
    // error: a client closing a vault it never searched has done nothing wrong.
    assert_eq!(reply(1)["result"]["dropped"], 0);
    // A vault is the whole of the request, so its absence is a mistake worth
    // reporting rather than a licence to forget everything.
    assert!(reply(2)["error"]["message"].as_str().unwrap().contains("vault"));
}

// ------------------------------------------------- answering while it indexes

/// Drive a server the tests keep talking to, rather than writing everything at
/// once — which is the only way to observe what it does *while* it is busy.
struct Session {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
}

impl Session {
    fn open() -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_org-semantic"))
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        stdin.write_all(&handshake()).unwrap();
        stdin.flush().unwrap();
        read_one(&mut stdout).expect("the handshake is answered");
        Session { child, stdin, stdout }
    }

    fn send(&mut self, v: &Value) {
        self.stdin.write_all(&frame(v)).unwrap();
        self.stdin.flush().unwrap();
    }

    /// The next message that is a reply, skipping the reports in between.
    fn reply(&mut self) -> Value {
        loop {
            let m = read_one(&mut self.stdout).expect("a reply");
            if m["method"] != "$/progress" {
                return m;
            }
        }
    }

    /// Start a full lexical index on a vault large enough to still be running
    /// when the next request lands, and wait until it says it has begun.
    fn indexing(v: &Path, id: i64) -> Session {
        let mut s = Session::open();
        s.send(&json!({ "jsonrpc": "2.0", "id": id, "method": "index",
                        "params": { "vault": v, "mode": "lexical", "full": true } }));
        let first = read_one(&mut s.stdout).expect("it reports before it finishes");
        assert_eq!(first["method"], "$/progress", "the run has begun: {first:?}");
        s
    }

    fn close(mut self) {
        self.send(&json!({ "jsonrpc": "2.0", "id": 999, "method": "shutdown" }));
        drop(self.stdin);
        while read_one(&mut self.stdout).is_some() {}
        self.child.wait().unwrap();
    }
}

/// A vault whose lexical index is already built, which is the situation a
/// rebuild actually happens in — and the only one where a search during it has a
/// committed version to answer from.
fn built(name: &str, notes: usize) -> PathBuf {
    let v = vault(name, notes);
    index(&v, "lexical");
    v
}

/// The whole feature, in one assertion: a search sent during a reindex is
/// answered **before** the reindex is, and answered from the version already
/// committed rather than refused.
///
/// Lexical on both sides, so this needs no embedding model and runs offline.
/// What it proves is the loop, not the model lock: that `index` returns to the
/// loop instead of holding it for the length of the run.
#[test]
fn a_search_is_answered_while_an_index_runs() {
    let v = built("concurrent", 3000);
    let mut s = Session::indexing(&v, 7);
    s.send(&json!({ "jsonrpc": "2.0", "id": 8, "method": "search",
                    "params": { "vault": v, "query": "atoms", "mode": "lexical" } }));

    let first = s.reply();
    assert_eq!(first["id"], 8, "the search answers first — the index is still running: {first:?}");
    assert!(
        !first["result"]["hits"].as_array().expect("hits, not an error").is_empty(),
        "and answers from the committed version: {first:?}"
    );
    let second = s.reply();
    assert_eq!(second["id"], 7, "and the index answers when it is done");
    s.close();
}

/// A search answered mid-run is a version behind, and says so, because the
/// alternative is an editor polling `status` per keystroke to find out.
#[test]
fn a_search_says_when_the_index_is_moving() {
    let v = built("moving", 3000);
    let mut s = Session::indexing(&v, 7);
    s.send(&json!({ "jsonrpc": "2.0", "id": 8, "method": "search",
                    "params": { "vault": v, "query": "atoms", "mode": "lexical" } }));
    let during = s.reply();
    assert_eq!(during["id"], 8);
    assert_eq!(during["result"]["indexing"], true, "answered from a version behind");

    // Wait the run out, then ask again.
    assert_eq!(s.reply()["id"], 7);
    s.send(&json!({ "jsonrpc": "2.0", "id": 9, "method": "search",
                    "params": { "vault": v, "query": "atoms", "mode": "lexical" } }));
    let after = s.reply();
    assert_eq!(after["result"]["indexing"], false, "and current once nothing is running");
    s.close();
}

/// LSP's two steps are two for a reason, and getting it wrong hangs the process
/// rather than failing it.
///
/// `shutdown` says stop accepting work; `exit` ends the process. Ending the loop
/// on `shutdown` meant waiting on a reader thread still blocked on a stdin the
/// client had not closed — a clean shutdown that never returned. Every test here
/// closes stdin, so none of them saw it; driving the server by hand did.
#[test]
fn shutdown_stops_the_work_and_exit_stops_the_process() {
    let mut s = Session::open();
    s.send(&json!({ "jsonrpc": "2.0", "id": 1, "method": "shutdown" }));
    assert_eq!(s.reply()["id"], 1, "it answers");

    // Still there, with stdin open and no `exit` sent.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(s.child.try_wait().unwrap().is_none(), "shutdown is not exit");

    s.send(
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "status", "params": { "vault": "/tmp" } }),
    );
    assert_eq!(s.reply()["error"]["code"], -32600, "and takes no more work");

    s.send(&json!({ "jsonrpc": "2.0", "method": "exit", "params": {} }));
    assert!(s.child.wait().unwrap().success(), "which is what ends it");
}

/// The model lock, which the lexical tests above cannot reach: a *semantic*
/// search answered during a *semantic* rebuild shares one `TextEmbedding` with
/// the indexer, and gets it between two batches.
///
/// Both halves matter. Mid-run the answer is the version already committed —
/// which must not include the note added since. After the run answers, the same
/// query finds it, from the version the run handed over rather than one read
/// back off disk.
#[test]
#[ignore = "needs an embedding model in the cache"]
fn a_semantic_search_answers_from_the_old_version_during_a_rebuild() {
    let v = vault("contend", 400);
    index(&v, "semantic");
    let ask = |id: i64| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "search",
                "params": { "vault": v, "query": "a hare on a bicycle", "k": 5 } })
    };
    let found = |m: &Value| {
        m["result"]["hits"]
            .as_array()
            .expect("hits")
            .iter()
            .any(|h| h["heading"].as_str().is_some_and(|s| s.contains("Hare")))
    };

    let mut s = Session::open();
    s.send(&ask(1));
    assert!(!found(&s.reply()), "the premise: nothing about hares yet");

    // A note the rebuild will pick up and the committed version cannot know.
    std::fs::write(v.join("hare.org"), "#+title: Hare\n* Hare on a bicycle\nIt pedals.\n").unwrap();
    // `conserveMemory`, so this run *shares* the resident model rather than
    // loading its own — the path where the searcher and the indexer really do
    // contend for one `TextEmbedding`, which is what this test is here to cover.
    // The default path leaves them independent and would prove less.
    s.send(&json!({ "jsonrpc": "2.0", "id": 7, "method": "index",
                    "params": { "vault": v, "full": true, "conserveMemory": true } }));
    let first = read_one(&mut s.stdout).expect("it reports before it finishes");
    assert_eq!(first["method"], "$/progress");

    s.send(&ask(8));
    let during = s.reply();
    assert_eq!(during["id"], 8, "answered while the rebuild runs: {during:?}");
    assert_eq!(during["result"]["indexing"], true);
    assert!(!found(&during), "and from the version committed before it: {during:?}");

    assert_eq!(s.reply()["id"], 7, "the rebuild answers");
    s.send(&ask(9));
    let after = s.reply();
    assert!(found(&after), "adopted without a reload: {after:?}");
    s.close();
}

/// Two vaults, one model, and the lifetime that makes sharing safe: closing one
/// vault must not disturb the other, and the model must survive until the last
/// vault using it is gone.
///
/// The `Weak` in `Server::models` is what gives that for free. Hold `Arc`s there
/// instead and the model would outlive every vault; drop it eagerly on the first
/// `close` and the second vault would be searching through a model that had been
/// unloaded underneath it.
#[test]
#[ignore = "needs an embedding model in the cache"]
fn one_model_serves_several_vaults_and_outlives_all_but_the_last() {
    let a = vault("share-a", 8);
    let b = vault("share-b", 8);
    index(&a, "semantic");
    index(&b, "semantic");
    let ask = |id: i64, v: &Path| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "search",
                "params": { "vault": v, "query": "trapped atoms", "k": 3 } })
    };
    let loaded = |id: i64, v: &Path| json!({ "jsonrpc": "2.0", "id": id, "method": "status", "params": { "vault": v } });

    let mut s = Session::open();
    s.send(&ask(1, &a));
    assert!(!s.reply()["result"]["hits"].as_array().unwrap().is_empty(), "a answers");
    s.send(&ask(2, &b));
    assert!(!s.reply()["result"]["hits"].as_array().unwrap().is_empty(), "b answers");
    s.send(&loaded(3, &a));
    assert_eq!(s.reply()["result"]["loaded"], true, "a is resident");

    // Both resident is also what `memory` is for: two vaults, and the model
    // counted **once**, because that is how it is actually held.
    s.send(&json!({ "jsonrpc": "2.0", "id": 90, "method": "memory" }));
    let mem = s.reply();
    assert_eq!(mem["result"]["vaults"].as_array().unwrap().len(), 2, "{mem}");
    let models = mem["result"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 1, "one model for both vaults: {mem}");
    assert!(
        models[0]["weightFile"].as_u64().is_some_and(|n| n > 1_000_000),
        "the weights are cached, so their size on disk is known: {mem}"
    );
    let one = &mem["result"]["vaults"][0];
    let chunks = one["chunks"].as_u64().unwrap();
    assert_eq!(one["vectors"].as_u64().unwrap(), chunks * 384 * 4, "vectors are exact");
    assert!(one["table"].as_u64().is_some_and(|n| n > chunks), "and the table is summed");

    s.send(&json!({ "jsonrpc": "2.0", "id": 4, "method": "close", "params": { "vault": a } }));
    assert_eq!(s.reply()["result"]["dropped"], 1);
    // Per vault, so this says which one went rather than merely how many did.
    s.send(&loaded(5, &a));
    assert_eq!(s.reply()["result"]["loaded"], false, "a was forgotten");
    s.send(&loaded(6, &b));
    assert_eq!(s.reply()["result"]["loaded"], true, "and b was not");

    // The half that a wrong lifetime would break: b never asked to be forgotten.
    s.send(&ask(7, &b));
    assert!(
        !s.reply()["result"]["hits"].as_array().unwrap().is_empty(),
        "closing one vault must not unload the model another is using"
    );

    // And with the last owner gone, a search simply loads it again.
    s.send(&json!({ "jsonrpc": "2.0", "id": 8, "method": "close", "params": { "vault": b } }));
    assert_eq!(s.reply()["result"]["dropped"], 1);
    s.send(&ask(9, &a));
    assert!(!s.reply()["result"]["hits"].as_array().unwrap().is_empty(), "and comes back");
    s.close();
}

/// Vaults index independently. A rebuild of one must not refuse a reindex of
/// another — they share no index, and the run slot is per vault for that reason.
#[test]
fn indexing_one_vault_does_not_block_another() {
    let a = vault("busy-a", 3000);
    let b = vault("busy-b", 400);
    let mut s = Session::indexing(&a, 7);
    s.send(&json!({ "jsonrpc": "2.0", "id": 8, "method": "index",
                    "params": { "vault": b, "mode": "lexical", "full": true } }));

    // b is small, so it finishes first — which is the point: it did not wait.
    let first = s.reply();
    assert_eq!(first["id"], 8, "the second vault ran rather than being refused: {first:?}");
    assert!(first.get("result").is_some(), "{first:?}");
    assert_eq!(s.reply()["id"], 7, "and the first still finishes");
    s.close();
}

/// `memory` answers about the **process**, in raw bytes, and only about what can
/// actually be counted.
///
/// Its own method rather than a field on `status`, which answers about a vault —
/// the distinction this codebase has got wrong twice. Nothing derived is sent: a
/// client wanting "unaccounted for" subtracts, and one wanting "MB" formats.
///
/// Deliberately absent: any figure for ONNX itself. `ort` exposes no usage
/// reporting, so `rss` minus the rest would be an invented measurement — the
/// remainder holds the allocator's retained pages and our own working set too.
#[test]
fn memory_reports_what_it_can_count_and_no_more() {
    let msgs = talk(&[json!({ "jsonrpc": "2.0", "id": 1, "method": "memory" })], None);
    let m = &msgs.iter().find(|m| m["id"] == 1).expect("a reply")["result"];

    // A real figure even with nothing loaded — this is the one number that covers
    // the runtime, and a server holding no index is still holding ONNX.
    assert!(m["rss"].as_u64().is_some_and(|n| n > 1_000_000), "rss is real: {m}");
    assert_eq!(m["vaults"], json!([]), "nothing searched, nothing loaded");
    assert_eq!(m["models"], json!([]));

    for invented in ["onnx", "runtime", "total", "accounted", "unattributed", "overhead"] {
        assert!(
            m.get(invented).is_none(),
            "`{invented}` would be a number we cannot measure or one the client derives: {m}"
        );
    }
}

/// A vault is claimed across processes, not just within one.
///
/// `Server::run` gives one run per vault inside a process; nothing stopped a plain
/// `org-semantic index` from writing the same index at the same time, and
/// `save_index` stages both data files at fixed paths. Most interleavings would be
/// caught by the length check, but chunks from one run paired with vectors from the
/// other at equal counts is silent — so the guard is a lock file, as tantivy
/// already does for the lexical index.
#[test]
fn a_command_line_run_is_refused_while_the_server_indexes_the_same_vault() {
    let v = vault("claimed", 3000);
    let mut s = Session::indexing(&v, 7);

    let cli = Command::new(env!("CARGO_BIN_EXE_org-semantic"))
        .args(["index", v.to_str().unwrap(), "--lexical", "--full"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&cli.stderr).trim().to_string();
    assert!(!cli.status.success(), "the second writer must not proceed: {said}");
    assert!(said.contains("indexing this vault"), "and must say why: {said}");

    // The run that held the claim is unaffected, and releases it.
    let reply = s.reply();
    assert_eq!(reply["id"], 7);
    assert!(reply.get("result").is_some(), "{reply:?}");
    assert!(
        !v.join(".org-semantic").join("index.lock").exists(),
        "the claim is released when the run ends"
    );
    s.close();
}

/// Cancelling one vault's run must leave the other's alone.
///
/// This is the half a **global** stop flag would get wrong, and silently: with
/// one `AtomicBool` for the process, `$/cancelRequest` for vault A stopped
/// vault B's run too, and B would answer `-32800` for something nobody
/// cancelled. Each run owns its own flag.
#[test]
fn cancelling_one_vault_leaves_another_running() {
    let a = vault("cancel-a", 3000);
    let b = vault("cancel-b", 3000);
    let mut s = Session::indexing(&a, 7);
    s.send(&json!({ "jsonrpc": "2.0", "id": 8, "method": "index",
                    "params": { "vault": b, "mode": "lexical", "full": true } }));
    s.send(&json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": 7 } }));

    let mut by_id = std::collections::HashMap::new();
    for _ in 0..2 {
        let m = s.reply();
        by_id.insert(m["id"].as_i64().unwrap(), m);
    }
    assert_eq!(by_id[&7]["error"]["data"]["kind"], "cancelled", "a was asked to stop");
    assert!(
        by_id[&8].get("result").is_some(),
        "b was not, and must have finished: {:?}",
        by_id[&8]
    );
    assert!(b.join(".org-semantic").join("lexical.json").exists(), "b really wrote its index");
    assert!(!a.join(".org-semantic").join("lexical.json").exists(), "and a really did not");
    s.close();
}

/// One run at a time **per vault**, refused rather than queued: a second is
/// nearly always the same work again, and a client firing on every save must
/// coalesce anyway. Labelled, so it can tell this from a failure and simply wait.
#[test]
fn a_second_index_is_refused_while_one_runs() {
    let v = vault("busy", 3000);
    let mut s = Session::indexing(&v, 7);
    s.send(&json!({ "jsonrpc": "2.0", "id": 8, "method": "index",
                    "params": { "vault": v, "mode": "lexical", "full": true } }));

    let refused = s.reply();
    assert_eq!(refused["id"], 8);
    assert_eq!(refused["error"]["data"]["kind"], "indexing", "{refused:?}");
    assert_eq!(s.reply()["id"], 7, "and the first one still finishes");
    s.close();
}

/// `shutdown` waits for the run in flight so its reply still goes out; `exit`
/// does not. Without the wait, every test that sends `index` and `shutdown` in
/// one write would be asserting on a run that had been killed.
#[test]
fn shutdown_waits_for_the_run_in_flight() {
    let v = vault("graceful", 3000);
    let mut s = Session::indexing(&v, 7);
    s.send(&json!({ "jsonrpc": "2.0", "id": 8, "method": "shutdown" }));
    drop(s.stdin);

    let mut ids = Vec::new();
    while let Some(m) = read_one(&mut s.stdout) {
        if m["method"] != "$/progress" {
            ids.push(m["id"].clone());
        }
    }
    s.child.wait().unwrap();
    assert_eq!(ids, [json!(7), json!(8)], "the run answered, and then we went");
    assert!(
        v.join(".org-semantic").join("lexical.json").exists(),
        "having actually finished rather than been abandoned"
    );
}

// --------------------------------------------------------------- cancellation

/// Stopping a run that is already under way, by the id it will answer under.
///
/// `$/cancelRequest`, which is possible again now the index runs on a worker:
/// the loop reads throughout, so the message arrives while there is still
/// something to stop. It was a `SIGINT` for as long as the loop sat inside the
/// index and read nothing until it answered.
///
/// Timed off the server's own first report rather than off a sleep: it says
/// when it has started, so nothing here guesses about whether the cancellation
/// landed mid-run. A sleep did guess, and guessed wrong — 3,000 notes index in
/// under half a second, which was less than the 400 ms it waited.
#[test]
fn a_run_is_cancelled_by_the_id_it_answers_under() {
    let v = vault("cancel", 3000);
    let mut child = Command::new(env!("CARGO_BIN_EXE_org-semantic"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    stdin.write_all(&handshake()).unwrap();
    stdin
        .write_all(&frame(&json!({ "jsonrpc": "2.0", "id": 7, "method": "index",
                                   "params": { "vault": v, "mode": "lexical", "full": true } })))
        .unwrap();
    stdin.flush().unwrap();

    let hello = read_one(&mut stdout).expect("the handshake is answered first");
    assert_eq!(hello["id"], 0);
    let first = read_one(&mut stdout).expect("it reports before it finishes");
    assert_eq!(first["method"], "$/progress", "and that is the first thing it says: {first:?}");
    stdin
        .write_all(&frame(
            &json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": 7 } }),
        ))
        .unwrap();
    stdin.flush().unwrap();

    // It answers rather than dies: one request ended, not the session.
    stdin.write_all(&frame(&json!({ "jsonrpc": "2.0", "id": 8, "method": "shutdown" }))).unwrap();
    drop(stdin);
    let mut msgs = vec![first];
    while let Some(m) = read_one(&mut stdout) {
        msgs.push(m);
    }
    child.wait().unwrap();

    let reply = msgs.iter().find(|m| m["id"] == 7).expect("the cancelled index still answered");
    assert_eq!(reply["error"]["code"], -32800, "LSP's RequestCancelled: {reply:?}");
    assert_eq!(reply["error"]["data"]["kind"], "cancelled");
    assert!(
        msgs.iter().any(|m| m["id"] == 8),
        "and the session survived it — one request ended, not the server"
    );
    assert!(
        !v.join(".org-semantic").join("lexical.json").exists(),
        "nothing half-written: the checks sit between units, never inside one"
    );
}

/// A cancellation for an id that is not running belongs to nothing. Without
/// rearming — and without matching on the id — it would stop whatever was asked
/// for next.
#[test]
fn a_cancellation_for_nothing_does_not_poison_the_next_run() {
    let v = vault("rearm", 4);
    let mut child = Command::new(env!("CARGO_BIN_EXE_org-semantic"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&handshake()).unwrap();

    // Idle: nothing is in flight to cancel, and id 7 is not running *yet*.
    stdin
        .write_all(&frame(
            &json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": 7 } }),
        ))
        .unwrap();
    stdin
        .write_all(&frame(&json!({ "jsonrpc": "2.0", "id": 7, "method": "index",
                                   "params": { "vault": v, "mode": "lexical", "full": true } })))
        .unwrap();
    stdin.write_all(&frame(&json!({ "jsonrpc": "2.0", "id": 8, "method": "shutdown" }))).unwrap();
    drop(stdin);
    let msgs = messages(&child.wait_with_output().unwrap().stdout);

    let reply = msgs.iter().find(|m| m["id"] == 7).expect("a reply");
    assert!(reply.get("result").is_some(), "the next request runs normally: {reply:?}");
}

// ------------------------------------------------------- what a cold machine sees

/// The wait this whole channel exists for: minutes of network before a single
/// note is looked at.
///
/// `#[ignore]` because it downloads. A fresh `XDG_CACHE_HOME` is what makes the
/// run cold, and a subprocess is what makes that possible at all — the
/// classifier caches in a `OnceLock`, so a second cold start in one process is
/// not cold.
fn assert_announced(msgs: &[Value], target: &str, cache: &Path) {
    let values = reports(msgs);
    let downloads: Vec<&&Value> = values.iter().filter(|v| v["phase"] == "download").collect();
    assert_eq!(downloads.len(), 1, "one download, announced once: {values:?}");
    assert_eq!(downloads[0]["target"], target);
    assert!(downloads[0]["bytes"].is_u64(), "a size was asked for and given: {downloads:?}");
    assert!(
        downloads[0].get("total").is_none(),
        "and no denominator, since nothing counts up to it: {downloads:?}"
    );

    // Before the work that waits on it — the point of saying it at all. Not
    // necessarily first: the semantic index scans the vault before it discovers
    // it has no model.
    let at = |phase: &str| values.iter().position(|v| v["phase"] == phase);
    assert!(
        at("download") < at("chunk").or(at("embed")).or(Some(usize::MAX)),
        "announced before the work it holds up: {values:?}"
    );

    let reply = msgs.iter().find(|m| m["id"] == 7).expect("the index replied");
    let remarks = reply["result"]["remarks"].as_array().expect("remarks in the reply");
    assert!(
        remarks.iter().any(|r| r["kind"] == "model-downloaded"),
        "and recorded, so the reply explains the minutes: {remarks:?}"
    );

    // The size that was quoted against what arrived. A wide band on purpose:
    // the cache holds a tokenizer and some JSON besides the weights, so this
    // catches the announcement naming a different artefact — a quantised
    // variant is four times out — not a few per cent.
    let announced = downloads[0]["bytes"].as_u64().unwrap();
    let landed = dir_bytes(cache);
    let ratio = announced as f64 / landed.max(1) as f64;
    assert!(
        (0.67..1.5).contains(&ratio),
        "announced {announced} bytes but {landed} arrived — what was asked for is not what \
         fastembed fetched"
    );
}

#[test]
#[ignore = "downloads the 938 kB language classifier"]
fn a_cold_classifier_is_announced_before_it_is_fetched() {
    let cache = scratch("lid-cache");
    let v = vault("lid", 3);
    let msgs = talk(
        &[json!({ "jsonrpc": "2.0", "id": 7, "method": "index",
                  "params": { "vault": v, "mode": "lexical", "full": true } })],
        Some(&cache),
    );
    assert_announced(&msgs, "lexical", &cache);
    assert!(
        cache.join("org-semantic").join("lid.176.ftz").exists(),
        "and the file landed where the announcement implied"
    );
}

/// `ORG_SEMANTIC_CACHE_HOME` decides where the bytes go — proved by fetching
/// them, not by reading a path back out of `models`.
///
/// The cheap end of the same mechanism: the classifier is 938 kB where a model
/// is 133 MB, and both resolve through the one `xdg_cache()`. Uses the real
/// network for the same reason the two tests above do — the download path is
/// the one thing no offline test can reach, and it is what a user meets first.
///
/// **Both variables are set, and the decoy is asserted empty.** Naming the
/// destination is only half the claim; the other half is that nothing leaked
/// into `$XDG_CACHE_HOME`, which is where every byte went before this existed
/// and where a partial override would put some of them again.
#[test]
#[ignore = "downloads the 938 kB language classifier"]
fn the_download_lands_in_the_directory_the_variable_names() {
    let real = scratch("cache-home-real");
    let decoy = scratch("cache-home-decoy");
    let v = vault("cache-home", 3);
    let msgs = talk_with(
        &[json!({ "jsonrpc": "2.0", "id": 7, "method": "index",
                  "params": { "vault": v, "mode": "lexical", "full": true } })],
        &[("ORG_SEMANTIC_CACHE_HOME", &real), ("XDG_CACHE_HOME", &decoy)],
    );

    let reply = msgs.iter().find(|m| m["id"] == 7).expect("the index replied");
    assert!(reply.get("result").is_some(), "the run has to succeed to prove anything: {reply:?}");
    assert!(
        real.join("org-semantic").join("lid.176.ftz").exists(),
        "the classifier landed under ORG_SEMANTIC_CACHE_HOME"
    );
    assert_eq!(
        std::fs::read_dir(&decoy).map(|d| d.count()).unwrap_or(0),
        0,
        "and nothing at all was written under XDG_CACHE_HOME — ours replaces it, \
         rather than covering some of the downloads and not others"
    );
}

#[test]
#[ignore = "downloads the 133 MB embedding model"]
fn a_cold_embedding_model_is_announced_before_it_is_fetched() {
    let cache = scratch("model-cache");
    let v = vault("model", 3);
    let msgs = talk(
        &[json!({ "jsonrpc": "2.0", "id": 7, "method": "index",
                  "params": { "vault": v, "mode": "semantic", "full": true } })],
        Some(&cache),
    );
    assert_announced(&msgs, "semantic", &cache);
}

/// A search never fetches a model, however much it would have to fetch.
///
/// Not a timeout problem, though a client meets it as one. A search is answered
/// *on the message loop*, so downloading inside it would stop the server dead
/// for the length of the fetch — 128 MB for `bge-small-en`, 2.24 GB for the
/// large multilingual ones — with nothing else read meanwhile: no lexical
/// search, no `status`, no `$/cancelRequest`, no `shutdown`. A download has no
/// unit boundaries to check a cancel flag between either, so the only way out
/// would have been killing the process.
///
/// This is the state a vault arrives in when it is copied to another machine,
/// or when a cache is cleared under it: the index is there and the weights are
/// not. `built_models` asks only that the manifest exists, so an empty file is
/// exactly that state.
///
/// Runs offline *because* it needs the model to be absent, which is the one
/// thing a fresh `XDG_CACHE_HOME` guarantees.
#[test]
fn a_search_refuses_to_fetch_a_model_and_says_where_to_get_it() {
    let v = vault("model-missing", 1);
    let dir = v.join(".org-semantic").join("semantic").join("bge-small-en");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), "{}").unwrap();
    let cache = scratch("model-missing-cache");

    let msgs = talk(
        &[
            json!({ "jsonrpc": "2.0", "id": 2, "method": "search",
                    "params": { "vault": v, "query": "atoms" } }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "status",
                    "params": { "vault": v } }),
        ],
        Some(&cache),
    );
    let reply = |id: i64| msgs.iter().find(|m| m["id"] == id).expect("a reply").clone();

    let err = reply(2)["error"].clone();
    assert_eq!(err["data"]["kind"], "model-missing", "{err:?}");
    assert_eq!(err["data"]["model"], "bge-small-en");
    assert_eq!(err["data"]["remedy"], "index", "and says which call fetches it");
    // No run is fetching it, so offering to start one is honest.  This is the
    // one error carrying `indexing', because it is the one a client repeats:
    // search-as-you-type meets it per keystroke, and without this the
    // hundredth refusal reads exactly like the first.
    assert_eq!(err["data"]["indexing"], false, "{err:?}");

    // Said by `status` too, so a client can offer to fetch before anyone is
    // turned down rather than after.
    let semantic = reply(3)["result"]["semantic"].clone();
    assert_eq!(semantic[0]["name"], "bge-small-en");
    assert_eq!(semantic[0]["cached"], false, "{semantic:?}");

    // The whole point, and asserted on disk rather than on the clock: refused,
    // not fetched.
    assert_eq!(
        std::fs::read_dir(&cache).map(|d| d.count()).unwrap_or(0),
        0,
        "a search must leave the model cache exactly as it found it"
    );
}

/// Where the downloads land is settable, and the variable is the *cache home* —
/// it stands in for `$XDG_CACHE_HOME`, so the layout under it is the same one.
///
/// Driven as a subprocess because that is the only way to set an environment
/// variable without racing every other test in this file: `std::env::set_var`
/// is process-global and `cargo test` runs these on threads.
///
/// It asserts the **override wins over `XDG_CACHE_HOME`**, since both being set
/// is the ordinary case — every Linux desktop sets the latter — and a rule that
/// only holds when the other is absent is not the rule anyone wants.
#[test]
fn the_download_directory_can_be_moved_and_the_move_is_visible() {
    let elsewhere = scratch("cache-elsewhere");
    let ignored = scratch("cache-ignored");
    let out = Command::new(env!("CARGO_BIN_EXE_org-semantic"))
        .arg("models")
        .env("ORG_SEMANTIC_CACHE_HOME", &elsewhere)
        .env("XDG_CACHE_HOME", &ignored)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(out.status.success(), "{said}");
    assert!(
        said.contains(elsewhere.join("fastembed").to_str().unwrap()),
        "`models` must name the directory the weights actually go to: {said}"
    );
    assert!(
        said.contains(elsewhere.join("org-semantic").to_str().unwrap()),
        "and the classifier's, which is the other download: {said}"
    );
    assert!(
        !said.contains(ignored.to_str().unwrap()),
        "ours replaces XDG_CACHE_HOME rather than losing to it: {said}"
    );
}

/// `lang:` is refused by the semantic side in **both** directions, through the
/// server as well as the command line.
///
/// This is here rather than beside the parser tests because that is where it
/// went wrong: the CLI's copy of the check learned about `-lang:` and the
/// server's did not, so a negated language through an editor was parsed,
/// silently ignored — the semantic index records no language for
/// `Filters::matches` to consult — and answered as though the exclusion had
/// applied. Both now call `Filters::wants_language`, and this test is what says
/// the server's path is covered.
///
/// Offline: the refusal happens before any index or model is touched, so a vault
/// with nothing built reaches it.
#[test]
fn a_negated_language_is_refused_by_the_semantic_side_too() {
    let v = vault("negated-lang", 2);
    let msgs = talk(
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "search",
                    "params": { "vault": v, "query": "-lang:en atoms" } }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "search",
                    "params": { "vault": v, "query": "lang:en atoms" } }),
        ],
        None,
    );
    for id in [1, 2] {
        let reply = msgs.iter().find(|m| m["id"] == id).expect("a reply");
        let message = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("lang:"),
            "id {id} must be refused for naming a language, not answered: {reply:?}"
        );
    }
}
