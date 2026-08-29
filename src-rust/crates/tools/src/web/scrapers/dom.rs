// Shared HTML/XML DOM helpers for the scraper handlers that need CSS-selector
// access (wikipedia, arxiv, iacr, twitter, go-pkg, readthedocs). Wraps the
// `scraper` crate so each handler works in terms of trimmed text and attributes.

use scraper::{ElementRef, Html, Selector};

/// Parse an HTML (or lenient XML) document.
pub fn parse(html: &str) -> Html {
    Html::parse_document(html)
}

/// Compile a CSS selector, panicking only on a static, developer-authored
/// pattern (never on runtime input).
pub fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("static css selector")
}

/// `textContent` of an element, trimmed.
pub fn text(el: ElementRef<'_>) -> String {
    el.text().collect::<String>().trim().to_string()
}

/// `textContent` with internal whitespace runs collapsed to single spaces.
pub fn collapsed_text(el: ElementRef<'_>) -> String {
    el.text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The value of an attribute on an element, if present and non-empty.
pub fn attr<'a>(el: ElementRef<'a>, name: &str) -> Option<&'a str> {
    el.value().attr(name).filter(|s| !s.is_empty())
}

/// The lowercase tag name of an element (e.g. `h2`).
pub fn tag_name<'a>(el: ElementRef<'a>) -> &'a str {
    el.value().name()
}
