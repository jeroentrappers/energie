//! Just enough HTML scraping for the browser-style logins (VW identity, Fluvius Azure B2C):
//! the first `<form>` with its inputs, a JSON object embedded in a script, a `csrf_token`.

use serde_json::Value;

/// A tag's name and attributes (values entity-decoded), as `HTMLParser` hands them over.
#[derive(Debug, PartialEq)]
struct Tag {
    name: String,
    closing: bool,
    attrs: Vec<(String, String)>,
}

impl Tag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Decode the entities that occur in attribute values.
pub fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Parse the tag starting right after a `<`; `None` for comments, doctypes and text.
fn parse_tag(s: &str) -> Option<Tag> {
    let (closing, s) = match s.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let end = s.find('>')?;
    let inner = s[..end].trim_end_matches('/');
    let name_len = inner.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(inner.len());
    if name_len == 0 {
        return None;
    }
    let name = inner[..name_len].to_ascii_lowercase();
    let mut rest = &inner[name_len..];
    let mut attrs = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        if rest.is_empty() {
            break;
        }
        let klen = rest.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(rest.len());
        let key = rest[..klen].to_ascii_lowercase();
        rest = rest[klen..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            let (v, tail) = match after.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let body = &after[1..];
                    let close = body.find(q).unwrap_or(body.len());
                    (&body[..close], body.get(close + 1..).unwrap_or(""))
                }
                _ => {
                    let len = after.find(char::is_whitespace).unwrap_or(after.len());
                    (&after[..len], &after[len..])
                }
            };
            rest = tail;
            unescape(v)
        } else {
            String::new()
        };
        attrs.push((key, value));
    }
    Some(Tag { name, closing, attrs })
}

fn tags(html: &str) -> impl Iterator<Item = Tag> + '_ {
    html.match_indices('<').filter_map(|(i, _)| parse_tag(&html[i + 1..]))
}

/// The first form's `action` and its named inputs, in document order.
pub fn first_form(html: &str) -> (Option<String>, Vec<(String, String)>) {
    let mut action = None;
    let mut fields = Vec::new();
    let mut in_form = false;
    for tag in tags(html) {
        match (tag.name.as_str(), tag.closing) {
            ("form", false) if action.is_none() && !in_form => {
                action = Some(tag.attr("action").unwrap_or_default().to_string());
                in_form = true;
            }
            ("form", true) if in_form => break,
            ("input", false) if in_form => {
                if let Some(name) = tag.attr("name").filter(|n| !n.is_empty()) {
                    fields.push((name.to_string(), tag.attr("value").unwrap_or_default().to_string()));
                }
            }
            _ => {}
        }
    }
    (action.filter(|a| !a.is_empty()), fields)
}

/// The JSON object that starts at the first `{` after `marker` (brace matching, like the
/// integrations' scrapers: braces inside strings are not special).
pub fn object_after(html: &str, marker: &str) -> Option<Value> {
    let start = html.find(marker)?;
    let brace = start + html[start..].find('{')?;
    let mut depth = 0usize;
    for (i, c) in html[brace..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&html[brace..=brace + i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// `csrf_token: '…'` / `csrf_token = "…"` in a script.
pub fn csrf_token(html: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = html[from..].find("csrf_token") {
        let rest = html[from + i + "csrf_token".len()..].trim_start();
        if let Some(rest) = rest.strip_prefix([':', '=']) {
            let rest = rest.trim_start();
            if let Some(body) = rest.strip_prefix(['\'', '"'])
                && let Some(end) = body.find(['\'', '"']).filter(|e| *e > 0)
            {
                return Some(body[..end].to_string());
            }
        }
        from += i + 1;
    }
    None
}

/// `var NAME = {…};` in a script: the object up to the first `};` (the B2C pages' `SETTINGS`
/// and `SA_FIELDS`).
pub fn js_var(html: &str, name: &str) -> Option<Value> {
    let marker = format!("var {name}");
    let mut from = 0;
    while let Some(i) = html[from..].find(&marker) {
        let rest = html[from + i + marker.len()..].trim_start();
        if let Some(rest) = rest.strip_prefix('=') {
            let rest = rest.trim_start();
            if rest.starts_with('{') {
                return rest.find("};").and_then(|end| serde_json::from_str(&rest[..=end]).ok());
            }
        }
        from += i + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_first_form() {
        let html = r#"<!DOCTYPE html><!-- c --><html><body>
            <form id="emailPasswordForm" method="POST" action="/signin-service/v1/x/login/identifier">
              <input type="hidden" name="_csrf" value="abc&amp;def"/>
              <input type=hidden name=relayState value=rs1>
              <input type="email" name="email" value='' />
              <input type="submit" value="Next">
              <input disabled name="">
            </form>
            <form action="/other"><input name="nope" value="x"></form>"#;
        let (action, fields) = first_form(html);
        assert_eq!(action.as_deref(), Some("/signin-service/v1/x/login/identifier"));
        assert_eq!(
            fields,
            vec![("_csrf".into(), "abc&def".into()), ("relayState".into(), "rs1".into()), ("email".into(), "".into())]
        );
        assert_eq!(first_form("<p>no form</p><input name=a>"), (None, vec![]));
        assert_eq!(first_form("<form><input name=a value=1></form>"), (None, vec![("a".into(), "1".into())]));
        assert_eq!(parse_tag("form action=\"x"), None);
        assert_eq!(parse_tag("input name=\"unterminated>").unwrap().attr("name"), Some("unterminated"));
        assert_eq!(unescape("&lt;&quot;&#39;&#x27;&gt;"), "<\"''>");
    }

    #[test]
    fn finds_embedded_objects() {
        let html = r#"<script>window._IDK = { templateModel: {"hmac":"h1","relayState":"r1","nested":{"a":1}}, csrf_token: 'tok-1' };</script>"#;
        let m = object_after(html, "templateModel").unwrap();
        assert_eq!((m["hmac"].as_str(), m["nested"]["a"].as_i64()), (Some("h1"), Some(1)));
        assert_eq!(object_after(html, "absent"), None);
        assert_eq!(object_after("templateModel: none", "templateModel"), None);
        assert_eq!(object_after("templateModel: {\"a\": {", "templateModel"), None);
        assert_eq!(object_after("templateModel: {bad}", "templateModel"), None);
        assert_eq!(csrf_token(html).as_deref(), Some("tok-1"));
        assert_eq!(csrf_token(r#"csrf_token_x csrf_token = "t2""#).as_deref(), Some("t2"));
        assert_eq!(csrf_token("csrf_token: x, csrf_token: ''"), None);
    }

    #[test]
    fn finds_js_vars() {
        let html = "var SETTINGSX = 1; var SETTINGS = {\"csrf\":\"c\",\"hosts\":{\"tenant\":\"/t\"}};\nvar SA_FIELDS = {\"AttributeFields\":[]};";
        assert_eq!(js_var(html, "SETTINGS").unwrap()["hosts"]["tenant"], "/t");
        assert!(js_var(html, "SA_FIELDS").unwrap()["AttributeFields"].is_array());
        assert_eq!(js_var(html, "NOPE"), None);
        assert_eq!(js_var("var SETTINGS = 5;", "SETTINGS"), None);
        assert_eq!(js_var("var SETTINGS = {oops};", "SETTINGS"), None);
    }
}
