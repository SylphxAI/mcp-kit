//! Local text and code embeddings with a small static model (model2vec).
//!
//! A static model maps each token to a fixed vector, so a text's embedding is
//! the mean of its tokens' vectors, normalized. Embedding a whole repository
//! takes a moment on a CPU and needs no GPU, no server and no API key.
//!
//! The model is downloaded once from Hugging Face at a pinned revision,
//! checked against its SHA-256, stored as int8 (a quarter of the size) and
//! used offline afterwards. It lives in one cache shared by every Sylphx tool
//! (`~/.cache/sylphx/models`, or `SYLPHX_MODEL_DIR`).
//!
//! The tokenizer is the model's own: BERT normalization (clean text, CJK
//! spacing, lowercase, strip accents), BERT pre-tokenization (whitespace and
//! punctuation) and WordPiece; unknown words are dropped and texts are cut to
//! 512 tokens, as in `model2vec`'s `encode`. The unit tests check this against
//! vectors computed by `model2vec` itself.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use unicode_normalization::UnicodeNormalization;

/// A model on Hugging Face, pinned to one revision and verified by hash.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// Short name, used for the cache folder and shown to users.
    pub id: &'static str,
    pub repo: &'static str,
    pub revision: &'static str,
    pub weights_sha256: &'static str,
    pub weights_bytes: u64,
    pub tokenizer_sha256: &'static str,
}

/// Code retrieval (distilled from CodeRankEmbed, 256 dimensions, MIT).
/// About 3× better than general models on code search in the CoIR benchmark.
pub const POTION_CODE_16M: Spec = Spec {
    id: "potion-code-16M-v2",
    repo: "minishlab/potion-code-16M-v2",
    revision: "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
    weights_sha256: "75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c",
    weights_bytes: 32_490_072,
    tokenizer_sha256: "107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45",
};

/// General English retrieval (distilled from bge-base-en-v1.5, 512 dimensions, MIT).
pub const POTION_RETRIEVAL_32M: Spec = Spec {
    id: "potion-retrieval-32M",
    repo: "minishlab/potion-retrieval-32M",
    revision: "6fc8051fab2a1e0ee76689cf08c853792ac285e7",
    weights_sha256: "07609e5bd33aad37900b3fd62f4ec96f6daec88ca4d46b9d8b928bfababf6ea0",
    weights_bytes: 129_210_456,
    tokenizer_sha256: "",
};

/// Tokens per text, as `model2vec`'s default `max_length`.
pub const MAX_TOKENS: usize = 512;
const MAX_WORD_CHARS: usize = 100;
/// After a failed download, wait this long before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(3600);

/// Where models are cached: `SYLPHX_MODEL_DIR`, else `<cache>/sylphx/models`.
pub fn models_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SYLPHX_MODEL_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("sylphx").join("models")
}

fn model_dir(spec: &Spec) -> PathBuf {
    models_dir().join(spec.id)
}

/// Is the model downloaded and converted?
pub fn installed(spec: &Spec) -> bool {
    let d = model_dir(spec);
    d.join("model.q8").is_file() && d.join("vocab.txt").is_file()
}

/// Download, verify and convert the model if it is missing. Before
/// downloading it prints one line to stderr: `<app>: downloading …`, ending
/// with `hint` (e.g. how to turn embeddings off). After a failure it does not
/// try again for an hour, so offline runs stay fast.
pub fn ensure(spec: &Spec, app: &str, hint: &str) -> Result<()> {
    if installed(spec) {
        return Ok(());
    }
    let dir = model_dir(spec);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let failed = dir.join("download-failed");
    if let Ok(t) = std::fs::metadata(&failed).and_then(|m| m.modified()) {
        if SystemTime::now().duration_since(t).unwrap_or_default() < RETRY_AFTER {
            bail!("the last download failed less than an hour ago: {}", std::fs::read_to_string(&failed).unwrap_or_default().trim());
        }
    }
    eprintln!(
        "{app}: downloading the embedding model {} ({} MB, once) from huggingface.co to {}. {hint}",
        spec.id,
        spec.weights_bytes.div_ceil(1_000_000),
        dir.display()
    );
    let r = download(spec, &dir);
    match &r {
        Ok(()) => {
            let _ = std::fs::remove_file(&failed);
        }
        Err(e) => {
            let _ = std::fs::write(&failed, format!("{e:#}\n"));
        }
    }
    r
}

fn download(spec: &Spec, dir: &Path) -> Result<()> {
    let base = std::env::var("SYLPHX_MODEL_URL")
        .map(|b| format!("{}/{}", b.trim_end_matches('/'), spec.id))
        .unwrap_or_else(|_| format!("https://huggingface.co/{}/resolve/{}", spec.repo, spec.revision));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(900)))
        .user_agent(concat!("sylphx-mcp-kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let fetch = |name: &str, limit: u64, sha: &str| -> Result<Vec<u8>> {
        let mut res = agent.get(&format!("{base}/{name}")).call().with_context(|| format!("downloading {name}"))?;
        let mut bytes = Vec::new();
        res.body_mut().with_config().limit(limit + 1).reader().read_to_end(&mut bytes).with_context(|| format!("downloading {name}"))?;
        let got = hex(&Sha256::digest(&bytes));
        if !sha.is_empty() && got != sha {
            bail!("{name} failed verification (sha256 {got}, {} bytes)", bytes.len());
        }
        Ok(bytes)
    };
    let tok = fetch("tokenizer.json", 64 << 20, spec.tokenizer_sha256)?;
    let weights = fetch("model.safetensors", spec.weights_bytes, spec.weights_sha256)?;
    if weights.len() as u64 != spec.weights_bytes {
        bail!("model.safetensors has {} bytes, expected {}", weights.len(), spec.weights_bytes);
    }
    convert(&tok, &weights, dir)
}

/// Write `vocab.txt` and `model.q8` (int8 rows with one scale each) from a
/// model2vec `tokenizer.json` and `model.safetensors`.
pub fn convert(tokenizer_json: &[u8], safetensors: &[u8], dir: &Path) -> Result<()> {
    let t: serde_json::Value = serde_json::from_slice(tokenizer_json).context("reading tokenizer.json")?;
    if t.pointer("/model/type").and_then(|v| v.as_str()) != Some("WordPiece") {
        bail!("only WordPiece tokenizers are supported");
    }
    let vocab = t.pointer("/model/vocab").and_then(|v| v.as_object()).context("tokenizer.json has no vocab")?;
    let mut rows: Vec<(&String, u64)> = vocab.iter().filter_map(|(k, v)| v.as_u64().map(|i| (k, i))).collect();
    rows.sort_by_key(|r| r.1);
    if rows.iter().enumerate().any(|(i, r)| r.1 != i as u64) {
        bail!("tokenizer ids are not contiguous");
    }
    let (dims, data) = parse_safetensors(safetensors)?;
    if data.len() / dims != rows.len() {
        bail!("model has {} rows but the vocab has {}", data.len() / dims, rows.len());
    }
    let n = rows.len();
    let mut out = Vec::with_capacity(8 + n * 4 + n * dims);
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&(dims as u32).to_le_bytes());
    let mut q = Vec::with_capacity(n * dims);
    for r in 0..n {
        let row = &data[r * dims..(r + 1) * dims];
        let (qs, s) = quantize_row(row);
        out.extend_from_slice(&s.to_le_bytes());
        q.extend(qs.iter().map(|x| *x as u8));
    }
    out.extend_from_slice(&q);
    std::fs::create_dir_all(dir)?;
    let words: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    std::fs::write(dir.join("vocab.txt"), words.join("\n"))?;
    std::fs::write(dir.join("model.q8.part"), &out)?;
    std::fs::rename(dir.join("model.q8.part"), dir.join("model.q8"))?;
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn parse_safetensors(bytes: &[u8]) -> Result<(usize, Vec<f32>)> {
    let hlen = u64::from_le_bytes(bytes.get(..8).context("short file")?.try_into()?) as usize;
    let header: serde_json::Value = serde_json::from_slice(bytes.get(8..8 + hlen).context("short header")?)?;
    let t = header.get("embeddings").context("no embeddings tensor")?;
    let dims = t.pointer("/shape/1").and_then(|d| d.as_u64()).context("no shape")? as usize;
    let off = |i: &str| t.pointer(i).and_then(|d| d.as_u64()).map(|o| 8 + hlen + o as usize).context("no offsets");
    let raw = bytes.get(off("/data_offsets/0")?..off("/data_offsets/1")?).context("short data")?;
    let data = match t.get("dtype").and_then(|d| d.as_str()) {
        Some("F32") => raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect(),
        Some("F16") => raw.as_chunks::<2>().0.iter().map(|c| f16_to_f32(u16::from_le_bytes(*c))).collect(),
        other => bail!("unsupported dtype {other:?}"),
    };
    Ok((dims, data))
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = match (exp, frac) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: normalize.
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((f & 0x3ff) << 13)
        }
        (0x1f, _) => sign | 0x7f80_0000 | (frac << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}

fn quantize_row(row: &[f32]) -> (Vec<i8>, f32) {
    let max = row.iter().fold(0f32, |m, x| m.max(x.abs())).max(1e-12);
    let s = max / 127.0;
    (row.iter().map(|x| (x / s).round().clamp(-127.0, 127.0) as i8).collect(), s)
}

type Vocab = HashMap<String, u32, std::hash::BuildHasherDefault<Fx>>;

/// A small fast hash for short vocabulary keys (the Firefox/rustc FxHash).
#[derive(Default)]
struct Fx(u64);

impl std::hash::Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut b = [0u8; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(b)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }
    fn write_u8(&mut self, i: u8) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// A loaded static embedding model.
pub struct Model {
    /// Word-initial pieces and `##` continuation pieces (stored without `##`).
    first: Vocab,
    cont: Vocab,
    dims: usize,
    /// Row-major int8 weights (stored as bytes) and one scale per row.
    q: Vec<u8>,
    scale: Vec<f32>,
    /// `model2vec` cuts a text to MAX_TOKENS × this many characters first.
    median_token_chars: usize,
}

impl Model {
    /// Load an installed model.
    pub fn load(spec: &Spec) -> Result<Model> {
        Model::load_dir(&model_dir(spec))
    }

    pub fn load_dir(dir: &Path) -> Result<Model> {
        let path = dir.join("model.q8");
        let mut f = std::fs::File::open(&path).context("reading model.q8")?;
        let len = f.metadata()?.len() as usize;
        let mut head = [0u8; 8];
        f.read_exact(&mut head).context("model.q8 is empty")?;
        let n = u32::from_le_bytes(head[0..4].try_into()?) as usize;
        let dims = u32::from_le_bytes(head[4..8].try_into()?) as usize;
        if len != 8 + n * 4 + n * dims {
            bail!("model.q8 is truncated");
        }
        let mut sb = vec![0u8; n * 4];
        f.read_exact(&mut sb)?;
        let scale: Vec<f32> = sb.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        let mut q = Vec::with_capacity(n * dims);
        f.read_to_end(&mut q)?;
        let words = std::fs::read_to_string(dir.join("vocab.txt")).context("reading vocab.txt")?;
        let words: Vec<&str> = words.split('\n').collect();
        if words.len() != n {
            bail!("vocab.txt has {} words, the model {n} rows", words.len());
        }
        Ok(Model::from_parts(&words, dims, q, scale))
    }

    fn from_parts(words: &[&str], dims: usize, q: Vec<u8>, scale: Vec<f32>) -> Model {
        let mut first = Vocab::with_capacity_and_hasher(words.len(), Default::default());
        let mut cont = Vocab::with_capacity_and_hasher(words.len() / 4, Default::default());
        let mut lens: Vec<usize> = Vec::with_capacity(words.len());
        for (i, w) in words.iter().enumerate() {
            lens.push(w.chars().count());
            match w.strip_prefix("##") {
                Some(rest) if !rest.is_empty() => cont.insert(rest.to_string(), i as u32),
                _ => first.insert(w.to_string(), i as u32),
            };
        }
        lens.sort_unstable();
        // numpy's median of an even count is the mean of the middle two; int() floors it.
        let median_token_chars = match lens.len() {
            0 => 1,
            l if l % 2 == 1 => lens[l / 2],
            l => (lens[l / 2 - 1] + lens[l / 2]) / 2,
        };
        Model { first, cont, dims, q, scale, median_token_chars: median_token_chars.max(1) }
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Token ids of a text (unknown words dropped, at most `MAX_TOKENS`).
    pub fn tokenize(&self, text: &str) -> Vec<u32> {
        let cut = MAX_TOKENS * self.median_token_chars;
        let text = match text.char_indices().nth(cut) {
            Some((i, _)) => &text[..i],
            None => text,
        };
        let mut ids = Vec::new();
        let mut word = String::new();
        let flush = |word: &mut String, ids: &mut Vec<u32>| {
            if !word.is_empty() {
                self.wordpiece(word, ids);
                word.clear();
            }
        };
        for c in normalize(text) {
            if c.is_whitespace() {
                flush(&mut word, &mut ids);
            } else if is_punct(c) {
                flush(&mut word, &mut ids);
                word.push(c);
                flush(&mut word, &mut ids);
            } else {
                word.push(c);
            }
            if ids.len() >= MAX_TOKENS {
                break;
            }
        }
        flush(&mut word, &mut ids);
        ids.truncate(MAX_TOKENS);
        ids
    }

    fn wordpiece(&self, word: &str, ids: &mut Vec<u32>) {
        if word.chars().count() > MAX_WORD_CHARS {
            return;
        }
        let start_len = ids.len();
        let mut start = 0;
        while start < word.len() {
            let map = if start == 0 { &self.first } else { &self.cont };
            let mut end = word.len();
            let mut found = None;
            while end > start {
                if word.is_char_boundary(end) {
                    if let Some(id) = map.get(&word[start..end]) {
                        found = Some(*id);
                        break;
                    }
                }
                end -= 1;
            }
            match found {
                Some(id) => {
                    ids.push(id);
                    start = end;
                }
                None => {
                    // The whole word is [UNK], which model2vec drops.
                    ids.truncate(start_len);
                    return;
                }
            }
        }
    }

    /// Unit-length embedding of a text, or None when no token is known.
    pub fn embed(&self, text: &str) -> Option<Vec<f32>> {
        self.embed_ids(&self.tokenize(text))
    }

    /// Unit-length mean of the given tokens' vectors.
    pub fn embed_ids(&self, ids: &[u32]) -> Option<Vec<f32>> {
        if ids.is_empty() {
            return None;
        }
        let mut acc = vec![0f32; self.dims];
        for id in ids {
            let r = *id as usize;
            let s = self.scale[r];
            for (a, q) in acc.iter_mut().zip(&self.q[r * self.dims..(r + 1) * self.dims]) {
                *a += *q as i8 as f32 * s;
            }
        }
        let norm = acc.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm < 1e-9 {
            return None;
        }
        acc.iter_mut().for_each(|x| *x /= norm);
        Some(acc)
    }

    /// `embed`, stored as int8.
    pub fn embed8(&self, text: &str) -> Option<Vec8> {
        self.embed(text).map(|v| quantize(&v))
    }
}

/// An embedding stored as int8 values and one scale (value = q × s).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Vec8 {
    pub q: Vec<i8>,
    pub s: f32,
}

pub fn quantize(v: &[f32]) -> Vec8 {
    let (q, s) = quantize_row(v);
    Vec8 { q, s }
}

/// Cosine similarity of a unit query with a stored unit vector.
pub fn cosine(query: &[f32], v: &Vec8) -> f32 {
    cosine_q(query, &v.q, v.s)
}

/// Cosine similarity of a unit query with int8 values `q` and scale `s`.
pub fn cosine_q(query: &[f32], q: &[i8], s: f32) -> f32 {
    if q.len() != query.len() {
        return 0.0;
    }
    let mut dot = 0f32;
    for (a, b) in query.iter().zip(q) {
        dot += a * *b as f32;
    }
    dot * s
}

/// BERT normalization: drop control characters, map whitespace to a space,
/// pad CJK ideographs with spaces, lowercase and strip accents.
fn normalize(text: &str) -> impl Iterator<Item = char> + '_ {
    let ascii = text.is_ascii();
    let mut buf: Vec<char> = Vec::new();
    let mut chars = text.chars();
    std::iter::from_fn(move || loop {
        if let Some(c) = buf.pop() {
            return Some(c);
        }
        let c = chars.next()?;
        if c == '\0' || c == '\u{fffd}' || (c.is_control() && !c.is_whitespace()) {
            continue;
        }
        if c.is_whitespace() {
            return Some(' ');
        }
        if ascii || c.is_ascii() {
            return Some(c.to_ascii_lowercase());
        }
        if is_cjk(c) {
            buf.extend([' ', c, ' '].iter().rev());
            continue;
        }
        let mut out: Vec<char> = c.to_lowercase().collect::<String>().nfd().filter(|c| !is_mark(*c)).collect();
        out.reverse();
        buf.extend(out);
    })
}

fn is_mark(c: char) -> bool {
    // Combining marks (Unicode category Mn) that NFD splits off accented letters.
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF | 0x2A700..=0x2B73F
        | 0x2B740..=0x2B81F | 0x2B820..=0x2CEAF | 0xF900..=0xFAFF | 0x2F800..=0x2FA1F)
}

fn is_punct(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_punctuation();
    }
    // Unicode punctuation blocks (category P); other symbols stay in words.
    matches!(c as u32, 0x2010..=0x2027 | 0x2030..=0x205E | 0x3000..=0x303F | 0xFF01..=0xFF0F | 0xFF1A..=0xFF20 | 0xFF3B..=0xFF40 | 0xFF5B..=0xFF65 | 0x00A1 | 0x00A7 | 0x00AB | 0x00B6 | 0x00B7 | 0x00BB | 0x00BF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toy() -> Model {
        let words = ["[PAD]", "[UNK]", "read", "file", "##s", "http", "##response", "_", "(", ")", "cafe", "中"];
        let dims = 2;
        let q: Vec<u8> = (0..words.len()).flat_map(|i| [((i % 3) * 10) as u8, (((i + 1) % 2) * 10) as u8]).collect();
        Model::from_parts(&words, dims, q, vec![0.1; words.len()])
    }

    #[test]
    fn bert_wordpiece() {
        let m = toy();
        // Lowercase, punctuation split, `##` continuation, unknown words dropped.
        assert_eq!(m.tokenize("read_files(HTTPResponse) zzz"), vec![2, 7, 3, 4, 8, 5, 6, 9]);
        // Accents stripped, CJK split per character.
        assert_eq!(m.tokenize("Café中文"), vec![10, 11]);
        let e = m.embed("read file").unwrap();
        assert!((e.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-4);
        assert!((cosine(&e, &quantize(&e)) - 1.0).abs() < 0.02);
        assert!(m.embed("zzzz").is_none());
    }

    #[test]
    fn half_floats() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert!((f16_to_f32(0x0001) - 5.960_464_5e-8).abs() < 1e-12);
    }

    /// Checks against `model2vec` with the real model. Runs when
    /// `SYLPHX_MODEL_TEST_DIR` points at a folder with the model's
    /// `tokenizer.json` and `model.safetensors`, and `expected.json` written by
    /// `model2vec` (see `tests/model2vec_expected.py`).
    #[test]
    fn matches_model2vec() {
        let Some(src) = std::env::var_os("SYLPHX_MODEL_TEST_DIR") else { return };
        let src = PathBuf::from(src);
        let out = std::env::temp_dir().join(format!("mcp-kit-embed-{}", std::process::id()));
        convert(&std::fs::read(src.join("tokenizer.json")).unwrap(), &std::fs::read(src.join("model.safetensors")).unwrap(), &out).unwrap();
        let m = Model::load_dir(&out).unwrap();
        let expected: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(src.join("expected.json")).unwrap()).unwrap();
        for e in expected {
            let text = e["text"].as_str().unwrap();
            let ids: Vec<u32> = e["ids"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
            assert_eq!(m.tokenize(text), ids, "tokens of {text:?}");
            let want: Vec<f32> = e["vector"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap() as f32).collect();
            let got = m.embed(text).unwrap();
            let cos: f32 = got.iter().zip(&want).map(|(a, b)| a * b).sum();
            assert!(cos > 0.999, "cosine {cos} for {text:?}");
        }
        let _ = std::fs::remove_dir_all(&out);
    }
}
