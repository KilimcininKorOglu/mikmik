// Web tooling internals shared by the `web_search` and `web_fetch` tools.
//
// Ported from oh-my-pi's `packages/coding-agent/src/web/` tree: a search
// provider registry with an auto-fallback chain, a structured query pipeline
// that parses Google-style directives, and (later) a registry of site-aware
// content scrapers for `web_fetch`.

pub mod search;
