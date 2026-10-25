//! Inline the images an HTML capture references by path (ADR 0038).
//!
//! `<img src>` values and CSS `url()` in `<style>` elements and `style`
//! attributes that name a local file become `data:` URIs, so the stored HTML
//! shows them without the files. lol_html only locates these values; every
//! other byte of the document is kept as written.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cssparser::{Parser, ParserInput, Token};
use lol_html::{HtmlRewriter, Settings, element, end_tag};
use markup5ever::data::{C1_REPLACEMENTS, NAMED_ENTITIES};
use sha2::{Digest, Sha256};
use tools::image::ImageFormat;

use super::{ArtifactKind, InlinedAsset, MAX_BUNDLED_HTML_BYTES, SourceRoots, read_limited};

/// Failing references quoted in a rejection; the rest are only counted.
const MAX_LISTED: usize = 8;
/// Characters of one reference quoted in a rejection.
const MAX_QUOTED: usize = 200;

/// Replace the local image references of `html`, read from the resolved
/// `source`, with data URIs. `label` names the file in errors. A document
/// without such references is returned unchanged.
pub(super) fn inline(
    roots: &SourceRoots,
    source: &Path,
    label: &Path,
    html: Vec<u8>,
) -> Result<(Vec<u8>, Vec<InlinedAsset>), String> {
    let text = std::str::from_utf8(&html)
        .map_err(|_| format!("{} does not contain valid text/html data", label.display()))?;
    let sites = locate(text).map_err(|error| {
        format!(
            "cannot read the HTML of {} to inline its images: {error}",
            label.display()
        )
    })?;
    let references = references(text, &sites)?;
    if references.is_empty() {
        return Ok((html, Vec::new()));
    }
    // Relative references resolve from the real file, not from a link to it.
    let base = source.parent().unwrap_or(Path::new("/"));
    let mut images = Images::default();
    let mut targets = Vec::with_capacity(references.len());
    let mut failures = Vec::new();
    let mut reported = HashSet::new();
    for reference in &references {
        match images.resolve(roots, &local_path(base, &reference.written)) {
            Ok(index) => targets.push(index),
            Err(reason) => {
                if reported.insert(reference.written.as_str()) {
                    failures.push((reference.written.as_str(), reason));
                }
            }
        }
    }
    if !failures.is_empty() {
        return Err(rejection(label, &failures));
    }
    let size = references
        .iter()
        .zip(&targets)
        .fold(text.len(), |size, (reference, &index)| {
            size - reference.range.len() + reference.replacement_len(&images.list[index])
        });
    if images.over_budget || size as u64 > MAX_BUNDLED_HTML_BYTES {
        return Err(format!(
            "{} would be {size} bytes with its images inlined, over the {} MiB limit for a captured HTML file; reference fewer or smaller images",
            label.display(),
            MAX_BUNDLED_HTML_BYTES / 1024 / 1024
        ));
    }
    let encoded = images
        .list
        .iter()
        .map(|image| BASE64.encode(image.bytes.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>();
    let mut bundled = String::with_capacity(size);
    let mut cursor = 0;
    for (reference, &index) in references.iter().zip(&targets) {
        bundled.push_str(&text[cursor..reference.range.start]);
        let image = &images.list[index];
        if reference.css {
            bundled.push_str("url(");
        }
        bundled.push_str("data:");
        bundled.push_str(image.format.media_type());
        bundled.push_str(";base64,");
        bundled.push_str(&encoded[index]);
        if reference.css {
            bundled.push(')');
        }
        cursor = reference.range.end;
    }
    bundled.push_str(&text[cursor..]);
    debug_assert_eq!(bundled.len(), size);
    let assets = images
        .list
        .into_iter()
        .map(|image| InlinedAsset {
            source_path: image.path.to_string_lossy().into_owned(),
            sha256: image.sha256,
            byte_len: image.byte_len,
        })
        .collect();
    Ok((bundled.into_bytes(), assets))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SiteKind {
    ImageSource,
    StyleAttribute,
    StyleElement,
}

/// HTML tokenizer context for character references. The legacy-name exception
/// belongs only to attributes, not Data/RCDATA. RawText, PlainText and CDATA
/// are literal (CDATA-bearing style elements are rejected before decoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharacterReferences {
    Literal,
    Attribute,
    Text,
}

/// A span of the document whose value may reference images.
#[derive(Debug)]
struct Site {
    kind: SiteKind,
    range: Range<usize>,
    context: CharacterReferences,
}

struct StyleSpan {
    range: Range<usize>,
    foreign: bool,
    closed: bool,
}

/// The first `src` of each `<img>`, the first `style` attribute of each
/// element (browsers drop repeated attributes) and the entire text of `<style>`.
///
/// A foreign style may contain markup/CDATA that splits CSS tokens across text
/// nodes. Rather than guess at DOM text and rewrite through markup boundaries,
/// reject all foreign styles containing a literal `<`, including comments,
/// CDATA and child elements. Encoded `<` is ordinary Data text and is supported.
fn locate(html: &str) -> Result<Vec<Site>, String> {
    let sites = RefCell::new(Vec::new());
    let styles = Rc::new(RefCell::new(Vec::<StyleSpan>::new()));
    let settings = Settings::new()
        // Rare misnested markup may parse differently in a browser; the
        // capture still stores the document as written, except for ambiguous
        // foreign styles which are explicitly refused below.
        .with_strict(false)
        .append_element_content_handler(element!("*", |element| {
            let image = element.tag_name() == "img";
            let mut seen = Vec::new();
            for attribute in element.attributes() {
                let kind = match attribute.name().as_str() {
                    "src" if image => SiteKind::ImageSource,
                    "style" => SiteKind::StyleAttribute,
                    _ => continue,
                };
                if seen.contains(&kind) {
                    continue;
                }
                seen.push(kind);
                let Some(location) = attribute.value_source_location() else {
                    continue;
                };
                let range = location.bytes();
                if html.get(range.clone()) != Some(attribute.value().as_str()) {
                    return Err("an attribute does not match its source position".into());
                }
                sites.borrow_mut().push(Site {
                    kind,
                    range,
                    context: CharacterReferences::Attribute,
                });
            }
            if element.tag_name() == "style" && element.can_have_content() {
                let index = styles.borrow().len();
                styles.borrow_mut().push(StyleSpan {
                    range: element.source_location().bytes().end..html.len(),
                    foreign: element.namespace_uri() != "http://www.w3.org/1999/xhtml",
                    closed: false,
                });
                let styles = Rc::clone(&styles);
                element.on_end_tag(end_tag!(move |end| {
                    let mut styles = styles.borrow_mut();
                    styles[index].range.end = end.source_location().bytes().start;
                    styles[index].closed = true;
                    Ok(())
                }))?;
            }
            Ok(())
        }));
    let mut rewriter = HtmlRewriter::new(settings, |_: &[u8]| {});
    rewriter
        .write(html.as_bytes())
        .and_then(|()| rewriter.end())
        .map_err(|error| error.to_string())?;
    let mut sites = sites.into_inner();
    let mut unsupported = Vec::new();
    for style in styles.borrow().iter() {
        let raw = html
            .get(style.range.clone())
            .ok_or("style content does not match its source position")?;
        if style.foreign && (!style.closed || raw.contains('<')) {
            unsupported.push(format!(
                "foreign <style> content at byte {}: {}",
                style.range.start,
                if !style.closed {
                    "no explicit closing tag"
                } else {
                    "literal '<' (CDATA, comments or child markup are not supported)"
                }
            ));
            continue;
        }
        sites.push(Site {
            kind: SiteKind::StyleElement,
            range: style.range.clone(),
            context: if style.foreign {
                CharacterReferences::Text
            } else {
                CharacterReferences::Literal
            },
        });
    }
    if !unsupported.is_empty() {
        let mut error = String::from("HTML was not captured: unsupported style content:");
        for reason in unsupported.iter().take(MAX_LISTED) {
            error.push_str(&format!("\n- {reason}"));
        }
        if unsupported.len() > MAX_LISTED {
            error.push_str(&format!("\n- and {} more", unsupported.len() - MAX_LISTED));
        }
        error.push_str("\nUse plain CSS text in foreign style elements, or move the CSS to an HTML <style> element.");
        return Err(error);
    }
    Ok(sites)
}

/// A reference to a local file and the bytes of the document it replaces.
#[derive(Debug)]
struct Reference {
    range: Range<usize>,
    /// The URL as the browser reads it: after character references and CSS escapes.
    written: String,
    /// Whether the range is a whole CSS `url()` token rather than an attribute value.
    css: bool,
}

impl Reference {
    fn replacement_len(&self, image: &Image) -> usize {
        let wrapper = if self.css { "url()".len() } else { 0 };
        wrapper
            + "data:;base64,".len()
            + image.format.media_type().len()
            + image.byte_len.div_ceil(3) as usize * 4
    }
}

/// The local references of `sites` in document order, none overlapping another.
fn references(html: &str, sites: &[Site]) -> Result<Vec<Reference>, String> {
    let mut found = Vec::new();
    for site in sites {
        let value = Decoded::new(&html[site.range.clone()], site.context);
        if site.kind == SiteKind::ImageSource {
            let url = browser_url(&value.text);
            if is_local(&url) {
                found.push(Reference {
                    range: site.range.clone(),
                    written: url,
                    css: false,
                });
            }
            continue;
        }
        for (token, url) in css_urls(&value.text)? {
            let url = browser_url(&url);
            if is_local(&url) {
                let raw = value.raw_range(token);
                found.push(Reference {
                    range: site.range.start + raw.start..site.range.start + raw.end,
                    written: url,
                    css: true,
                });
            }
        }
    }
    found.sort_by_key(|reference| (reference.range.start, reference.range.end));
    let mut end = 0;
    found.retain(|reference| {
        let disjoint = reference.range.start >= end;
        if disjoint {
            end = reference.range.end;
        }
        disjoint
    });
    Ok(found)
}

/// The URL parser drops surrounding spaces and control characters, and every
/// tab and newline.
fn browser_url(value: &str) -> String {
    value
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

/// Whether `url` names a file relative to the document: not empty, not only a
/// fragment, and without a scheme or a host.
fn is_local(url: &str) -> bool {
    !(url.is_empty()
        || url.starts_with('#')
        || url.starts_with("//")
        || url.starts_with("\\\\")
        || has_scheme(url))
}

fn has_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

/// Resolve `url` as a browser would against the file URL of `base`: drop the
/// query and fragment, decode percent escapes and apply dot segments.
/// A path starting with `/` resolves from the filesystem root.
fn local_path(base: &Path, url: &str) -> PathBuf {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let decoded = percent_decode(&path.replace('\\', "/"));
    let mut resolved = if decoded.starts_with(b"/") {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    };
    for segment in decoded.split(|byte| *byte == b'/') {
        match segment {
            b"" | b"." => {}
            b".." => {
                resolved.pop();
            }
            name => resolved.push(OsStr::from_bytes(name)),
        }
    }
    resolved
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |offset: usize| {
            bytes
                .get(index + offset)
                .and_then(|byte| char::from(*byte).to_digit(16))
        };
        if bytes[index] == b'%'
            && let (Some(high), Some(low)) = (hex(1), hex(2))
        {
            decoded.push((high * 16 + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    decoded
}

/// One image file read for inlining.
struct Image {
    path: PathBuf,
    format: ImageFormat,
    sha256: String,
    byte_len: u64,
    /// Dropped once the files read could not fit in a captured HTML file.
    bytes: Option<Vec<u8>>,
}

/// Image files read so far, each once however often and however it is referenced.
#[derive(Default)]
struct Images {
    list: Vec<Image>,
    by_reference: HashMap<PathBuf, Result<usize, String>>,
    by_file: HashMap<PathBuf, usize>,
    encoded_len: u64,
    over_budget: bool,
}

impl Images {
    /// The index of the image at the unresolved `path`, or why it cannot be inlined.
    fn resolve(&mut self, roots: &SourceRoots, path: &Path) -> Result<usize, String> {
        if let Some(outcome) = self.by_reference.get(path) {
            return outcome.clone();
        }
        let outcome = self.load(roots, path);
        self.by_reference
            .insert(path.to_path_buf(), outcome.clone());
        outcome
    }

    fn load(&mut self, roots: &SourceRoots, path: &Path) -> Result<usize, String> {
        let file = path.canonicalize().map_err(|error| {
            // Say nothing about what exists outside the roots.
            if !roots.contains(path) {
                roots.outside().to_owned()
            } else if error.kind() == std::io::ErrorKind::NotFound {
                "was not found".to_owned()
            } else {
                format!("cannot be opened: {error}")
            }
        })?;
        if !roots.contains(&file) {
            return Err(roots.outside().to_owned());
        }
        if let Some(&index) = self.by_file.get(&file) {
            return Ok(index);
        }
        let format = ImageFormat::from_extension(&file)
            .ok_or_else(|| "is not a .png, .jpg, .gif or .webp file".to_owned())?;
        let bytes = read_limited(roots, &file, ArtifactKind::Image(format))?;
        let byte_len = bytes.len() as u64;
        self.encoded_len += byte_len.div_ceil(3) * 4;
        // The document cannot fit any more; keep checking the rest without holding them.
        self.over_budget |= self.encoded_len > MAX_BUNDLED_HTML_BYTES;
        let index = self.list.len();
        self.list.push(Image {
            path: file.clone(),
            format,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            byte_len,
            bytes: (!self.over_budget).then_some(bytes),
        });
        self.by_file.insert(file, index);
        Ok(index)
    }
}

fn rejection(label: &Path, failures: &[(&str, String)]) -> String {
    let mut message = format!(
        "{} was not captured: {} image reference{} cannot be inlined",
        label.display(),
        failures.len(),
        if failures.len() == 1 { "" } else { "s" }
    );
    for (written, reason) in failures.iter().take(MAX_LISTED) {
        let mut quoted = written.chars().take(MAX_QUOTED).collect::<String>();
        if written.chars().count() > MAX_QUOTED {
            quoted.push('…');
        }
        message.push_str(&format!("\n- {quoted:?} {reason}"));
    }
    if failures.len() > MAX_LISTED {
        message.push_str(&format!("\n- and {} more", failures.len() - MAX_LISTED));
    }
    message.push_str("\nReferenced images must be .png, .jpg, .gif or .webp files of up to 8 MiB inside the workspace or $TMPDIR; relative paths resolve from the HTML file's directory.");
    message
}

/// A value with its character references decoded, mapping each decoded byte
/// back to its offset in the raw value.
struct Decoded<'a> {
    text: Cow<'a, str>,
    offsets: Option<Vec<usize>>,
}

impl<'a> Decoded<'a> {
    fn new(raw: &'a str, context: CharacterReferences) -> Self {
        if context == CharacterReferences::Literal || !raw.contains('&') {
            return Self {
                text: Cow::Borrowed(raw),
                offsets: None,
            };
        }
        let mut text = String::with_capacity(raw.len());
        let mut offsets = Vec::with_capacity(raw.len() + 1);
        let mut index = 0;
        while let Some(next) = raw[index..].chars().next() {
            let (decoded, len) = if next == '&' {
                char_reference(&raw[index + 1..], context)
                    .map_or((['&', '\0'], 1), |(chars, len)| (chars, len + 1))
            } else {
                ([next, '\0'], next.len_utf8())
            };
            for (position, c) in decoded.into_iter().enumerate() {
                if position == 1 && c == '\0' {
                    continue;
                }
                offsets.extend(std::iter::repeat_n(index, c.len_utf8()));
                text.push(c);
            }
            index += len;
        }
        offsets.push(raw.len());
        Self {
            text: Cow::Owned(text),
            offsets: Some(offsets),
        }
    }

    fn raw_range(&self, range: Range<usize>) -> Range<usize> {
        match &self.offsets {
            Some(offsets) => offsets[range.start]..offsets[range.end],
            None => range,
        }
    }
}

/// WHATWG character references in attribute or Data/RCDATA context, including
/// the longest legacy name, attribute-only exception and two-codepoint results.
fn char_reference(rest: &str, context: CharacterReferences) -> Option<([char; 2], usize)> {
    if let Some(number) = rest.strip_prefix('#') {
        let (radix, start) = match number.as_bytes().first() {
            Some(b'x' | b'X') => (16, 2),
            _ => (10, 1),
        };
        let digits = rest[start..]
            .bytes()
            .take_while(|byte| char::from(*byte).is_digit(radix))
            .count();
        if digits == 0 {
            return None;
        }
        let end = start + digits;
        let codepoint = u32::from_str_radix(&rest[start..end], radix).unwrap_or(u32::MAX);
        let decoded = if (0x80..=0x9f).contains(&codepoint) {
            C1_REPLACEMENTS[(codepoint - 0x80) as usize]
                .unwrap_or_else(|| char::from_u32(codepoint).unwrap())
        } else {
            char::from_u32(codepoint)
                .filter(|c| *c != '\0')
                .unwrap_or('\u{FFFD}')
        };
        let end = end + usize::from(rest.as_bytes().get(end) == Some(&b';'));
        return Some(([decoded, '\0'], end));
    }
    let mut longest = None;
    for (index, byte) in rest.bytes().enumerate() {
        if !(byte.is_ascii_alphanumeric() || byte == b';') {
            break;
        }
        let len = index + 1;
        let Some(&(first, second)) = NAMED_ENTITIES.get(&rest[..len]) else {
            break;
        };
        if first != 0 {
            longest = Some((
                [
                    char::from_u32(first).unwrap(),
                    char::from_u32(second).unwrap(),
                ],
                len,
            ));
        }
    }
    let (chars, len) = longest?;
    if context == CharacterReferences::Attribute
        && !rest[..len].ends_with(';')
        && rest
            .as_bytes()
            .get(len)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'=')
    {
        return None;
    }
    Some((chars, len))
}

/// Tokenize without parsing declarations: nested blocks still need visiting,
/// while comments, strings and bad URLs must never become image references.
fn css_urls(css: &str) -> Result<Vec<(Range<usize>, String)>, String> {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    let mut urls = Vec::new();
    collect_css_urls(&mut parser, &mut urls, 0)?;
    Ok(urls)
}

fn collect_css_urls(
    parser: &mut Parser<'_, '_>,
    urls: &mut Vec<(Range<usize>, String)>,
    depth: usize,
) -> Result<(), String> {
    loop {
        let start = parser.position().byte_index();
        let Ok(token) = parser.next_including_whitespace_and_comments().cloned() else {
            break;
        };
        match token {
            Token::UnquotedUrl(value) => {
                urls.push((start..parser.position().byte_index(), value.to_string()))
            }
            Token::Function(name) if name.eq_ignore_ascii_case("url") => {
                let value = parser.parse_nested_block(|args| {
                    let value = args.expect_string_cloned()?;
                    args.expect_exhausted()?;
                    Ok::<_, cssparser::ParseError<'_, ()>>(value)
                });
                if let Ok(value) = value {
                    urls.push((start..parser.position().byte_index(), value.to_string()));
                }
            }
            Token::Function(_)
            | Token::ParenthesisBlock
            | Token::SquareBracketBlock
            | Token::CurlyBracketBlock => {
                // Reject rather than silently miss URLs or exhaust the stack.
                if depth >= 128 {
                    return Err("HTML was not captured: CSS nesting exceeds 128 levels".into());
                }
                let mut result = Ok(());
                let _: Result<(), cssparser::ParseError<'_, ()>> =
                    parser.parse_nested_block(|nested| {
                        result = collect_css_urls(nested, urls, depth + 1);
                        Ok(())
                    });
                result?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
