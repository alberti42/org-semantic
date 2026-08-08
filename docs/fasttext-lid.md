# fastText language identification — unresolved

**Status: abandoned mid-investigation, not concluded.** The tree is on `whichlang`.
Nothing here was committed beyond this note; the fastText work was reverted.

Read this before re-attempting `--lang auto` with fastText.

## What we wanted

`--lang auto` on `whichlang` mislabels ~4% of chunks (see CLAUDE.md, "Language
detection, and where it fails"). fastText's `lid.176` was expected to fix it
because it offers two things whichlang does not:

- `predict(text, k, threshold)` returns the **top-k labels with probabilities**,
  so the result can be filtered to an allowed set — `--lang auto:en,de` makes a
  French answer impossible, which is exactly the observed failure.
- Those probabilities give a **confidence threshold**, so a `$LSCOLORS` table can
  be answered with "no language" instead of a guess.

It is also what LanguageTool uses for language identification.

## Exactly what was done

- `fasttext = { version = "0.8", default-features = false }` (pure Rust; the
  `default` feature pulls a clap-based CLI).
- Model: `lid.176.ftz`, downloaded to `$XDG_CACHE_HOME/org-semantic/`, 938,013
  bytes, sha256 begins `8f3472cfe8738a7b6099e8e9`.
- `FastText::load_model(path)` — succeeded, no error.
- `model.predict(&text, 8, 0.0)`, newlines in `text` replaced with spaces.
- Labels arrive as `__label__xx`; the prefix was stripped.

## What it returned

```
"The damped oscillations of a trapped atom in an optical tweezer array"
     0.5392  __label__hi
     0.4579  __label__ar
"Die Wörter der deutschen Sprache sind manchmal sehr lang und kompliziert"
     0.9994  __label__ar
"hello world this is plain english text"
     0.3296  __label__ar
     0.2905  __label__hi
     0.2252  __label__sr
```

Across the 951-note vault: 3823 en, **1531 ar, 859 hi**, 36 zh, 35 eo, 34 id.
whichlang on the same corpus gave 4812 en, 1230 de, 260 fr — wrong in places but
recognisably related to the input.

## What was ruled out

- **Not a failed download.** File size matches the published 917 kB, and
  `load_model` parses it without error.
- **Not a constant output.** Different inputs give different answers with
  different confidences, so features are being computed and do vary.
- **Not a mis-parsed label table.** All 176 labels are present and well-formed.

## The context that matters most

`fasttext` 0.8.0 is a **recent pure-Rust rewrite**, not the long-established
crate:

| version | downloads | published |
|---|---|---|
| 0.8.0 (used here) | 24,465 | 2026-04-18 |
| 0.7.8 | 2,074,183 | 2023-09-11 |

The crate's reputation belongs to 0.7.8, a binding to the C++ library. The
rewrite is issue [#24](https://github.com/messense/fasttext-rs/issues/24) and
carries about 1% of the crate's usage. So "widely used, therefore we are holding
it wrong" is weaker than it looks — though still worth testing first.

Its own C++-validated reference tests (`VAL-INF-007` … `VAL-INF-011`,
"probability values match C++") all use the **cooking** model, which is
unquantized. No test in the suite exercises a quantized model against reference
values, and `lid.176.ftz` is product-quantized.

## Hypotheses, most likely first

1. **Product-quantization reconstruction is wrong.** In a `.ftz` the input matrix
   is centroid indices into per-subvector codebooks; reconstructing needs
   `nsubq`, `dsub` and the codebook stride read correctly. An error there leaves
   features looked up correctly but pointing at noise — matching the evidence
   that outputs vary with input yet mean nothing. The separately quantized row
   norms (`qnorm`) are a specific suspect: mishandling them gives *confidently*
   wrong answers, which is what German's `ar 0.9994` looks like.
2. **We are calling it wrong.** Not ruled out. Candidates: the C++ reads a line
   and appends `</s>`, so a trailing newline may be required for EOS; there may
   be a different entry point intended for supervised models; `predict_on_words`
   exists and was not tried.
3. **Tokenisation or n-gram hashing differs from C++.** Would affect every model,
   which the passing cooking-model reference tests make unlikely.

## What to do next, cheapest first

1. **Run the C++ `fasttext` CLI** on the same file and text —
   `fasttext predict lid.176.ftz -` — to establish ground truth. If C++ also
   returns `hi` for English, the model or download is at fault, not the crate.
   This is the cheapest discriminator and was never done.
2. **Try `lid.176.bin`** (125 MB, unquantized) through the same crate. Correct
   output confines the bug to the PQ path and makes a clean upstream report;
   incorrect output points at tokenisation and contradicts hypothesis 1.
3. **Check the crate's examples** for supervised-model prediction, and try
   appending `"\n"` to the input.
4. If the bug is confirmed, file it — the maintainers have a validation harness
   and appear only to lack a quantized fixture. Reproduction: `lid.176.ftz`,
   input `hello world this is plain english text`, expected `__label__en`,
   observed `__label__ar`.

## Alternatives if fastText is dropped

- **lingua** — `from_languages()` restricts the candidate set (kills the false
  French by construction) and exposes confidence with
  `with_minimum_relative_distance()`. Pure Rust, MIT, no runtime download.
  Cost: models are compile-time Cargo features, so the language set is fixed at
  build time, and the manifest has 98 entries (mostly those model crates).
- **whichlang** — where the tree currently is. Works, en/de correct, no
  restriction and no confidence, so the 4% stands. Mitigable per note with
  `# ltex: language=en-US`, which such notes want anyway.
- **fastText via the 0.7.8 C++ binding** — battle-tested, but reintroduces a C++
  build, which the project has otherwise avoided.

## Licence note, if fastText returns

`lid.176.*` is CC BY-SA 3.0 while this crate is MIT. The implementation
downloaded the model at first use rather than embedding it, so the model is
never redistributed here and ShareAlike never engages. Keep that property.
