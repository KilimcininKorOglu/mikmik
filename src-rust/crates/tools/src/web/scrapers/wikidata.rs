// Wikidata handler: renders an entity from the EntityData API, resolving
// referenced entity ids to English labels.

use super::util::{build_result, format_number, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::time::Duration;

pub struct WikidataHandler;

const MAX_VALUES: usize = 10;
const MAX_PROPERTIES: usize = 50;

static QID_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)/(?:wiki|entity)/(Q\d+)").expect("static wikidata regex"));
static UNIT_QID: Lazy<Regex> = Lazy::new(|| Regex::new(r"Q\d+$").expect("static wikidata unit"));
static TIME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^([+-]?\d+)-(\d{2})-(\d{2})").expect("static wikidata time"));

/// Human label for a well-known Wikidata property, if any.
fn property_label(id: &str) -> Option<&'static str> {
    let label = match id {
        "P31" => "Instance of",
        "P279" => "Subclass of",
        "P17" => "Country",
        "P131" => "Located in",
        "P625" => "Coordinates",
        "P18" => "Image",
        "P154" => "Logo",
        "P571" => "Founded",
        "P576" => "Dissolved",
        "P169" => "CEO",
        "P112" => "Founded by",
        "P159" => "Headquarters",
        "P452" => "Industry",
        "P1128" => "Employees",
        "P2139" => "Revenue",
        "P856" => "Website",
        "P21" => "Sex/Gender",
        "P27" => "Citizenship",
        "P569" => "Born",
        "P570" => "Died",
        "P19" => "Birthplace",
        "P20" => "Death place",
        "P106" => "Occupation",
        "P108" => "Employer",
        "P69" => "Educated at",
        "P22" => "Father",
        "P25" => "Mother",
        "P26" => "Spouse",
        "P40" => "Child",
        "P166" => "Award",
        "P136" => "Genre",
        "P495" => "Country of origin",
        "P577" => "Publication date",
        "P50" => "Author",
        "P123" => "Publisher",
        "P364" => "Original language",
        "P86" => "Composer",
        "P57" => "Director",
        "P161" => "Cast member",
        "P170" => "Creator",
        "P178" => "Developer",
        "P275" => "License",
        "P306" => "Operating system",
        "P277" => "Programming language",
        "P348" => "Version",
        "P1566" => "GeoNames ID",
        "P214" => "VIAF ID",
        "P227" => "GND ID",
        "P213" => "ISNI",
        "P496" => "ORCID",
        _ => return None,
    };
    Some(label)
}

fn parse_qid(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("wikidata.org") {
        return None;
    }
    Some(QID_PATH.captures(parsed.path())?[1].to_uppercase())
}

/// Preferred-language value from a `{lang: {value}}` map, else the first.
fn localized_value<'a>(map: Option<&'a Value>, lang: &str) -> Option<&'a str> {
    let obj = map?.as_object()?;
    if let Some(value) = obj
        .get(lang)
        .and_then(|v| v.get("value"))
        .and_then(Value::as_str)
    {
        return Some(value);
    }
    obj.values()
        .next()
        .and_then(|v| v.get("value"))
        .and_then(Value::as_str)
}

fn localized_aliases(aliases: Option<&Value>, lang: &str) -> Vec<String> {
    aliases
        .and_then(|a| a.get(lang))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| a.get("value").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Format a Wikidata time string honoring its precision (9 year … 11 day).
fn format_time(time: &str, precision: i64) -> String {
    let Some(caps) = TIME.captures(time) else {
        return time.to_string();
    };
    let year: i64 = caps[1].parse().unwrap_or(0);
    let era = if year < 0 { " BCE" } else { "" };
    let abs_year = year.abs();
    let month = &caps[2];
    let day = &caps[3];
    if precision >= 11 {
        format!("{day}/{month}/{abs_year}{era}")
    } else if precision >= 10 {
        format!("{month}/{abs_year}{era}")
    } else {
        format!("{abs_year}{era}")
    }
}

/// Render one claim's main value to a display string, resolving entity ids.
fn format_claim_value(claim: &Value, labels: &HashMap<String, String>) -> Option<String> {
    let snak = claim.get("mainsnak")?;
    if snak.get("snaktype").and_then(Value::as_str) != Some("value") {
        return None;
    }
    let datavalue = snak.get("datavalue")?;
    let value = datavalue.get("value")?;
    match datavalue.get("type").and_then(Value::as_str)? {
        "wikibase-entityid" => {
            let id = value.get("id").and_then(Value::as_str)?;
            Some(labels.get(id).cloned().unwrap_or_else(|| id.to_string()))
        }
        "string" => value.as_str().map(str::to_string),
        "time" => {
            let time = value.get("time").and_then(Value::as_str)?;
            let precision = value.get("precision").and_then(Value::as_i64).unwrap_or(11);
            Some(format_time(time, precision))
        }
        "quantity" => Some(format_quantity(value, labels)),
        "monolingualtext" => value
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string),
        "globecoordinate" => {
            let lat = value.get("latitude").and_then(Value::as_f64)?;
            let lon = value.get("longitude").and_then(Value::as_f64)?;
            Some(format!("{lat:.4}, {lon:.4}"))
        }
        _ => None,
    }
}

fn format_quantity(value: &Value, labels: &HashMap<String, String>) -> String {
    let amount = value
        .get("amount")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start_matches('+');
    let unit = value
        .get("unit")
        .and_then(Value::as_str)
        .and_then(|u| UNIT_QID.find(u))
        .and_then(|m| labels.get(m.as_str()))
        .map(String::as_str)
        .unwrap_or("");
    if unit.is_empty() {
        amount.to_string()
    } else {
        format!("{amount} {unit}")
    }
}

/// Entity ids referenced by any claim, so their labels can be resolved.
fn referenced_entity_ids(claims: &Map<String, Value>) -> Vec<String> {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for claim_list in claims.values() {
        for claim in claim_list.as_array().into_iter().flatten() {
            let datavalue = claim.get("mainsnak").and_then(|s| s.get("datavalue"));
            if datavalue
                .and_then(|d| d.get("type"))
                .and_then(Value::as_str)
                == Some("wikibase-entityid")
            {
                if let Some(id) = datavalue
                    .and_then(|d| d.get("value"))
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    ids.into_iter().take(50).collect()
}

/// One rendered property line plus whether its property is well known.
struct PropertyLine {
    known: bool,
    line: String,
}

fn build_property_line(
    prop_id: &str,
    claims: &Value,
    labels: &HashMap<String, String>,
) -> Option<PropertyLine> {
    let mut values: Vec<String> = Vec::new();
    for claim in claims.as_array().into_iter().flatten() {
        if claim.get("rank").and_then(Value::as_str) == Some("deprecated") {
            continue;
        }
        if let Some(value) = format_claim_value(claim, labels) {
            if !values.contains(&value) {
                values.push(value);
            }
        }
    }
    if values.is_empty() {
        return None;
    }
    let known = property_label(prop_id);
    let label = known.unwrap_or(prop_id);
    let overflow = if values.len() > MAX_VALUES {
        format!(" […{} values elided…]", values.len() - MAX_VALUES)
    } else {
        String::new()
    };
    let shown = values
        .iter()
        .take(MAX_VALUES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    Some(PropertyLine {
        known: known.is_some(),
        line: format!("- **{label}:** {shown}{overflow}"),
    })
}

fn append_properties(
    md: &mut String,
    claims: &Map<String, Value>,
    labels: &HashMap<String, String>,
) {
    let mut lines: Vec<PropertyLine> = claims
        .iter()
        .filter_map(|(prop_id, claim_list)| build_property_line(prop_id, claim_list, labels))
        .collect();
    if lines.is_empty() {
        return;
    }
    // Known properties first, then alphabetical by rendered line.
    lines.sort_by(|a, b| b.known.cmp(&a.known).then_with(|| a.line.cmp(&b.line)));

    md.push_str("## Properties\n\n");
    let shown: Vec<&str> = lines
        .iter()
        .take(MAX_PROPERTIES)
        .map(|p| p.line.as_str())
        .collect();
    md.push_str(&shown.join("\n"));
    if lines.len() > MAX_PROPERTIES {
        let _ = write!(
            md,
            "\n\n[…{} properties elided…]",
            lines.len() - MAX_PROPERTIES
        );
    }
    md.push('\n');
}

fn append_wikipedia_links(md: &mut String, sitelinks: &Value) {
    let notable = ["enwiki", "dewiki", "frwiki", "eswiki", "jawiki", "zhwiki"];
    let mut links: Vec<String> = Vec::new();
    for site in notable {
        if let Some(title) = sitelinks
            .get(site)
            .and_then(|s| s.get("title"))
            .and_then(Value::as_str)
        {
            let lang = site.trim_end_matches("wiki");
            let wiki_url = format!(
                "https://{lang}.wikipedia.org/wiki/{}",
                super::util::percent_encode_component(title)
            );
            links.push(format!("[{}]({wiki_url})", lang.to_uppercase()));
        }
    }
    if !links.is_empty() {
        let _ = write!(md, "\n## Wikipedia Links\n\n{}\n", links.join(" · "));
    }
}

fn render(entity: &Value, qid: &str, labels: &HashMap<String, String>) -> String {
    let label = localized_value(entity.get("labels"), "en").unwrap_or(qid);
    let mut md = format!("# {label} ({qid})\n\n");
    if let Some(description) = localized_value(entity.get("descriptions"), "en") {
        let _ = write!(md, "*{description}*\n\n");
    }
    let aliases = localized_aliases(entity.get("aliases"), "en");
    if !aliases.is_empty() {
        let _ = write!(md, "**Also known as:** {}\n\n", aliases.join(", "));
    }
    if let Some(sitelinks) = entity.get("sitelinks").and_then(Value::as_object) {
        if !sitelinks.is_empty() {
            let _ = write!(
                md,
                "**Wikipedia articles:** {} languages\n\n",
                format_number(sitelinks.len() as u64)
            );
        }
    }
    if let Some(claims) = entity.get("claims").and_then(Value::as_object) {
        append_properties(&mut md, claims, labels);
    }
    if let Some(sitelinks) = entity.get("sitelinks") {
        append_wikipedia_links(&mut md, sitelinks);
    }
    md
}

/// Resolve up to 50 entity ids to English labels via the wbgetentities API.
async fn resolve_labels(ids: &[String], timeout: Duration) -> HashMap<String, String> {
    let mut labels: HashMap<String, String> = HashMap::new();
    if ids.is_empty() {
        return labels;
    }
    let api_url = format!(
        "https://www.wikidata.org/w/api.php?action=wbgetentities&ids={}&props=labels&languages=en&format=json",
        ids.join("|")
    );
    let result = load_page(
        &api_url,
        LoadOptions {
            timeout: timeout.min(Duration::from_secs(10)),
            ..Default::default()
        },
    )
    .await;
    if !result.ok {
        return labels;
    }
    if let Ok(data) = serde_json::from_str::<Value>(&result.content) {
        if let Some(entities) = data.get("entities").and_then(Value::as_object) {
            for (id, entity) in entities {
                if let Some(label) = entity
                    .get("labels")
                    .and_then(|l| l.get("en"))
                    .and_then(|e| e.get("value"))
                    .and_then(Value::as_str)
                {
                    labels.insert(id.clone(), label.to_string());
                }
            }
        }
    }
    labels
}

#[async_trait]
impl SpecialHandler for WikidataHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let qid = parse_qid(url)?;
        let api_url = format!("https://www.wikidata.org/wiki/Special:EntityData/{qid}.json");
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let data: Value = serde_json::from_str(&result.content).ok()?;
        let entity = data.get("entities")?.get(&qid)?;

        let ids = entity
            .get("claims")
            .and_then(Value::as_object)
            .map(referenced_entity_ids)
            .unwrap_or_default();
        let labels = resolve_labels(&ids, timeout).await;

        let md = render(entity, &qid, &labels);
        Some(build_result(
            &md,
            url,
            "wikidata",
            vec!["Fetched via Wikidata EntityData API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn qid_parses_from_wiki_and_entity_paths() {
        assert_eq!(
            parse_qid("https://www.wikidata.org/wiki/Q42"),
            Some("Q42".to_string())
        );
        assert_eq!(
            parse_qid("https://www.wikidata.org/entity/q64"),
            Some("Q64".to_string())
        );
        assert_eq!(parse_qid("https://example.com/wiki/Q42"), None);
    }

    #[test]
    fn time_formats_by_precision() {
        assert_eq!(format_time("+1879-03-14T00:00:00Z", 11), "14/03/1879");
        assert_eq!(format_time("+1879-03-14T00:00:00Z", 10), "03/1879");
        assert_eq!(format_time("+1879-03-14T00:00:00Z", 9), "1879");
        assert_eq!(format_time("-0044-03-15T00:00:00Z", 11), "15/03/44 BCE");
    }

    #[test]
    fn claim_value_resolves_entity_labels() {
        let mut labels = HashMap::new();
        labels.insert("Q5".to_string(), "human".to_string());
        let claim = json!({
            "mainsnak": {
                "snaktype": "value",
                "datavalue": { "type": "wikibase-entityid", "value": { "id": "Q5" } }
            }
        });
        assert_eq!(
            format_claim_value(&claim, &labels),
            Some("human".to_string())
        );
    }

    #[test]
    fn render_lays_out_entity() {
        let mut labels = HashMap::new();
        labels.insert("Q5".to_string(), "human".to_string());
        let entity = json!({
            "labels": { "en": { "value": "Douglas Adams" } },
            "descriptions": { "en": { "value": "English author" } },
            "aliases": { "en": [{ "value": "Douglas Noel Adams" }] },
            "sitelinks": { "enwiki": { "title": "Douglas Adams" } },
            "claims": {
                "P31": [{
                    "rank": "normal",
                    "mainsnak": { "snaktype": "value", "datavalue": { "type": "wikibase-entityid", "value": { "id": "Q5" } } }
                }]
            }
        });
        let md = render(&entity, "Q42", &labels);
        assert!(md.contains("# Douglas Adams (Q42)"));
        assert!(md.contains("*English author*"));
        assert!(md.contains("**Also known as:** Douglas Noel Adams"));
        assert!(md.contains("- **Instance of:** human"));
        assert!(md.contains("[EN](https://en.wikipedia.org/wiki/Douglas%20Adams)"));
    }
}
