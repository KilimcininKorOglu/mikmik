// Open Library handler: renders a work, an edition, or an ISBN lookup via the
// Open Library APIs, resolving author keys to names.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct OpenLibraryHandler;

static WORK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^/works/(OL\d+W)").expect("static ol work regex"));
static EDITION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^/books/(OL\d+M)").expect("static ol book regex"));
static ISBN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/isbn/(\d{13}|\d{10})").expect("static ol isbn regex"));

enum Target {
    Work(String),
    Edition(String),
    Isbn(String),
}

fn parse_target(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("openlibrary.org") {
        return None;
    }
    let path = parsed.path();
    if let Some(m) = WORK.captures(path) {
        return Some(Target::Work(m[1].to_string()));
    }
    if let Some(m) = EDITION.captures(path) {
        return Some(Target::Edition(m[1].to_string()));
    }
    ISBN.captures(path).map(|m| Target::Isbn(m[1].to_string()))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A description that may be a plain string or a `{ value: … }` object.
fn extract_description(desc: &Value) -> Option<String> {
    match desc {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(_) => str_field(desc, "value").map(str::to_string),
        _ => None,
    }
}

fn str_list<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

async fn fetch_json(url: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    if !result.ok {
        return None;
    }
    serde_json::from_str(&result.content).ok()
}

/// Resolve up to five author keys to display names, fetched concurrently.
async fn fetch_author_names(keys: Vec<String>, timeout: Duration) -> Vec<String> {
    let futures = keys.into_iter().take(5).map(|key| {
        let path = if key.starts_with("/authors/") {
            key
        } else {
            format!("/authors/{key}")
        };
        async move {
            let url = format!("https://openlibrary.org{path}.json");
            fetch_json(&url, timeout.min(Duration::from_secs(5)))
                .await
                .and_then(|a| str_field(&a, "name").map(str::to_string))
        }
    });
    futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect()
}

fn append_description_and_subjects(md: &mut String, item: &Value) {
    if let Some(desc) = item.get("description").and_then(extract_description) {
        let _ = write!(md, "## Description\n\n{desc}\n\n");
    }
    let subjects = str_list(item, "subjects");
    if !subjects.is_empty() {
        let list = subjects
            .iter()
            .take(20)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(md, "## Subjects\n\n{list}\n");
    }
}

async fn render_work(work_id: &str, timeout: Duration) -> Option<String> {
    let work = fetch_json(
        &format!("https://openlibrary.org/works/{work_id}.json"),
        timeout,
    )
    .await?;
    let mut md = format!("# {}\n\n", str_field(&work, "title").unwrap_or("(work)"));
    let keys: Vec<String> = work
        .get("authors")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("author").and_then(|a| str_field(a, "key")))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let names = fetch_author_names(keys, timeout).await;
    if !names.is_empty() {
        let _ = writeln!(md, "**Authors:** {}", names.join(", "));
    }
    if let Some(published) = str_field(&work, "first_publish_date") {
        let _ = writeln!(md, "**First Published:** {published}");
    }
    if let Some(cover) = work
        .get("covers")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_i64)
    {
        let _ = writeln!(
            md,
            "**Cover:** https://covers.openlibrary.org/b/id/{cover}-L.jpg"
        );
    }
    let _ = write!(
        md,
        "**Open Library:** https://openlibrary.org/works/{work_id}\n\n"
    );
    append_description_and_subjects(&mut md, &work);
    Some(md)
}

async fn render_edition(edition_id: &str, timeout: Duration) -> Option<String> {
    let edition = fetch_json(
        &format!("https://openlibrary.org/books/{edition_id}.json"),
        timeout,
    )
    .await?;
    let mut md = format!(
        "# {}\n\n",
        str_field(&edition, "title").unwrap_or("(edition)")
    );
    let keys: Vec<String> = edition
        .get("authors")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| str_field(x, "key"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let names = fetch_author_names(keys, timeout).await;
    if !names.is_empty() {
        let _ = writeln!(md, "**Authors:** {}", names.join(", "));
    }
    append_edition_fields(&mut md, &edition, edition_id);
    append_description_and_subjects(&mut md, &edition);
    Some(md)
}

fn append_edition_fields(md: &mut String, edition: &Value, edition_id: &str) {
    let publishers = str_list(edition, "publishers");
    if !publishers.is_empty() {
        let _ = writeln!(md, "**Publishers:** {}", publishers.join(", "));
    }
    if let Some(date) = str_field(edition, "publish_date") {
        let _ = writeln!(md, "**Published:** {date}");
    }
    if let Some(pages) = edition.get("number_of_pages").and_then(Value::as_u64) {
        let _ = writeln!(md, "**Pages:** {pages}");
    }
    let mut isbns = str_list(edition, "isbn_13");
    isbns.extend(str_list(edition, "isbn_10"));
    if let Some(isbn) = isbns.first() {
        let _ = writeln!(md, "**ISBN:** {isbn}");
    }
    if let Some(cover) = edition
        .get("covers")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_i64)
    {
        let _ = writeln!(
            md,
            "**Cover:** https://covers.openlibrary.org/b/id/{cover}-L.jpg"
        );
    }
    let _ = writeln!(
        md,
        "**Open Library:** https://openlibrary.org/books/{edition_id}"
    );
    if let Some(work_key) = edition
        .get("works")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|w| str_field(w, "key"))
    {
        let key = work_key.strip_prefix("/works/").unwrap_or(work_key);
        let _ = writeln!(md, "**Work:** https://openlibrary.org/works/{key}");
    }
    md.push('\n');
}

async fn render_isbn(isbn: &str, timeout: Duration) -> Option<String> {
    let url =
        format!("https://openlibrary.org/api/books?bibkeys=ISBN:{isbn}&format=json&jscmd=data");
    if let Some(data) = fetch_json(&url, timeout).await {
        if let Some(book) = data.get(format!("ISBN:{isbn}")) {
            return Some(render_book_data(book, isbn));
        }
    }
    render_isbn_search(isbn, timeout).await
}

fn render_book_data(book: &Value, isbn: &str) -> String {
    let mut md = format!("# {}\n\n", str_field(book, "title").unwrap_or("(book)"));
    let authors: Vec<&str> = book
        .get("authors")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| str_field(x, "name")).collect())
        .unwrap_or_default();
    if !authors.is_empty() {
        let _ = writeln!(md, "**Authors:** {}", authors.join(", "));
    }
    let publishers: Vec<&str> = book
        .get("publishers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| str_field(x, "name")).collect())
        .unwrap_or_default();
    if !publishers.is_empty() {
        let _ = writeln!(md, "**Publishers:** {}", publishers.join(", "));
    }
    if let Some(date) = str_field(book, "publish_date") {
        let _ = writeln!(md, "**Published:** {date}");
    }
    if let Some(pages) = book.get("number_of_pages").and_then(Value::as_u64) {
        let _ = writeln!(md, "**Pages:** {pages}");
    }
    let _ = writeln!(md, "**ISBN:** {isbn}");
    if let Some(cover) = book
        .get("cover")
        .and_then(|c| str_field(c, "large").or_else(|| str_field(c, "medium")))
    {
        let _ = writeln!(md, "**Cover:** {cover}");
    }
    if let Some(link) = str_field(book, "url") {
        let _ = writeln!(md, "**Open Library:** {link}");
    }
    let subjects: Vec<&str> = book
        .get("subjects")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| str_field(x, "name"))
                .take(20)
                .collect()
        })
        .unwrap_or_default();
    if !subjects.is_empty() {
        let _ = write!(md, "\n## Subjects\n\n{}\n", subjects.join(", "));
    }
    md
}

async fn render_isbn_search(isbn: &str, timeout: Duration) -> Option<String> {
    let url = format!("https://openlibrary.org/search.json?isbn={isbn}&limit=1");
    let unavailable =
        format!("# Open Library Book\n\n**ISBN:** {isbn}\n\nBook details are currently unavailable from Open Library.\n");
    let Some(data) = fetch_json(&url, timeout).await else {
        return Some(unavailable);
    };
    let doc = data
        .get("docs")
        .and_then(Value::as_array)
        .and_then(|a| a.first());
    let Some(doc) = doc.filter(|d| str_field(d, "title").is_some()) else {
        return Some(unavailable);
    };
    let mut md = format!("# {}\n\n", str_field(doc, "title").unwrap_or("(book)"));
    let authors = str_list(doc, "author_name");
    if !authors.is_empty() {
        let _ = writeln!(md, "**Authors:** {}", authors.join(", "));
    }
    if let Some(year) = doc.get("first_publish_year").and_then(Value::as_i64) {
        let _ = writeln!(md, "**First Published:** {year}");
    }
    let _ = writeln!(md, "**ISBN:** {isbn}");
    if let Some(key) = str_field(doc, "key") {
        let _ = writeln!(md, "**Open Library:** https://openlibrary.org{key}");
    }
    Some(md)
}

#[async_trait]
impl SpecialHandler for OpenLibraryHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let md = match parse_target(url)? {
            Target::Work(id) => render_work(&id, timeout).await?,
            Target::Edition(id) => render_edition(&id, timeout).await?,
            Target::Isbn(isbn) => render_isbn(&isbn, timeout).await?,
        };
        Some(build_result(
            &md,
            url,
            "openlibrary",
            vec!["Fetched via Open Library API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_target_reads_work_edition_and_isbn() {
        assert!(
            matches!(parse_target("https://openlibrary.org/works/OL45804W/title"), Some(Target::Work(i)) if i == "OL45804W")
        );
        assert!(
            matches!(parse_target("https://openlibrary.org/books/OL7353617M"), Some(Target::Edition(i)) if i == "OL7353617M")
        );
        assert!(
            matches!(parse_target("https://openlibrary.org/isbn/9780140328721"), Some(Target::Isbn(i)) if i == "9780140328721")
        );
        assert!(parse_target("https://example.com/works/OL1W").is_none());
    }

    #[test]
    fn description_reads_string_or_value_object() {
        assert_eq!(
            extract_description(&json!("plain")).as_deref(),
            Some("plain")
        );
        assert_eq!(
            extract_description(&json!({ "value": "wrapped" })).as_deref(),
            Some("wrapped")
        );
        assert_eq!(extract_description(&json!(null)), None);
    }

    #[test]
    fn book_data_lays_out_the_fields() {
        let book = json!({
            "title": "Matilda",
            "authors": [{ "name": "Roald Dahl" }],
            "publishers": [{ "name": "Puffin" }],
            "publish_date": "1988",
            "number_of_pages": 240,
            "cover": { "large": "https://covers/large.jpg" },
            "url": "https://openlibrary.org/books/OL1M",
            "subjects": [{ "name": "Children" }]
        });
        let md = render_book_data(&book, "9780140328721");
        assert!(md.contains("# Matilda"));
        assert!(md.contains("**Authors:** Roald Dahl"));
        assert!(md.contains("**Publishers:** Puffin"));
        assert!(md.contains("**Pages:** 240"));
        assert!(md.contains("**ISBN:** 9780140328721"));
        assert!(md.contains("**Cover:** https://covers/large.jpg"));
        assert!(md.contains("## Subjects\n\nChildren"));
    }
}
