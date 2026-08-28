// Concrete search providers, one module per backend.
//
// Each implements the `SearchProvider` trait from the parent `provider`
// module, mapping its native wire response onto a `SearchResponse`.

pub mod brave;
pub mod duckduckgo;
pub mod exa;
pub mod firecrawl;
pub mod jina;
pub mod searxng;
pub mod synthetic;
pub mod tavily;
pub mod tinyfish;

/// Minimal percent-encoding for a URL query-parameter value.
pub(crate) fn urlencode(s: &str) -> String {
    let mut encoded = String::new();
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => encoded.push(ch),
            ' ' => encoded.push('+'),
            _ => {
                for byte in ch.to_string().as_bytes() {
                    encoded.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::urlencode;

    #[test]
    fn urlencode_maps_spaces_and_escapes_reserved_bytes() {
        assert_eq!(urlencode("rust ownership"), "rust+ownership");
        assert_eq!(urlencode("a/b?c"), "a%2Fb%3Fc");
        assert_eq!(urlencode("crate.io~1_x-y"), "crate.io~1_x-y");
    }
}
