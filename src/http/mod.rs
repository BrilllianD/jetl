//! Parses `.http` request files for the curl runner: blocks, headers, body, the `dev` environment and curl's
//! argument list. Pure apart from reading one environment file. `HEAD` goes to curl as `--head`, not `-X HEAD`.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

/// One request of a `.http` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// `# @name x` / `// @name x`, else the text after `###`, else `{METHOD} {url}`.
    pub name: String,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// 0-based lines of the block.
    pub lines: std::ops::Range<usize>,
}

/// Seconds after which curl gives up.
pub const MAX_TIME_SECS: u64 = 30;

fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

fn is_separator(line: &str) -> bool {
    line.trim_start().starts_with("###")
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    (t.starts_with('#') && !t.starts_with("###")) || t.starts_with("//")
}

/// The name given by a `@name` comment line, when it is one.
fn name_comment(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let t = t.strip_prefix("//").unwrap_or_else(|| t.trim_start_matches('#')).trim();
    let rest = t.strip_prefix("@name")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim();
    (!rest.is_empty()).then_some(rest)
}

fn is_token(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
}

/// Byte offsets of the `{{` that a later `}}` closes. Braces pair like
/// brackets, innermost first; an unpaired `{{` or `}}` is plain text.
fn brace_openers(line: &str) -> Vec<usize> {
    let b = line.as_bytes();
    let mut marks = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if (b[i] == b'{' || b[i] == b'}') && b[i + 1] == b[i] {
            marks.push((i, b[i] == b'{'));
            i += 2;
        } else {
            i += 1;
        }
    }
    let (mut out, mut pending) = (Vec::new(), 0usize);
    for &(at, open) in marks.iter().rev() {
        if !open {
            pending += 1;
        } else if pending > 0 {
            pending -= 1;
            out.push(at);
        }
    }
    out
}

/// The words of a request line; spaces inside a paired `{{ ... }}` do not split.
fn words(line: &str) -> Vec<&str> {
    let openers = brace_openers(line);
    let mut out = Vec::new();
    let (mut start, mut depth) = (None, 0usize);
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_whitespace() && depth == 0 {
            if let Some(s) = start.take() {
                out.push(&line[s..i]);
            }
            continue;
        }
        start.get_or_insert(i);
        if c == '{' && chars.peek().is_some_and(|&(_, n)| n == '{') {
            chars.next();
            if openers.contains(&i) {
                depth += 1;
            }
        } else if c == '}' && chars.peek().is_some_and(|&(_, n)| n == '}') {
            chars.next();
            depth = depth.saturating_sub(1);
        }
    }
    if let Some(s) = start {
        out.push(&line[s..]);
    }
    out
}

/// Method and url of a request line.
fn request_line(line: &str) -> Option<(String, String)> {
    let mut words = words(line).into_iter();
    let first = words.next()?;
    if first.chars().all(|c| c.is_ascii_uppercase()) {
        return Some((first.to_string(), words.next()?.to_string()));
    }
    let lower = first.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(("GET".to_string(), first.to_string()));
    }
    None
}

fn parse_block(lines: &[&str], range: std::ops::Range<usize>) -> Option<HttpRequest> {
    let mut i = range.start;
    let mut separator = "";
    if is_separator(lines[i]) {
        separator = lines[i].trim_start().trim_start_matches("###").trim();
        i += 1;
    }
    let mut name = None;
    while i < range.end && (lines[i].trim().is_empty() || is_comment(lines[i])) {
        if let Some(n) = name_comment(lines[i]) {
            name = Some(n);
        }
        i += 1;
    }
    if i >= range.end {
        return None;
    }
    let (method, url) = request_line(lines[i])?;
    i += 1;
    let mut headers = Vec::new();
    while i < range.end && !lines[i].trim().is_empty() {
        if !is_comment(lines[i])
            && let Some((n, v)) = lines[i].split_once(':')
            && is_token(n.trim())
        {
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
        i += 1;
    }
    let mut body: Vec<&str> = Vec::new();
    let mut in_script = false;
    for line in &lines[(i + 1).min(range.end)..range.end] {
        let t = line.trim_start();
        if in_script || t.starts_with("> {%") {
            in_script = !line.trim_end().ends_with("%}");
        } else if !(t.starts_with("> ") || t.starts_with("<> ")) {
            body.push(line);
        }
    }
    while body.first().is_some_and(|l| l.trim().is_empty()) {
        body.remove(0);
    }
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }
    let name = match name {
        Some(n) => n.to_string(),
        None if !separator.is_empty() => separator.to_string(),
        None => format!("{method} {url}"),
    };
    Some(HttpRequest { name, method, url, headers, body: (!body.is_empty()).then(|| body.join("\n")), lines: range })
}

/// All requests of `text`, in order.
pub fn parse(text: &str) -> Vec<HttpRequest> {
    let lines = split_lines(text);
    let mut starts = vec![0];
    starts.extend((1..lines.len()).filter(|&i| is_separator(lines[i])));
    let mut out = Vec::new();
    for (k, &start) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(lines.len());
        out.extend(parse_block(&lines, start..end));
    }
    out
}

/// The request whose block holds `line`.
pub fn request_at(text: &str, line: usize) -> Option<HttpRequest> {
    parse(text).into_iter().find(|r| r.lines.contains(&line))
}

/// The `dev` variables of the first `http-client.env.json` found in `dirs`; no file gives an empty map.
pub fn load_env(dirs: &[&Path]) -> Result<BTreeMap<String, String>, String> {
    let Some(file) = dirs.iter().map(|d| d.join("http-client.env.json")).find(|f| f.is_file()) else {
        return Ok(BTreeMap::new());
    };
    let text = std::fs::read_to_string(&file).map_err(|e| format!("http-client.env.json: {e}"))?;
    let root: Value = serde_json::from_str(&text).map_err(|e| format!("http-client.env.json: {e}"))?;
    let Value::Object(root) = root else {
        return Err("http-client.env.json: not a JSON object".into());
    };
    let mut vars = BTreeMap::new();
    match root.get("dev") {
        None => {}
        Some(Value::Object(dev)) => {
            for (k, v) in dev {
                match v {
                    Value::String(s) => vars.insert(k.clone(), s.clone()),
                    Value::Number(n) => vars.insert(k.clone(), n.to_string()),
                    _ => None,
                };
            }
        }
        Some(_) => return Err("http-client.env.json: \"dev\" is not an object".into()),
    }
    Ok(vars)
}

fn fill(s: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find("{{") {
        let Some(len) = rest[open + 2..].find("}}") else {
            break;
        };
        out.push_str(&rest[..open]);
        let name = rest[open + 2..open + 2 + len].trim();
        match vars.get(name) {
            Some(v) => out.push_str(v),
            None => return Err(format!("Unknown variable {{{{{name}}}}}")),
        }
        rest = &rest[open + 2 + len + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `req` with `{{name}}` filled in the url, header values and body.
pub fn substitute(req: &HttpRequest, vars: &BTreeMap<String, String>) -> Result<HttpRequest, String> {
    Ok(HttpRequest {
        url: fill(&req.url, vars)?,
        headers: req.headers.iter().map(|(n, v)| Ok((n.clone(), fill(v, vars)?))).collect::<Result<_, String>>()?,
        body: req.body.as_deref().map(|b| fill(b, vars)).transpose()?,
        ..req.clone()
    })
}

/// curl's arguments for `req`; the caller writes the body to `body_file`.
pub fn curl_argv(req: &HttpRequest, body_file: Option<&Path>) -> Vec<String> {
    // -q must come first: curl then skips ~/.curlrc.
    let mut argv: Vec<String> = ["curl", "-q", "-sS", "-i", "--max-time"].map(String::from).into();
    argv.push(MAX_TIME_SECS.to_string());
    if req.method == "HEAD" {
        // `-X HEAD` makes curl wait for a body the server never sends.
        argv.push("--head".into());
    } else {
        argv.push("-X".into());
        argv.push(req.method.clone());
    }
    for (n, v) in &req.headers {
        argv.push("-H".into());
        // `Name;` is curl's form for sending an empty header.
        argv.push(if v.is_empty() { format!("{n};") } else { format!("{n}: {v}") });
    }
    if let Some(p) = body_file {
        argv.push("--data-binary".into());
        argv.push(format!("@{}", p.display()));
    }
    argv.push(req.url.clone());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("jetl-test-http-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn req(method: &str, url: &str) -> HttpRequest {
        HttpRequest { name: format!("{method} {url}"), method: method.into(), url: url.into(), headers: vec![], body: None, lines: 0..1 }
    }

    const FULL: &str = "POST {{host}}/items HTTP/1.1\nContent-Type: application/json\nX-Time: 12:30\n\n{\n  \"a\": 1\n}\n\n\n";

    #[test]
    fn spaces_inside_variable_braces_stay_in_the_url() {
        let got = parse("POST http://{{ host }}/items/{{id}} HTTP/1.1\n");
        assert_eq!(got[0].url, "http://{{ host }}/items/{{id}}");
    }

    #[test]
    fn parses_two_named_blocks() {
        let text =
            "### Get users\nGET http://x/users\n###\n# @name create\nPOST http://x/users\n###\n// @name third\nDELETE http://x/users/1";
        let got = parse(text);
        assert_eq!(
            got,
            vec![
                HttpRequest { name: "Get users".into(), lines: 0..2, ..req("GET", "http://x/users") },
                HttpRequest { name: "create".into(), lines: 2..5, ..req("POST", "http://x/users") },
                HttpRequest { name: "third".into(), lines: 5..8, ..req("DELETE", "http://x/users/1") },
            ]
        );
    }

    #[test]
    fn name_falls_back_to_method_and_url() {
        let got = parse("###\nGET http://x/y\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "GET http://x/y");
        assert_eq!(got[0].lines, 0..3);
    }

    #[test]
    fn bare_url_line_is_get() {
        let got = parse("https://example.com/a HTTP/1.1\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].url, "https://example.com/a");
    }

    #[test]
    fn headers_and_json_body() {
        let got = parse(FULL);
        assert_eq!(
            got,
            vec![HttpRequest {
                name: "POST {{host}}/items".into(),
                method: "POST".into(),
                url: "{{host}}/items".into(),
                headers: vec![("Content-Type".into(), "application/json".into()), ("X-Time".into(), "12:30".into()),],
                body: Some("{\n  \"a\": 1\n}".into()),
                lines: 0..10,
            }]
        );
    }

    #[test]
    fn response_handlers_dropped() {
        let text = "POST http://x\n\n{\"a\": 1}\n> {%\nclient.test();\nclient.log(1);\n%}\n> ./handler.js\n<> 2024-01-01T000000.200.json\n";
        let got = parse(text);
        assert_eq!(got[0].body.as_deref(), Some("{\"a\": 1}"));
    }

    #[test]
    fn comments_skipped_before_request_and_in_headers() {
        let text = "# note\n// note\nGET http://x\n# c\nA: 1\n// c\nB: 2\n";
        let got = parse(text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].headers, vec![("A".to_string(), "1".to_string()), ("B".into(), "2".into())]);
    }

    #[test]
    fn block_without_request_yields_nothing() {
        let text = "# only\n// comments\n###\nhello world\nGET http://no\n###\nGET http://yes\n";
        let got = parse(text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "http://yes");
    }

    #[test]
    fn crlf_text_parses() {
        let crlf = FULL.replace('\n', "\r\n");
        assert_eq!(parse(&crlf), parse(FULL));
    }

    #[test]
    fn request_at_lines() {
        let text = "# c\n### A\n# in block\nGET http://a\n\nbody\n###\nhello\n###\nGET http://b\n";
        // lines: 0 comment (no request), 1 sep, 2 comment, 3 req, 4 blank, 5 body, 6 sep, 7 hello,
        // 8 sep, 9 req, 10 empty
        assert_eq!(request_at(text, 0), None);
        assert_eq!(request_at(text, 1).unwrap().url, "http://a");
        assert_eq!(request_at(text, 2).unwrap().name, "A");
        assert_eq!(request_at(text, 3).unwrap().url, "http://a");
        assert_eq!(request_at(text, 5).unwrap().body.as_deref(), Some("body"));
        assert_eq!(request_at(text, 7), None);
        assert_eq!(request_at(text, 10).unwrap().url, "http://b");
        assert_eq!(request_at(text, 11), None);
    }

    #[test]
    fn invalid_header_name_ignored() {
        let got = parse("GET http://x\n@file: x\nBad Name: y\nGood: z\n");
        assert_eq!(got[0].headers, vec![("Good".to_string(), "z".to_string())]);
    }

    #[test]
    fn env_missing_file_is_empty() {
        let d = tmp("missing");
        assert_eq!(load_env(&[d.as_path()]), Ok(BTreeMap::new()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn env_dev_strings_and_numbers() {
        let a = tmp("env-a");
        let b = tmp("env-b");
        let json = r#"{"dev": {"host": "http://127.0.0.1:8080", "port": 8080, "flag": true}, "prod": {"host": "x"}}"#;
        std::fs::write(b.join("http-client.env.json"), json).unwrap();
        let got = load_env(&[a.as_path(), b.as_path()]).unwrap();
        let want: BTreeMap<String, String> =
            [("host".to_string(), "http://127.0.0.1:8080".to_string()), ("port".to_string(), "8080".to_string())].into();
        assert_eq!(got, want);
        std::fs::write(a.join("http-client.env.json"), r#"{"dev": {"host": "first"}}"#).unwrap();
        let got = load_env(&[a.as_path(), b.as_path()]).unwrap();
        assert_eq!(got.get("host").map(String::as_str), Some("first"));
        assert_eq!(got.len(), 1);
        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
    }

    #[test]
    fn env_bad_json_is_error() {
        let d = tmp("bad");
        let f = d.join("http-client.env.json");
        std::fs::write(&f, "{\"dev\": ").unwrap();
        let err = load_env(&[d.as_path()]).unwrap_err();
        assert!(err.starts_with("http-client.env.json: "), "{err}");
        std::fs::write(&f, "[1]").unwrap();
        assert_eq!(load_env(&[d.as_path()]), Err("http-client.env.json: not a JSON object".to_string()));
        std::fs::write(&f, r#"{"prod": {"a": "b"}}"#).unwrap();
        assert_eq!(load_env(&[d.as_path()]), Ok(BTreeMap::new()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn substitute_url_headers_body() {
        let vars: BTreeMap<String, String> = [
            ("host".to_string(), "http://h".to_string()),
            ("token".to_string(), "t{{x}}".to_string()),
            ("id".to_string(), "7".to_string()),
        ]
        .into();
        let r = HttpRequest {
            name: "n {{host}}".into(),
            method: "POST".into(),
            url: "{{host}}/a".into(),
            headers: vec![("{{h}}".into(), "Bearer {{ token }}".into())],
            body: Some("{\"id\": {{id}}, \"o\": \"{{oops\"}".into()),
            lines: 3..9,
        };
        let got = substitute(&r, &vars).unwrap();
        assert_eq!(
            got,
            HttpRequest {
                url: "http://h/a".into(),
                headers: vec![("{{h}}".into(), "Bearer t{{x}}".to_string())],
                body: Some("{\"id\": 7, \"o\": \"{{oops\"}".into()),
                ..r
            }
        );
    }

    #[test]
    fn substitute_unknown_variable() {
        let r = HttpRequest { url: "{{missing}}/x".into(), ..req("GET", "x") };
        assert_eq!(substitute(&r, &BTreeMap::new()), Err("Unknown variable {{missing}}".to_string()));
    }

    #[test]
    fn curl_argv_without_body() {
        let mut r = req("GET", "http://x/y");
        r.headers = vec![("A".into(), "1".into())];
        let want: Vec<String> =
            ["curl", "-q", "-sS", "-i", "--max-time", "30", "-X", "GET", "-H", "A: 1", "http://x/y"].map(String::from).to_vec();
        assert_eq!(curl_argv(&r, None), want);
    }

    #[test]
    fn curl_argv_head_uses_the_head_flag() {
        let r = req("HEAD", "http://x/y");
        let want: Vec<String> = ["curl", "-q", "-sS", "-i", "--max-time", "30", "--head", "http://x/y"].map(String::from).to_vec();
        assert_eq!(curl_argv(&r, None), want);
    }

    #[test]
    fn curl_argv_empty_header_value_uses_the_semicolon_form() {
        let mut r = req("GET", "http://x/y");
        r.headers = vec![("X-Empty".into(), "".into()), ("A".into(), "1".into())];
        let got = curl_argv(&r, None);
        assert_eq!(got[got.len() - 5..], ["-H", "X-Empty;", "-H", "A: 1", "http://x/y"].map(String::from));
    }

    #[test]
    fn braces_pair_by_order_not_by_count() {
        assert_eq!(words("GET http://x/}} {{oops HTTP/1.1"), ["GET", "http://x/}}", "{{oops", "HTTP/1.1"]);
        assert_eq!(words("GET http://x/{{ a }} {{b HTTP/1.1"), ["GET", "http://x/{{ a }}", "{{b", "HTTP/1.1"]);
        assert_eq!(words("{{a {{b}} c"), ["{{a", "{{b}}", "c"]);
        assert_eq!(parse("GET http://x/{{ a }} {{b HTTP/1.1\n")[0].url, "http://x/{{ a }}");
    }

    #[test]
    fn unclosed_braces_split_like_plain_text() {
        let got = parse("GET http://x/{{oops HTTP/1.1\n");
        assert_eq!(got[0].url, "http://x/{{oops");
    }

    #[test]
    fn env_dev_not_an_object_is_error() {
        let d = tmp("dev-array");
        std::fs::write(d.join("http-client.env.json"), r#"{"dev": [1]}"#).unwrap();
        assert_eq!(load_env(&[d.as_path()]), Err("http-client.env.json: \"dev\" is not an object".to_string()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn curl_argv_with_body() {
        let r = req("POST", "http://x/y");
        let want: Vec<String> =
            ["curl", "-q", "-sS", "-i", "--max-time", "30", "-X", "POST", "--data-binary", "@/tmp/jetl-test.body", "http://x/y"]
                .map(String::from)
                .to_vec();
        assert_eq!(curl_argv(&r, Some(Path::new("/tmp/jetl-test.body"))), want);
    }
}
