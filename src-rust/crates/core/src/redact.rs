//! Find and mask credentials in text that is about to be stored.
//!
//! The auto-memory directory is the reason this exists. A memory file enters
//! the system prompt of every later session in the same project, so a token
//! that reaches it is not written once: it is re-sent on every request, to
//! whichever provider that session happens to use, until somebody notices the
//! file. Writing the memory is the last moment where the text is still one
//! string in one process.
//!
//! Every pattern here anchors on a vendor prefix or on an assignment. That is
//! deliberate: a rule broad enough to catch an unknown credential by shape
//! alone also catches ordinary identifiers, and a search that masks
//! `keyboard_shortcuts_v2` teaches the reader to ignore `[REDACTED]`.

use regex::Regex;
use std::sync::LazyLock;

/// What replaces a match.
pub const PLACEHOLDER: &str = "[REDACTED]";

/// A pattern and the name reported when it fires.
struct Rule {
    class: &'static str,
    pattern: Regex,
    /// Which capture group holds the secret. `0` means the whole match.
    ///
    /// An assignment rule matches `token: <value>` so that the key word can
    /// carry the proof, but only the value is a secret; masking the key too
    /// would destroy the sentence the memory was written to record.
    group: usize,
}

impl Rule {
    fn new(class: &'static str, pattern: &str, group: usize) -> Option<Self> {
        match Regex::new(pattern) {
            Ok(pattern) => Some(Self {
                class,
                pattern,
                group,
            }),
            Err(error) => {
                // A rule that does not compile is a programming error, but it
                // must not take the process with it: the caller is a memory
                // writer, and the alternative to a missing rule is no memory.
                tracing::error!(class, %error, "secret-detection rule failed to compile");
                None
            }
        }
    }
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    [
        // Vendor prefixes. `sk-ant-` is listed before the shorter `sk-` so the
        // more specific class is the one reported.
        Rule::new("anthropic", r"sk-ant-[A-Za-z0-9_\-]{20,}", 0),
        Rule::new("openai", r"sk-[A-Za-z0-9]{20,}", 0),
        Rule::new("github", r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}", 0),
        Rule::new("github", r"github_pat_[A-Za-z0-9_]{20,}", 0),
        Rule::new("gitlab", r"glpat-[A-Za-z0-9_\-]{20,}", 0),
        Rule::new("npm", r"npm_[A-Za-z0-9]{30,}", 0),
        Rule::new("slack", r"xox[baprs]-[A-Za-z0-9\-]{10,}", 0),
        Rule::new("google", r"AIza[A-Za-z0-9_\-]{30,}", 0),
        Rule::new("aws", r"(?:AKIA|ASIA)[A-Z0-9]{16}", 0),
        Rule::new("huggingface", r"hf_[A-Za-z0-9]{30,}", 0),
        // A JWT anchored on the base64 of `{"alg"`, so an ordinary dotted
        // triple such as a version string or a package path stays put.
        Rule::new(
            "jwt",
            r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
            0,
        ),
        // The whole armoured block, newlines included, because the body is the
        // key and the header alone proves nothing.
        Rule::new(
            "private-key",
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            0,
        ),
        // The catch-all for a vendor nobody listed: a credential word, an
        // assignment, then a long opaque value. Only the value is masked.
        Rule::new(
            "assignment",
            r#"(?i)\b(?:api[_\-]?key|secret|token|passwd|password|credential)\b\s*[:=]\s*["']?([A-Za-z0-9_\-./+]{16,})["']?"#,
            1,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// The result of a redaction pass.
pub struct Redacted {
    pub text: String,
    /// Which classes fired, in rule order, without repeats. Empty means the
    /// text came through untouched.
    pub classes: Vec<&'static str>,
}

impl Redacted {
    /// Whether anything was masked.
    pub fn is_clean(&self) -> bool {
        self.classes.is_empty()
    }
}

/// Mask every credential this module recognises.
pub fn redact_secrets(input: &str) -> Redacted {
    let mut text = input.to_string();
    let mut classes: Vec<&'static str> = Vec::new();

    for rule in RULES.iter() {
        // Collect the byte ranges first, then splice from the end, so an
        // earlier replacement cannot move a later match's offsets.
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for captures in rule.pattern.captures_iter(&text) {
            if let Some(found) = captures.get(rule.group) {
                spans.push((found.start(), found.end()));
            }
        }

        if spans.is_empty() {
            continue;
        }
        if !classes.contains(&rule.class) {
            classes.push(rule.class);
        }
        for (start, end) in spans.into_iter().rev() {
            text.replace_range(start..end, PLACEHOLDER);
        }
    }

    Redacted { text, classes }
}

/// Which classes the text carries, without building a copy of it.
///
/// The write path uses this: a refusal names the class and never quotes the
/// value, so the error message does not become the next place the secret is
/// stored.
pub fn find_secrets(input: &str) -> Vec<&'static str> {
    let mut classes: Vec<&'static str> = Vec::new();
    for rule in RULES.iter() {
        if classes.contains(&rule.class) {
            continue;
        }
        if rule.pattern.is_match(input) {
            classes.push(rule.class);
        }
    }
    classes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked(input: &str) -> String {
        redact_secrets(input).text
    }

    /// A credential-shaped string, assembled at run time.
    ///
    /// Never write one of these out as a single literal. A scanner reads the
    /// source, not the program: a contiguous `hf_AAAA…` in a test file is a
    /// Hugging Face token as far as GitHub push protection is concerned, and
    /// the push is rejected. Splitting the prefix from the body means the
    /// pattern exists only in memory. `team_memory_sync.rs` does the same.
    fn shaped(prefix: &str, joint: &str, fill: usize) -> String {
        format!("{prefix}{joint}{}", "A".repeat(fill))
    }

    #[test]
    fn each_vendor_prefix_is_masked() {
        let cases = [
            ("anthropic", shaped("sk-", "ant-api03-", 24)),
            ("openai", shaped("sk", "-", 24)),
            ("github", shaped("ghp", "_", 30)),
            ("github", shaped("github", "_pat_", 24)),
            ("gitlab", shaped("glpat", "-", 24)),
            ("npm", shaped("npm", "_", 36)),
            ("slack", shaped("xox", "b-1111111111-", 12)),
            ("google", shaped("AIz", "a", 35)),
            ("aws", shaped("AKI", "A", 16)),
            ("huggingface", shaped("hf", "_", 36)),
        ];

        for (class, literal) in cases {
            let result = redact_secrets(&format!("the key is {literal} ok"));
            assert_eq!(
                result.text, "the key is [REDACTED] ok",
                "{class} was not masked"
            );
            assert!(
                result.classes.contains(&class),
                "{class} did not report itself: {:?}",
                result.classes
            );
        }
    }

    #[test]
    fn a_jwt_is_masked_but_a_dotted_triple_is_not() {
        let jwt = format!(
            "{}{}.{}.{}",
            "ey",
            "JhbGciOiJIUzI1NiJ9",
            "A".repeat(20),
            "B".repeat(20)
        );
        assert_eq!(masked(&format!("token {jwt}")), "token [REDACTED]");
        // A version string and a package path both look like `a.b.c`.
        let ordinary = "crates_core_memdir.find_relevant_memories.simple_scoring";
        assert_eq!(masked(ordinary), ordinary);
    }

    #[test]
    fn an_armoured_key_is_masked_whole() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nAAAA\nBBBB\n-----END RSA PRIVATE KEY-----";
        let result = redact_secrets(&format!("before\n{pem}\nafter"));
        assert_eq!(result.text, "before\n[REDACTED]\nafter");
        assert!(result.classes.contains(&"private-key"));
    }

    #[test]
    fn an_assignment_loses_its_value_and_keeps_its_key() {
        let result = redact_secrets("The setting is api_key = ABCDEFGHIJKLMNOPQRSTUV here.");
        assert_eq!(
            result.text, "The setting is api_key = [REDACTED] here.",
            "the key word carries the meaning and must survive"
        );
        assert!(result.classes.contains(&"assignment"));
    }

    /// The reference's broadest rule masks any word starting `key`/`token`/… .
    /// That rule would fire on all of these, and a reader who sees
    /// `[REDACTED]` in place of a module name stops trusting the marker.
    #[test]
    fn ordinary_prose_and_identifiers_are_left_alone() {
        for text in [
            "keyboard_shortcuts_v2 lives in the settings screen",
            "the token ring is a network topology",
            "password reset flows go through the auth store",
            "secretariat is a horse",
            "sk-8 is too short to be a key",
            "AKIA1234 is not sixteen characters",
        ] {
            let result = redact_secrets(text);
            assert!(
                result.is_clean(),
                "{text:?} was masked as {:?}",
                result.classes
            );
            assert_eq!(result.text, text);
        }
    }

    #[test]
    fn several_secrets_in_one_string_all_go() {
        let github = shaped("ghp", "_", 30);
        let aws = shaped("AKI", "A", 16);
        let result = redact_secrets(&format!("first {github} then {aws} done"));
        assert_eq!(result.text, "first [REDACTED] then [REDACTED] done");
        assert_eq!(result.classes, vec!["github", "aws"]);
    }

    #[test]
    fn a_repeated_class_is_reported_once() {
        let one = shaped("AKI", "A", 16);
        let two = shaped("ASI", "A", 16);
        let result = redact_secrets(&format!("{one} and {two}"));
        assert_eq!(result.classes, vec!["aws"]);
        assert_eq!(result.text, "[REDACTED] and [REDACTED]");
    }

    #[test]
    fn finding_does_not_quote_what_it_found() {
        let key = shaped("sk-", "ant-api03-", 24);
        assert_eq!(find_secrets(&format!("key {key}")), vec!["anthropic"]);
        assert!(find_secrets("nothing to see here").is_empty());
    }

    /// The two entry points must agree, or a write refused by one would be
    /// stored unmasked by the other.
    #[test]
    fn finding_and_masking_agree() {
        for text in [
            shaped("ghp", "_", 30),
            "plain text".to_string(),
            "password: ABCDEFGHIJKLMNOPQRSTUV".to_string(),
        ] {
            let text = text.as_str();
            assert_eq!(
                find_secrets(text).is_empty(),
                redact_secrets(text).is_clean(),
                "the two paths disagree about {text:?}"
            );
        }
    }
}
