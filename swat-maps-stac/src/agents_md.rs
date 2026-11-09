//! A short summary of an AGENTS.md (or README.md): its headings and the
//! first lines under the first ones, for a card's "Notes for agents" and a
//! tool's result. The full text is kept by the caller.

/// Up to `max_chars` of the document: the title, then each section's
/// heading and its first non-empty paragraph, Markdown kept light.
pub fn summary(md: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut in_code = false;
    let mut took_para = false;
    for raw in md.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let piece = if let Some(h) = t.strip_prefix('#') {
            took_para = false;
            format!("\n{}\n", h.trim_start_matches('#').trim())
        } else if took_para {
            continue;
        } else {
            took_para = true;
            // One paragraph per section: its first line.
            format!("{}\n", strip_md(t))
        };
        if out.chars().count() + piece.chars().count() > max_chars {
            break;
        }
        out.push_str(&piece);
    }
    out.trim().to_string()
}

/// `**bold**`, `` `code` `` and `[text](url)` as plain text.
pub fn strip_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '`' => {}
            '[' => {
                let mut text = String::new();
                for d in chars.by_ref() {
                    if d == ']' {
                        break;
                    }
                    text.push(d);
                }
                if chars.peek() == Some(&'(') {
                    for d in chars.by_ref() {
                        if d == ')' {
                            break;
                        }
                    }
                }
                out.push_str(&text);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_and_first_paragraphs_survive_code_does_not() {
        let md = "# Natura 2000\n\nBoundaries of **162** areas.\nSecond line.\n\nMore text.\n\n## Fields\n\n- `naam_n2k`: the [name](https://x)\n\n```sql\nSELECT 1\n```\n\n## Tips\n\nSum `shape_area` by `nr`.\n";
        let s = summary(md, 500);
        assert_eq!(s, "Natura 2000\nBoundaries of 162 areas.\n\nFields\n- naam_n2k: the name\n\nTips\nSum shape_area by nr.");
        assert!(summary(md, 40).chars().count() <= 40);
    }
}
