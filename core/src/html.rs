//! Turns item HTML into simple text blocks for display. Deliberately small:
//! keeps structure (paragraphs, headings, lists, quotes, code) and drops the rest.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    Paragraph,
    Heading,
    Bullet,
    Quote,
    Code,
}

#[derive(Debug, PartialEq)]
pub struct Block {
    pub style: Style,
    pub text: String,
}

pub fn to_blocks(html: &str) -> Vec<Block> {
    if !html.contains('<') {
        let text = decode_entities(html);
        return text.split("\n\n").map(|p| block(Style::Paragraph, p)).filter(|b| !b.text.is_empty()).collect();
    }
    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut style = Style::Paragraph;
    let mut quote_depth = 0;
    let mut rest = html;
    // Source line breaks are just whitespace in HTML, except inside <pre>.
    let push = |text: &mut String, raw: &str, style: Style| {
        if style == Style::Code { text.push_str(raw) } else { text.push_str(&raw.replace('\n', " ")) }
    };
    while let Some(lt) = rest.find('<') {
        push(&mut text, &rest[..lt], style);
        let Some(gt) = rest[lt..].find('>') else { break };
        let tag = &rest[lt + 1..lt + gt];
        rest = &rest[lt + gt + 1..];
        let closing = tag.starts_with('/');
        let name = tag.trim_start_matches('/').split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "script" | "style" | "head" if !closing => {
                // Skip everything up to the closing tag.
                let end = format!("</{name}");
                rest = rest.to_ascii_lowercase().find(&end).map_or("", |i| &rest[i..]);
            }
            "br" => text.push('\n'),
            "p" | "div" | "section" | "article" | "figure" | "figcaption" | "ul" | "ol" | "table" | "tr" | "hr" => {
                flush(&mut blocks, &mut text, style);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "pre" | "blockquote" => {
                flush(&mut blocks, &mut text, style);
                if name == "blockquote" {
                    quote_depth = if closing { quote_depth - 1 } else { quote_depth + 1 };
                }
                style = match (closing, name.as_bytes()[0]) {
                    (true, _) => Style::Paragraph,
                    (false, b'h') => Style::Heading,
                    (false, b'l') => Style::Bullet,
                    (false, b'p') => Style::Code,
                    (false, _) => Style::Quote,
                };
            }
            "img" => {
                if let Some(alt) = img_alt(tag) {
                    text.push_str(&format!("[{alt}]"));
                }
            }
            _ => {}
        }
        if quote_depth > 0 && style == Style::Paragraph {
            style = Style::Quote;
        }
    }
    push(&mut text, rest, style);
    flush(&mut blocks, &mut text, style);
    blocks
}

/// Turns Markdown (as NIP-23 posts use) into the simple HTML this module reads.
pub fn markdown_to_html(markdown: &str) -> String {
    let mut out = String::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut in_code = false;
    let flush = |out: &mut String, paragraph: &mut Vec<&str>| {
        if !paragraph.is_empty() {
            out.push_str(&format!("<p>{}</p>", inline_markdown(&paragraph.join(" "))));
            paragraph.clear();
        }
    };
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            flush(&mut out, &mut paragraph);
            out.push_str(if in_code { "</pre>" } else { "<pre>" });
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push_str(&escape(line));
            out.push('\n');
            continue;
        }
        let line = line.trim();
        // Some posts use HTML line breaks on lines of their own as paragraph breaks.
        let line = if is_line_break(line) { "" } else { line };
        let bullet = ["- ", "* ", "+ "].iter().find_map(|b| line.strip_prefix(b)).or_else(|| {
            let (number, rest) = line.split_once(". ")?;
            number.chars().all(|c| c.is_ascii_digit()).then_some(rest)
        });
        let rule = line.len() >= 3 && ['-', '*', '_'].iter().any(|&c| line.chars().all(|l| l == c || l == ' '));
        if line.is_empty() {
            flush(&mut out, &mut paragraph);
        } else if rule {
            flush(&mut out, &mut paragraph);
            out.push_str("<hr>");
        } else if line.starts_with('#') {
            flush(&mut out, &mut paragraph);
            out.push_str(&format!("<h2>{}</h2>", inline_markdown(line.trim_start_matches('#').trim())));
        } else if let Some(item) = bullet {
            flush(&mut out, &mut paragraph);
            out.push_str(&format!("<li>{}</li>", inline_markdown(item)));
        } else if let Some(quote) = line.strip_prefix('>') {
            flush(&mut out, &mut paragraph);
            out.push_str(&format!("<blockquote>{}</blockquote>", inline_markdown(quote.trim())));
        } else {
            paragraph.push(line);
        }
    }
    flush(&mut out, &mut paragraph);
    if in_code {
        out.push_str("</pre>");
    }
    out
}

fn is_line_break(text: &str) -> bool {
    let tag = text.to_ascii_lowercase().replace(' ', "");
    ["<br>", "<br/>"].contains(&tag.as_str())
}

/// Inline Markdown to plain (escaped) text: links keep their text, images their description.
fn inline_markdown(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let image = open > 0 && rest[..open].ends_with('!');
        let Some(close) = rest[open..].find("](").map(|i| open + i) else { break };
        let Some(end) = rest[close..].find(')').map(|i| close + i) else { break };
        out.push_str(&rest[..if image { open - 1 } else { open }]);
        let label = &rest[open + 1..close];
        if image {
            if !label.is_empty() {
                out.push_str(&format!("[{label}]"));
            }
        } else {
            out.push_str(label);
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    escape(&strip_emphasis(&out.replace("**", "").replace("__", "").replace('`', "")))
}

/// Drops the `*` that mark emphasis (`*word*`), keeping ones that stand
/// alone (`2 * 3`) or sit inside a word.
fn strip_emphasis(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let word = |i: Option<usize>| i.and_then(|i| chars.get(i)).is_some_and(|c| !c.is_whitespace());
    let marks = |i: usize| chars[i] == '*' && word(i.checked_sub(1)) != word(Some(i + 1));
    (0..chars.len()).filter(|&i| !marks(i)).map(|i| chars[i]).collect()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Whether a link is a web page. Links come from feeds, and opening anything
/// else (`file:` paths, other apps' link types) would let a feed open local
/// files or start other programs.
pub fn is_web_link(link: &str) -> bool {
    reqwest::Url::parse(link).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
}

/// All of an item's text, without markup.
pub fn plain_text(html: &str) -> String {
    let texts: Vec<String> = to_blocks(html).into_iter().map(|b| b.text.replace('\n', " ")).collect();
    texts.join(" ")
}

/// Plain text from the start of an item, for previews.
pub fn snippet(html: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for block in to_blocks(html) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&block.text.replace('\n', " "));
        if out.chars().count() >= max_chars {
            break;
        }
    }
    out.chars().take(max_chars).collect()
}

fn flush(blocks: &mut Vec<Block>, text: &mut String, style: Style) {
    let b = block(style, text);
    if !b.text.is_empty() {
        blocks.push(b);
    }
    text.clear();
}

fn block(style: Style, raw: &str) -> Block {
    let decoded = decode_entities(raw);
    let text = if style == Style::Code {
        decoded.trim_matches('\n').to_string()
    } else {
        // Collapse whitespace, but keep explicit line breaks.
        decoded
            .split('\n')
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    };
    Block { style, text }
}

fn img_alt(tag: &str) -> Option<String> {
    let alt = attribute(tag, "alt")?.trim().to_string();
    (!alt.is_empty()).then_some(alt)
}

/// The value of the attribute `name` (lowercase) in a tag's source, like
/// `a href="/x"`, quoted or not, with entities decoded.
pub fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let start = from + found;
        from = start + name.len();
        // A whole attribute name, not the end of a longer one like `data-alt`.
        if !lower[..start].ends_with(char::is_whitespace) {
            continue;
        }
        let Some(value) = tag[from..].trim_start().strip_prefix('=') else { continue };
        let value = value.trim_start();
        let value = match value.chars().next() {
            Some(quote @ ('"' | '\'')) => value[1..].split(quote).next()?,
            _ => value.split(char::is_whitespace).next()?,
        };
        return Some(decode_entities(value));
    }
    None
}

pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let entity = rest[1..].find(';').filter(|&i| i <= 10).map(|i| &rest[1..i + 1]);
        let decoded = entity.and_then(|e| match e {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "lsquo" => Some('‘'),
            "rsquo" => Some('’'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            "copy" => Some('©'),
            _ => {
                let num = e.strip_prefix('#')?;
                let code = match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => num.parse().ok()?,
                };
                char::from_u32(code)
            }
        });
        match (entity, decoded) {
            (Some(e), Some(c)) => {
                out.push(c);
                rest = &rest[e.len() + 2..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_structure() {
        let blocks = to_blocks(
            "<h2>Title</h2><p>Hello &amp; <b>welcome</b>,\n  friend</p><ul><li>One</li><li>Two</li></ul><script>x()</script><pre>a\n  b</pre>",
        );
        let expected = [
            (Style::Heading, "Title"),
            (Style::Paragraph, "Hello & welcome, friend"),
            (Style::Bullet, "One"),
            (Style::Bullet, "Two"),
            (Style::Code, "a\n  b"),
        ];
        let got: Vec<_> = blocks.iter().map(|b| (b.style, b.text.as_str())).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn plain_text_paragraphs() {
        let got: Vec<_> = to_blocks("One\n\nTwo &#8217; &#x41;").into_iter().map(|b| b.text).collect();
        assert_eq!(got, ["One", "Two ’ A"]);
    }

    #[test]
    fn reads_attributes_of_any_kind() {
        // Unquoted values starting with a multi-byte character used to crash.
        let got: Vec<_> = to_blocks("<p>x<img alt=éclair src=a.png></p>").into_iter().map(|b| b.text).collect();
        assert_eq!(got, ["x[éclair]"]);
        assert_eq!(attribute("img data-alt=\"no\" ALT = 'Yes &amp; no'", "alt").as_deref(), Some("Yes & no"));
        assert_eq!(attribute("link href=/feed.xml type=x", "href").as_deref(), Some("/feed.xml"));
        assert_eq!(attribute("img src=a.png", "alt"), None);
    }

    #[test]
    fn only_web_links_are_opened() {
        assert!(is_web_link("https://example.com/post") && is_web_link("http://example.com"));
        for link in
            ["file:///etc/passwd", "/home/user/x.desktop", "javascript:alert(1)", "sftp://host/x", "", "https://"]
        {
            assert!(!is_web_link(link), "{link}");
        }
    }

    #[test]
    fn leaves_unknown_entities() {
        assert_eq!(decode_entities("a & b &bogus; &"), "a & b &bogus; &");
    }

    #[test]
    fn snippets_are_plain_and_short() {
        let text = snippet("<h1>Title</h1><p>Some <b>bold</b>\ntext.</p><p>More</p>", 20);
        assert_eq!(text, "Title Some bold text");
        assert_eq!(snippet("<p>Cut off in the mid", 100), "Cut off in the mid");
    }

    #[test]
    fn markdown_emphasis_and_html_breaks() {
        let md = "One *two* three.\n<br />\n*(source:* *Somewhere)* and 2 * 3 = 6";
        let got: Vec<_> = to_blocks(&markdown_to_html(md)).into_iter().map(|b| b.text).collect();
        assert_eq!(got, ["One two three.", "(source: Somewhere) and 2 * 3 = 6"]);
    }

    #[test]
    fn markdown_becomes_blocks() {
        let md = "# Hello\n\n---\nSome **bold** text with a [link](https://x.example) and ![a cat](cat.png).\nSame paragraph.\n\n- one\n2. two\n> quoted <tag>\n\n```\nlet x = 1 < 2;\n```";
        let got: Vec<_> = to_blocks(&markdown_to_html(md)).into_iter().map(|b| (b.style, b.text)).collect();
        let expected = [
            (Style::Heading, "Hello"),
            (Style::Paragraph, "Some bold text with a link and [a cat]. Same paragraph."),
            (Style::Bullet, "one"),
            (Style::Bullet, "two"),
            (Style::Quote, "quoted <tag>"),
            (Style::Code, "let x = 1 < 2;"),
        ];
        assert_eq!(got, expected.map(|(s, t)| (s, t.to_string())));
    }
}
