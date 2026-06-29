//! Turn an LLM reply (Markdown + URLs) into clean *speakable* text.
//!
//! The brain answers in Markdown and frequently emits links. macOS `say`
//! (and any neural TTS) then reads the raw string literally:
//!   - `**bold**`  → it stumbles over the asterisks
//!   - a URL       → it spells the path out token-by-token ("h-t-t-p-s colon…")
//!   - `snake_case`→ "snake underscore case"
//!   - emoji       → it reads the CLDR name ("check mark button")
//!
//! This module is a PURE function (`to_speech`) that strips Markdown syntax,
//! rewrites links to a short spoken host ("github dot com"), and drops noise
//! (fenced code, emoji). It is the single normalisation choke point, called
//! from inside `tts::start_speaking`, so the HUD keeps the raw Markdown while
//! every audio path — replies, confirmations, proactive blurbs — speaks clean.
//!
//! It is deliberately conservative: when in doubt it keeps words rather than
//! dropping them, the one exception being fenced code blocks (reading code
//! aloud is gibberish, and the user still sees it on screen).

use regex::Regex;
use std::sync::LazyLock;

// ── Precompiled patterns (compiled once on first use) ──────────────────────

/// Fenced code block ```lang\n…```  — dropped wholesale (see module docs).
static RE_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```[a-zA-Z0-9_-]*\n?.*?```").unwrap());
/// Image  ![alt](url) → alt   (must run before the link rule).
static RE_IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\([^)]*\)").unwrap());
/// Link   [label](url) → label
static RE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]*\)").unwrap());
/// Bare URL (http(s):// … or www. …) → spoken host, via `host_to_speech`.
/// The final character class forbids ending on sentence punctuation so a
/// trailing `.`/`,`/`!` stays in the spoken sentence instead of the URL.
static RE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:https?://|www\.)[^\s<>()\[\]]*[^\s<>()\[\].,;:!?]").unwrap()
});
/// Inline code `x` → x   (keep the content; lose the backticks).
static RE_INLINE_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());
/// Bold **x** / __x__ → x   (run before the italic rule).
static RE_BOLD_STAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*\*([^*]+)\*\*").unwrap());
static RE_BOLD_UNDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"__([^_]+)__").unwrap());
/// Italic *x* → x
static RE_ITALIC_STAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*([^*\n]+)\*").unwrap());
/// Heading markers at line start (`#`…`######`).
static RE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s{0,3}#{1,6}\s*").unwrap());
/// Blockquote marker at line start.
static RE_QUOTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s{0,3}>\s?").unwrap());
/// Horizontal rule line (---, ***, ___) — removed entirely. (The `regex`
/// crate has no backreferences, so each rule character is spelled out.)
static RE_HR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s{0,3}(?:(?:-\s*){3,}|(?:\*\s*){3,}|(?:_\s*){3,})$").unwrap()
});
/// List bullet (-, *, +) at line start — strip the marker, keep the item.
static RE_BULLET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s{0,3}[-*+]\s+").unwrap());
/// Emoji / pictographs / variation-selectors / ZWJ — dropped.
static RE_EMOJI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"[\x{1F000}-\x{1FAFF}\x{2600}-\x{27BF}\x{2B00}-\x{2BFF}\x{2190}-\x{21FF}\x{FE00}-\x{FE0F}\x{1F1E6}-\x{1F1FF}\x{200D}\x{2122}\x{2139}\x{24C2}\x{3030}\x{303D}\x{3297}\x{3299}]",
    )
    .unwrap()
});
/// Run of horizontal whitespace → one space.
static RE_HSPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]{2,}").unwrap());
/// 3+ newlines → a paragraph break (two).
static RE_VSPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());
/// Stray spaces hugging a newline.
static RE_NL_TRIM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]*\n[ \t]*").unwrap());

/// Normalise an LLM reply into clean speakable text.
pub fn to_speech(input: &str) -> String {
    let mut s = input.to_string();

    // 1. Drop fenced code blocks (gibberish aloud; still visible on the HUD).
    s = RE_FENCE.replace_all(&s, " ").into_owned();

    // 2. Markdown links/images → their visible label (before URL rewriting,
    //    so the label text survives and only *bare* URLs hit the URL rule).
    s = RE_IMAGE.replace_all(&s, "$1").into_owned();
    s = RE_LINK.replace_all(&s, "$1").into_owned();

    // 3. Bare URLs → a short spoken host ("github dot com"), never the path.
    s = RE_URL
        .replace_all(&s, |caps: &regex::Captures| host_to_speech(&caps[0]))
        .into_owned();

    // 4. Block-level markers (line-anchored), before inline emphasis.
    s = RE_HR.replace_all(&s, "").into_owned();
    s = RE_HEADER.replace_all(&s, "").into_owned();
    s = RE_QUOTE.replace_all(&s, "").into_owned();
    s = RE_BULLET.replace_all(&s, "").into_owned();

    // 5. Inline emphasis + code (bold before italic; both before the blanket
    //    `*`/`_` cleanup below so we keep the wrapped words).
    s = RE_INLINE_CODE.replace_all(&s, "$1").into_owned();
    s = RE_BOLD_STAR.replace_all(&s, "$1").into_owned();
    s = RE_BOLD_UNDER.replace_all(&s, "$1").into_owned();
    s = RE_ITALIC_STAR.replace_all(&s, "$1").into_owned();

    // 6. Residual symbols that read badly. Underscores become spaces so
    //    `get_focused_app` speaks as "get focused app"; stray markup chars go.
    s = s.replace('_', " ");
    s = s.replace(['*', '#', '`', '|', '~'], "");

    // 7. Emoji / pictographs.
    s = RE_EMOJI.replace_all(&s, "").into_owned();

    // 8. Whitespace cleanup.
    s = RE_NL_TRIM.replace_all(&s, "\n").into_owned();
    s = RE_VSPACE.replace_all(&s, "\n\n").into_owned();
    s = RE_HSPACE.replace_all(&s, " ").into_owned();

    s.trim().to_string()
}

/// Convert a matched URL into a short, naturally-spoken host.
///
/// `https://www.github.com/Jeet-Desai/jarvis?tab=x` → `"github dot com"`.
/// Strips scheme + `www.`, drops the path/query, and reads the dots so the
/// host isn't spelled out. Falls back to `"a link"` if there's no usable host.
fn host_to_speech(url: &str) -> String {
    // Trim trailing sentence punctuation the greedy match may have grabbed.
    let url = url.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '"' | '\''));

    let mut host = url;
    for scheme in ["https://", "http://"] {
        if let Some(rest) = host.strip_prefix(scheme) {
            host = rest;
            break;
        }
    }
    host = host.strip_prefix("www.").unwrap_or(host);
    // Host ends at the first path/query/fragment/credential separator.
    let end = host
        .find(|c: char| matches!(c, '/' | '?' | '#' | '@' | ':'))
        .unwrap_or(host.len());
    let host = &host[..end];

    if host.is_empty() {
        return "a link".to_string();
    }
    // Read the dots ("github dot com") so TTS doesn't spell the host out.
    let spoken = host.replace('.', " dot ");
    // Collapse any doubled spaces the replace introduced.
    spoken.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bold_italic_inline_code() {
        assert_eq!(
            to_speech("Use **bold** and *italic* and `code` now."),
            "Use bold and italic and code now."
        );
    }

    #[test]
    fn underscore_bold_and_snake_case() {
        assert_eq!(to_speech("__very__ important"), "very important");
        // snake_case identifiers should read as words, not "underscore".
        assert_eq!(to_speech("call get_focused_app then"), "call get focused app then");
    }

    #[test]
    fn markdown_link_keeps_label() {
        assert_eq!(
            to_speech("Read [the docs](https://example.com/guide) please"),
            "Read the docs please"
        );
    }

    #[test]
    fn bare_url_becomes_spoken_host() {
        assert_eq!(
            to_speech("See https://github.com/Jeet-Desai/jarvis for code"),
            "See github dot com for code"
        );
        assert_eq!(to_speech("go to www.youtube.com now"), "go to youtube dot com now");
    }

    #[test]
    fn url_with_trailing_period() {
        // The sentence period must not become part of the host.
        assert_eq!(
            to_speech("It's at https://news.ycombinator.com."),
            "It's at news dot ycombinator dot com."
        );
    }

    #[test]
    fn headers_and_bullets() {
        assert_eq!(
            to_speech("# Title\n- one\n- two\n* three"),
            "Title\none\ntwo\nthree"
        );
    }

    #[test]
    fn fenced_code_is_dropped() {
        let out = to_speech("Here you go:\n```bash\nrm -rf /\n```\nDone.");
        assert!(!out.contains("rm -rf"), "code should not be spoken: {out:?}");
        assert!(out.contains("Here you go"));
        assert!(out.contains("Done."));
    }

    #[test]
    fn emoji_is_stripped() {
        assert_eq!(to_speech("All set ✅ shipping it 🚀"), "All set shipping it");
    }

    #[test]
    fn plain_text_is_untouched() {
        let plain = "Sure, I'll open Safari and search for that.";
        assert_eq!(to_speech(plain), plain);
    }

    #[test]
    fn empty_and_whitespace() {
        assert_eq!(to_speech(""), "");
        assert_eq!(to_speech("   \n\n  "), "");
    }

    #[test]
    fn collapses_excess_blank_lines() {
        assert_eq!(to_speech("a\n\n\n\nb"), "a\n\nb");
    }
}
