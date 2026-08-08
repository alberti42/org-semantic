# fastText language identification — why the version is pinned

**Resolved.** `--lang auto` uses fastText's `lid.176` through the `fasttext`
crate, pinned to the **0.7** line. Read this before changing that pin.

## The finding

`fasttext` 0.8.0 returns confident nonsense for product-quantized models.
`lid.176.ftz` is product-quantized. 0.7.x, which binds the C++ library, is
correct on the same file.

Same model, same sha256 `8f3472cf…`, same inputs, same `predict(text, k, thr)`
call:

| input | C++ CLI | crate 0.7.8 | crate 0.8.0 |
|---|---|---|---|
| `The damped oscillations of a trapped atom…` | `en 0.418` | `en 0.443` | **`hi 0.539`** |
| `Die Wörter der deutschen Sprache…` | `de 0.996` | `de 0.997` | **`ar 0.999`** |
| `hello world this is plain english text` | `en 0.828` | `en 0.852` | **`ar 0.330`** |

Over the 951-note vault, 0.8.0 produced 1531 `ar` and 859 `hi`. The German case
is the tell: wrong, and at p = 0.999.

The small CLI-vs-binding differences are the `</s>` token — the CLI reads a
*line* and appends end-of-sentence, the binding predicts on a bare string.

## Why this is worth a written note

**The two crates take the same arguments.** `FastText::new()`,
`load_model(&str)`, `predict(text, k, threshold)` — identical signatures. Bumping
0.7 → 0.8 compiles without a warning and silently poisons every language
assignment in the index, which then stems each chunk in the wrong language.
Nothing fails loudly.

It is also a natural mistake to make, because the version numbers suggest a
routine upgrade when the reality is a rewrite:

| version | downloads | published | implementation |
|---|---|---|---|
| 0.7.8 | 2,074,183 | 2023-09-11 | C++ binding |
| 0.8.0 | 24,465 | 2026-04-18 | pure Rust |

The crate's reputation belongs to 0.7.8. The rewrite is issue
[#24](https://github.com/messense/fasttext-rs/issues/24) and carries about 1% of
the usage.

## Where the bug most likely is

The reference tests that validate against C++ (`VAL-INF-007` … `VAL-INF-011`,
"probability values match C++") all use the **cooking** model, which is
unquantized. No test exercises a quantized model against reference values.

That points at product-quantization reconstruction: in a `.ftz` the input matrix
is centroid indices into per-subvector codebooks, and reconstructing needs
`nsubq`, `dsub` and the codebook stride read correctly. An error there leaves
features looked up correctly but pointing at noise — which matches outputs that
vary with the input yet mean nothing. The separately quantized row norms
(`qnorm`) are the specific suspect: mishandling them yields *confidently* wrong
answers, which is what `ar 0.999` looks like.

Two hypotheses that the C++ comparison **ruled out**: a bad download or model
(the same file is correct through the CLI), and misuse of the API (0.7.8 takes
the identical call and is correct).

Worth reporting upstream. The maintainers have a validation harness and appear
only to lack a quantized fixture. Reproduction: `lid.176.ftz`, input
`hello world this is plain english text`, expected `__label__en`, observed
`__label__ar`.

## What this costs

`fasttext` 0.7.8 pulls `cfasttext-sys`, which compiles the vendored C++ — about
10 s, four crates (`cc`, `cfasttext-sys`, `find-msvc-tools`, `shlex`), and a C++
toolchain at build time. No runtime dependency; it links statically, as `ort`
already does for the ONNX runtime.

## Licence

`lid.176.*` is CC BY-SA 3.0 while this crate is MIT. The model is downloaded on
first use rather than vendored, so it is never redistributed here and ShareAlike
never engages. **Keep that property.**
