//! Format-preserving INI in git-config style.
//!
//! The file is kept as a list of lines. Only the lines a mutation touches are
//! rewritten; comments, blank lines, ordering and unrelated sections survive
//! byte for byte.
//!
//! Dialect:
//! - `[section]` and `[section "sub"]` headers. Section and key names are
//!   case-insensitive, subsections are case-sensitive.
//! - `key = value`, or a bare `key` meaning `true`.
//! - Full-line comments start with `#` or `;`. A trailing comment starts at a
//!   `#` that follows whitespace. `;` does not start a trailing comment, so
//!   prompts can contain semicolons.
//! - A value that starts with `"` and ends with a closing `"` is quoted and
//!   supports `\\ \" \n \t`. Anything else is taken literally.

use std::fmt;

#[derive(Debug, Clone)]
enum Line {
    /// Blank lines, comments, anything we keep verbatim.
    Other(String),
    Section {
        raw: String,
        name: String,
        sub: Option<String>,
    },
    Entry {
        indent: String,
        raw_key: String,
        key: String,
        value: String,
        /// Everything after the value (whitespace plus any trailing comment).
        trailing: String,
        /// The original text, reused verbatim until the entry is modified.
        raw: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Ini {
    lines: Vec<Line>,
    crlf: bool,
}

#[derive(Debug)]
pub struct ParseError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

/// One `section[.sub].key = value` entry, flattened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
    pub line: usize,
}

/// A dotted key split into its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPath {
    pub section: String,
    pub sub: Option<String>,
    pub name: String,
}

impl KeyPath {
    /// `a.b` is section `a`, key `b`. `a.x.y.b` is section `a`, subsection
    /// `x.y`, key `b` (first and last dot, as git does).
    pub fn parse(dotted: &str) -> Option<KeyPath> {
        let first = dotted.find('.')?;
        let last = dotted.rfind('.')?;
        let section = &dotted[..first];
        let name = &dotted[last + 1..];
        let sub = if first == last {
            None
        } else {
            Some(dotted[first + 1..last].to_string())
        };
        if !valid_ident(section) || !valid_ident(name) {
            return None;
        }
        if let Some(s) = &sub
            && (s.is_empty() || s.contains('\n'))
        {
            return None;
        }
        Some(KeyPath {
            section: section.to_ascii_lowercase(),
            sub,
            name: name.to_ascii_lowercase(),
        })
    }

    pub fn dotted(&self) -> String {
        flatten(&self.section, self.sub.as_deref(), &self.name)
    }
}

pub fn flatten(section: &str, sub: Option<&str>, key: &str) -> String {
    match sub {
        Some(s) => format!("{section}.{s}.{key}"),
        None => format!("{section}.{key}"),
    }
}

fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Ini {
    pub fn parse(text: &str) -> Result<Ini, ParseError> {
        let crlf = text.contains("\r\n");
        let mut lines = Vec::new();
        let mut in_section = false;
        for (i, raw) in text.lines().enumerate() {
            let n = i + 1;
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                lines.push(Line::Other(raw.to_string()));
            } else if trimmed.starts_with('[') {
                let (name, sub) = parse_header(trimmed).map_err(|msg| ParseError { line: n, msg })?;
                in_section = true;
                lines.push(Line::Section {
                    raw: raw.to_string(),
                    name,
                    sub,
                });
            } else {
                if !in_section {
                    return Err(ParseError {
                        line: n,
                        msg: "entry outside of a [section]".into(),
                    });
                }
                lines.push(parse_entry(raw).map_err(|msg| ParseError { line: n, msg })?);
            }
        }
        Ok(Ini { lines, crlf })
    }

    pub fn render(&self) -> String {
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut out = String::new();
        for l in &self.lines {
            out.push_str(match l {
                Line::Other(s) => s,
                Line::Section { raw, .. } | Line::Entry { raw, .. } => raw,
            });
            out.push_str(eol);
        }
        out
    }

    /// Every entry in file order.
    pub fn entries(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        let mut cur: Option<(&str, Option<&str>)> = None;
        for (i, l) in self.lines.iter().enumerate() {
            match l {
                Line::Section { name, sub, .. } => cur = Some((name, sub.as_deref())),
                Line::Entry { key, value, .. } => {
                    if let Some((s, sub)) = cur {
                        out.push(Entry {
                            key: flatten(s, sub, key),
                            value: value.clone(),
                            line: i + 1,
                        });
                    }
                }
                Line::Other(_) => {}
            }
        }
        out
    }

    /// The last value for `dotted` (later entries win, as in git).
    pub fn get(&self, dotted: &str) -> Option<String> {
        let kp = KeyPath::parse(dotted)?;
        let want = kp.dotted();
        self.entries()
            .into_iter()
            .rev()
            .find(|e| e.key == want)
            .map(|e| e.value)
    }

    /// Set `dotted` to `value`. Replaces the last existing entry in place
    /// (keeping indentation and any trailing comment), otherwise adds the
    /// entry to the end of its section, creating the section if needed.
    pub fn set(&mut self, dotted: &str, value: &str) -> Result<(), String> {
        let kp = KeyPath::parse(dotted).ok_or_else(|| format!("invalid key: {dotted}"))?;
        let enc = encode_value(value);

        let mut last_entry: Option<usize> = None;
        let mut last_in_section: Option<usize> = None; // last entry or header of the matching section
        let mut cur_match = false;
        for (i, l) in self.lines.iter().enumerate() {
            match l {
                Line::Section { name, sub, .. } => {
                    cur_match = *name == kp.section && *sub == kp.sub;
                    if cur_match {
                        last_in_section = Some(i);
                    }
                }
                Line::Entry { key, .. } if cur_match => {
                    last_in_section = Some(i);
                    if *key == kp.name {
                        last_entry = Some(i);
                    }
                }
                _ => {}
            }
        }

        if let Some(i) = last_entry {
            if let Line::Entry {
                indent,
                raw_key,
                value: v,
                trailing,
                raw,
                ..
            } = &mut self.lines[i]
            {
                *raw = format!("{indent}{raw_key} = {enc}{trailing}");
                *v = value.to_string();
            }
            return Ok(());
        }

        let indent = self.guess_indent();
        let new_entry = Line::Entry {
            indent: indent.clone(),
            raw_key: kp.name.clone(),
            key: kp.name.clone(),
            value: value.to_string(),
            trailing: String::new(),
            raw: format!("{indent}{} = {enc}", kp.name),
        };
        if let Some(i) = last_in_section {
            self.lines.insert(i + 1, new_entry);
        } else {
            if !self.lines.is_empty() && !matches!(self.lines.last(), Some(Line::Other(s)) if s.trim().is_empty())
            {
                self.lines.push(Line::Other(String::new()));
            }
            let header = match &kp.sub {
                Some(s) => format!("[{} \"{}\"]", kp.section, escape_sub(s)),
                None => format!("[{}]", kp.section),
            };
            self.lines.push(Line::Section {
                raw: header,
                name: kp.section.clone(),
                sub: kp.sub.clone(),
            });
            self.lines.push(new_entry);
        }
        Ok(())
    }

    /// Remove every entry for `dotted`. A section left with no entries and no
    /// comments is removed too. Returns whether anything was removed.
    pub fn unset(&mut self, dotted: &str) -> bool {
        let Some(kp) = KeyPath::parse(dotted) else {
            return false;
        };
        let mut removed = false;
        let mut cur_match = false;
        let mut kept = Vec::with_capacity(self.lines.len());
        for l in self.lines.drain(..) {
            match &l {
                Line::Section { name, sub, .. } => {
                    cur_match = *name == kp.section && *sub == kp.sub;
                    kept.push(l);
                }
                Line::Entry { key, .. } if cur_match && *key == kp.name => removed = true,
                _ => kept.push(l),
            }
        }
        self.lines = kept;
        if removed {
            self.drop_empty_sections();
        }
        removed
    }

    fn drop_empty_sections(&mut self) {
        let mut out: Vec<Line> = Vec::with_capacity(self.lines.len());
        let mut i = 0;
        while i < self.lines.len() {
            if matches!(self.lines[i], Line::Section { .. }) {
                let mut j = i + 1;
                let mut only_blank = true;
                while j < self.lines.len() && !matches!(self.lines[j], Line::Section { .. }) {
                    match &self.lines[j] {
                        Line::Other(s) if s.trim().is_empty() => {}
                        _ => only_blank = false,
                    }
                    j += 1;
                }
                if only_blank {
                    i = j;
                    continue;
                }
            }
            out.push(self.lines[i].clone());
            i += 1;
        }
        self.lines = out;
    }

    /// Indentation of the first existing entry, so new entries match the file.
    fn guess_indent(&self) -> String {
        self.lines
            .iter()
            .find_map(|l| match l {
                Line::Entry { indent, .. } => Some(indent.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "    ".to_string())
    }
}

fn escape_sub(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_header(t: &str) -> Result<(String, Option<String>), String> {
    // t starts with '[' and is trimmed. A trailing comment after ']' is allowed.
    let close = find_header_close(t).ok_or("unterminated section header")?;
    let inner = t[1..close].trim();
    let rest = t[close + 1..].trim();
    if !rest.is_empty() && !rest.starts_with('#') && !rest.starts_with(';') {
        return Err("unexpected text after section header".into());
    }
    match inner.find(char::is_whitespace) {
        None => {
            if !valid_ident(inner) {
                return Err(format!("invalid section name: {inner}"));
            }
            Ok((inner.to_ascii_lowercase(), None))
        }
        Some(sp) => {
            let name = &inner[..sp];
            let sub = inner[sp..].trim();
            if !valid_ident(name) {
                return Err(format!("invalid section name: {name}"));
            }
            if sub.len() < 2 || !sub.starts_with('"') || !sub.ends_with('"') {
                return Err("subsection must be quoted".into());
            }
            let sub = unescape(&sub[1..sub.len() - 1]);
            Ok((name.to_ascii_lowercase(), Some(sub)))
        }
    }
}

/// Index of the `]` that closes the header, skipping any inside quotes.
fn find_header_close(t: &str) -> Option<usize> {
    let mut in_q = false;
    let mut esc = false;
    for (i, c) in t.char_indices().skip(1) {
        if esc {
            esc = false;
        } else if in_q && c == '\\' {
            esc = true;
        } else if c == '"' {
            in_q = !in_q;
        } else if c == ']' && !in_q {
            return Some(i);
        }
    }
    None
}

fn parse_entry(raw: &str) -> Result<Line, String> {
    let body = raw.trim_start();
    let indent = raw[..raw.len() - body.len()].to_string();
    let key_end = body
        .find(|c: char| c == '=' || c.is_whitespace())
        .unwrap_or(body.len());
    let raw_key = &body[..key_end];
    if !valid_ident(raw_key) {
        return Err(format!("invalid key name: {raw_key:?}"));
    }
    let after = body[key_end..].trim_start();
    let (value, trailing) = if let Some(rest) = after.strip_prefix('=') {
        split_value(rest.trim_start())
    } else if after.is_empty() {
        ("true".to_string(), String::new())
    } else if after.starts_with('#') || after.starts_with(';') {
        ("true".to_string(), format!(" {after}"))
    } else {
        return Err("expected '=' after key".into());
    };
    Ok(Line::Entry {
        indent,
        raw_key: raw_key.to_string(),
        key: raw_key.to_ascii_lowercase(),
        value,
        trailing,
        raw: raw.to_string(),
    })
}

/// Split text after `=` into (value, trailing). `trailing` keeps the original
/// whitespace and comment so a rewrite can preserve it.
fn split_value(s: &str) -> (String, String) {
    if s.starts_with('"')
        && let Some((val, used)) = parse_quoted(s)
    {
        let rest = &s[used..];
        let t = rest.trim_start();
        if t.is_empty() || t.starts_with('#') {
            return (val, rest.trim_end().to_string());
        }
    }
    // Literal value: ends at a '#' that follows whitespace.
    let mut end = s.len();
    let mut prev_ws = true;
    for (i, c) in s.char_indices() {
        if c == '#' && prev_ws {
            end = i;
            break;
        }
        prev_ws = c.is_whitespace();
    }
    let value = s[..end].trim_end();
    let trailing = &s[value.len()..];
    let trailing = if trailing.is_empty() {
        String::new()
    } else {
        // Keep the gap before the comment; drop bare trailing whitespace.
        let t = trailing.trim_end();
        if t.trim().is_empty() { String::new() } else { t.to_string() }
    };
    (value.to_string(), trailing)
}

/// Parse a leading `"..."`. Returns the decoded text and bytes consumed.
fn parse_quoted(s: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    chars.next(); // opening quote
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, i + 1)),
            '\\' => match chars.next() {
                Some((_, 'n')) => out.push('\n'),
                Some((_, 't')) => out.push('\t'),
                Some((_, '\\')) => out.push('\\'),
                Some((_, '"')) => out.push('"'),
                Some((_, o)) => {
                    out.push('\\');
                    out.push(o);
                }
                None => return None,
            },
            c => out.push(c),
        }
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Quote a value only when the literal form would not read back the same.
pub fn encode_value(v: &str) -> String {
    let needs_quote = v.starts_with('"')
        || v != v.trim()
        || v.contains(['#', '\n', '\r', '\t']);
    if !needs_quote {
        return v.to_string();
    }
    let mut out = String::from('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# my config
[core]
    provider = anthropic   # active
    max_tokens = 800

[provider \"anthropic\"]
    type = anthropic
    key = sk-ant-abc

; keep me
[alias \"fix\"]
    prompt = Do this; then that
";

    #[test]
    fn roundtrip_is_byte_exact() {
        let ini = Ini::parse(SAMPLE).unwrap();
        assert_eq!(ini.render(), SAMPLE);
    }

    #[test]
    fn reads_values_and_strips_inline_comments() {
        let ini = Ini::parse(SAMPLE).unwrap();
        assert_eq!(ini.get("core.provider").as_deref(), Some("anthropic"));
        assert_eq!(ini.get("provider.anthropic.key").as_deref(), Some("sk-ant-abc"));
        assert_eq!(ini.get("alias.fix.prompt").as_deref(), Some("Do this; then that"));
        assert_eq!(ini.get("CORE.Provider").as_deref(), Some("anthropic"));
        assert_eq!(ini.get("core.nope"), None);
    }

    #[test]
    fn set_replaces_in_place_and_keeps_comment() {
        let mut ini = Ini::parse(SAMPLE).unwrap();
        ini.set("core.provider", "openai").unwrap();
        let out = ini.render();
        assert!(out.contains("    provider = openai   # active\n"), "{out}");
        assert_eq!(out.lines().count(), SAMPLE.lines().count());
    }

    #[test]
    fn set_appends_to_existing_section() {
        let mut ini = Ini::parse(SAMPLE).unwrap();
        ini.set("provider.anthropic.model", "claude-haiku-4-5").unwrap();
        let out = ini.render();
        let want = "    key = sk-ant-abc\n    model = claude-haiku-4-5\n\n; keep me";
        assert!(out.contains(want), "{out}");
    }

    #[test]
    fn set_creates_new_section() {
        let mut ini = Ini::parse(SAMPLE).unwrap();
        ini.set("provider.ollama.url", "http://localhost:11434/v1").unwrap();
        let out = ini.render();
        assert!(out.ends_with("\n\n[provider \"ollama\"]\n    url = http://localhost:11434/v1\n"), "{out}");
    }

    #[test]
    fn set_on_empty_file() {
        let mut ini = Ini::parse("").unwrap();
        ini.set("core.provider", "x").unwrap();
        assert_eq!(ini.render(), "[core]\n    provider = x\n");
    }

    #[test]
    fn values_needing_quotes_roundtrip() {
        for v in ["a # b", " lead", "trail ", "\"q\"", "line1\nline2", "tab\there", "back\\slash # x"] {
            let mut ini = Ini::parse("").unwrap();
            ini.set("alias.t.prompt", v).unwrap();
            let again = Ini::parse(&ini.render()).unwrap();
            assert_eq!(again.get("alias.t.prompt").as_deref(), Some(v), "value {v:?}");
        }
    }

    #[test]
    fn literal_backslashes_and_quotes_are_untouched() {
        let ini = Ini::parse("[a]\n  k = C:\\Users\\me\\bin\n  q = say \"hi\"\n").unwrap();
        assert_eq!(ini.get("a.k").as_deref(), Some("C:\\Users\\me\\bin"));
        assert_eq!(ini.get("a.q").as_deref(), Some("say \"hi\""));
    }

    #[test]
    fn unset_removes_entry_and_empty_section() {
        let mut ini = Ini::parse(SAMPLE).unwrap();
        assert!(ini.unset("alias.fix.prompt"));
        let out = ini.render();
        assert!(!out.contains("alias"), "{out}");
        assert!(out.contains("; keep me"));
        assert!(!ini.unset("alias.fix.prompt"));
    }

    #[test]
    fn unset_keeps_section_with_other_entries() {
        let mut ini = Ini::parse(SAMPLE).unwrap();
        assert!(ini.unset("core.max_tokens"));
        let out = ini.render();
        assert!(out.contains("[core]") && out.contains("provider = anthropic"));
        assert!(!out.contains("max_tokens"));
    }

    #[test]
    fn subsection_with_dots() {
        let mut ini = Ini::parse("").unwrap();
        ini.set("provider.my.host.model", "m").unwrap();
        assert_eq!(ini.render(), "[provider \"my.host\"]\n    model = m\n");
        assert_eq!(ini.get("provider.my.host.model").as_deref(), Some("m"));
    }

    #[test]
    fn bare_key_is_true() {
        let ini = Ini::parse("[a]\n  flag\n").unwrap();
        assert_eq!(ini.get("a.flag").as_deref(), Some("true"));
    }

    #[test]
    fn last_value_wins() {
        let ini = Ini::parse("[a]\n k = 1\n k = 2\n").unwrap();
        assert_eq!(ini.get("a.k").as_deref(), Some("2"));
    }

    #[test]
    fn crlf_preserved() {
        let mut ini = Ini::parse("[a]\r\n  k = 1\r\n").unwrap();
        ini.set("a.j", "2").unwrap();
        assert_eq!(ini.render(), "[a]\r\n  k = 1\r\n  j = 2\r\n");
    }

    #[test]
    fn parse_errors_carry_line_numbers() {
        assert_eq!(Ini::parse("k = v\n").unwrap_err().line, 1);
        assert_eq!(Ini::parse("[a]\n[b\n").unwrap_err().line, 2);
        assert_eq!(Ini::parse("[a]\nbad line here\n").unwrap_err().line, 2);
        assert!(Ini::parse("[a b]\n").is_err());
    }

    #[test]
    fn invalid_keys_rejected() {
        let mut ini = Ini::parse("").unwrap();
        assert!(ini.set("nodots", "x").is_err());
        assert!(ini.set("a..b", "x").is_err());
        assert!(ini.set("a.b c", "x").is_err());
    }
}
