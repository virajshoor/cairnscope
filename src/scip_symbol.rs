//! Helpers for SCIP symbol strings, e.g.
//! `scip-typescript npm pkg 1.0.0 src/`a.ts`/Foo#bar().`
//! Grammar: https://github.com/scip-code/scip/blob/main/scip.proto (Symbol).

/// The last descriptor of a symbol with its suffix character.
fn last_descriptor(symbol: &str) -> Option<(&str, char)> {
    let descs = descriptors(symbol);
    let s = descs.trim_end();
    let suffix = s.chars().last()?;
    let body = &s[..s.len() - suffix.len_utf8()];
    // Methods end in `(disambiguator).`
    if suffix == '.' && body.ends_with(')') {
        let open = body.rfind('(')?;
        return Some((name_before(&body[..open]), '('));
    }
    if suffix == ')' || suffix == ']' {
        let open = body.rfind(if suffix == ')' { '(' } else { '[' })?;
        return Some((unquote(&body[open + 1..]), suffix));
    }
    Some((name_before(body), suffix))
}

/// Descriptor part: everything after scheme, manager, package name and version.
fn descriptors(symbol: &str) -> &str {
    if symbol.starts_with("local ") {
        return symbol;
    }
    let mut rest = symbol;
    for _ in 0..4 {
        rest = skip_token(rest);
    }
    rest
}

/// Skip one space-separated token (a double space escapes a literal space).
fn skip_token(s: &str) -> &str {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b' ' {
            if b.get(i + 1) == Some(&b' ') {
                i += 2;
                continue;
            }
            return &s[i + 1..];
        }
        i += 1;
    }
    ""
}

/// The trailing name in a descriptor run, handling backtick-escaped names.
fn name_before(s: &str) -> &str {
    if let Some(stripped) = s.strip_suffix('`') {
        if let Some(open) = stripped.rfind('`') {
            return &stripped[open + 1..];
        }
    }
    let start = s
        .rfind(['/', '#', '.', ':', '`'])
        .map(|i| i + 1)
        .unwrap_or(0);
    &s[start..]
}

fn unquote(s: &str) -> &str {
    s.trim_matches('`')
}

/// Human-readable name for a symbol (last descriptor).
pub fn display_name(symbol: &str) -> String {
    if symbol.starts_with("local ") {
        return symbol.to_string();
    }
    match last_descriptor(symbol) {
        Some((n, _)) if !n.is_empty() => n.to_string(),
        _ => symbol.to_string(),
    }
}

/// Coarse kind derived from the descriptor suffix, used when the indexer gives none.
pub fn kind_from_suffix(symbol: &str) -> &'static str {
    if symbol.starts_with("local ") {
        return "local";
    }
    match last_descriptor(symbol).map(|d| d.1) {
        Some('(') => "function",
        Some('#') => "type",
        Some('.') => "variable",
        Some('/') => "namespace",
        Some(':') => "meta",
        Some('!') => "macro",
        Some(')') => "parameter",
        Some(']') => "type_parameter",
        _ => "unknown",
    }
}

/// The enclosing symbol (owner), e.g. `Foo#` for `Foo#bar().`, if any.
pub fn parent(symbol: &str) -> Option<String> {
    if symbol.starts_with("local ") {
        return None;
    }
    let descs = descriptors(symbol);
    let prefix_len = symbol.len() - descs.len();
    let (name, suffix) = last_descriptor(symbol)?;
    // Find where the last descriptor starts in `descs`.
    let trimmed = descs.trim_end();
    let cut = match suffix {
        '(' => trimmed[..trimmed.len() - 1]
            .rfind('(')
            .and_then(|i| trimmed[..i].rfind(name))?,
        ')' => trimmed.rfind('(')?,
        ']' => trimmed.rfind('[')?,
        _ => trimmed[..trimmed.len() - 1].rfind(name)?,
    };
    let cut = if cut > 0 && trimmed.as_bytes()[cut - 1] == b'`' {
        cut - 1
    } else {
        cut
    };
    if cut == 0 {
        return None;
    }
    let p = &symbol[..prefix_len + cut];
    // Namespaces are package paths, not owners worth showing as parents.
    if p.ends_with('/') {
        None
    } else {
        Some(p.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = "scip-typescript npm pkg 1.0.0 src/`a.ts`/Foo#bar().";

    #[test]
    fn names_and_kinds() {
        assert_eq!(display_name(M), "bar");
        assert_eq!(kind_from_suffix(M), "function");
        assert_eq!(
            parent(M).as_deref(),
            Some("scip-typescript npm pkg 1.0.0 src/`a.ts`/Foo#")
        );
        let t = "scip-typescript npm pkg 1.0.0 src/`a.ts`/Foo#";
        assert_eq!(display_name(t), "Foo");
        assert_eq!(kind_from_suffix(t), "type");
        assert_eq!(parent(t), None);
        assert_eq!(
            display_name("rust-analyzer cargo foo 0.1.0 bar/baz()."),
            "baz"
        );
        assert_eq!(display_name("cxx . . $ main(49f6e7a06ebc5aa8)."), "main");
        assert_eq!(kind_from_suffix("local 4"), "local");
        assert_eq!(display_name("a b c d x/`weird name`#"), "weird name");
        assert_eq!(kind_from_suffix("a b c d x/Foo#bar().(arg)"), "parameter");
        assert_eq!(
            parent("a b c d x/Foo#bar().(arg)").as_deref(),
            Some("a b c d x/Foo#bar().")
        );
    }
}
