// Structured web-search query parsing.
//
// Ported from oh-my-pi `web/search/query.ts`. Agents embed Google-style
// directives (`site:`, `before:`/`after:`, `inurl:`, `filetype:`, quoted
// phrases, `OR` groups, `-exclusions`) whether or not the backing engine
// parses them. This turns a raw query into a `StructuredQuery` so each provider
// can (1) map constraints onto native API parameters, (2) rebuild a query
// string containing only the syntax the target engine understands
// (`format_query`), and (3) post-filter sources leniently
// (`apply_query_constraints`): a dimension that would eliminate every result is
// dropped and reported rather than returning nothing.

use super::types::SearchSource;
use chrono::{NaiveDate, Utc};
use once_cell::sync::Lazy;
use regex::Regex;

/// One free-text token of the query (everything that is not a directive).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryTerm {
    /// Term text without quotes or operator prefixes.
    pub text: String,
    /// Quoted exact phrase (`"like this"`) or verbatim-required (`+term`).
    pub phrase: bool,
    /// Excluded via `-term` or `NOT term`.
    pub negated: bool,
    /// OR-group id. Terms sharing an id are alternatives (`a OR b`); terms
    /// without a group are implicitly AND-ed.
    pub group: Option<u32>,
}

/// A raw query decomposed into free text plus every recognized constraint.
#[derive(Debug, Clone, Default)]
pub struct StructuredQuery {
    /// Original query string, verbatim.
    pub raw: String,
    /// Free-text remainder with all directives removed.
    pub text: String,
    /// Ordered free-text terms (phrases, exclusions, OR groups).
    pub terms: Vec<QueryTerm>,
    /// `site:`/`domain:`/`host:` includes. Lowercased, scheme stripped, may carry a path.
    pub sites: Vec<String>,
    /// `-site:` exclusions, same normalization as `sites`.
    pub excluded_sites: Vec<String>,
    /// `inurl:`/`url:`/`allinurl:` substrings, all must appear in the URL.
    pub in_url: Vec<String>,
    /// `-inurl:` substrings, none may appear in the URL.
    pub excluded_in_url: Vec<String>,
    /// `intitle:`/`title:`/`allintitle:` substrings, all must appear in the title.
    pub in_title: Vec<String>,
    /// `-intitle:` substrings, none may appear in the title.
    pub excluded_in_title: Vec<String>,
    /// `intext:`/`inbody:`/`inanchor:`/`allintext:` body substrings, query-building only.
    pub in_text: Vec<String>,
    /// `-intext:` body exclusions, query-building only.
    pub excluded_in_text: Vec<String>,
    /// `filetype:`/`ext:` extensions. Lowercased, no leading dot.
    pub filetypes: Vec<String>,
    /// `-filetype:`/`-ext:` extensions, none may match.
    pub excluded_filetypes: Vec<String>,
    /// Inclusive lower publish-date bound from `after:`/`since:`, ISO `YYYY-MM-DD`.
    pub after: Option<String>,
    /// Exclusive upper publish-date bound from `before:`/`until:`, ISO `YYYY-MM-DD`.
    pub before: Option<String>,
    /// Language code from `lang:`/`language:`, lowercased.
    pub lang: Option<String>,
    /// True when any directive or boolean operator was recognized.
    pub has_directives: bool,
    /// True when any post-filterable constraint is set.
    pub has_constraints: bool,
}

/// Query-syntax capabilities of a target engine, used by [`format_query`] to
/// decide which parsed features are re-emitted as query text. Everything
/// defaults to `false`: the zero value produces plain keywords.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuerySyntax {
    pub phrases: bool,
    pub negation: bool,
    pub or: bool,
    pub site: bool,
    pub in_url: bool,
    pub in_title: bool,
    pub in_text: bool,
    pub filetype: bool,
    pub date_range: bool,
}

impl QuerySyntax {
    /// Full Google-style syntax for engines that parse the classic operator set.
    pub const fn google() -> Self {
        Self {
            phrases: true,
            negation: true,
            or: true,
            site: true,
            in_url: true,
            in_title: true,
            in_text: true,
            filetype: true,
            date_range: true,
        }
    }

    /// The syntax used to render the free-text remainder (`StructuredQuery::text`).
    const fn text_default() -> Self {
        Self {
            phrases: true,
            negation: true,
            or: true,
            ..Self::none()
        }
    }

    const fn none() -> Self {
        Self {
            phrases: false,
            negation: false,
            or: false,
            site: false,
            in_url: false,
            in_title: false,
            in_text: false,
            filetype: false,
            date_range: false,
        }
    }
}

/// Result of [`apply_query_constraints`].
#[derive(Debug, Clone, Default)]
pub struct ConstraintFilterResult {
    /// Sources surviving the lenient filter, never empty when the input was not.
    pub sources: Vec<SearchSource>,
    /// Directive renderings of the dimensions that matched zero sources and
    /// were therefore relaxed instead of enforced.
    pub dropped: Vec<String>,
}

static DIRECTIVE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^([+-]?)([a-z][a-z-]*):(.*)$").expect("static directive regex"));

/// Which structured field a directive name maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectiveField {
    Site,
    InUrl,
    InTitle,
    InText,
    Filetype,
    Before,
    After,
    Lang,
}

/// The subset of directive fields that [`QueryParser::push_constraint`] handles
/// (dates and language are stored directly, not through it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConstraintField {
    Site,
    InUrl,
    InTitle,
    InText,
    Filetype,
}

/// `allin*:` directives that capture every following plain term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllMode {
    Title,
    Url,
    Text,
}

impl AllMode {
    fn as_constraint(self) -> ConstraintField {
        match self {
            Self::Title => ConstraintField::InTitle,
            Self::Url => ConstraintField::InUrl,
            Self::Text => ConstraintField::InText,
        }
    }
}

fn directive_field(name: &str) -> Option<DirectiveField> {
    match name {
        "site" | "domain" | "host" => Some(DirectiveField::Site),
        "inurl" | "url" => Some(DirectiveField::InUrl),
        "intitle" | "title" => Some(DirectiveField::InTitle),
        "intext" | "inbody" | "inanchor" => Some(DirectiveField::InText),
        "filetype" | "ext" => Some(DirectiveField::Filetype),
        "before" | "until" => Some(DirectiveField::Before),
        "after" | "since" => Some(DirectiveField::After),
        "lang" | "language" => Some(DirectiveField::Lang),
        _ => None,
    }
}

fn all_mode(name: &str) -> Option<AllMode> {
    match name {
        "allintitle" => Some(AllMode::Title),
        "allinurl" => Some(AllMode::Url),
        "allintext" => Some(AllMode::Text),
        _ => None,
    }
}

fn is_quote(ch: char) -> bool {
    ch == '"' || ch == '\u{201c}' || ch == '\u{201d}'
}

/// A whitespace-delimited token, honoring quoted spans and standalone parens.
#[derive(Debug, Clone)]
struct RawToken {
    text: String,
    /// Entire token was a quoted phrase.
    quoted: bool,
    /// Directive value was quoted (`intitle:"a b"`).
    quoted_value: bool,
}

/// Split a raw query into tokens, honoring quoted spans and standalone parens.
fn tokenize(raw: &str) -> Vec<RawToken> {
    let chars: Vec<char> = raw.chars().collect();
    let n = chars.len();
    let mut tokens: Vec<RawToken> = Vec::new();
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if is_quote(ch) {
            i = take_phrase(&chars, i, &mut tokens);
            continue;
        }
        i = take_word(&chars, i, &mut tokens);
    }
    split_parens(tokens)
}

/// Consume a `"quoted phrase"` starting at the opening quote `i`.
fn take_phrase(chars: &[char], i: usize, tokens: &mut Vec<RawToken>) -> usize {
    let n = chars.len();
    let mut j = i + 1;
    let mut buf = String::new();
    while j < n && !is_quote(chars[j]) {
        buf.push(chars[j]);
        j += 1;
    }
    if !buf.trim().is_empty() {
        tokens.push(RawToken {
            text: buf.trim().to_string(),
            quoted: true,
            quoted_value: false,
        });
    }
    j + 1
}

/// Consume a bare word starting at `i`, swallowing a `name:"quoted value"` span.
fn take_word(chars: &[char], start: usize, tokens: &mut Vec<RawToken>) -> usize {
    let n = chars.len();
    let mut i = start;
    let mut buf = String::new();
    let mut quoted_value = false;
    while i < n && !chars[i].is_whitespace() {
        let c = chars[i];
        if is_quote(c) && buf.ends_with(':') {
            let mut j = i + 1;
            while j < n && !is_quote(chars[j]) {
                buf.push(chars[j]);
                j += 1;
            }
            quoted_value = true;
            i = j + 1;
            continue;
        }
        if is_quote(c) {
            break; // `foo"bar` — stop the word, let the quote start a phrase
        }
        buf.push(c);
        i += 1;
    }
    if !buf.is_empty() {
        tokens.push(RawToken {
            text: buf,
            quoted: false,
            quoted_value,
        });
    }
    i
}

/// Split leading `(` and unbalanced trailing `)` into standalone tokens so
/// `(react OR vue)` parses while `site:wikipedia.org/Foo_(bar)` stays whole.
fn split_parens(tokens: Vec<RawToken>) -> Vec<RawToken> {
    let mut out: Vec<RawToken> = Vec::new();
    for tok in tokens {
        if tok.quoted || tok.quoted_value {
            out.push(tok);
            continue;
        }
        let mut text = tok.text.as_str();
        while let Some(rest) = text.strip_prefix('(') {
            out.push(paren_token("("));
            text = rest;
        }
        let mut trailing = 0;
        while text.ends_with(')') && !closes_inner_opener(text) {
            text = &text[..text.len() - 1];
            trailing += 1;
        }
        if !text.is_empty() {
            out.push(paren_token(text));
        }
        for _ in 0..trailing {
            out.push(paren_token(")"));
        }
    }
    out
}

/// True when stripping the trailing `)` would close an opener inside the word.
fn closes_inner_opener(text: &str) -> bool {
    let body = &text[..text.len() - 1];
    let mut depth = 0i32;
    for c in body.chars() {
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
        }
    }
    depth > 0
}

fn paren_token(text: &str) -> RawToken {
    RawToken {
        text: text.to_string(),
        quoted: false,
        quoted_value: false,
    }
}

/// Convert year/month/day parts to a validated ISO date, or `None`.
fn iso_date(year: i32, month: u32, day: u32) -> Option<String> {
    if !(1000..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

static DATE_YMD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(\d{4})(?:[-/.](\d{1,2})(?:[-/.](\d{1,2}))?)?$").expect("static date regex")
});
static DATE_MDY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\d{1,2})[-/.](\d{1,2})[-/.](\d{4})$").expect("static date regex"));

/// Parse a `before:`/`after:` value into ISO `YYYY-MM-DD`.
///
/// Accepts `YYYY`, `YYYY-MM`, `YYYY-MM-DD` (also `/` and `.` separators) and
/// `MM/DD/YYYY` (day-first assumed when the first field exceeds 12). Bare
/// years/months resolve to the first day of the period.
pub fn parse_date_value(value: &str) -> Option<String> {
    let t = value.trim();
    if let Some(m) = DATE_YMD.captures(t) {
        let year: i32 = m.get(1)?.as_str().parse().ok()?;
        let month = m.get(2).map_or(1, |g| g.as_str().parse().unwrap_or(1));
        let day = m.get(3).map_or(1, |g| g.as_str().parse().unwrap_or(1));
        return iso_date(year, month, day);
    }
    if let Some(m) = DATE_MDY.captures(t) {
        let mut month: u32 = m.get(1)?.as_str().parse().ok()?;
        let mut day: u32 = m.get(2)?.as_str().parse().ok()?;
        let year: i32 = m.get(3)?.as_str().parse().ok()?;
        if month > 12 && day <= 12 {
            std::mem::swap(&mut month, &mut day);
        }
        return iso_date(year, month, day);
    }
    None
}

static SCHEME_PREFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://").expect("static scheme regex"));

/// Lowercase a `site:` value and strip scheme, `*.` wildcard, and trailing slash/dot.
fn normalize_site(value: &str) -> String {
    let mut site = value.trim().to_lowercase();
    site = SCHEME_PREFIX.replace(&site, "").into_owned();
    if let Some(rest) = site.strip_prefix("*.") {
        site = rest.to_string();
    }
    site.trim_end_matches(['/', '.']).to_string()
}

/// True for operator/paren tokens and recognized directives.
fn is_reserved_token(text: &str) -> bool {
    if matches!(
        text,
        "(" | ")" | "OR" | "AND" | "NOT" | "|" | "||" | "&&" | "!"
    ) {
        return true;
    }
    match DIRECTIVE_PATTERN.captures(text) {
        Some(m) => {
            let name = m
                .get(2)
                .map(|g| g.as_str().to_lowercase())
                .unwrap_or_default();
            directive_field(&name).is_some() || all_mode(&name).is_some()
        }
        None => false,
    }
}

/// Incremental state machine building a [`StructuredQuery`] from tokens.
struct QueryParser {
    q: StructuredQuery,
    negate_next: bool,
    or_pending: bool,
    last_was_term: bool,
    group_seq: u32,
    all_mode: Option<AllMode>,
}

impl QueryParser {
    fn new(raw: &str) -> Self {
        Self {
            q: StructuredQuery {
                raw: raw.to_string(),
                ..Default::default()
            },
            negate_next: false,
            or_pending: false,
            last_was_term: false,
            group_seq: 0,
            all_mode: None,
        }
    }

    fn push_constraint(&mut self, field: ConstraintField, value: &str, negated: bool) {
        self.q.has_directives = true;
        self.or_pending = false;
        self.last_was_term = false;
        let v = value.trim();
        if v.is_empty() {
            return;
        }
        match field {
            ConstraintField::Site => {
                let site = normalize_site(v);
                if !site.is_empty() {
                    target(&mut self.q.sites, &mut self.q.excluded_sites, negated).push(site);
                }
            }
            ConstraintField::InUrl => {
                target(&mut self.q.in_url, &mut self.q.excluded_in_url, negated)
                    .push(v.to_string());
            }
            ConstraintField::InTitle => {
                target(&mut self.q.in_title, &mut self.q.excluded_in_title, negated)
                    .push(v.to_string());
            }
            ConstraintField::InText => {
                target(&mut self.q.in_text, &mut self.q.excluded_in_text, negated)
                    .push(v.to_string());
            }
            ConstraintField::Filetype => {
                let ext = v.trim_start_matches('.').to_lowercase();
                if !ext.is_empty() {
                    target(
                        &mut self.q.filetypes,
                        &mut self.q.excluded_filetypes,
                        negated,
                    )
                    .push(ext);
                }
            }
        }
    }

    fn push_term(&mut self, text: &str, phrase: bool) {
        let negated = self.negate_next;
        self.negate_next = false;
        if let Some(mode) = self.all_mode {
            self.push_constraint(mode.as_constraint(), text, negated);
            return;
        }
        let mut term = QueryTerm {
            text: text.to_string(),
            phrase,
            negated,
            group: None,
        };
        if self.or_pending && self.last_was_term {
            if let Some(prev) = self.q.terms.last_mut() {
                let group = *prev.group.get_or_insert_with(|| {
                    self.group_seq += 1;
                    self.group_seq
                });
                term.group = Some(group);
            }
        }
        self.or_pending = false;
        self.last_was_term = true;
        self.q.terms.push(term);
    }

    /// Consume the token at `idx`; returns how many extra tokens were consumed
    /// (a bare `site: value` adopts the following token, returning 1).
    fn step(&mut self, tokens: &[RawToken], idx: usize) -> usize {
        let tok = &tokens[idx];
        if tok.quoted {
            self.push_term(&tok.text, true);
            return 0;
        }
        if self.try_operator(&tok.text, tokens, idx) {
            return 0;
        }
        if let Some(consumed) = self.try_directive(tok, tokens, idx) {
            return consumed;
        }
        self.handle_plain(&tok.text);
        0
    }

    /// Boolean operators and grouping parens; returns true when handled.
    fn try_operator(&mut self, text: &str, tokens: &[RawToken], idx: usize) -> bool {
        match text {
            "(" | ")" => true,
            "OR" | "|" | "||" => {
                self.or_pending = true;
                self.q.has_directives = true;
                true
            }
            "AND" | "&&" => {
                self.q.has_directives = true;
                true
            }
            "NOT" | "!" => {
                self.negate_next = true;
                self.q.has_directives = true;
                true
            }
            "-" | "+" => {
                if text == "-" && tokens.get(idx + 1).is_some_and(|t| t.quoted) {
                    self.negate_next = true;
                }
                true
            }
            _ => false,
        }
    }

    /// A `name:value` directive; returns `Some(extra_consumed)` when handled.
    fn try_directive(&mut self, tok: &RawToken, tokens: &[RawToken], idx: usize) -> Option<usize> {
        let m = DIRECTIVE_PATTERN.captures(&tok.text)?;
        let sign = m.get(1).map(|g| g.as_str()).unwrap_or("");
        let name = m.get(2)?.as_str().to_lowercase();
        let inline = m.get(3).map(|g| g.as_str().trim()).unwrap_or("");

        if let Some(mode) = all_mode(&name) {
            self.all_mode = Some(mode);
            self.q.has_directives = true;
            if !inline.is_empty() {
                self.push_constraint(mode.as_constraint(), inline, sign == "-");
            }
            self.or_pending = false;
            self.last_was_term = false;
            return Some(0);
        }

        let field = directive_field(&name)?;
        let (value, consumed) = self.resolve_directive_value(inline, tokens, idx);
        if value.is_empty() {
            self.q.has_directives = true;
            return Some(consumed);
        }
        let negated = sign == "-" || self.negate_next;
        self.negate_next = false;
        self.apply_directive(field, &tok.text, &value, negated);
        Some(consumed)
    }

    /// The directive value, adopting the next plain token when `inline` is empty.
    fn resolve_directive_value(
        &self,
        inline: &str,
        tokens: &[RawToken],
        idx: usize,
    ) -> (String, usize) {
        if !inline.is_empty() {
            return (inline.to_string(), 0);
        }
        if let Some(next) = tokens.get(idx + 1) {
            if next.quoted || !is_reserved_token(&next.text) {
                return (next.text.trim().to_string(), 1);
            }
        }
        (String::new(), 0)
    }

    fn apply_directive(
        &mut self,
        field: DirectiveField,
        raw_tok: &str,
        value: &str,
        negated: bool,
    ) {
        match field {
            DirectiveField::Before | DirectiveField::After => match parse_date_value(value) {
                Some(iso) => {
                    if field == DirectiveField::Before {
                        self.q.before = Some(iso);
                    } else {
                        self.q.after = Some(iso);
                    }
                    self.q.has_directives = true;
                    self.or_pending = false;
                    self.last_was_term = false;
                }
                None => self.push_term(raw_tok, false),
            },
            DirectiveField::Lang => {
                self.q.lang = Some(value.to_lowercase());
                self.q.has_directives = true;
                self.or_pending = false;
                self.last_was_term = false;
            }
            DirectiveField::Site => self.push_constraint(ConstraintField::Site, value, negated),
            DirectiveField::InUrl => self.push_constraint(ConstraintField::InUrl, value, negated),
            DirectiveField::InTitle => {
                self.push_constraint(ConstraintField::InTitle, value, negated)
            }
            DirectiveField::InText => self.push_constraint(ConstraintField::InText, value, negated),
            DirectiveField::Filetype => {
                self.push_constraint(ConstraintField::Filetype, value, negated)
            }
        }
    }

    /// A plain term with an optional `+`/`-` prefix.
    fn handle_plain(&mut self, text: &str) {
        if let Some(rest) = strip_leading(text, '-') {
            self.negate_next = true;
            self.q.has_directives = true;
            if rest.is_empty() {
                return;
            }
            if self.try_negated_directive(rest) {
                return;
            }
            self.push_term(rest, false);
            return;
        }
        if let Some(rest) = strip_leading(text, '+') {
            self.push_term(rest, true);
            self.q.has_directives = true;
            return;
        }
        self.push_term(text, false);
    }

    /// `-site:x` written as a single `-`-prefixed token; returns true when handled.
    fn try_negated_directive(&mut self, rest: &str) -> bool {
        let Some(m) = DIRECTIVE_PATTERN.captures(rest) else {
            return false;
        };
        let name = m
            .get(2)
            .map(|g| g.as_str().to_lowercase())
            .unwrap_or_default();
        let field = directive_field(&name);
        let constraint = match field {
            Some(DirectiveField::Site) => ConstraintField::Site,
            Some(DirectiveField::InUrl) => ConstraintField::InUrl,
            Some(DirectiveField::InTitle) => ConstraintField::InTitle,
            Some(DirectiveField::InText) => ConstraintField::InText,
            Some(DirectiveField::Filetype) => ConstraintField::Filetype,
            _ => return false,
        };
        self.negate_next = false;
        let value = m.get(3).map(|g| g.as_str().trim()).unwrap_or("");
        self.push_constraint(constraint, value, true);
        true
    }

    fn finish(mut self) -> StructuredQuery {
        self.q.text = render_terms(&self.q.terms, QuerySyntax::text_default());
        self.q.has_constraints = !self.q.sites.is_empty()
            || !self.q.excluded_sites.is_empty()
            || !self.q.in_url.is_empty()
            || !self.q.excluded_in_url.is_empty()
            || !self.q.in_title.is_empty()
            || !self.q.excluded_in_title.is_empty()
            || !self.q.filetypes.is_empty()
            || !self.q.excluded_filetypes.is_empty()
            || self.q.before.is_some()
            || self.q.after.is_some();
        self.q
    }
}

/// The include or exclude list, chosen by `negated`.
fn target<'a>(
    include: &'a mut Vec<String>,
    exclude: &'a mut Vec<String>,
    negated: bool,
) -> &'a mut Vec<String> {
    if negated {
        exclude
    } else {
        include
    }
}

/// Strip a single leading `sign` when at least one other char follows.
fn strip_leading(text: &str, sign: char) -> Option<&str> {
    let rest = text.strip_prefix(sign)?;
    if rest.is_empty() {
        return None;
    }
    // `--term` collapses to `term` for `-`, matching the original's `^-+` strip.
    if sign == '-' {
        return Some(rest.trim_start_matches('-'));
    }
    Some(rest)
}

/// Parse a raw query into a [`StructuredQuery`].
///
/// Lenient by construction: unknown `name:value` tokens (URLs, `C:\paths`,
/// `TS2345:`) stay in the free text, and a directive with an unparseable value
/// degrades to a plain term instead of being dropped.
pub fn parse_search_query(raw: &str) -> StructuredQuery {
    let tokens = tokenize(raw);
    let mut parser = QueryParser::new(raw);
    let mut idx = 0;
    while idx < tokens.len() {
        idx += 1 + parser.step(&tokens, idx);
    }
    parser.finish()
}

/// Quote a directive value when it contains whitespace.
fn quote_value(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

fn render_term(term: &QueryTerm, syntax: QuerySyntax) -> Option<String> {
    if term.negated && !syntax.negation {
        return None;
    }
    let body = if term.phrase && syntax.phrases {
        format!("\"{}\"", term.text)
    } else {
        term.text.clone()
    };
    Some(if term.negated {
        format!("-{body}")
    } else {
        body
    })
}

/// Render the free-text terms per the target syntax.
fn render_terms(terms: &[QueryTerm], syntax: QuerySyntax) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < terms.len() {
        let term = &terms[i];
        if term.group.is_some() && syntax.or {
            let (rendered, next) = render_or_group(terms, i, term.group, syntax);
            if let Some(rendered) = rendered {
                parts.push(rendered);
            }
            i = next;
            continue;
        }
        if let Some(rendered) = render_term(term, syntax) {
            parts.push(rendered);
        }
        i += 1;
    }
    parts.join(" ")
}

/// Render a contiguous OR-group run; returns the text and the next index.
fn render_or_group(
    terms: &[QueryTerm],
    start: usize,
    group: Option<u32>,
    syntax: QuerySyntax,
) -> (Option<String>, usize) {
    let mut members: Vec<String> = Vec::new();
    let mut j = start;
    while j < terms.len() && terms[j].group == group {
        if let Some(rendered) = render_term(&terms[j], syntax) {
            members.push(rendered);
        }
        j += 1;
    }
    let rendered = match members.len() {
        0 => None,
        1 => Some(members.remove(0)),
        _ => Some(format!("({})", members.join(" OR "))),
    };
    (rendered, j)
}

/// Rebuild a query string for an engine with the given [`QuerySyntax`].
///
/// Constraints whose syntax the engine lacks are omitted. Never returns an
/// empty string for a non-empty input: a directives-only query falls back to
/// the constraint values as keywords, then to `raw`.
pub fn format_query(q: &StructuredQuery, syntax: QuerySyntax) -> String {
    let mut parts: Vec<String> = Vec::new();
    let text = render_terms(&q.terms, syntax);
    if !text.is_empty() {
        parts.push(text);
    }
    append_site_parts(&mut parts, q, syntax);
    append_affix_parts(&mut parts, q, syntax);
    append_filetype_parts(&mut parts, q, syntax);
    if syntax.date_range {
        if let Some(after) = &q.after {
            parts.push(format!("after:{after}"));
        }
        if let Some(before) = &q.before {
            parts.push(format!("before:{before}"));
        }
    }

    let result = parts.join(" ").trim().to_string();
    if !result.is_empty() {
        return result;
    }
    let fallback: Vec<String> = q
        .sites
        .iter()
        .chain(&q.in_title)
        .chain(&q.in_url)
        .chain(&q.in_text)
        .chain(&q.filetypes)
        .cloned()
        .collect();
    let joined = fallback.join(" ").trim().to_string();
    if joined.is_empty() {
        q.raw.trim().to_string()
    } else {
        joined
    }
}

fn append_site_parts(parts: &mut Vec<String>, q: &StructuredQuery, syntax: QuerySyntax) {
    if !syntax.site {
        return;
    }
    if q.sites.len() > 1 && syntax.or {
        let group: Vec<String> = q.sites.iter().map(|s| format!("site:{s}")).collect();
        parts.push(format!("({})", group.join(" OR ")));
    } else {
        parts.extend(q.sites.iter().map(|s| format!("site:{s}")));
    }
    parts.extend(q.excluded_sites.iter().map(|s| format!("-site:{s}")));
}

fn append_affix_parts(parts: &mut Vec<String>, q: &StructuredQuery, syntax: QuerySyntax) {
    if syntax.in_url {
        parts.extend(q.in_url.iter().map(|v| format!("inurl:{}", quote_value(v))));
        parts.extend(
            q.excluded_in_url
                .iter()
                .map(|v| format!("-inurl:{}", quote_value(v))),
        );
    }
    if syntax.in_title {
        parts.extend(
            q.in_title
                .iter()
                .map(|v| format!("intitle:{}", quote_value(v))),
        );
        parts.extend(
            q.excluded_in_title
                .iter()
                .map(|v| format!("-intitle:{}", quote_value(v))),
        );
    }
    if syntax.in_text {
        parts.extend(
            q.in_text
                .iter()
                .map(|v| format!("intext:{}", quote_value(v))),
        );
        parts.extend(
            q.excluded_in_text
                .iter()
                .map(|v| format!("-intext:{}", quote_value(v))),
        );
    }
}

fn append_filetype_parts(parts: &mut Vec<String>, q: &StructuredQuery, syntax: QuerySyntax) {
    if !syntax.filetype {
        return;
    }
    if q.filetypes.len() > 1 && syntax.or {
        let group: Vec<String> = q
            .filetypes
            .iter()
            .map(|f| format!("filetype:{f}"))
            .collect();
        parts.push(format!("({})", group.join(" OR ")));
    } else {
        parts.extend(q.filetypes.iter().map(|f| format!("filetype:{f}")));
    }
    parts.extend(
        q.excluded_filetypes
            .iter()
            .map(|f| format!("-filetype:{f}")),
    );
}

/// Build the engine query for a credential-free HTML engine.
///
/// Canonicalizes directives via [`format_query`] after demoting the operators
/// that zero-match across the scraper set: path-carrying `site:` and every
/// `inurl:` become plain keywords, while bare-domain `site:` filters are kept.
/// Negated forms pass through untouched. Directive-free queries pass through
/// byte-identical.
pub fn format_scraper_query(query: &str, parsed: &StructuredQuery, syntax: QuerySyntax) -> String {
    if !parsed.has_directives {
        return query.to_string();
    }
    let mut demoted: Vec<String> = parsed
        .sites
        .iter()
        .filter(|s| s.contains('/'))
        .cloned()
        .collect();
    demoted.extend(parsed.in_url.iter().cloned());

    let mut downgraded = parsed.clone();
    downgraded.sites = parsed
        .sites
        .iter()
        .filter(|s| !s.contains('/'))
        .cloned()
        .collect();
    downgraded.in_url = Vec::new();
    downgraded
        .terms
        .extend(demoted.into_iter().map(|text| QueryTerm {
            text,
            ..Default::default()
        }));
    format_query(&downgraded, syntax)
}

/// Hostname (lowercased) and pathname of a URL, or `None` when unparsable.
fn host_and_path(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_lowercase();
    Some((host, parsed.path().to_string()))
}

/// `site:` matcher: exact host or subdomain of `site`; when `site` carries a
/// path, the URL path must start with it.
pub fn matches_site(url: &str, site: &str) -> bool {
    let Some((host, path)) = host_and_path(url) else {
        return false;
    };
    let (site_host, site_path) = match site.find('/') {
        Some(slash) => (&site[..slash], &site[slash..]),
        None => (site, ""),
    };
    if host != site_host && !host.ends_with(&format!(".{site_host}")) {
        return false;
    }
    if !site_path.is_empty() && !path.to_lowercase().starts_with(&site_path.to_lowercase()) {
        return false;
    }
    true
}

/// `filetype:` matcher: URL pathname ends with `.ext`.
fn matches_filetype(url: &str, ext: &str) -> bool {
    match host_and_path(url) {
        Some((_, path)) => path.to_lowercase().ends_with(&format!(".{ext}")),
        None => false,
    }
}

static RELATIVE_AGE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(\d+)\s*(minute|min|hour|hr|day|week|month|mo|year|yr|[mhdwy])s?\s+ago$")
        .expect("static relative-age regex")
});

fn relative_unit_seconds(unit: &str) -> Option<f64> {
    let s = match unit {
        "m" | "min" | "minute" => 60.0,
        "h" | "hr" | "hour" => 3600.0,
        "d" | "day" => 86_400.0,
        "w" | "week" => 604_800.0,
        "mo" | "month" => 2_592_000.0,
        "y" | "yr" | "year" => 31_536_000.0,
        _ => return None,
    };
    Some(s)
}

fn parse_iso_ms(value: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(dt.timestamp_millis());
    }
    let date = NaiveDate::parse_from_str(value.get(..10)?, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

/// Best-effort publish time (ms epoch) from `age_seconds`, ISO, or relative dates.
fn source_time(source: &SearchSource) -> Option<i64> {
    let now = Utc::now().timestamp_millis();
    if let Some(age) = source.age_seconds {
        if age.is_finite() {
            return Some(now - (age * 1000.0) as i64);
        }
    }
    let published = source.published_date.as_ref()?;
    if let Some(rel) = RELATIVE_AGE.captures(published.trim()) {
        let count: f64 = rel.get(1)?.as_str().parse().ok()?;
        let unit = rel.get(2)?.as_str().to_lowercase();
        let seconds = count * relative_unit_seconds(&unit)?;
        return if seconds > 0.0 {
            Some(now - (seconds * 1000.0) as i64)
        } else {
            None
        };
    }
    parse_iso_ms(published)
}

/// A single filterable constraint dimension: a label plus a predicate.
struct ConstraintDimension {
    label: String,
    pred: Box<dyn Fn(&SearchSource) -> bool>,
}

fn constraint_dimensions(q: &StructuredQuery) -> Vec<ConstraintDimension> {
    let mut dims: Vec<ConstraintDimension> = Vec::new();
    push_site_dimensions(&mut dims, q);
    push_affix_dimensions(&mut dims, q);
    push_filetype_dimensions(&mut dims, q);
    push_date_dimension(&mut dims, q);
    dims
}

fn push_site_dimensions(dims: &mut Vec<ConstraintDimension>, q: &StructuredQuery) {
    if !q.sites.is_empty() {
        let sites = q.sites.clone();
        dims.push(ConstraintDimension {
            label: join_labels(&q.sites, "site:", " OR "),
            pred: Box::new(move |src| sites.iter().any(|s| matches_site(&src.url, s))),
        });
    }
    if !q.excluded_sites.is_empty() {
        let sites = q.excluded_sites.clone();
        dims.push(ConstraintDimension {
            label: join_labels(&q.excluded_sites, "-site:", " "),
            pred: Box::new(move |src| !sites.iter().any(|s| matches_site(&src.url, s))),
        });
    }
}

fn push_affix_dimensions(dims: &mut Vec<ConstraintDimension>, q: &StructuredQuery) {
    if !q.in_url.is_empty() {
        let vals = lowered(&q.in_url);
        dims.push(ConstraintDimension {
            label: join_labels(&q.in_url, "inurl:", " "),
            pred: Box::new(move |src| {
                let url = src.url.to_lowercase();
                vals.iter().all(|v| url.contains(v))
            }),
        });
    }
    if !q.excluded_in_url.is_empty() {
        let vals = lowered(&q.excluded_in_url);
        dims.push(ConstraintDimension {
            label: join_labels(&q.excluded_in_url, "-inurl:", " "),
            pred: Box::new(move |src| {
                let url = src.url.to_lowercase();
                !vals.iter().any(|v| url.contains(v))
            }),
        });
    }
    if !q.in_title.is_empty() {
        let vals = lowered(&q.in_title);
        dims.push(ConstraintDimension {
            label: join_labels(&q.in_title, "intitle:", " "),
            pred: Box::new(move |src| {
                let title = src.title.to_lowercase();
                vals.iter().all(|v| title.contains(v))
            }),
        });
    }
    if !q.excluded_in_title.is_empty() {
        let vals = lowered(&q.excluded_in_title);
        dims.push(ConstraintDimension {
            label: join_labels(&q.excluded_in_title, "-intitle:", " "),
            pred: Box::new(move |src| {
                let title = src.title.to_lowercase();
                !vals.iter().any(|v| title.contains(v))
            }),
        });
    }
}

fn push_filetype_dimensions(dims: &mut Vec<ConstraintDimension>, q: &StructuredQuery) {
    if !q.filetypes.is_empty() {
        let exts = q.filetypes.clone();
        dims.push(ConstraintDimension {
            label: join_labels(&q.filetypes, "filetype:", " OR "),
            pred: Box::new(move |src| exts.iter().any(|e| matches_filetype(&src.url, e))),
        });
    }
    if !q.excluded_filetypes.is_empty() {
        let exts = q.excluded_filetypes.clone();
        dims.push(ConstraintDimension {
            label: join_labels(&q.excluded_filetypes, "-filetype:", " "),
            pred: Box::new(move |src| !exts.iter().any(|e| matches_filetype(&src.url, e))),
        });
    }
}

fn push_date_dimension(dims: &mut Vec<ConstraintDimension>, q: &StructuredQuery) {
    if q.after.is_none() && q.before.is_none() {
        return;
    }
    let after_ms = q.after.as_deref().and_then(parse_iso_ms);
    let before_ms = q.before.as_deref().and_then(parse_iso_ms);
    let mut label_parts: Vec<String> = Vec::new();
    if let Some(after) = &q.after {
        label_parts.push(format!("after:{after}"));
    }
    if let Some(before) = &q.before {
        label_parts.push(format!("before:{before}"));
    }
    dims.push(ConstraintDimension {
        label: label_parts.join(" "),
        pred: Box::new(move |src| {
            let Some(time) = source_time(src) else {
                return true; // undated → cannot prove violation
            };
            if after_ms.is_some_and(|a| time < a) {
                return false;
            }
            if before_ms.is_some_and(|b| time >= b) {
                return false;
            }
            true
        }),
    });
}

fn lowered(values: &[String]) -> Vec<String> {
    values.iter().map(|v| v.to_lowercase()).collect()
}

fn join_labels(values: &[String], prefix: &str, sep: &str) -> String {
    values
        .iter()
        .map(|v| format!("{prefix}{v}"))
        .collect::<Vec<_>>()
        .join(sep)
}

/// Strict per-source constraint check: every filterable dimension must pass.
/// Sources without a resolvable date pass date bounds.
pub fn matches_query_constraints(source: &SearchSource, q: &StructuredQuery) -> bool {
    constraint_dimensions(q)
        .iter()
        .all(|dim| (dim.pred)(source))
}

/// Lenient post-filter: applies each constraint dimension in turn, skipping and
/// reporting any dimension that would eliminate every remaining source.
/// Guarantees a non-empty result for a non-empty input.
pub fn apply_query_constraints(
    sources: &[SearchSource],
    q: &StructuredQuery,
) -> ConstraintFilterResult {
    let mut current: Vec<SearchSource> = sources.to_vec();
    let mut dropped: Vec<String> = Vec::new();
    if current.is_empty() {
        return ConstraintFilterResult {
            sources: current,
            dropped,
        };
    }
    for dim in constraint_dimensions(q) {
        let kept: Vec<SearchSource> = current.iter().filter(|s| (dim.pred)(s)).cloned().collect();
        if kept.is_empty() {
            dropped.push(dim.label);
        } else {
            current = kept;
        }
    }
    ConstraintFilterResult {
        sources: current,
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(title: &str, url: &str) -> SearchSource {
        SearchSource {
            title: title.into(),
            url: url.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_plain_query_has_no_directives() {
        let q = parse_search_query("rust ownership rules");
        assert!(!q.has_directives);
        assert!(!q.has_constraints);
        assert_eq!(q.terms.len(), 3);
        assert_eq!(q.text, "rust ownership rules");
    }

    #[test]
    fn site_and_filetype_directives_parse_into_constraints() {
        let q = parse_search_query("react hooks site:reactjs.org filetype:pdf");
        assert_eq!(q.sites, vec!["reactjs.org"]);
        assert_eq!(q.filetypes, vec!["pdf"]);
        assert!(q.has_constraints);
        assert!(q.has_directives);
        // The free text drops the directives.
        assert_eq!(q.text, "react hooks");
    }

    #[test]
    fn a_bare_site_adopts_the_next_token_as_its_value() {
        let q = parse_search_query("budget site: example.com tips");
        assert_eq!(q.sites, vec!["example.com"]);
        assert_eq!(q.text, "budget tips");
    }

    #[test]
    fn negated_site_becomes_an_exclusion() {
        let q = parse_search_query("cats -site:pinterest.com");
        assert_eq!(q.excluded_sites, vec!["pinterest.com"]);
        assert!(q.sites.is_empty());
    }

    #[test]
    fn or_groups_terms_into_a_shared_group() {
        let q = parse_search_query("react OR vue OR svelte");
        let groups: Vec<Option<u32>> = q.terms.iter().map(|t| t.group).collect();
        assert_eq!(groups, vec![Some(1), Some(1), Some(1)]);
    }

    #[test]
    fn a_quoted_phrase_stays_together() {
        let q = parse_search_query("\"exact phrase\" loose");
        assert!(q.terms[0].phrase);
        assert_eq!(q.terms[0].text, "exact phrase");
        assert!(!q.terms[1].phrase);
    }

    #[test]
    fn allintitle_captures_every_following_term() {
        let q = parse_search_query("allintitle: budget tips");
        assert_eq!(q.in_title, vec!["budget", "tips"]);
    }

    #[test]
    fn date_directives_parse_to_iso_and_degrade_when_unparseable() {
        let q = parse_search_query("news after:2024 before:2024-06-30");
        assert_eq!(q.after.as_deref(), Some("2024-01-01"));
        assert_eq!(q.before.as_deref(), Some("2024-06-30"));

        let bad = parse_search_query("before:someday");
        assert!(bad.before.is_none());
        assert!(bad.terms.iter().any(|t| t.text == "before:someday"));
    }

    #[test]
    fn parse_date_value_covers_the_accepted_shapes() {
        assert_eq!(parse_date_value("2024").as_deref(), Some("2024-01-01"));
        assert_eq!(parse_date_value("2024-03").as_deref(), Some("2024-03-01"));
        assert_eq!(
            parse_date_value("2024/03/15").as_deref(),
            Some("2024-03-15")
        );
        assert_eq!(
            parse_date_value("03/15/2024").as_deref(),
            Some("2024-03-15")
        );
        // Day-first when the first field exceeds 12.
        assert_eq!(
            parse_date_value("15/03/2024").as_deref(),
            Some("2024-03-15")
        );
        assert_eq!(parse_date_value("nonsense"), None);
    }

    #[test]
    fn an_unknown_colon_token_stays_in_free_text() {
        let q = parse_search_query("error TS2345: bad code");
        assert!(q.sites.is_empty());
        assert!(q.terms.iter().any(|t| t.text == "TS2345:"));
    }

    #[test]
    fn format_query_reemits_only_supported_syntax() {
        let q = parse_search_query("react site:reactjs.org filetype:pdf");
        let plain = format_query(&q, QuerySyntax::default());
        assert_eq!(plain, "react");
        let google = format_query(&q, QuerySyntax::google());
        assert!(google.contains("site:reactjs.org"));
        assert!(google.contains("filetype:pdf"));
    }

    #[test]
    fn format_query_never_returns_empty_for_a_directives_only_query() {
        let q = parse_search_query("site:reactjs.org");
        let plain = format_query(&q, QuerySyntax::default());
        assert_eq!(plain, "reactjs.org");
    }

    #[test]
    fn format_scraper_query_demotes_path_sites_and_inurl() {
        let q = parse_search_query("api site:github.com/anthropics inurl:blob");
        let scraped = format_scraper_query(
            "api site:github.com/anthropics inurl:blob",
            &q,
            QuerySyntax::google(),
        );
        // Path-carrying site and inurl become plain keywords.
        assert!(scraped.contains("github.com/anthropics"));
        assert!(scraped.contains("blob"));
        assert!(!scraped.contains("site:github.com/anthropics"));
        assert!(!scraped.contains("inurl:"));
    }

    #[test]
    fn matches_site_handles_subdomains_and_paths() {
        assert!(matches_site("https://docs.reactjs.org/x", "reactjs.org"));
        assert!(matches_site("https://reactjs.org/x", "reactjs.org"));
        assert!(!matches_site("https://evil.com", "reactjs.org"));
        assert!(matches_site(
            "https://github.com/anthropics/x",
            "github.com/anthropics"
        ));
        assert!(!matches_site(
            "https://github.com/other/x",
            "github.com/anthropics"
        ));
    }

    #[test]
    fn apply_constraints_relaxes_a_dimension_that_would_empty_results() {
        let sources = vec![
            src("A", "https://reactjs.org/a"),
            src("B", "https://vuejs.org/b"),
        ];
        // A site filter that matches nothing is relaxed, not enforced.
        let q = parse_search_query("x site:angular.io");
        let result = apply_query_constraints(&sources, &q);
        assert_eq!(result.sources.len(), 2);
        assert_eq!(result.dropped, vec!["site:angular.io"]);
    }

    #[test]
    fn apply_constraints_keeps_a_dimension_that_matches() {
        let sources = vec![
            src("A", "https://reactjs.org/a"),
            src("B", "https://vuejs.org/b"),
        ];
        let q = parse_search_query("x site:reactjs.org");
        let result = apply_query_constraints(&sources, &q);
        assert_eq!(result.sources.len(), 1);
        assert_eq!(result.sources[0].url, "https://reactjs.org/a");
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn an_empty_input_stays_empty() {
        let q = parse_search_query("site:reactjs.org");
        let result = apply_query_constraints(&[], &q);
        assert!(result.sources.is_empty());
        assert!(result.dropped.is_empty());
    }
}
