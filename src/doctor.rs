//! What a vault has, and what is wrong with it.
//!
//! One report, built in one place, rendered three ways: printed by
//! `doctor <vault>`, returned as JSON by the same command with `--json`, and
//! returned to an editor by the `doctor` method.
//!
//! It answers a different question from `status`. `status` says what a vault
//! has, for a program deciding what to offer, and the save hook asks it on
//! every save; every field there is a fact to branch on. This says what is
//! wrong and what to do about it, for a person, and may cost more to find out.
//!
//! Every fact is optional and every `None` produces a finding. That is not
//! defensive: a broken vault is exactly when this is asked, so it has to
//! report on a vault whose notes cannot even be located.

use crate::*;

/// Something is wrong. A search answers with less, or with the wrong thing.
const PROBLEM: &str = "problem";
/// Worth knowing. Nothing is wrong.
const NOTICE: &str = "notice";

/// One thing the report says.
///
/// `kind` is what a client branches on and `message` is what a reader reads,
/// the same division the labelled failures already use. `remedy` is the
/// machine form, so no client parses prose to know which call to offer.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Finding {
    kind: &'static str,
    severity: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    remedy: Option<&'static str>,
    /// The file to open, when the remedy is to edit one. No line number: the
    /// message already carries it, and reporting it separately would mean
    /// giving the parser a structured error for one offer.
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    /// Which index this is about, where only one of them is.
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'static str>,
    /// The model to fetch, when the remedy is to download one. A client
    /// cannot take it from the sentence, and a vault may have an index under
    /// more than one model.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

/// One semantic index, which is one model's.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SemanticIndex {
    model: String,
    /// Notes it covers, from the manifest, so nothing large is read.
    notes: usize,
    /// Whether this binary can read the layout it was written under.
    readable: bool,
    /// Whether the model that built it is still downloaded here.
    cached: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LexicalIndex {
    /// The languages it stems, read back from the index rather than from the
    /// corpus, so this is what a query is actually analyzed against.
    ///
    /// Empty when this binary cannot read the stored analyzer, which is
    /// reported as a finding of its own.
    languages: Vec<String>,
    /// Whether accents are folded, so `eleves` matches `élèves`.
    folding: bool,
    notes: usize,
}

/// One of the three files a vault may hold, and what became of it.
///
/// `state` is one of `absent`, `read` or `unreadable`, said in one word so a
/// client renders it without knowing which file it is.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigFile {
    name: &'static str,
    state: &'static str,
    /// Where it was found. Absent when there is no such file.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// What it holds, in that file's own terms: settings, rules, or where the
    /// notes are. Absent when the file is.
    #[serde(skip_serializing_if = "Option::is_none")]
    holds: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Downloads {
    models: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_bytes: Option<u64>,
    classifier: String,
    classifier_present: bool,
}

/// Everything the doctor found, facts and findings together.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    vault: String,
    /// Where the notes are. Absent when that cannot be worked out, which is
    /// itself a finding.
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    state: String,
    /// `.org` files the walk yields for the semantic index, after exclusions.
    #[serde(skip_serializing_if = "Option::is_none")]
    notes_found: Option<usize>,
    semantic: Vec<SemanticIndex>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lexical: Option<LexicalIndex>,
    files: Vec<ConfigFile>,
    downloads: Downloads,
    /// Worst first, and problems before notices.
    findings: Vec<Finding>,
}

impl Report {
    /// Everything else in this file exists to fill this in.
    pub fn on(vault: &Path) -> Report {
        let state = state_dir(vault);
        let mut f: Vec<Finding> = Vec::new();

        // Resolved first, because almost nothing can be checked without it.
        // A vault whose notes cannot be located reports that and goes on: the
        // two indexes and the downloads are still worth saying.
        let notes = match notes_root(vault) {
            Ok(n) => Some(n),
            Err(e) => {
                f.push(Finding {
                    kind: "notes-unknown",
                    severity: PROBLEM,
                    message: format!("{e:#}"),
                    remedy: Some("edit"),
                    file: Some(vault_file(vault).display().to_string()),
                    target: None,
                    model: None,
                });
                None
            }
        };

        let excludes = notes.as_ref().map(|n| Excludes::read(vault, n));
        let policy = notes.as_ref().map(|n| resolve_config(vault, n, None));
        let files = describe_files(vault, notes.as_deref(), &excludes, &policy);
        report_unreadable_files(&mut f, vault, notes.as_deref(), &excludes, &policy);

        // The walk needs the rules, so a broken exclusion list leaves the
        // count unknown rather than reporting a number under no rules at all.
        let notes_found = match (&notes, &excludes) {
            (Some(_), Some(Ok(_))) => match walk_notes(vault, Target::Semantic) {
                Ok(w) => Some(w.files.len()),
                Err(_) => None,
            },
            _ => None,
        };
        if notes_found == Some(0) {
            f.push(Finding {
                kind: "no-notes",
                severity: PROBLEM,
                message: match &notes {
                    Some(n) => format!("no .org files under {}", n.display()),
                    None => "no .org files to index".to_string(),
                },
                remedy: None,
                file: None,
                target: None,
                model: None,
            });
        }

        let semantic = describe_semantic(vault, &mut f);
        let lexical = describe_lexical(&state, &mut f);
        // Not said for a vault with no notes: there is nothing to index, so
        // "run index" would be advice that cannot work, beside the finding
        // that already explains why.
        if semantic.is_empty() && lexical.is_none() && notes_found != Some(0) {
            f.push(Finding {
                kind: "no-index",
                severity: PROBLEM,
                message: "nothing is built for this vault, so no search can answer".to_string(),
                remedy: Some("index"),
                file: None,
                target: None,
                model: None,
            });
        }
        check_drift(vault, &state, &excludes, &policy, &semantic, &lexical, &mut f);
        report_running(vault, &state, &mut f);

        // Problems before notices, and each group in the order it was found,
        // which is the order the checks run in and therefore stable.
        f.sort_by_key(|x| x.severity != PROBLEM);
        Report {
            vault: vault.display().to_string(),
            notes: notes.as_ref().map(|n| n.display().to_string()),
            state: state.display().to_string(),
            notes_found,
            semantic,
            lexical,
            files,
            downloads: describe_downloads(),
            findings: f,
        }
    }

    /// Whether anything is wrong, which is what an exit status reports.
    pub fn healthy(&self) -> bool {
        !self.findings.iter().any(|x| x.severity == PROBLEM)
    }
}

/// The three files a vault may hold, always all three, in a fixed order.
///
/// Absent is stated rather than left out: two of the three are optional and a
/// reader checking why a setting has no effect needs to see that the file is
/// not there.
fn describe_files(
    vault: &Path,
    notes: Option<&Path>,
    excludes: &Option<Result<Excludes>>,
    policy: &Option<Result<Config>>,
) -> Vec<ConfigFile> {
    let found = |name: &str| notes.and_then(|n| user_file(vault, n, name));
    let mut out = Vec::new();

    out.push(match (found(CONFIG_FILE), policy) {
        (Some(p), Some(Ok(_))) => ConfigFile {
            name: CONFIG_FILE,
            state: "read",
            holds: Some(match settings_set(&p) {
                Some(1) => "1 setting".to_string(),
                Some(n) => format!("{n} settings"),
                None => "the defaults".to_string(),
            }),
            path: Some(p.display().to_string()),
        },
        (Some(p), _) => ConfigFile {
            name: CONFIG_FILE,
            state: "unreadable",
            holds: None,
            path: Some(p.display().to_string()),
        },
        (None, _) => ConfigFile { name: CONFIG_FILE, state: "absent", holds: None, path: None },
    });

    out.push(match (found(IGNORE_FILE), excludes) {
        (Some(p), Some(Ok(ex))) => ConfigFile {
            name: IGNORE_FILE,
            state: "read",
            holds: Some(match ex.len() {
                1 => "1 rule".to_string(),
                n => format!("{n} rules"),
            }),
            path: Some(p.display().to_string()),
        },
        (Some(p), _) => ConfigFile {
            name: IGNORE_FILE,
            state: "unreadable",
            holds: None,
            path: Some(p.display().to_string()),
        },
        (None, _) => ConfigFile { name: IGNORE_FILE, state: "absent", holds: None, path: None },
    });

    // This one is read only from beside the index, never from the notes: it is
    // what says where the notes are, so looking for it there would be circular.
    let said = vault_file(vault);
    out.push(if !said.exists() {
        ConfigFile { name: VAULT_FILE, state: "absent", holds: None, path: None }
    } else {
        match notes {
            Some(n) if n != vault => ConfigFile {
                name: VAULT_FILE,
                state: "read",
                holds: Some(format!("notes in {}", n.display())),
                path: Some(said.display().to_string()),
            },
            Some(_) => ConfigFile {
                name: VAULT_FILE,
                state: "read",
                holds: Some("nothing about where the notes are".to_string()),
                path: Some(said.display().to_string()),
            },
            None => ConfigFile {
                name: VAULT_FILE,
                state: "unreadable",
                holds: None,
                path: Some(said.display().to_string()),
            },
        }
    });
    out
}

/// How many settings a policy file states, by counting its top-level keys.
///
/// The parsed `Config` cannot answer this: every key it lacks reads as its
/// default, so a file stating one setting and a file stating none are the same
/// value. What a reader wants to know is how much they wrote down.
fn settings_set(path: &Path) -> Option<usize> {
    let text = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    match v.as_object().map(|o| o.len()) {
        Some(0) | None => None,
        n => n,
    }
}

/// A configuration file this binary cannot read.
///
/// One finding each, and the remedy is to edit the file, so it carries the
/// path. The message is the whole chain, which names the file and the line.
fn report_unreadable_files(
    f: &mut Vec<Finding>,
    vault: &Path,
    notes: Option<&Path>,
    excludes: &Option<Result<Excludes>>,
    policy: &Option<Result<Config>>,
) {
    let mut say = |kind: &'static str, name: &str, e: &anyhow::Error| {
        f.push(Finding {
            kind,
            severity: PROBLEM,
            message: format!("{e:#}"),
            remedy: Some("edit"),
            file: notes.and_then(|n| user_file(vault, n, name)).map(|p| p.display().to_string()),
            target: None,
            model: None,
        });
    };
    if let Some(Err(e)) = excludes {
        say("exclude-unreadable", IGNORE_FILE, e);
    }
    if let Some(Err(e)) = policy {
        say("config-unreadable", CONFIG_FILE, e);
    }
}

/// Each model with an index, and what is wrong with it.
///
/// A manifest that will not parse is a layout this binary cannot read, which
/// is what `--full` is for; the message says so rather than naming serde.
fn describe_semantic(vault: &Path, f: &mut Vec<Finding>) -> Vec<SemanticIndex> {
    let mut out = Vec::new();
    for m in built_models(vault) {
        let dir = semantic_dir(vault, m);
        let stored: Option<Manifest> = stored_hash(&dir.join("manifest.json"));
        let readable = stored.as_ref().is_some_and(|s| s.version == INDEX_VERSION);
        let cached = weights_cached(m);
        if !readable {
            f.push(Finding {
                kind: "index-layout",
                severity: PROBLEM,
                message: format!(
                    "the {} index was written by another version of org-semantic and \
                     cannot be read",
                    m.name
                ),
                remedy: Some("reindex-full"),
                file: None,
                target: Some("semantic"),
                model: None,
            });
        }
        if !cached {
            f.push(Finding {
                kind: "model-missing",
                severity: PROBLEM,
                message: format!(
                    "{} has an index here and is not downloaded, so no search by meaning \
                     can answer",
                    m.name
                ),
                remedy: Some("download"),
                file: None,
                target: Some("semantic"),
                model: Some(m.name.to_string()),
            });
        }
        out.push(SemanticIndex {
            model: m.name.to_string(),
            notes: stored.as_ref().map(|s| s.files.len()).unwrap_or(0),
            readable,
            cached,
        });
    }
    out
}

/// The word index, and the analyzer it will answer a query with.
///
/// The stored key is not reported as it is stored. It reads `v6 langs=de+en
/// fold=false`, which is a version number and two encodings in one string —
/// written for the code that compares it, not for a reader.
fn describe_lexical(state: &Path, f: &mut Vec<Finding>) -> Option<LexicalIndex> {
    let key = lexical::stored_key(state)?;
    let stored: Option<LexManifest> = stored_hash(&lex_manifest_path(state));
    let analyzer = lexical::Analyzer::from_key(&key);
    if analyzer.is_none() {
        f.push(Finding {
            kind: "index-layout",
            severity: PROBLEM,
            message: "the word index was written by another version of org-semantic and \
                      cannot be read"
                .to_string(),
            remedy: Some("reindex-full"),
            file: None,
            target: Some("lexical"),
            model: None,
        });
    }
    Some(LexicalIndex {
        languages: analyzer.as_ref().map(|a| a.langs.clone()).unwrap_or_default(),
        folding: analyzer.as_ref().is_some_and(|a| a.fold),
        notes: stored.as_ref().map(|s| s.files.len()).unwrap_or(0),
    })
}

/// An index built under a policy or an exclusion list that has since changed.
///
/// The layout is asked first and the hash only for an index this binary can
/// read, which is the rule `recorded_policy` exists for: an out-of-date index
/// must not be reported as an edit the reader never made.
#[allow(clippy::too_many_arguments)]
fn check_drift(
    vault: &Path,
    state: &Path,
    excludes: &Option<Result<Excludes>>,
    policy: &Option<Result<Config>>,
    semantic: &[SemanticIndex],
    lexical: &Option<LexicalIndex>,
    f: &mut Vec<Finding>,
) {
    let Some(Ok(ex)) = excludes else { return };
    let Some(Ok(cfg)) = policy else { return };
    // Two sentences and not one with a hole in it. They are different facts
    // with different costs: a changed policy re-embeds the corpus, a changed
    // exclusion list embeds nothing. Both borrow the wording already used by
    // `check_config` and by the warning a search prints.
    let policy_moved = |target: &'static str, f: &mut Vec<Finding>| {
        f.push(Finding {
            kind: "config-drift",
            severity: PROBLEM,
            message: format!(
                "the {target} index was built under a different policy than {CONFIG_FILE} \
                 now holds"
            ),
            remedy: Some("reindex-full"),
            file: None,
            target: Some(target),
            model: None,
        });
    };
    let list_moved = |target: &'static str, f: &mut Vec<Finding>| {
        f.push(Finding {
            kind: "exclude-drift",
            severity: PROBLEM,
            message: format!(
                "the {target} index was built with a different exclusion list, so searches \
                 follow the old list"
            ),
            remedy: Some("index"),
            file: None,
            target: Some(target),
            model: None,
        });
    };
    for s in semantic.iter().filter(|s| s.readable) {
        let Ok(m) = model_named(&s.model) else { continue };
        let dir = semantic_dir(vault, m);
        if recorded_policy(&dir) != Some(cfg.hash_for(Target::Semantic)) {
            policy_moved("semantic", f);
        }
        if stored_hash::<Manifest>(&dir.join("manifest.json"))
            .is_some_and(|s| s.exclude != ex.hash(Target::Semantic))
        {
            list_moved("semantic", f);
        }
    }
    // Asked only for an index this binary can read, which is what
    // `recorded_lex_policy` answers `None` for: an out-of-date index must not
    // be reported as an edit the reader never made.
    if recorded_lex_policy(state).is_some() {
        if recorded_lex_policy(state) != Some(cfg.hash_for(Target::Lexical)) {
            policy_moved("lexical", f);
        }
        if stored_hash::<LexManifest>(&lex_manifest_path(state))
            .is_some_and(|s| s.exclude != ex.hash(Target::Lexical))
        {
            list_moved("lexical", f);
        }
    }
    let _ = lexical;
}

/// A run in flight, or a lock nobody owns.
///
/// Both are notices. A run finishes on its own, and a stale lock is taken over
/// by the next run rather than refusing it — so neither is anything to do,
/// and both explain a report that looks wrong for a minute.
fn report_running(vault: &Path, state: &Path, f: &mut Vec<Finding>) {
    let lock = state.join("index.lock");
    if !lock.exists() {
        return;
    }
    if being_indexed(vault) {
        f.push(Finding {
            kind: "indexing",
            severity: NOTICE,
            message: "an index of this vault is running now, here or in another process"
                .to_string(),
            remedy: Some("wait"),
            file: None,
            target: None,
            model: None,
        });
    } else {
        f.push(Finding {
            kind: "stale-lock",
            severity: NOTICE,
            message: format!(
                "{} was left by a run that did not finish; the next run takes it over",
                lock.display()
            ),
            remedy: None,
            file: None,
            target: None,
            model: None,
        });
    }
}

/// Where the gigabytes are. The one thing about a vault's search that no file
/// in the vault says.
fn describe_downloads() -> Downloads {
    let models = cache_dir();
    Downloads {
        model_bytes: MODELS.iter().filter_map(weight_bytes).sum::<u64>().into(),
        models: models.display().to_string(),
        classifier: lid_path().display().to_string(),
        classifier_present: lid_path().exists(),
    }
}

/// The report as a person reads it.
///
/// Facts first, then what is wrong. A reader who came here because something
/// is broken still needs the facts to make sense of the finding, which is why
/// this is not a list of problems alone.
pub fn print(r: &Report, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "Vault  {}", r.vault)?;
    match &r.notes {
        Some(n) if *n != r.vault => writeln!(out, "Notes  {n}")?,
        Some(_) => {}
        None => writeln!(out, "Notes  not known — see below")?,
    }
    writeln!(out, "Index  {}", r.state)?;
    if let Some(n) = r.notes_found {
        writeln!(out, "Found  {n} .org file{}", plural(n))?;
    }

    writeln!(out, "\nBuilt")?;
    if r.semantic.is_empty() && r.lexical.is_none() {
        writeln!(out, "  nothing")?;
    }
    for s in &r.semantic {
        writeln!(
            out,
            "  semantic  {:<14}  {} note{}{}{}",
            s.model,
            s.notes,
            plural(s.notes),
            if s.readable { "" } else { "  unreadable" },
            if s.cached { "" } else { "  model not downloaded" }
        )?;
    }
    if let Some(l) = &r.lexical {
        let langs =
            if l.languages.is_empty() { "unreadable".to_string() } else { l.languages.join(", ") };
        writeln!(
            out,
            "  lexical   {:<14}  {} note{}{}",
            langs,
            l.notes,
            plural(l.notes),
            if l.folding { "  accents folded" } else { "" }
        )?;
    }

    writeln!(out, "\nConfiguration")?;
    for c in &r.files {
        match (&c.holds, c.state) {
            (Some(h), _) => writeln!(out, "  {:<26}  {h}", c.name)?,
            (None, "absent") => writeln!(out, "  {:<26}  absent", c.name)?,
            (None, _) => writeln!(out, "  {:<26}  will not read", c.name)?,
        }
    }

    writeln!(out, "\nDownloads")?;
    match r.downloads.model_bytes {
        Some(0) | None => writeln!(out, "  models      {}  (nothing yet)", r.downloads.models)?,
        Some(b) => writeln!(out, "  models      {}  ({})", r.downloads.models, human_bytes(b))?,
    }
    writeln!(
        out,
        "  classifier  {}  ({})",
        r.downloads.classifier,
        if r.downloads.classifier_present { "downloaded" } else { "nothing yet" }
    )?;

    // Two headings and never one list, because a reader acts on the first
    // group and only reads the second.
    for (severity, heading) in [(PROBLEM, "Problems"), (NOTICE, "Worth knowing")] {
        let group: Vec<&Finding> = r.findings.iter().filter(|x| x.severity == severity).collect();
        if group.is_empty() {
            continue;
        }
        writeln!(out, "\n{heading}")?;
        for x in group {
            writeln!(out, "  {}", x.message.replace('\n', "\n  "))?;
            // The remedy is spelled for *this* reader, who has a command line.
            // Over the wire it stays the machine word, and the editor draws a
            // key: no message names a flag unless its caller can type one.
            if let Some(line) = spelled(x, &r.vault) {
                writeln!(out, "    {line}")?;
            }
        }
    }
    if r.healthy() {
        writeln!(out, "\nNothing to report.")?;
    }
    Ok(())
}

/// What to do about one finding, in the terminal's own words.
///
/// `None` where there is nothing to do. `wait` is one of those: a run finishes
/// on its own, and telling somebody to wait is not a remedy.
fn spelled(x: &Finding, vault: &str) -> Option<String> {
    match x.remedy? {
        "index" => Some(format!("run: org-semantic index {vault} --both")),
        "reindex-full" => Some(format!("run: org-semantic index {vault} --both --full")),
        // The CLI downloads a model rather than refusing, so indexing is how a
        // terminal gets one; `serve` is the only caller that has to ask.
        "download" => Some(format!("run: org-semantic index {vault}")),
        "edit" => Some(format!("edit {}", x.file.as_deref().unwrap_or("the file named above"))),
        _ => None,
    }
}

/// The printed report as a string, for a test that reads what a person reads.
///
/// Asserting on the rendering and not on the struct is deliberate: the fields
/// are only ever seen through one of the three renderings, and a field that is
/// right and unprinted is not a report.
#[cfg(test)]
pub fn rendered(r: &Report) -> String {
    let mut out = Vec::new();
    print(r, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}
