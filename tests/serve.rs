//! The server, driven the way an editor drives it.
//!
//! Everything here spawns the real binary and speaks JSON-RPC 2.0 over its
//! stdio with LSP framing, because that is the half of `serve.rs` no unit test
//! reaches: framing, the request id doubling as a progress token, the send-rate
//! floor, and what a notification with no id is owed. Those were checked by hand
//! from a scratch script until this file existed, which is how the floor came to
//! be dropping the one notification that matters.
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

/// Send REQUESTS to a fresh server and return everything it said, notifications
/// included. CACHE, when given, becomes its `XDG_CACHE_HOME` — which is how a
/// first run on a bare machine is staged.
fn talk(requests: &[Value], cache: Option<&Path>) -> Vec<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_org-semantic"));
    cmd.arg("serve").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    if let Some(c) = cache {
        cmd.env("XDG_CACHE_HOME", c);
    }
    let mut child = cmd.spawn().expect("spawning the binary Cargo just built");
    let mut input: Vec<u8> = requests.iter().flat_map(frame).collect();
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
    let msgs = talk(
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "index",
                    "params": { "vault": v, "mode": "lexical", "full": true } }),
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

// --------------------------------------------------------------- cancellation

/// Stopping a run that is already under way.
///
/// A signal rather than `$/cancelRequest`, because the server is inside the
/// index for the whole of it and does not read its next message until the
/// current one is answered — a cancellation over the pipe would arrive after
/// the thing it cancels.
///
/// Timed off the server's own first report rather than off a sleep: it says
/// when it has started, so nothing here guesses about whether the signal landed
/// mid-run. A sleep did guess, and guessed wrong — 3,000 notes index in under
/// half a second, which was less than the 400 ms it waited.
#[test]
#[cfg(unix)]
fn a_signal_stops_a_run_that_is_under_way() {
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
    stdin
        .write_all(&frame(&json!({ "jsonrpc": "2.0", "id": 7, "method": "index",
                                   "params": { "vault": v, "mode": "lexical", "full": true } })))
        .unwrap();
    stdin.flush().unwrap();

    let first = read_one(&mut stdout).expect("it reports before it finishes");
    assert_eq!(first["method"], "$/progress", "and that is the first thing it says: {first:?}");
    assert!(
        Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap().success(),
        "the server was there to be signalled"
    );

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

/// A signal that arrives while nothing is running belongs to nothing. Without
/// rearming, it would cancel whatever was asked for next.
#[test]
#[cfg(unix)]
fn a_signal_between_requests_does_not_poison_the_next_one() {
    let v = vault("rearm", 4);
    let mut child = Command::new(env!("CARGO_BIN_EXE_org-semantic"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();

    // Idle: nothing is in flight to cancel.
    std::thread::sleep(std::time::Duration::from_millis(150));
    Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));

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
