// Site-aware content scrapers for the web-fetch tool.
//
// Ported from oh-my-pi `web/scrapers/`. Each handler recognizes a family of
// URLs, fetches structured data from that site's API, and renders markdown.
// `dispatch` tries the registered handlers in order and returns the first
// non-`None` result; a URL no handler claims falls through to the generic
// web-fetch path.

pub mod artifacthub;
pub mod aur;
pub mod brew;
pub mod cisa_kev;
pub mod clojars;
pub mod coingecko;
pub mod crates_io;
pub mod crossref;
pub mod devto;
pub mod discogs;
pub mod dockerhub;
pub mod hackage;
pub mod hackernews;
pub mod hex;
pub mod huggingface;
pub mod lobsters;
pub mod maven;
pub mod metacpan;
pub mod musicbrainz;
pub mod npm;
pub mod nuget;
pub mod nvd;
pub mod openlibrary;
pub mod osv;
pub mod packagist;
pub mod pub_dev;
pub mod pypi;
pub mod reddit;
pub mod rubygems;
pub mod sec_edgar;
pub mod semantic_scholar;
pub mod snapcraft;
pub mod spdx;
pub mod terraform;
pub mod util;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::time::Duration;
use util::RenderResult;

/// A site-specific content handler.
#[async_trait]
pub trait SpecialHandler: Send + Sync {
    /// Render the URL, or `None` when this handler does not claim it.
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult>;
}

/// The registered handlers, tried in order. Ordering mirrors omp's
/// `specialHandlers[]`: more specific hosts precede broader ones.
static HANDLERS: Lazy<Vec<Box<dyn SpecialHandler>>> = Lazy::new(|| {
    vec![
        Box::new(npm::NpmHandler) as Box<dyn SpecialHandler>,
        Box::new(pypi::PypiHandler),
        Box::new(crates_io::CratesIoHandler),
        Box::new(rubygems::RubyGemsHandler),
        Box::new(hex::HexHandler),
        Box::new(hackage::HackageHandler),
        Box::new(maven::MavenHandler),
        Box::new(nuget::NuGetHandler),
        Box::new(packagist::PackagistHandler),
        Box::new(pub_dev::PubDevHandler),
        Box::new(metacpan::MetaCpanHandler),
        Box::new(clojars::ClojarsHandler),
        Box::new(aur::AurHandler),
        Box::new(dockerhub::DockerHubHandler),
        Box::new(artifacthub::ArtifactHubHandler),
        Box::new(brew::BrewHandler),
        Box::new(snapcraft::SnapcraftHandler),
        Box::new(hackernews::HackerNewsHandler),
        Box::new(reddit::RedditHandler),
        Box::new(lobsters::LobstersHandler),
        Box::new(terraform::TerraformHandler),
        Box::new(devto::DevToHandler),
        Box::new(huggingface::HuggingFaceHandler),
        Box::new(openlibrary::OpenLibraryHandler),
        Box::new(coingecko::CoinGeckoHandler),
        Box::new(semantic_scholar::SemanticScholarHandler),
        Box::new(crossref::CrossrefHandler),
        Box::new(musicbrainz::MusicBrainzHandler),
        Box::new(discogs::DiscogsHandler),
        Box::new(sec_edgar::SecEdgarHandler),
        Box::new(spdx::SpdxHandler),
        Box::new(cisa_kev::CisaKevHandler),
        Box::new(nvd::NvdHandler),
        Box::new(osv::OsvHandler),
    ]
});

/// Try each handler in turn; the first to claim `url` wins.
pub async fn dispatch(url: &str, timeout: Duration) -> Option<RenderResult> {
    for handler in HANDLERS.iter() {
        if let Some(result) = handler.handle(url, timeout).await {
            return Some(result);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_url_no_handler_claims_falls_through() {
        // A plain page reaches no registered handler, so dispatch returns None
        // and the generic web-fetch path takes over. No network is touched
        // because npm's handler rejects the host before fetching.
        let result = dispatch("https://example.com/page", Duration::from_secs(1)).await;
        assert!(result.is_none());
    }
}
