//! A small, dependency-free XML DOM reader/writer sufficient for FCPXML.

use std::fmt::Write as _;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("XML parse error at byte {pos}: {msg}")]
pub struct XmlError {
    pub pos: usize,
    pub msg: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn new(name: impl Into<String>) -> Self {
        Element { name: name.into(), attrs: Vec::new(), children: Vec::new() }
    }

    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn set(mut self, key: &str, value: impl Into<String>) -> Self {
        self.attrs.push((key.to_string(), value.into()));
        self
    }

    pub fn push(&mut self, child: Element) {
        self.children.push(Node::Element(child));
    }

    pub fn with_child(mut self, child: Element) -> Self {
        self.push(child);
        self
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.children.push(Node::Text(text.into()));
        self
    }

    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.elements().find(|e| e.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.elements().filter(move |e| e.name == name)
    }

    /// Depth-first search for the first descendant named `name`.
    pub fn find(&self, name: &str) -> Option<&Element> {
        for e in self.elements() {
            if e.name == name {
                return Some(e);
            }
            if let Some(f) = e.find(name) {
                return Some(f);
            }
        }
        None
    }

    /// Concatenated text content of this element and its descendants.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for n in &self.children {
            match n {
                Node::Text(t) => s.push_str(t),
                Node::Element(e) => s.push_str(&e.text()),
            }
        }
        s
    }

    /// Serialize with two-space indentation.
    pub fn to_xml_string(&self) -> String {
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        if self.name == "fcpxml" {
            out.push_str("<!DOCTYPE fcpxml>\n\n");
        }
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        let pad = "  ".repeat(depth);
        let _ = write!(out, "{pad}<{}", self.name);
        for (k, v) in &self.attrs {
            let _ = write!(out, " {k}=\"{}\"", escape(v));
        }
        if self.children.is_empty() {
            out.push_str("/>\n");
            return;
        }
        let only_text = self.children.iter().all(|c| matches!(c, Node::Text(_)));
        if only_text {
            out.push('>');
            for c in &self.children {
                if let Node::Text(t) = c {
                    out.push_str(&escape(t));
                }
            }
            let _ = writeln!(out, "</{}>", self.name);
            return;
        }
        out.push_str(">\n");
        for c in &self.children {
            match c {
                Node::Element(e) => e.write(out, depth + 1),
                Node::Text(t) => {
                    let _ = writeln!(out, "{pad}  {}", escape(t));
                }
            }
        }
        let _ = writeln!(out, "{pad}</{}>", self.name);
    }
}

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            '\n' => o.push_str("&#10;"),
            c => o.push(c),
        }
    }
    o
}

fn unescape(s: &str, pos: usize) -> Result<String, XmlError> {
    if !s.contains('&') {
        return Ok(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let end = after.find(';').ok_or_else(|| XmlError { pos, msg: "unterminated entity".into() })?;
        let ent = &after[..end];
        let ch = match ent {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                char::from_u32(u32::from_str_radix(&ent[2..], 16).map_err(|_| XmlError { pos, msg: format!("bad entity &{ent};") })?)
                    .ok_or_else(|| XmlError { pos, msg: "bad char".into() })?
            }
            _ if ent.starts_with('#') => char::from_u32(ent[1..].parse().map_err(|_| XmlError { pos, msg: format!("bad entity &{ent};") })?)
                .ok_or_else(|| XmlError { pos, msg: "bad char".into() })?,
            _ => return Err(XmlError { pos, msg: format!("unknown entity &{ent};") }),
        };
        out.push(ch);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, msg: impl Into<String>) -> Result<T, XmlError> {
        Err(XmlError { pos: self.pos, msg: msg.into() })
    }
    fn rest(&self) -> &'a str {
        &self.s[self.pos..]
    }
    fn starts(&self, p: &str) -> bool {
        self.rest().starts_with(p)
    }
    fn skip_ws(&mut self) {
        let r = self.rest();
        let trimmed = r.trim_start();
        self.pos += r.len() - trimmed.len();
    }
    fn skip_until(&mut self, pat: &str) -> Result<(), XmlError> {
        match self.rest().find(pat) {
            Some(i) => {
                self.pos += i + pat.len();
                Ok(())
            }
            None => self.err(format!("expected `{pat}`")),
        }
    }
    /// Skip prolog, comments, doctype and processing instructions.
    fn skip_misc(&mut self) -> Result<(), XmlError> {
        loop {
            self.skip_ws();
            if self.starts("<?") {
                self.skip_until("?>")?;
            } else if self.starts("<!--") {
                self.skip_until("-->")?;
            } else if self.starts("<!DOCTYPE") || self.starts("<!doctype") {
                // DOCTYPE may contain an internal subset in [...]
                let r = self.rest();
                let close = match (r.find('['), r.find('>')) {
                    (Some(b), Some(g)) if b < g => r.find("]>").map(|i| i + 2),
                    (_, Some(g)) => Some(g + 1),
                    _ => None,
                };
                match close {
                    Some(n) => self.pos += n,
                    None => return self.err("unterminated DOCTYPE"),
                }
            } else {
                return Ok(());
            }
        }
    }
    fn name(&mut self) -> Result<String, XmlError> {
        let r = self.rest();
        let n = r
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
            .map(|(i, _)| i)
            .unwrap_or(r.len());
        if n == 0 {
            return self.err("expected name");
        }
        self.pos += n;
        Ok(r[..n].to_string())
    }
    fn element(&mut self) -> Result<Element, XmlError> {
        if !self.starts("<") {
            return self.err("expected `<`");
        }
        self.pos += 1;
        let name = self.name()?;
        let mut el = Element::new(name);
        loop {
            self.skip_ws();
            if self.starts("/>") {
                self.pos += 2;
                return Ok(el);
            }
            if self.starts(">") {
                self.pos += 1;
                break;
            }
            let key = self.name()?;
            self.skip_ws();
            if !self.starts("=") {
                return self.err("expected `=`");
            }
            self.pos += 1;
            self.skip_ws();
            let q = match self.rest().chars().next() {
                Some(c @ ('"' | '\'')) => c,
                _ => return self.err("expected quote"),
            };
            self.pos += 1;
            let end = self.rest().find(q).ok_or_else(|| XmlError { pos: self.pos, msg: "unterminated attribute".into() })?;
            let raw = &self.rest()[..end];
            let v = unescape(raw, self.pos)?;
            self.pos += end + 1;
            el.attrs.push((key, v));
        }
        // children
        loop {
            if self.starts("</") {
                self.pos += 2;
                let close = self.name()?;
                if close != el.name {
                    return self.err(format!("mismatched </{close}> for <{}>", el.name));
                }
                self.skip_ws();
                if !self.starts(">") {
                    return self.err("expected `>`");
                }
                self.pos += 1;
                return Ok(el);
            } else if self.starts("<!--") {
                self.skip_until("-->")?;
            } else if self.starts("<![CDATA[") {
                self.pos += 9;
                let end = self.rest().find("]]>").ok_or_else(|| XmlError { pos: self.pos, msg: "unterminated CDATA".into() })?;
                el.children.push(Node::Text(self.rest()[..end].to_string()));
                self.pos += end + 3;
            } else if self.starts("<?") {
                self.skip_until("?>")?;
            } else if self.starts("<") {
                let child = self.element()?;
                el.children.push(Node::Element(child));
            } else if self.rest().is_empty() {
                return self.err(format!("unexpected end inside <{}>", el.name));
            } else {
                let end = self.rest().find('<').unwrap_or(self.rest().len());
                let raw = &self.rest()[..end];
                let start = self.pos;
                self.pos += end;
                if !raw.trim().is_empty() {
                    el.children.push(Node::Text(unescape(raw, start)?));
                }
            }
        }
    }
}

/// Parse a document and return its root element.
pub fn parse(input: &str) -> Result<Element, XmlError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut p = Parser { s: input, pos: 0 };
    p.skip_misc()?;
    let root = p.element()?;
    p.skip_misc()?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_write_round_trip() {
        let src = r#"<?xml version="1.0"?><!DOCTYPE fcpxml><!-- c --><a x="1 &amp; 2"><b/><c y='q'>hi &lt;there&gt;</c></a>"#;
        let e = parse(src).unwrap();
        assert_eq!(e.attr("x"), Some("1 & 2"));
        assert_eq!(e.child("c").unwrap().text(), "hi <there>");
        let again = parse(&e.to_xml_string()).unwrap();
        assert_eq!(e, again);
    }
}
