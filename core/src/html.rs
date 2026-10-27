//! Turns item HTML into simple blocks of styled text for display. Deliberately
//! small: keeps structure (paragraphs, headings, lists, quotes, code, tables)
//! and inline styles (bold, italic, code, links) and drops the rest.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Style {
    #[default]
    Paragraph,
    /// Level 1 to 6.
    Heading(u8),
    Quote,
    Code,
    /// A horizontal rule.
    Rule,
}

/// A run of text that looks the same throughout.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    /// Where it links to: only web pages (see [`link_target`]).
    pub link: Option<String>,
}

impl Span {
    fn same_look(&self, other: &Span) -> bool {
        (self.bold, self.italic, self.code, self.strike, &self.link)
            == (other.bold, other.italic, other.code, other.strike, &other.link)
    }
}

#[derive(Debug, PartialEq)]
pub struct Block {
    pub style: Style,
    /// How deep in lists it is (0 outside lists).
    pub depth: u8,
    /// The list marker ("•", "3.") of an item's first block.
    pub marker: Option<String>,
    pub spans: Vec<Span>,
}

impl Block {
    /// The block's text, without styles.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

pub fn to_blocks(html: &str) -> Vec<Block> {
    if !html.contains('<') {
        let text = decode_entities(html);
        let paragraphs = text.split("\n\n").map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "));
        return paragraphs
            .filter(|p| !p.is_empty())
            .map(|text| Block {
                style: Style::Paragraph,
                depth: 0,
                marker: None,
                spans: vec![Span { text, ..Span::default() }],
            })
            .collect();
    }
    let mut reader = Reader::default();
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        reader.text(&rest[..lt]);
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
            _ => reader.tag(&name, tag, closing),
        }
    }
    reader.text(rest);
    reader.flush();
    reader.blocks
}

/// What [`to_blocks`] has read so far.
#[derive(Default)]
struct Reader {
    blocks: Vec<Block>,
    /// The block being read.
    spans: Vec<Span>,
    style: Style,
    quotes: usize,
    /// Open lists: `None` for bullets, or the next number.
    lists: Vec<Option<u32>>,
    marker: Option<String>,
    bold: usize,
    italic: usize,
    code: usize,
    strike: usize,
    links: Vec<Option<String>>,
}

impl Reader {
    fn tag(&mut self, name: &str, tag: &str, closing: bool) {
        let count = |n: &mut usize| *n = if closing { n.saturating_sub(1) } else { *n + 1 };
        match name {
            "br" => self.push("\n"),
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "aside" | "figure" | "figcaption"
            | "table" | "tr" | "dl" | "dt" | "dd" => self.flush(),
            "td" | "th" => {
                if !closing && !self.spans.is_empty() {
                    self.push(" | ");
                }
                if name == "th" {
                    count(&mut self.bold);
                }
            }
            "ul" | "ol" => {
                self.flush();
                if closing {
                    self.lists.pop();
                } else {
                    let start = attribute(tag, "start").and_then(|s| s.trim().parse().ok());
                    self.lists.push((name == "ol").then(|| start.unwrap_or(1)));
                }
            }
            "li" => {
                self.flush();
                self.marker = (!closing).then(|| match self.lists.last_mut() {
                    Some(Some(number)) => {
                        *number += 1;
                        format!("{}.", *number - 1)
                    }
                    _ => "•".to_string(),
                });
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                self.style = if closing { Style::Paragraph } else { Style::Heading(name.as_bytes()[1] - b'0') };
            }
            "pre" => {
                self.flush();
                self.style = if closing { Style::Paragraph } else { Style::Code };
            }
            "blockquote" => {
                self.flush();
                count(&mut self.quotes);
            }
            "hr" => {
                self.flush();
                self.blocks.push(Block { style: Style::Rule, depth: 0, marker: None, spans: Vec::new() });
            }
            "b" | "strong" => count(&mut self.bold),
            "i" | "em" => count(&mut self.italic),
            "code" => count(&mut self.code),
            "s" | "del" | "strike" => count(&mut self.strike),
            "a" if closing => {
                self.links.pop();
            }
            "a" => self.links.push(attribute(tag, "href").and_then(|href| link_target(&href))),
            "img" => {
                let label = img_alt(tag).map_or("[Image]".to_string(), |alt| format!("[Image: {alt}]"));
                self.links.push(attribute(tag, "src").and_then(|src| link_target(&src)));
                self.push(&label);
                self.links.pop();
            }
            _ => {}
        }
    }

    /// Text from the HTML source, where line breaks are just whitespace (except in code blocks).
    fn text(&mut self, raw: &str) {
        let raw = if self.style == Style::Code { raw.to_string() } else { raw.replace('\n', " ") };
        self.push(&decode_entities(&raw));
    }

    fn push(&mut self, text: &str) {
        let pre = self.style == Style::Code;
        let look = Span {
            text: String::new(),
            bold: self.bold > 0,
            italic: self.italic > 0,
            code: self.code > 0 && !pre,
            strike: self.strike > 0,
            link: self.links.iter().rev().find_map(Clone::clone),
        };
        for c in text.chars() {
            let last = self.spans.last().and_then(|s| s.text.chars().last());
            // Outside code blocks, runs of whitespace become one space, and none
            // starts a block or a line.
            let c = match c {
                '\n' if !pre => {
                    if let Some(span) = self.spans.last_mut().filter(|s| s.text.ends_with(' ')) {
                        span.text.pop();
                    }
                    '\n'
                }
                c if c.is_whitespace() && !pre => {
                    if matches!(last, None | Some(' ' | '\n')) {
                        continue;
                    }
                    ' '
                }
                c => c,
            };
            match self.spans.last_mut() {
                Some(span) if span.same_look(&look) => span.text.push(c),
                _ => self.spans.push(Span { text: c.to_string(), ..look.clone() }),
            }
        }
    }

    fn flush(&mut self) {
        let mut spans = std::mem::take(&mut self.spans);
        let pre = self.style == Style::Code;
        if let Some(first) = spans.first_mut() {
            first.text = if pre { first.text.trim_start_matches('\n') } else { first.text.trim_start() }.to_string();
        }
        // Trimming can empty the last span, revealing another to trim.
        while let Some(last) = spans.last_mut() {
            last.text = if pre { last.text.trim_end_matches('\n') } else { last.text.trim_end() }.to_string();
            if !last.text.is_empty() {
                break;
            }
            spans.pop();
        }
        spans.retain(|s| !s.text.is_empty());
        if spans.is_empty() {
            return;
        }
        let style = if self.quotes > 0 && self.style == Style::Paragraph { Style::Quote } else { self.style };
        let depth = self.lists.len().min(u8::MAX as usize) as u8;
        self.blocks.push(Block { style, depth, marker: self.marker.take(), spans });
    }
}

/// Turns Markdown (as NIP-23 posts use) into HTML for [`to_blocks`]. HTML in
/// the Markdown is passed along; it's only ever read for its text.
pub fn markdown_to_html(markdown: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(markdown, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH));
    out
}

/// Where a link in an item may lead: web pages, and Nostr links
/// (`nostr:npub1…`) to their page on the web viewer. Anything else (`file:`
/// paths, other apps' links) is left as plain text.
pub fn link_target(href: &str) -> Option<String> {
    let href = href.trim();
    if is_web_link(href) {
        return Some(href.to_string());
    }
    let id = href.strip_prefix("nostr:")?;
    let bech32 = !id.is_empty() && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    bech32.then(|| format!("{}/{id}", crate::longform::WEB_VIEWER))
}

/// Whether a link is a web page. Links come from feeds, and opening anything
/// else (`file:` paths, other apps' link types) would let a feed open local
/// files or start other programs.
pub fn is_web_link(link: &str) -> bool {
    reqwest::Url::parse(link).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
}

/// All of an item's text, without markup.
pub fn plain_text(html: &str) -> String {
    let texts: Vec<String> = to_blocks(html).into_iter().map(|b| b.text().replace('\n', " ")).collect();
    texts.join(" ")
}

/// Plain text from the start of an item, for previews.
pub fn snippet(html: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for block in to_blocks(html) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&block.text().replace('\n', " "));
        if out.chars().count() >= max_chars {
            break;
        }
    }
    out.chars().take(max_chars).collect()
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

    /// (style, depth, marker, text) of each block.
    fn outline(blocks: &[Block]) -> Vec<(Style, u8, Option<&str>, String)> {
        blocks.iter().map(|b| (b.style, b.depth, b.marker.as_deref(), b.text())).collect()
    }

    fn texts(html: &str) -> Vec<String> {
        to_blocks(html).iter().map(Block::text).collect()
    }

    #[test]
    fn converts_structure() {
        let blocks = to_blocks(
            "<h2>Title</h2><p>Hello &amp; <b>welcome</b>,\n  friend</p><ul><li>One</li><li>Two</li></ul><script>x()</script><pre>a\n  b</pre><hr>",
        );
        let expected = [
            (Style::Heading(2), 0, None, "Title"),
            (Style::Paragraph, 0, None, "Hello & welcome, friend"),
            (Style::Paragraph, 1, Some("•"), "One"),
            (Style::Paragraph, 1, Some("•"), "Two"),
            (Style::Code, 0, None, "a\n  b"),
            (Style::Rule, 0, None, ""),
        ];
        assert_eq!(outline(&blocks), expected.map(|(s, d, m, t)| (s, d, m, t.to_string())));
    }

    #[test]
    fn keeps_inline_styles_and_safe_links() {
        let blocks = to_blocks(
            "<p>A <strong>bold</strong> and <em>soft</em> <code>x()</code> <a href=\"https://x.example/a\">link</a> \
             <a href=\"file:///etc/passwd\">file</a> <del>gone</del></p>",
        );
        let spans = &blocks[0].spans;
        let find = |text: &str| spans.iter().find(|s| s.text == text).unwrap();
        assert!(find("bold").bold && find("soft").italic && find("x()").code && find("gone").strike);
        assert_eq!(find("link").link.as_deref(), Some("https://x.example/a"));
        // Not a web page: shown, but not a link.
        assert_eq!(find(" file ").link, None);
        assert_eq!(blocks[0].text(), "A bold and soft x() link file gone");
    }

    #[test]
    fn plain_text_paragraphs() {
        assert_eq!(texts("One\n\nTwo &#8217; &#x41;"), ["One", "Two ’ A"]);
    }

    #[test]
    fn reads_attributes_of_any_kind() {
        // Unquoted values starting with a multi-byte character used to crash.
        assert_eq!(texts("<p>x<img alt=éclair src=a.png></p>"), ["x[Image: éclair]"]);
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
            assert_eq!(link_target(link), None, "{link}");
        }
        assert_eq!(link_target("nostr:npub1abc").as_deref(), Some("https://ditto.pub/npub1abc"));
        assert_eq!(link_target("nostr:npub1abc?x=1"), None);
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
    fn whitespace_collapses_across_styles_and_line_breaks() {
        assert_eq!(texts("<p>  a <b> b </b> c<br> \n d  </p>"), ["a b c\nd"]);
        assert_eq!(texts("<p><br>start</p>"), ["start"]);
    }

    #[test]
    fn markdown_becomes_blocks() {
        let md = "# Hello\n\n---\n\nSome **bold** and _soft_ text with a [link](https://x.example) and ![a cat](https://x.example/cat.png).\nSame paragraph.\n\n- one\n  - inner\n- two\n\n3. three\n4. four\n\n> quoted <b>tag</b>\n> still quoted\n\n```\nlet x = 1 < 2;\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n~~old~~ `code` and nostr:npub1abc [mention](nostr:npub1abc)";
        let blocks = to_blocks(&markdown_to_html(md));
        let expected = [
            (Style::Heading(1), 0, None, "Hello"),
            (Style::Rule, 0, None, ""),
            (Style::Paragraph, 0, None, "Some bold and soft text with a link and [Image: a cat]. Same paragraph."),
            (Style::Paragraph, 1, Some("•"), "one"),
            (Style::Paragraph, 2, Some("•"), "inner"),
            (Style::Paragraph, 1, Some("•"), "two"),
            (Style::Paragraph, 1, Some("3."), "three"),
            (Style::Paragraph, 1, Some("4."), "four"),
            (Style::Quote, 0, None, "quoted tag still quoted"),
            (Style::Code, 0, None, "let x = 1 < 2;"),
            (Style::Paragraph, 0, None, "A | B"),
            (Style::Paragraph, 0, None, "1 | 2"),
            (Style::Paragraph, 0, None, "old code and nostr:npub1abc mention"),
        ];
        assert_eq!(outline(&blocks), expected.map(|(s, d, m, t)| (s, d, m, t.to_string())));
        let spans: Vec<&Span> = blocks.iter().flat_map(|b| &b.spans).collect();
        let find = |text: &str| *spans.iter().find(|s| s.text == text).unwrap();
        assert!(find("bold").bold && find("soft").italic && find("old").strike && find("code").code);
        assert!(find("A").bold, "table headers are bold");
        assert_eq!(find("link").link.as_deref(), Some("https://x.example"));
        assert_eq!(find("[Image: a cat]").link.as_deref(), Some("https://x.example/cat.png"));
        assert_eq!(find("mention").link.as_deref(), Some("https://ditto.pub/npub1abc"));
    }

    #[test]
    fn markdown_html_stays_text() {
        let md = "One *two* three.\n<br />\n*(source:* *Somewhere)* and 2 * 3 = 6\n\n<script>alert(1)</script>";
        assert_eq!(texts(&markdown_to_html(md)), ["One two three.\n(source: Somewhere) and 2 * 3 = 6"]);
    }
}
