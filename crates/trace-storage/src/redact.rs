use std::borrow::Cow;

const MASK: &[u8] = b"[REDACTED]";

/// A token shape: a literal prefix followed by at least `min` bytes accepted by `body`.
struct Rule {
    prefix: &'static [u8],
    min: usize,
    keep_prefix: bool,
    body: fn(u8) -> bool,
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
fn token(b: u8) -> bool {
    word(b) || b == b'-'
}
fn upper(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}
fn bearer(b: u8) -> bool {
    token(b) || b"._~+/=".contains(&b)
}

const fn rule(prefix: &'static [u8], min: usize, body: fn(u8) -> bool) -> Rule {
    Rule { prefix, min, keep_prefix: false, body }
}

const RULES: &[Rule] = &[
    rule(b"sk-", 16, token),
    rule(b"AKIA", 16, upper),
    rule(b"github_pat_", 20, word),
    rule(b"ghp_", 20, word),
    rule(b"gho_", 20, word),
    rule(b"ghu_", 20, word),
    rule(b"ghs_", 20, word),
    rule(b"ghr_", 20, word),
    rule(b"xoxa-", 10, token),
    rule(b"xoxb-", 10, token),
    rule(b"xoxp-", 10, token),
    Rule { prefix: b"Bearer ", min: 8, keep_prefix: true, body: bearer },
    Rule { prefix: b"bearer ", min: 8, keep_prefix: true, body: bearer },
];

/// Replaces known secret shapes with `[REDACTED]`. Borrowed when nothing matched.
pub fn redact(s: &str) -> Cow<'_, str> {
    match redact_bytes(s.as_bytes()) {
        Cow::Borrowed(_) => Cow::Borrowed(s),
        Cow::Owned(v) => Cow::Owned(String::from_utf8(v).expect("only ASCII runs are replaced")),
    }
}

pub(crate) fn redact_bytes(s: &[u8]) -> Cow<'_, [u8]> {
    let mut out = Vec::new();
    let (mut copied, mut i) = (0, 0);
    while i < s.len() {
        match secret_at(s, i) {
            Some((start, end)) => {
                out.extend_from_slice(&s[copied..start]);
                out.extend_from_slice(MASK);
                (copied, i) = (end, end);
            }
            None => i += 1,
        }
    }
    if copied == 0 {
        return Cow::Borrowed(s);
    }
    out.extend_from_slice(&s[copied..]);
    Cow::Owned(out)
}

/// Byte range to mask when a secret starts at `i`.
fn secret_at(s: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = &s[i..];
    if rest.starts_with(b"-----BEGIN ") {
        return pem_block(s, i);
    }
    if i > 0 && word(s[i - 1]) {
        return None;
    }
    RULES.iter().find_map(|r| {
        if !rest.starts_with(r.prefix) {
            return None;
        }
        let body_start = i + r.prefix.len();
        let len = s[body_start..].iter().take_while(|b| (r.body)(**b)).count();
        (len >= r.min).then_some((if r.keep_prefix { body_start } else { i }, body_start + len))
    })
}

/// `-----BEGIN ... PRIVATE KEY-----` through its END line, or to the end of input when unterminated.
fn pem_block(s: &[u8], i: usize) -> Option<(usize, usize)> {
    let header_end = i + 11 + find(&s[i + 11..], b"-----")?;
    if !s[i..header_end].ends_with(b"PRIVATE KEY") {
        return None;
    }
    let end = find(&s[header_end..], b"-----END ")
        .and_then(|e| {
            let from = header_end + e + 9;
            find(&s[from..], b"-----").map(|f| from + f + 5)
        })
        .unwrap_or(s.len());
    Some((i, end))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
