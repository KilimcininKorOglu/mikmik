// SEC EDGAR handler: renders a company's profile and recent filings via the
// data.sec.gov submissions API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct SecEdgarHandler;

static CIK_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)/cik/(\d+)").expect("static sec cik regex"));
static SUBMISSIONS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)/submissions/CIK(\d+)\.json").expect("static sec sub regex"));
static ARCHIVES: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/Archives/edgar/data/(\d+)").expect("static sec arch regex"));

/// Left-pad the digits of a CIK to ten characters.
fn normalize_cik(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    format!("{digits:0>10}")
}

fn extract_cik(url: &url::Url) -> Option<String> {
    if !url.host_str()?.contains("sec.gov") {
        return None;
    }
    for (key, value) in url.query_pairs() {
        if key.eq_ignore_ascii_case("cik") {
            return Some(normalize_cik(&value));
        }
    }
    let path = url.path();
    for re in [&*CIK_PATH, &*SUBMISSIONS, &*ARCHIVES] {
        if let Some(m) = re.captures(path) {
            return Some(normalize_cik(&m[1]));
        }
    }
    None
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn str_list<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// One filing row pulled from the columnar `filings.recent` arrays.
struct Filing {
    date: String,
    form: String,
    description: String,
    url: String,
}

fn column<'a>(recent: &'a Value, key: &str) -> Vec<&'a str> {
    recent
        .get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| v.as_str().unwrap_or("")).collect())
        .unwrap_or_default()
}

fn filing_url(cik: &str, accession: &str, document: &str) -> String {
    let no_dashes: String = accession.chars().filter(|c| *c != '-').collect();
    let cik_num = cik.trim_start_matches('0');
    let cik_num = if cik_num.is_empty() { "0" } else { cik_num };
    format!("https://www.sec.gov/Archives/edgar/data/{cik_num}/{no_dashes}/{document}")
}

/// Gather up to `limit` filings, optionally restricted to `form_types`.
fn recent_filings(company: &Value, form_types: &[&str], limit: usize, cik: &str) -> Vec<Filing> {
    let recent = company
        .get("filings")
        .and_then(|f| f.get("recent"))
        .cloned()
        .unwrap_or(Value::Null);
    let forms = column(&recent, "form");
    let dates = column(&recent, "filingDate");
    let accessions = column(&recent, "accessionNumber");
    let documents = column(&recent, "primaryDocument");
    let descriptions = column(&recent, "primaryDocDescription");

    let mut out: Vec<Filing> = Vec::new();
    for (i, &form) in forms.iter().enumerate() {
        if out.len() >= limit {
            break;
        }
        if !form_types.is_empty() && !form_types.contains(&form) {
            continue;
        }
        let accession = accessions.get(i).copied().unwrap_or("");
        let document = documents.get(i).copied().unwrap_or("");
        let desc = descriptions
            .get(i)
            .copied()
            .filter(|d| !d.is_empty())
            .unwrap_or(form);
        out.push(Filing {
            date: dates.get(i).copied().unwrap_or("").to_string(),
            form: form.to_string(),
            description: desc.to_string(),
            url: filing_url(cik, accession, document),
        });
    }
    out
}

fn append_filing_table(md: &mut String, heading: &str, filings: &[Filing]) {
    if filings.is_empty() {
        return;
    }
    let _ = write!(
        md,
        "## {heading}\n\n| Date | Form | Description |\n|------|------|-------------|\n"
    );
    for f in filings {
        let _ = writeln!(
            md,
            "| {} | [{}]({}) | {} |",
            f.date, f.form, f.url, f.description
        );
    }
    md.push('\n');
}

fn append_address(md: &mut String, company: &Value) {
    let Some(business) = company.get("addresses").and_then(|a| a.get("business")) else {
        return;
    };
    let mut lines: Vec<String> = Vec::new();
    if let Some(street) = str_field(business, "street1") {
        lines.push(street.to_string());
    }
    if let Some(street) = str_field(business, "street2") {
        lines.push(street.to_string());
    }
    let city_line: Vec<&str> = ["city", "stateOrCountry", "zipCode"]
        .iter()
        .filter_map(|k| str_field(business, k))
        .collect();
    if !city_line.is_empty() {
        lines.push(city_line.join(", "));
    }
    if !lines.is_empty() {
        let _ = write!(md, "## Business Address\n\n{}\n\n", lines.join("\n"));
    }
}

fn append_header(md: &mut String, company: &Value) {
    if let Some(cik) = str_field(company, "cik") {
        let _ = write!(md, "**CIK:** {cik}");
    }
    let tickers = str_list(company, "tickers");
    if !tickers.is_empty() {
        let label = if tickers.len() > 1 {
            "Tickers"
        } else {
            "Ticker"
        };
        let _ = write!(md, " · **{label}:** {}", tickers.join(", "));
        let exchanges = str_list(company, "exchanges");
        if !exchanges.is_empty() {
            let _ = write!(md, " ({})", exchanges.join(", "));
        }
    }
    md.push('\n');
    if let Some(entity) = str_field(company, "entityType") {
        let _ = writeln!(md, "**Entity Type:** {entity}");
    }
    if let Some(sic) = str_field(company, "sic") {
        let desc = str_field(company, "sicDescription").unwrap_or("");
        let _ = writeln!(md, "**SIC:** {sic} - {desc}");
    }
    if let Some(state) = str_field(company, "stateOfIncorporation") {
        let _ = writeln!(md, "**State of Incorporation:** {state}");
    }
    if let Some(ein) = str_field(company, "ein") {
        let _ = writeln!(md, "**EIN:** {ein}");
    }
    if let Some(fy) = str_field(company, "fiscalYearEnd").filter(|f| f.len() >= 4) {
        let _ = writeln!(md, "**Fiscal Year End:** {}/{}", &fy[..2], &fy[2..]);
    }
    md.push('\n');
}

fn render(company: &Value, cik: &str) -> String {
    let mut md = format!(
        "# {}\n\n",
        str_field(company, "name").unwrap_or("(company)")
    );
    append_header(&mut md, company);
    append_address(&mut md, company);
    let key_forms = ["10-K", "10-K/A", "10-Q", "10-Q/A", "8-K", "8-K/A"];
    append_filing_table(
        &mut md,
        "Recent Filings (10-K, 10-Q, 8-K)",
        &recent_filings(company, &key_forms, 15, cik),
    );
    append_filing_table(
        &mut md,
        "All Recent Filings",
        &recent_filings(company, &[], 20, cik),
    );
    let name = str_field(company, "name").unwrap_or("");
    let _ = write!(
        md,
        "## Links\n\n- [SEC EDGAR Filings](https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK={cik}&type=&dateb=&owner=include&count=40)\n- [Company Search](https://www.sec.gov/cgi-bin/browse-edgar?company={}&CIK=&type=&owner=include&count=40&action=getcompany)\n",
        super::util::percent_encode_component(name)
    );
    md
}

#[async_trait]
impl SpecialHandler for SecEdgarHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let cik = extract_cik(&parsed)?;
        let api_url = format!("https://data.sec.gov/submissions/CIK{cik}.json");
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                headers: vec![("Accept".to_string(), "application/json".to_string())],
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let company: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&company, &cik);
        Some(build_result(
            &md,
            url,
            "sec-edgar",
            vec!["Fetched via SEC EDGAR API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cik_of(url: &str) -> Option<String> {
        extract_cik(&url::Url::parse(url).expect("url"))
    }

    #[test]
    fn cik_extraction_normalizes_to_ten_digits() {
        assert_eq!(
            cik_of("https://www.sec.gov/cgi-bin/browse-edgar?CIK=320193"),
            Some("0000320193".to_string())
        );
        assert_eq!(
            cik_of("https://data.sec.gov/submissions/CIK0000320193.json"),
            Some("0000320193".to_string())
        );
        assert_eq!(
            cik_of("https://www.sec.gov/Archives/edgar/data/320193/000032019323000106"),
            Some("0000320193".to_string())
        );
        assert_eq!(cik_of("https://example.com/?CIK=1"), None);
    }

    #[test]
    fn filing_url_drops_dashes_and_leading_zeros() {
        assert_eq!(
            filing_url("0000320193", "0000320193-23-000106", "aapl.htm"),
            "https://www.sec.gov/Archives/edgar/data/320193/000032019323000106/aapl.htm"
        );
    }

    #[test]
    fn render_lays_out_header_and_filings() {
        let company = json!({
            "name": "Apple Inc.",
            "cik": "0000320193",
            "tickers": ["AAPL"],
            "exchanges": ["Nasdaq"],
            "entityType": "operating",
            "sic": "3571",
            "sicDescription": "Electronic Computers",
            "fiscalYearEnd": "0930",
            "addresses": { "business": { "street1": "One Apple Park Way", "city": "Cupertino", "stateOrCountry": "CA", "zipCode": "95014" } },
            "filings": { "recent": {
                "form": ["10-K", "4"],
                "filingDate": ["2023-11-03", "2023-11-01"],
                "accessionNumber": ["0000320193-23-000106", "0000320193-23-000105"],
                "primaryDocument": ["aapl.htm", "form4.xml"],
                "primaryDocDescription": ["10-K", "Statement of changes"]
            } }
        });
        let md = render(&company, "0000320193");
        assert!(md.contains("# Apple Inc."));
        assert!(md.contains("**CIK:** 0000320193 · **Ticker:** AAPL (Nasdaq)"));
        assert!(md.contains("**SIC:** 3571 - Electronic Computers"));
        assert!(md.contains("**Fiscal Year End:** 09/30"));
        assert!(md.contains("## Business Address\n\nOne Apple Park Way\nCupertino, CA, 95014"));
        assert!(md.contains("## Recent Filings (10-K, 10-Q, 8-K)"));
        assert!(md.contains("| 2023-11-03 | [10-K](https://www.sec.gov/Archives/edgar/data/320193/000032019323000106/aapl.htm) | 10-K |"));
        assert!(md.contains("## All Recent Filings"));
    }
}
