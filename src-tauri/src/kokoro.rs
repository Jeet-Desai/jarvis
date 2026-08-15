//! Kokoro neural TTS — a natural local voice that replaces macOS `say`.
//!
//! Runs `kokoro-v1.0.onnx` via `ort` (onnxruntime), phonemising text with
//! `misaki-rs` (the pure-Rust port of Kokoro's reference G2P — no native
//! espeak-ng, nothing to install). Fully offline; the model + voice pack live
//! in `models/` alongside the whisper model.
//!
//! The engine is expensive to build (ONNX session + 28 MB voice pack + G2P
//! data) so it's loaded ONCE behind a `Mutex` — `ort::Session::run` takes
//! `&mut self`, and speaking is serialised by the state machine anyway. If the
//! models are missing or init fails, `available()` reports false and the TTS
//! layer falls back to `say`, so Kokoro is a strict upgrade, never a regression.
//!
//! Model I/O (read off the file, see `kokoro` de-risk): inputs `tokens`
//! (int64 `[1,N]`), `style` (f32 `[1,256]`), `speed` (f32 `[1]`) → output
//! `audio` (f32, 24 kHz mono). NOTE the input is named `tokens`, not
//! `input_ids` — this export differs from some docs.

use anyhow::{anyhow, Context, Result};
use ndarray::Array3;
use ndarray_npy::NpzReader;
use ort::session::Session;
use ort::value::Tensor;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::kokoro_vocab::KOKORO_VOCAB;

/// Kokoro emits 24 kHz mono.
pub const SAMPLE_RATE: u32 = 24_000;
/// Default voice (British male; user-selected). Override with `JARVIS_TTS_VOICE`.
const DEFAULT_VOICE: &str = "bm_lewis";
/// Kokoro's phoneme budget (the voice pack has a style row per length 0..510).
const MAX_TOKENS: usize = 510;

struct Engine {
    session: Session,
    g2p: misaki_rs::G2P,
    vocab: HashMap<char, i64>,
    /// Selected voice's style tensor `[510, 1, 256]`; row indexed by token count.
    style: Array3<f32>,
    voice: String,
    /// Lowercased word set from the misaki US lexicon (gold+silver keys).
    /// Drives OOV compound-splitting: without the espeak feature, misaki-rs
    /// SPELLS OUT unknown words letter-by-letter ("worktree" → W-O-R-K-…), so
    /// we pre-split compounds into two known halves it can pronounce.
    words: HashSet<String>,
    /// Curated respellings for jargon with a conventional spoken form the
    /// lexicon can't derive ("json" → "jason"). Extend without a rebuild via
    /// `JARVIS_TTS_RESPELL="word=spoken form,word2=…"`.
    respell: HashMap<String, String>,
}

impl Engine {
    /// text → 24 kHz mono f32 samples. Empty if nothing phonemised.
    fn synthesize(&mut self, text: &str) -> Result<Vec<f32>> {
        // 0. rescue words the G2P would letter-spell (OOV compounds + jargon)
        let text = respell_oov(text, &self.words, &self.respell);

        // 1. text → IPA phoneme string, WITH punctuation preserved (see
        // `phonemize` — misaki-rs deletes every mark, which flattens prosody).
        let phonemes = self.phonemize(&text)?;

        // Surface silent drops: '❓' is misaki's unknown marker, and any char
        // missing from KOKORO_VOCAB is skipped below — both mean audible gaps.
        let unknown = phonemes.chars().filter(|c| *c == '❓').count();
        if unknown > 0 {
            tracing::warn!("kokoro: {unknown} unknown token(s) in phonemes for {:?}", text);
        }

        // 2. phonemes → Kokoro token ids (unmapped chars, e.g. ZWJ joiners, skipped)
        let mut ids: Vec<i64> = phonemes.chars().filter_map(|c| self.vocab.get(&c).copied()).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        ids.truncate(MAX_TOKENS);
        let n = ids.len();

        // 3. voice style row for this token count → flat [1,256]
        let idx = n.min(509);
        let style_vec: Vec<f32> = self.style.slice(ndarray::s![idx, .., ..]).iter().copied().collect();

        // 4. pad-wrap tokens: [0, …ids, 0]
        let mut tokens = Vec::with_capacity(n + 2);
        tokens.push(0i64);
        tokens.extend_from_slice(&ids);
        tokens.push(0i64);
        let tok_len = tokens.len();

        // 5. inference (plain (shape, Vec) tensors — no ndarray-version coupling)
        let outputs = self.session.run(ort::inputs![
            "tokens" => Tensor::from_array(([1usize, tok_len], tokens))?,
            "style"  => Tensor::from_array(([1usize, 256usize], style_vec))?,
            "speed"  => Tensor::from_array(([1usize], vec![speed()]))?,
        ])?;
        let (_shape, audio) = outputs["audio"].try_extract_tensor::<f32>()?;
        Ok(audio.to_vec())
    }

    /// text → IPA phoneme string **with punctuation preserved**.
    ///
    /// This is the difference between a voice that phrases and one that
    /// drones. `misaki-rs` (any feature set) replaces EVERY punctuation mark
    /// with a plain space before we ever see it:
    ///
    /// ```text
    /// misaki-rs/src/g2p.rs:262
    ///   if c.is_ascii_punctuation() || "—–…".contains(c) {
    ///       tokens[i].phonemes = Some(" ".to_string());
    ///   }
    /// ```
    ///
    /// So `"Hello, world. Right?"` reached Kokoro as `"həlˈoʊ  wˈɜːld  ɹˈaɪt "`
    /// — no comma, no full stop, no question mark, even though KOKORO_VOCAB has
    /// token ids for all of them (`,`=3 `.`=4 `!`=5 `?`=6 `;`=1 `:`=2 `—`=9
    /// `…`=10). Punctuation is what drives sentence-final falling intonation,
    /// question rise, and clause grouping, so without it the model has no
    /// phrase structure at all: it never pauses where a sentence ends, and the
    /// only pauses left are whatever it invents from runs of spaces.
    ///
    /// Fix: segment the TEXT at each mark, phonemise the runs between marks
    /// separately, and splice the literal mark back into the PHONEME string.
    /// (The mark can't ride along in the g2p input — that's what gets deleted;
    /// same reason `split_adjective_live` splices its IPA rather than inlining
    /// it.)
    fn phonemize(&self, text: &str) -> Result<String> {
        phonemize_with(&self.g2p, text)
    }
}

/// Free-function core of [`Engine::phonemize`], so it can be exercised without
/// building an ONNX session (see the tests).
fn phonemize_with(g2p: &misaki_rs::G2P, text: &str) -> Result<String> {
    let mut pieces = segment_prosody(text);
    // Guarantee utterance-final prosody: a reply the model ended without a
    // full stop ("Done", "Right away sir") otherwise gets no falling
    // intonation and just stops dead. Chunks are already cut at sentence
    // boundaries, so a terminal mark here is never mid-sentence.
    if matches!(pieces.last(), Some(Piece::Text(_))) {
        pieces.push(Piece::Mark('.'));
    }

    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Mark(c) => {
                // Attach the mark directly to the preceding phonemes
                // (", " not " , ") — that's the reference formatting.
                while out.ends_with(' ') {
                    out.pop();
                }
                if !out.is_empty() {
                    out.push(c);
                    out.push(' ');
                }
            }
            Piece::Text(t) => {
                // The lexicon's only entry for "live" is the verb /lɪv/, so
                // where context says adjective ("the demo is live", "live
                // output") we g2p around the word and splice the correct
                // phonemes in.
                for (i, seg) in split_adjective_live(&t).iter().enumerate() {
                    if i > 0 {
                        out.push_str(" lˈa\u{200d}ɪv ");
                    }
                    if seg.trim().is_empty() {
                        continue;
                    }
                    let (p, _toks) = g2p.g2p(seg).map_err(|e| anyhow!("g2p: {e}"))?;
                    out.push_str(p.trim_end());
                    out.push(' ');
                }
            }
        }
    }
    Ok(out)
}

/// One span of the reply on its way to the phonemiser: either text to
/// phonemise, or a punctuation mark to splice in verbatim.
#[derive(Debug, PartialEq)]
enum Piece {
    Text(String),
    Mark(char),
}

/// Punctuation Kokoro renders as real prosody. Every one of these has a token
/// id in `KOKORO_VOCAB`, and every one is destroyed by misaki-rs — see
/// `Engine::phonemize`.
fn is_prosody_mark(c: char) -> bool {
    matches!(c, ',' | '.' | '!' | '?' | ';' | ':' | '—' | '…')
}

/// True when a `.` or `:` sits between two digits — a decimal or a clock time
/// ("3.5", "3:30"), not a sentence end. Splitting there would read "three.
/// five." as two sentences.
fn is_numeric_context(chars: &[char], i: usize) -> bool {
    if !matches!(chars[i], '.' | ':') {
        return false;
    }
    let prev = i.checked_sub(1).and_then(|p| chars.get(p));
    let next = chars.get(i + 1);
    matches!((prev, next), (Some(p), Some(n)) if p.is_ascii_digit() && n.is_ascii_digit())
}

/// Split a reply into text runs and the prosodic marks between them.
///
/// Line breaks become a full stop: `speakable::to_speech` turns Markdown
/// bullets and paragraphs into bare newlines, and misaki drops those too — so
/// a three-item list was read as one breathless run-on. Consecutive marks
/// collapse to the first (the author's own punctuation wins over a synthesised
/// one), so `"Done.\nNext"` yields a single `.`, not two.
fn segment_prosody(text: &str) -> Vec<Piece> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Piece> = Vec::new();
    let mut buf = String::new();

    fn flush(out: &mut Vec<Piece>, buf: &mut String) {
        let t = buf.trim();
        if !t.is_empty() {
            out.push(Piece::Text(t.to_string()));
        }
        buf.clear();
    }
    fn mark(out: &mut Vec<Piece>, c: char) {
        // Never two marks in a row, and never lead with one.
        if matches!(out.last(), Some(Piece::Mark(_))) || out.is_empty() {
            return;
        }
        out.push(Piece::Mark(c));
    }

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            flush(&mut out, &mut buf);
            mark(&mut out, '.');
            // Swallow the whole run of blank space so one paragraph break
            // doesn't emit several marks.
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            continue;
        }
        if is_prosody_mark(c) && !is_numeric_context(&chars, i) {
            flush(&mut out, &mut buf);
            mark(&mut out, c);
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut out, &mut buf);
    out
}

/// `None` = engine unavailable (models missing / init failed) → caller uses `say`.
static ENGINE: OnceLock<Option<Mutex<Engine>>> = OnceLock::new();

/// Is `w` (lowercase ascii) pronounceable via the lexicon — directly or through
/// the G2P's own -s / -ed / -ing stemming?
fn known_word(words: &HashSet<String>, w: &str) -> bool {
    if words.contains(w) {
        return true;
    }
    let n = w.len();
    if n > 4 && w.ends_with("ing") {
        let stem = &w[..n - 3];
        if words.contains(stem) || words.contains(&format!("{stem}e")) {
            return true;
        }
    }
    if n > 3
        && ((w.ends_with('s') && words.contains(&w[..n - 1]))
            || (w.ends_with("ed") && (words.contains(&w[..n - 1]) || words.contains(&w[..n - 2]))))
    {
        return true;
    }
    false
}

/// Replace words the G2P would letter-spell: curated respellings first, then
/// OOV compounds split into two lexicon-known halves ("worktree" → "work
/// tree", "hardcoded" → "hard coded"). Known words, short words, and
/// non-alphabetic tokens (acronyms like "npx", where letter-spelling is the
/// correct reading) pass through untouched.
fn respell_oov(text: &str, words: &HashSet<String>, respell: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut word = String::new();
    for c in text.chars() {
        if c.is_alphabetic() {
            word.push(c);
            continue;
        }
        flush_word(&mut out, &word, words, respell);
        word.clear();
        out.push(c);
    }
    flush_word(&mut out, &word, words, respell);
    out
}

fn flush_word(out: &mut String, word: &str, words: &HashSet<String>, respell: &HashMap<String, String>) {
    if word.is_empty() {
        return;
    }
    if !word.chars().all(|c| c.is_ascii_alphabetic()) {
        out.push_str(word);
        return;
    }
    let lower = word.to_ascii_lowercase();
    if let Some(r) = respell.get(&lower) {
        out.push_str(r);
        return;
    }
    if lower.len() < 6 || known_word(words, &lower) {
        out.push_str(word);
        return;
    }
    // best compound split = the one with the most balanced halves, so
    // "worktree" picks work+tree over wo+rktree-style accidents
    let n = lower.len();
    let mut best: Option<(usize, usize)> = None;
    for i in 3..=n - 3 {
        if known_word(words, &lower[..i]) && known_word(words, &lower[i..]) {
            let score = i.min(n - i);
            if best.map_or(true, |(s, _)| score > s) {
                best = Some((score, i));
            }
        }
    }
    match best {
        Some((_, i)) => {
            out.push_str(&lower[..i]);
            out.push(' ');
            out.push_str(&lower[i..]);
        }
        None => out.push_str(word),
    }
}

/// Split `text` around occurrences of adjective-"live" (the caller splices the
/// /laɪv/ phonemes back between segments). Verb uses ("I live in Mumbai")
/// stay in place and keep the lexicon's /lɪv/.
fn split_adjective_live(text: &str) -> Vec<String> {
    const BEFORE: &[&str] = &[
        "is", "are", "was", "were", "go", "goes", "going", "went", "gone", "now", "currently",
    ];
    const AFTER: &[&str] = &[
        "output", "stream", "streaming", "streams", "demo", "mode", "agent", "agents", "site",
        "version", "preview", "app", "server", "data", "feed", "view", "dashboard", "url", "link",
        "page", "session", "deploy", "deployment", "update", "updates",
    ];

    // byte spans of every word
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphabetic() || c == '\'' {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            spans.push((s, i));
        }
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }

    let lower_at = |i: usize| text[spans[i].0..spans[i].1].to_ascii_lowercase();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for i in 0..spans.len() {
        if lower_at(i) != "live" {
            continue;
        }
        let adj = (i > 0 && BEFORE.contains(&lower_at(i - 1).as_str()))
            || (i + 1 < spans.len() && AFTER.contains(&lower_at(i + 1).as_str()));
        if adj {
            cuts.push(spans[i]);
        }
    }
    if cuts.is_empty() {
        return vec![text.to_string()];
    }
    let mut segs = Vec::with_capacity(cuts.len() + 1);
    let mut pos = 0;
    for (s, e) in cuts {
        segs.push(text[pos..s].to_string());
        pos = e;
    }
    segs.push(text[pos..].to_string());
    segs
}

/// The `models/` dir, one level above `src-tauri/` (mirrors `stt::models_dir`).
fn models_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("models"))
        .unwrap_or_else(|| PathBuf::from("models"))
}

/// Default speaking rate. >1 because Kokoro at 1.0 reads a touch slowly for a
/// voice assistant. Override with `JARVIS_TTS_SPEED`.
const DEFAULT_SPEED: f32 = 1.15;

/// Speaking rate multiplier (1.0 = normal). Override with `JARVIS_TTS_SPEED`.
fn speed() -> f32 {
    std::env::var("JARVIS_TTS_SPEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|s: &f32| *s > 0.1 && *s < 3.0)
        .unwrap_or(DEFAULT_SPEED)
}

/// Trim near-silence from the head and tail of a clip (Kokoro pads ~100-250ms
/// of quiet around an utterance). Tightens the gap between streamed chunks so
/// sentence transitions don't drag. Keeps a small pad so onsets aren't clipped.
fn trim_silence(samples: &[f32]) -> &[f32] {
    const THRESH: f32 = 0.008;
    let first = samples.iter().position(|s| s.abs() > THRESH);
    let last = samples.iter().rposition(|s| s.abs() > THRESH);
    match (first, last) {
        (Some(a), Some(b)) if a <= b => {
            let pad = SAMPLE_RATE as usize / 200; // ~5ms
            let s = a.saturating_sub(pad);
            let e = (b + 1 + pad).min(samples.len());
            &samples[s..e]
        }
        _ => &[], // all silence
    }
}

fn build_engine() -> Result<Engine> {
    let dir = models_dir();
    let onnx = dir.join("kokoro-v1.0.onnx");
    let voices = dir.join("voices-v1.0.bin");
    if !onnx.exists() || !voices.exists() {
        return Err(anyhow!(
            "kokoro models missing in {} (need kokoro-v1.0.onnx + voices-v1.0.bin)",
            dir.display()
        ));
    }

    let session = Session::builder()?
        .commit_from_file(&onnx)
        .with_context(|| format!("loading {}", onnx.display()))?;

    let voice = std::env::var("JARVIS_TTS_VOICE").unwrap_or_else(|_| DEFAULT_VOICE.into());
    let mut npz = NpzReader::new(std::fs::File::open(&voices)?)?;
    // np.savez stores arrays as "<name>.npy"; accept either spelling.
    let style: Array3<f32> = npz
        .by_name(&format!("{voice}.npy"))
        .or_else(|_| npz.by_name(&voice))
        .with_context(|| format!("voice '{voice}' not found in voice pack"))?;

    let g2p = misaki_rs::G2P::new(misaki_rs::Language::EnglishUS);
    let vocab: HashMap<char, i64> = KOKORO_VOCAB.iter().copied().collect();

    // Word set for the OOV compound splitter — the same gold+silver lexicons
    // the G2P consults (keys only; ~1s parse, on the prewarm thread).
    let words: HashSet<String> = misaki_rs::data::load_us_gold()
        .keys()
        .chain(misaki_rs::data::load_us_silver().keys())
        .map(|k| k.to_lowercase())
        .collect();

    // Built-in respellings (the splitter can't rescue these), plus user
    // overrides from JARVIS_TTS_RESPELL ("word=spoken form,word2=…").
    let mut respell: HashMap<String, String> = [
        ("json", "jason"),
        ("stdout", "standard out"),
        ("stderr", "standard error"),
        ("sudo", "pseudo"),
        ("kokoro", "cocoa row"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    if let Ok(extra) = std::env::var("JARVIS_TTS_RESPELL") {
        for pair in extra.split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                if !k.is_empty() && !v.is_empty() {
                    respell.insert(k, v);
                }
            }
        }
    }

    Ok(Engine { session, g2p, vocab, style, voice, words, respell })
}

fn engine() -> Option<&'static Mutex<Engine>> {
    ENGINE
        .get_or_init(|| match build_engine() {
            Ok(e) => {
                tracing::info!("kokoro: engine ready (voice {})", e.voice);
                Some(Mutex::new(e))
            }
            Err(e) => {
                tracing::warn!("kokoro: disabled, will use `say` — {e}");
                None
            }
        })
        .as_ref()
}

/// Is Kokoro usable? Triggers (one-time) engine init if not already warmed.
pub fn available() -> bool {
    engine().is_some()
}

/// Load the engine on a background thread so the first utterance is instant.
pub fn prewarm() {
    std::thread::spawn(|| {
        let _ = engine();
    });
}

/// One synthesized clip: the WAV is on disk; `envelope` is the clip's loudness
/// curve (RMS per `ENV_DT_MS` window, normalized to the clip's peak), which the
/// TTS player replays as `jarvis:level` events so the orb pulses with the
/// actual voice.
pub struct Clip {
    pub duration_secs: f32,
    pub envelope: Vec<f32>,
}

/// Envelope resolution — one level value per this many milliseconds of audio.
pub const ENV_DT_MS: u64 = 45;

/// Per-window RMS of the mono clip, peak-normalized and gamma-lifted so quiet
/// syllables still read visually (raw speech RMS has a huge dynamic range).
/// A 3-tap moving average knocks down window-to-window whipsaw (consonant
/// gaps) — the renderer adds attack/release ballistics on top, so the bars
/// follow speech without strobing.
fn envelope(mono: &[f32]) -> Vec<f32> {
    let win = (SAMPLE_RATE as u64 * ENV_DT_MS / 1000).max(1) as usize;
    let raw: Vec<f32> = mono
        .chunks(win)
        .map(|w| (w.iter().map(|s| s * s).sum::<f32>() / w.len() as f32).sqrt())
        .collect();
    let n = raw.len();
    let mut env: Vec<f32> = (0..n)
        .map(|i| {
            let a = raw[i.saturating_sub(1)];
            let b = raw[i];
            let c = raw[(i + 1).min(n - 1)];
            (a + 2.0 * b + c) / 4.0
        })
        .collect();
    let peak = env.iter().cloned().fold(0.0f32, f32::max);
    if peak > 1e-4 {
        for v in &mut env {
            *v = (*v / peak).min(1.0).powf(0.6);
        }
    }
    env
}

/// Synthesize `text` and write a **stereo** 24 kHz WAV (mono duplicated to L/R
/// so playback isn't single-speaker). Returns the clip's duration + loudness
/// envelope, or `None` if Kokoro is unavailable or produced no audio (→ caller
/// falls back to `say`).
pub fn synth_to_wav(text: &str, path: &Path) -> Result<Option<Clip>> {
    let Some(eng) = engine() else {
        return Ok(None);
    };
    let mono = eng.lock().synthesize(text)?;
    let mono = trim_silence(&mono);
    if mono.is_empty() {
        return Ok(None);
    }
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in mono {
        w.write_sample(*s)?; // L
        w.write_sample(*s)?; // R
    }
    w.finalize()?;
    Ok(Some(Clip {
        duration_secs: mono.len() as f32 / SAMPLE_RATE as f32,
        envelope: envelope(mono),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    // ── prosody: punctuation must survive into the phoneme stream ──────────

    #[test]
    fn segments_at_every_prosodic_mark() {
        use Piece::*;
        assert_eq!(
            segment_prosody("Hello, world. Right?"),
            vec![
                Text("Hello".into()), Mark(','),
                Text("world".into()), Mark('.'),
                Text("Right".into()), Mark('?'),
            ]
        );
    }

    #[test]
    fn newlines_become_sentence_breaks() {
        use Piece::*;
        // speakable::to_speech turns bullets into bare newlines; misaki drops
        // them, so a list used to be read as one breathless run-on.
        assert_eq!(
            segment_prosody("one\ntwo\nthree"),
            vec![Text("one".into()), Mark('.'), Text("two".into()), Mark('.'), Text("three".into())]
        );
        // A paragraph break emits ONE mark, not one per newline.
        assert_eq!(
            segment_prosody("first\n\n\nsecond"),
            vec![Text("first".into()), Mark('.'), Text("second".into())]
        );
        // The author's own punctuation wins over a synthesised one.
        assert_eq!(
            segment_prosody("Done!\nNext"),
            vec![Text("Done".into()), Mark('!'), Text("Next".into())]
        );
    }

    #[test]
    fn decimals_and_clock_times_are_not_sentence_ends() {
        use Piece::*;
        // "3.5" must not read as "three. five." — nor "3:30" as two clauses.
        assert_eq!(segment_prosody("version 3.5"), vec![Text("version 3.5".into())]);
        assert_eq!(segment_prosody("at 3:30 sharp"), vec![Text("at 3:30 sharp".into())]);
        // …but a real sentence end after a digit still counts.
        assert_eq!(
            segment_prosody("It is 5. Yes"),
            vec![Text("It is 5".into()), Mark('.'), Text("Yes".into())]
        );
    }

    #[test]
    fn never_leads_with_or_doubles_a_mark() {
        use Piece::*;
        assert_eq!(segment_prosody("...well"), vec![Text("well".into())]);
        assert_eq!(segment_prosody("Really?!"), vec![Text("Really".into()), Mark('?')]);
        assert_eq!(segment_prosody("   "), vec![]);
    }

    /// The regression that started this: every mark used to be deleted by
    /// misaki-rs before Kokoro saw it, leaving the model with no phrase
    /// structure. Assert the marks now reach the token stream, in order.
    #[test]
    fn punctuation_reaches_the_kokoro_token_stream() {
        let g2p = misaki_rs::G2P::new(misaki_rs::Language::EnglishUS);
        let vocab: HashMap<char, i64> = KOKORO_VOCAB.iter().copied().collect();

        let phonemes = phonemize_with(&g2p, "Hello, world. Is it? Yes — quite.").expect("phonemize");
        let marks: String = phonemes.chars().filter(|c| is_prosody_mark(*c)).collect();
        assert_eq!(marks, ",.?—.", "marks lost on the way to Kokoro: {phonemes:?}");

        // Every mark must be a REAL token, not silently dropped by the vocab
        // filter in `synthesize` — that filter is what hid this bug.
        for c in marks.chars() {
            assert!(vocab.contains_key(&c), "{c:?} has no Kokoro token id");
        }

        // Marks hug the preceding word ("wˈɜːld." not "wˈɜːld ."), which is the
        // formatting the reference implementation feeds the model.
        assert!(!phonemes.contains(" ,") && !phonemes.contains(" ."), "{phonemes:?}");
    }

    #[test]
    fn bulleted_list_gets_a_pause_per_item() {
        let g2p = misaki_rs::G2P::new(misaki_rs::Language::EnglishUS);
        let phonemes = phonemize_with(&g2p, "one\ntwo\nthree").expect("phonemize");
        // two separators + the utterance-final full stop
        assert_eq!(
            phonemes.chars().filter(|c| *c == '.').count(),
            3,
            "list items must be separated by real pauses: {phonemes:?}"
        );
    }

    #[test]
    fn unterminated_reply_gets_final_intonation() {
        let g2p = misaki_rs::G2P::new(misaki_rs::Language::EnglishUS);
        let p = phonemize_with(&g2p, "Right away sir").expect("phonemize");
        assert!(p.trim_end().ends_with('.'), "no utterance-final mark: {p:?}");
        // An existing terminal mark is not doubled.
        let q = phonemize_with(&g2p, "Is it done?").expect("phonemize");
        let marks: String = q.chars().filter(|c| is_prosody_mark(*c)).collect();
        assert_eq!(marks, "?");
    }

    #[test]
    fn adjective_live_splice_still_works_through_the_new_path() {
        let g2p = misaki_rs::G2P::new(misaki_rs::Language::EnglishUS);
        let p = phonemize_with(&g2p, "The demo is live, sir.").expect("phonemize");
        assert!(p.contains("lˈa\u{200d}ɪv"), "adjective 'live' lost its splice: {p:?}");
        // and the comma + full stop survived alongside it
        let marks: String = p.chars().filter(|c| is_prosody_mark(*c)).collect();
        assert_eq!(marks, ",.");
    }

    #[test]
    fn splits_oov_compounds_into_known_halves() {
        let w = words(&["work", "tree", "hard", "coded", "code", "wake", "word", "front", "end"]);
        let r = HashMap::new();
        assert_eq!(respell_oov("the worktree was hardcoded", &w, &r), "the work tree was hard coded");
        assert_eq!(respell_oov("wakeword frontend", &w, &r), "wake word front end");
    }

    #[test]
    fn balanced_split_beats_lopsided_split() {
        // "worktree": wor+ktree invalid, work+tree valid; with a decoy "w"+"orktree"
        // style set the balanced pair must win.
        let w = words(&["work", "tree", "wo", "rktree"]);
        let r = HashMap::new();
        assert_eq!(respell_oov("worktree", &w, &r), "work tree");
    }

    #[test]
    fn known_short_and_acronym_words_untouched() {
        let w = words(&["clipboard", "screen"]);
        let r = HashMap::new();
        // known word: untouched even though it could split
        assert_eq!(respell_oov("clipboard", &w, &r), "clipboard");
        // short word and acronym-ish tokens: untouched (letter-spelling is right for npx)
        assert_eq!(respell_oov("npx zsh", &w, &r), "npx zsh");
        // unknown and unsplittable: left alone
        assert_eq!(respell_oov("qqqqqqqq", &w, &r), "qqqqqqqq");
    }

    #[test]
    fn respell_map_wins_and_punctuation_survives() {
        let w = words(&[]);
        let r: HashMap<String, String> =
            [("json".to_string(), "jason".to_string())].into_iter().collect();
        assert_eq!(respell_oov("Parse the JSON, please.", &w, &r), "Parse the jason, please.");
    }

    #[test]
    fn stemmed_halves_count_as_known() {
        let w = words(&["hard", "code"]);
        let r = HashMap::new();
        // "coded" is known via the -ed stem of "code"
        assert_eq!(respell_oov("hardcoded", &w, &r), "hard coded");
    }

    #[test]
    fn adjective_live_is_cut_out_verb_live_stays() {
        // adjective by preceding word
        assert_eq!(split_adjective_live("the demo is live."), vec!["the demo is ", "."]);
        // adjective by following word
        assert_eq!(split_adjective_live("live output is here"), vec!["", " output is here"]);
        // verb use: untouched
        assert_eq!(split_adjective_live("I live in Mumbai"), vec!["I live in Mumbai"]);
        // no live at all
        assert_eq!(split_adjective_live("hello there"), vec!["hello there"]);
        // two occurrences, mixed
        assert_eq!(
            split_adjective_live("it went live and I live nearby"),
            vec!["it went ", " and I live nearby"]
        );
    }
}
