// docs.rs handler: renders a crate module or item from the rustdoc JSON that
// docs.rs publishes as a gzip payload. The decompressed JSON is cached on disk.

use super::util::{build_result, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use futures::StreamExt;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write as _;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

pub struct DocsRsHandler;

/// Cap on the raw gzip download: a docs.rs payload above this is refused.
const MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;
/// Cap on decompressed rustdoc JSON, guarding against a decompression bomb.
const MAX_GUNZIP_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TYPE_DEPTH: usize = 10;

static ITEM_PAGE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(struct|trait|fn|enum|macro|type|constant|static|attr|derive|union|primitive)\.(.+)\.html$")
        .expect("static docs.rs item regex")
});
static SANITIZE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[^A-Za-z0-9._-]+").expect("static docs.rs sanitize regex"));

struct Target {
    crate_name: String,
    version: String,
    module_path: Vec<String>,
    item_name: Option<String>,
}

fn parse_docs_rs_url(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "docs.rs" {
        return None;
    }
    let segments: Vec<String> = parsed
        .path()
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(super::util::percent_decode)
        .collect();
    if segments.first().map(String::as_str) == Some("crate") || segments.len() < 3 {
        return None;
    }
    let crate_name = segments[0].clone();
    let version = segments[1].clone();
    let mut rest: Vec<String> = segments[2..].to_vec();
    let item_name = pop_item_name(&mut rest);
    Some(Target {
        crate_name,
        version,
        module_path: rest,
        item_name,
    })
}

/// Strip a trailing `struct.Foo.html`/`index.html` segment; return the item name.
fn pop_item_name(rest: &mut Vec<String>) -> Option<String> {
    let last = rest.last()?.clone();
    if let Some(caps) = ITEM_PAGE.captures(&last) {
        let name = caps[2].to_string();
        rest.pop();
        return Some(name);
    }
    if last == "index.html" {
        rest.pop();
    }
    None
}

// --- rustdoc value helpers ---

fn item_name(item: &Value) -> Option<&str> {
    item.get("name").and_then(Value::as_str)
}

fn inner_kind(item: &Value) -> Option<&str> {
    item.get("inner")
        .and_then(Value::as_object)
        .and_then(|m| m.keys().next())
        .map(String::as_str)
}

fn index_get<'a>(index: &'a Value, id: &Value) -> Option<&'a Value> {
    let key = id.as_i64()?.to_string();
    index.get(key)
}

fn first_line(docs: &str) -> String {
    let line = docs.lines().next().unwrap_or("").trim();
    if line.chars().count() > 200 {
        let truncated: String = line.chars().take(197).collect();
        format!("{truncated}...")
    } else {
        line.to_string()
    }
}

// --- type rendering ---

fn render_type(ty: &Value, depth: usize) -> String {
    if depth > MAX_TYPE_DEPTH {
        return "_".to_string();
    }
    if let Some(s) = ty.as_str() {
        return s.to_string();
    }
    render_named(ty, depth)
        .or_else(|| render_ptr(ty, depth))
        .or_else(|| render_compound(ty, depth))
        .or_else(|| render_bounds(ty, depth))
        .unwrap_or_else(|| "_".to_string())
}

fn render_path_with_args(path: &str, args: Option<&Value>, depth: usize) -> String {
    let inner = args
        .and_then(|a| a.pointer("/angle_bracketed/args"))
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty());
    match inner {
        Some(list) => {
            let rendered: Vec<String> = list.iter().map(|a| render_arg(a, depth)).collect();
            format!("{path}<{}>", rendered.join(", "))
        }
        None => path.to_string(),
    }
}

fn render_arg(arg: &Value, depth: usize) -> String {
    if let Some(ty) = arg.get("type") {
        return render_type(ty, depth + 1);
    }
    if let Some(lt) = arg.get("lifetime").and_then(Value::as_str) {
        return format!("'{lt}");
    }
    "_".to_string()
}

fn render_named(ty: &Value, depth: usize) -> Option<String> {
    if let Some(g) = ty.get("generic").and_then(Value::as_str) {
        return Some(g.to_string());
    }
    if let Some(p) = ty.get("primitive").and_then(Value::as_str) {
        return Some(p.to_string());
    }
    if ty.get("infer").is_some() {
        return Some("_".to_string());
    }
    let rp = ty.get("resolved_path")?;
    let path = rp.get("path").and_then(Value::as_str)?;
    Some(render_path_with_args(path, rp.get("args"), depth))
}

fn render_ptr(ty: &Value, depth: usize) -> Option<String> {
    if let Some(br) = ty.get("borrowed_ref") {
        let lt = br
            .get("lifetime")
            .and_then(Value::as_str)
            .map(|l| format!("'{l} "))
            .unwrap_or_default();
        let mutable = if br.get("is_mutable").and_then(Value::as_bool) == Some(true) {
            "mut "
        } else {
            ""
        };
        let inner = render_type(br.get("type").unwrap_or(&Value::Null), depth + 1);
        return Some(format!("&{lt}{mutable}{inner}"));
    }
    if let Some(rp) = ty.get("raw_pointer") {
        let kind = if rp.get("is_mutable").and_then(Value::as_bool) == Some(true) {
            "mut"
        } else {
            "const"
        };
        let inner = render_type(rp.get("type").unwrap_or(&Value::Null), depth + 1);
        return Some(format!("*{kind} {inner}"));
    }
    let qp = ty.get("qualified_path")?;
    let name = qp.get("name").and_then(Value::as_str).unwrap_or("");
    let self_ = render_type(qp.get("self_type").unwrap_or(&Value::Null), depth + 1);
    match qp.get("trait_").filter(|t| !t.is_null()) {
        Some(trait_) => Some(format!(
            "<{self_} as {}>::{name}",
            render_type(trait_, depth + 1)
        )),
        None => Some(format!("{self_}::{name}")),
    }
}

fn render_compound(ty: &Value, depth: usize) -> Option<String> {
    if let Some(items) = ty.get("tuple").and_then(Value::as_array) {
        let parts: Vec<String> = items.iter().map(|t| render_type(t, depth + 1)).collect();
        return Some(format!("({})", parts.join(", ")));
    }
    if let Some(slice) = ty.get("slice") {
        return Some(format!("[{}]", render_type(slice, depth + 1)));
    }
    let array = ty.get("array")?;
    let ty_str = render_type(array.get("type").unwrap_or(&Value::Null), depth + 1);
    let len = array.get("len").and_then(Value::as_str).unwrap_or("");
    Some(format!("[{ty_str}; {len}]"))
}

fn render_bounds(ty: &Value, depth: usize) -> Option<String> {
    if let Some(bounds) = ty.get("impl_trait").and_then(Value::as_array) {
        let parts: Vec<String> = bounds
            .iter()
            .map(|b| match b.pointer("/trait_bound/trait") {
                Some(t) => render_type(t, depth + 1),
                None => "?".to_string(),
            })
            .collect();
        return Some(format!("impl {}", parts.join(" + ")));
    }
    if let Some(dt) = ty.get("dyn_trait") {
        let parts: Vec<String> = dt
            .get("traits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|t| t.get("trait").map(|tr| render_type(tr, depth + 1)))
            .collect();
        let lt = dt
            .get("lifetime")
            .and_then(Value::as_str)
            .map(|l| format!(" + '{l}"))
            .unwrap_or_default();
        return Some(format!("dyn {}{lt}", parts.join(" + ")));
    }
    ty.get("function_pointer").map(|_| "fn(...)".to_string())
}

// --- declaration rendering ---

fn render_generics(generics: &Value) -> String {
    let params = generics.get("params").and_then(Value::as_array);
    let Some(params) = params else {
        return String::new();
    };
    let names: Vec<&str> = params
        .iter()
        .filter(|p| p.pointer("/kind/lifetime").is_none())
        .filter_map(|p| p.get("name").and_then(Value::as_str))
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!("<{}>", names.join(", "))
    }
}

fn render_function_sig(name: &str, fn_: &Value) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if fn_.get("is_const").and_then(Value::as_bool) == Some(true) {
        parts.push("const");
    }
    if fn_.get("is_async").and_then(Value::as_bool) == Some(true) {
        parts.push("async");
    }
    if fn_.get("is_unsafe").and_then(Value::as_bool) == Some(true) {
        parts.push("unsafe");
    }
    parts.push("fn");
    let gen = fn_.get("generics").map(render_generics).unwrap_or_default();
    let inputs = render_fn_inputs(fn_);
    let output = fn_
        .pointer("/sig/output")
        .filter(|o| !o.is_null())
        .map(|o| format!(" -> {}", render_type(o, 0)))
        .unwrap_or_default();
    format!("{} {name}{gen}({inputs}){output}", parts.join(" "))
}

fn render_fn_inputs(fn_: &Value) -> String {
    let inputs = fn_.pointer("/sig/inputs").and_then(Value::as_array);
    let Some(inputs) = inputs else {
        return String::new();
    };
    let rendered: Vec<String> = inputs
        .iter()
        .filter_map(|pair| pair.as_array())
        .map(|pair| {
            let name = pair.first().and_then(Value::as_str).unwrap_or("");
            let ty = render_type(pair.get(1).unwrap_or(&Value::Null), 0);
            if name == "self" {
                ty
            } else {
                format!("{name}: {ty}")
            }
        })
        .collect();
    rendered.join(", ")
}

fn render_item_decl(item: &Value) -> Option<String> {
    let inner = item.get("inner")?;
    let name = item_name(item).unwrap_or("?");
    if let Some(func) = inner.get("function") {
        return Some(render_function_sig(name, func));
    }
    render_type_like_decl(inner, name).or_else(|| render_value_decl(inner, name))
}

fn generics_of<'a>(inner: &'a Value, key: &str) -> &'a Value {
    inner
        .pointer(&format!("/{key}/generics"))
        .unwrap_or(&Value::Null)
}

fn render_type_like_decl(inner: &Value, name: &str) -> Option<String> {
    if inner.get("struct").is_some() {
        return Some(format!(
            "struct {name}{}",
            render_generics(generics_of(inner, "struct"))
        ));
    }
    if inner.get("enum").is_some() {
        return Some(format!(
            "enum {name}{}",
            render_generics(generics_of(inner, "enum"))
        ));
    }
    if let Some(t) = inner.get("trait") {
        let prefix = if t.get("is_unsafe").and_then(Value::as_bool) == Some(true) {
            "unsafe "
        } else {
            ""
        };
        return Some(format!(
            "{prefix}trait {name}{}",
            render_generics(generics_of(inner, "trait"))
        ));
    }
    None
}

fn render_value_decl(inner: &Value, name: &str) -> Option<String> {
    if let Some(ta) = inner.get("type_alias") {
        let gen = render_generics(generics_of(inner, "type_alias"));
        let ty = ta
            .get("type")
            .filter(|t| !t.is_null())
            .map(|t| format!(" = {}", render_type(t, 0)))
            .unwrap_or_default();
        return Some(format!("type {name}{gen}{ty}"));
    }
    if inner.get("macro_def").is_some() {
        return Some(format!("macro {name}!(...)"));
    }
    let c = inner.get("constant")?;
    let ty = render_type(c.get("type").unwrap_or(&Value::Null), 0);
    let value = c
        .get("value")
        .and_then(Value::as_str)
        .map(|v| format!(" = {v}"))
        .unwrap_or_default();
    Some(format!("const {name}: {ty}{value}"))
}

// --- item lookup ---

fn find_item_in_module<'a>(module: &Value, name: &str, index: &'a Value) -> Option<&'a Value> {
    let items = module
        .pointer("/inner/module/items")
        .and_then(Value::as_array)?;
    for id in items {
        let Some(item) = index_get(index, id) else {
            continue;
        };
        if item_name(item) == Some(name) {
            return Some(item);
        }
        if let Some(use_) = item.pointer("/inner/use") {
            if use_.get("name").and_then(Value::as_str) == Some(name) {
                if let Some(target) = use_.get("id").and_then(|id| index_get(index, id)) {
                    return Some(target);
                }
            }
        }
    }
    None
}

fn walk_module_path<'a>(
    root: &'a Value,
    module_path: &[String],
    index: &'a Value,
) -> Option<&'a Value> {
    let mut current = root;
    for seg in module_path.iter().skip(1) {
        let items = current
            .pointer("/inner/module/items")
            .and_then(Value::as_array)?;
        current = items
            .iter()
            .filter_map(|id| index_get(index, id))
            .find(|it| item_name(it) == Some(seg) && it.pointer("/inner/module").is_some())?;
    }
    Some(current)
}

// --- item / module rendering ---

fn method_line(item: &Value) -> Option<String> {
    let func = item.get("inner")?.get("function")?;
    let name = item_name(item)?;
    let sig = render_function_sig(name, func);
    let doc = item
        .get("docs")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .map(|d| format!(" — {}", first_line(d)))
        .unwrap_or_default();
    Some(format!("- `{sig}`{doc}"))
}

fn collect_inherent_methods(impls: &[Value], index: &Value) -> Vec<String> {
    let mut methods = Vec::new();
    for impl_id in impls {
        let Some(impl_) = index_get(index, impl_id) else {
            continue;
        };
        let Some(data) = impl_.pointer("/inner/impl") else {
            continue;
        };
        let skip = data.get("is_synthetic").and_then(Value::as_bool) == Some(true)
            || data.get("trait").filter(|t| !t.is_null()).is_some()
            || data.get("blanket_impl").filter(|b| !b.is_null()).is_some();
        if skip {
            continue;
        }
        for mid in data
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(line) = index_get(index, mid).and_then(method_line) {
                methods.push(line);
            }
        }
    }
    methods
}

fn collect_trait_impl_names(impls: &[Value], index: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for impl_id in impls {
        let Some(impl_) = index_get(index, impl_id) else {
            continue;
        };
        let Some(data) = impl_.pointer("/inner/impl") else {
            continue;
        };
        let synthetic = data.get("is_synthetic").and_then(Value::as_bool) == Some(true);
        let blanket = data.get("blanket_impl").filter(|b| !b.is_null()).is_some();
        let Some(trait_) = data.get("trait").filter(|t| !t.is_null()) else {
            continue;
        };
        if synthetic || blanket {
            continue;
        }
        let path = trait_.get("path").and_then(Value::as_str).unwrap_or("");
        let name = render_path_with_args(path, trait_.get("args"), 0);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn append_trait_items(md: &mut String, trait_items: &[Value], index: &Value) {
    let mut required = Vec::new();
    let mut provided = Vec::new();
    for id in trait_items {
        let Some(child) = index_get(index, id) else {
            continue;
        };
        if let Some(func) = child.pointer("/inner/function") {
            if let Some(line) = method_line(child) {
                if func.get("has_body").and_then(Value::as_bool) == Some(true) {
                    provided.push(line);
                } else {
                    required.push(line);
                }
            }
        } else if child.pointer("/inner/assoc_type").is_some() {
            let name = item_name(child).unwrap_or("?");
            let doc = child
                .get("docs")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .map(|d| format!(" — {}", first_line(d)))
                .unwrap_or_default();
            required.push(format!("- `type {name}`{doc}"));
        }
    }
    if !required.is_empty() {
        let _ = write!(md, "## Required Methods\n\n{}\n\n", required.join("\n"));
    }
    if !provided.is_empty() {
        let _ = write!(md, "## Provided Methods\n\n{}\n\n", provided.join("\n"));
    }
}

fn append_variants(md: &mut String, item: &Value, index: &Value) {
    let variants = item
        .pointer("/inner/enum/variants")
        .and_then(Value::as_array);
    let Some(variants) = variants else {
        return;
    };
    let mut lines = Vec::new();
    for vid in variants {
        let Some(v) = index_get(index, vid) else {
            continue;
        };
        let Some(name) = item_name(v) else {
            continue;
        };
        let doc = v
            .get("docs")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(|d| format!(" — {}", first_line(d)))
            .unwrap_or_default();
        lines.push(format!("- `{name}`{doc}"));
    }
    if !lines.is_empty() {
        let _ = write!(md, "## Variants\n\n{}\n\n", lines.join("\n"));
    }
}

fn append_impls(md: &mut String, item: &Value, kind: &str, index: &Value) {
    let empty = Vec::new();
    let container = item.pointer(&format!("/inner/{kind}"));
    let impls = container
        .and_then(|c| c.get("impls"))
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let trait_items = container
        .and_then(|c| c.get("items"))
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if !trait_items.is_empty() {
        append_trait_items(md, trait_items, index);
    }
    let methods = collect_inherent_methods(impls, index);
    if !methods.is_empty() {
        let _ = write!(md, "## Methods\n\n{}\n\n", methods.join("\n"));
    }
    let trait_impls = collect_trait_impl_names(impls, index);
    if !trait_impls.is_empty() {
        let lines: Vec<String> = trait_impls.iter().map(|t| format!("- {t}")).collect();
        let _ = write!(md, "## Trait Implementations\n\n{}\n\n", lines.join("\n"));
    }
}

fn render_single_item(item: &Value, index: &Value, crate_version: Option<&str>) -> String {
    let kind = inner_kind(item).unwrap_or("unknown");
    let name = item_name(item).unwrap_or("?");
    let mut md = format!("# {kind} {name}\n\n");
    if let Some(dep) = item.get("deprecation").filter(|d| !d.is_null()) {
        let note = dep
            .get("note")
            .and_then(Value::as_str)
            .map(|n| format!(": {n}"))
            .unwrap_or_default();
        let _ = write!(md, "> **Deprecated**{note}\n\n");
    }
    if let Some(decl) = render_item_decl(item) {
        let _ = write!(md, "```rust\n{decl}\n```\n\n");
    }
    if let Some(docs) = item
        .get("docs")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    {
        let _ = write!(md, "{docs}\n\n");
    }
    if ["struct", "enum", "trait", "union"].contains(&kind) {
        append_impls(&mut md, item, kind, index);
    }
    if kind == "enum" {
        append_variants(&mut md, item, index);
    }
    if let Some(version) = crate_version {
        let _ = write!(md, "---\n*{version}*\n");
    }
    md
}

const KIND_ORDER: [(&str, &str); 10] = [
    ("module", "Modules"),
    ("macro_def", "Macros"),
    ("struct", "Structs"),
    ("enum", "Enums"),
    ("trait", "Traits"),
    ("function", "Functions"),
    ("type_alias", "Type Aliases"),
    ("constant", "Constants"),
    ("static", "Statics"),
    ("union", "Unions"),
];

struct ModuleEntry {
    name: String,
    docs: String,
    decl: Option<String>,
}

fn resolve_module_entry(item: &Value, index: &Value) -> Option<(String, Value)> {
    if let Some(use_) = item.pointer("/inner/use") {
        let name = use_.get("name").and_then(Value::as_str)?.to_string();
        let resolved = use_.get("id").and_then(|id| index_get(index, id))?;
        return Some((name, resolved.clone()));
    }
    let name = item_name(item)?.to_string();
    Some((name, item.clone()))
}

fn is_hidden(item: &Value) -> bool {
    match item.get("visibility") {
        Some(Value::String(s)) => s == "crate",
        Some(v) => v.get("restricted").is_some(),
        None => false,
    }
}

fn group_module_items(
    module: &Value,
    index: &Value,
) -> std::collections::BTreeMap<String, Vec<ModuleEntry>> {
    let mut groups: std::collections::BTreeMap<String, Vec<ModuleEntry>> = Default::default();
    let items = module
        .pointer("/inner/module/items")
        .and_then(Value::as_array);
    for id in items.into_iter().flatten() {
        let Some(item) = index_get(index, id) else {
            continue;
        };
        let Some((name, resolved)) = resolve_module_entry(item, index) else {
            continue;
        };
        if is_hidden(&resolved) {
            continue;
        }
        let kind = inner_kind(&resolved).unwrap_or("unknown").to_string();
        let docs = first_line(resolved.get("docs").and_then(Value::as_str).unwrap_or(""));
        let decl = render_item_decl(&resolved);
        groups
            .entry(kind)
            .or_default()
            .push(ModuleEntry { name, docs, decl });
    }
    groups
}

fn render_module(
    module: &Value,
    index: &Value,
    crate_version: Option<&str>,
    target: &Target,
) -> String {
    let mut md = format!("# {}\n\n", target.module_path.join("::"));
    if let Some(docs) = module
        .get("docs")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    {
        let _ = write!(md, "{docs}\n\n");
    }
    let groups = group_module_items(module, index);
    for (kind, label) in KIND_ORDER {
        let Some(entries) = groups.get(kind).filter(|e| !e.is_empty()) else {
            continue;
        };
        let _ = write!(md, "## {label}\n\n");
        for entry in entries {
            let doc = if entry.docs.is_empty() {
                String::new()
            } else {
                format!(" — {}", entry.docs)
            };
            match (&entry.decl, kind) {
                (Some(decl), "function") => {
                    let _ = writeln!(md, "- `{decl}`{doc}");
                }
                _ => {
                    let _ = writeln!(md, "- **{}**{doc}", entry.name);
                }
            }
        }
        md.push('\n');
    }
    if let Some(version) = crate_version {
        let _ = write!(md, "---\n*{version}*\n");
    }
    md
}

// --- fetch + cache ---

fn sanitize_segment(value: &str) -> String {
    SANITIZE.replace_all(value, "_").to_string()
}

fn cache_path(target: &Target) -> PathBuf {
    let crate_seg = sanitize_segment(&target.crate_name);
    let version_seg = if target.version == "latest" {
        chrono::Utc::now().format("%Y-%m-%d").to_string()
    } else {
        sanitize_segment(&target.version)
    };
    mikmik_core::config::Settings::config_dir()
        .join("docsrs_cache")
        .join(format!("docsrs_{crate_seg}_{version_seg}"))
        .join("rustdoc.json")
}

fn read_cache(target: &Target) -> Option<Value> {
    let json = std::fs::read_to_string(cache_path(target)).ok()?;
    let crate_: Value = serde_json::from_str(&json).ok()?;
    crate_.get("index").is_some().then_some(crate_)
}

fn write_cache(target: &Target, json: &str) {
    let path = cache_path(target);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

fn gunzip_capped(compressed: &[u8]) -> Option<String> {
    let mut decoder = flate2::read::GzDecoder::new(compressed).take(MAX_GUNZIP_BYTES + 1);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    if out.len() as u64 > MAX_GUNZIP_BYTES {
        return None;
    }
    String::from_utf8(out).ok()
}

async fn download_gzip(url: &str, timeout: Duration) -> Option<Vec<u8>> {
    let client = reqwest::Client::builder().timeout(timeout).build().ok()?;
    let response = client
        .get(url)
        .header("User-Agent", "mikmik-web-fetch/1.0")
        .header("Accept", "application/gzip")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut stream = response.bytes_stream();
    let mut compressed = Vec::new();
    while let Some(chunk) = stream.next().await {
        compressed.extend_from_slice(&chunk.ok()?);
        if compressed.len() > MAX_DOWNLOAD_BYTES {
            break;
        }
    }
    Some(compressed)
}

async fn load_crate(target: &Target, timeout: Duration) -> Option<Value> {
    if let Some(cached) = read_cache(target) {
        return Some(cached);
    }
    let url = format!(
        "https://docs.rs/crate/{}/{}/json.gz",
        target.crate_name, target.version
    );
    let compressed = download_gzip(&url, timeout).await?;
    let json = gunzip_capped(&compressed)?;
    let crate_: Value = serde_json::from_str(&json).ok()?;
    if crate_.get("index").is_some() {
        write_cache(target, &json);
    }
    Some(crate_)
}

fn render_target(crate_: &Value, target: &Target) -> Option<String> {
    let index = crate_.get("index")?;
    let root = index_get(index, crate_.get("root")?)?;
    let module = walk_module_path(root, &target.module_path, index)?;
    let crate_version = crate_.get("crate_version").and_then(Value::as_str);
    if let Some(name) = &target.item_name {
        let found = find_item_in_module(module, name, index)?;
        return Some(render_single_item(found, index, crate_version));
    }
    Some(render_module(module, index, crate_version, target))
}

#[async_trait]
impl SpecialHandler for DocsRsHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let target = parse_docs_rs_url(url)?;
        let crate_ = load_crate(&target, timeout).await?;
        let md = render_target(&crate_, &target)?;
        Some(build_result(
            &md,
            url,
            "docs.rs",
            vec!["Fetched via docs.rs rustdoc JSON".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_reads_item_page() {
        let target =
            parse_docs_rs_url("https://docs.rs/serde/latest/serde/trait.Serialize.html").unwrap();
        assert_eq!(target.crate_name, "serde");
        assert_eq!(target.version, "latest");
        assert_eq!(target.module_path, vec!["serde".to_string()]);
        assert_eq!(target.item_name.as_deref(), Some("Serialize"));
    }

    #[test]
    fn parse_rejects_crate_overview() {
        assert!(parse_docs_rs_url("https://docs.rs/crate/serde/latest").is_none());
    }

    #[test]
    fn render_type_handles_references_and_generics() {
        let ty = json!({
            "borrowed_ref": {
                "lifetime": "a",
                "is_mutable": false,
                "type": { "resolved_path": { "path": "Vec", "args": { "angle_bracketed": { "args": [ { "type": { "primitive": "u8" } } ] } } } }
            }
        });
        assert_eq!(render_type(&ty, 0), "&'a Vec<u8>");
    }

    #[test]
    fn function_sig_renders_modifiers() {
        let func = json!({
            "is_const": false,
            "is_async": true,
            "is_unsafe": false,
            "generics": { "params": [] },
            "sig": {
                "inputs": [["self", { "generic": "Self" }], ["value", { "primitive": "u32" }]],
                "output": { "primitive": "bool" }
            }
        });
        assert_eq!(
            render_function_sig("run", &func),
            "async fn run(Self, value: u32) -> bool"
        );
    }

    #[test]
    fn single_item_lays_out_struct() {
        let index = json!({
            "1": {
                "name": "Config",
                "docs": "A config.",
                "inner": { "struct": { "generics": { "params": [] }, "impls": [] } }
            }
        });
        let item = index.get("1").unwrap();
        let md = render_single_item(item, &index, Some("1.0.0"));
        assert!(md.contains("# struct Config"));
        assert!(md.contains("```rust\nstruct Config\n```"));
        assert!(md.contains("A config."));
        assert!(md.contains("---\n*1.0.0*"));
    }
}
