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

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use lol_html::html_content::TextType;
use lol_html::{HtmlRewriter, Settings, element, text};
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
    let references = references(text, &sites);
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

/// A span of the document whose value may reference images.
#[derive(Debug)]
struct Site {
    kind: SiteKind,
    range: Range<usize>,
    /// Whether character references apply, as everywhere but in raw text.
    escaped: bool,
}

/// The first `src` of each `<img>`, the first `style` attribute of each
/// element (browsers drop repeated attributes) and the text of `<style>`.
fn locate(html: &str) -> Result<Vec<Site>, String> {
    let sites = RefCell::new(Vec::new());
    let style = RefCell::new(None::<(Range<usize>, bool)>);
    let settings = Settings::new()
        // Rare misnested markup may parse differently in a browser; the
        // capture still stores the document as written.
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
                    escaped: true,
                });
            }
            Ok(())
        }))
        .append_element_content_handler(text!("style", |chunk| {
            let range = chunk.source_location().bytes();
            if html.get(range.clone()) != Some(chunk.as_str()) {
                return Err("style text does not match its source position".into());
            }
            let mut pending = style.borrow_mut();
            let (span, _) = pending.get_or_insert_with(|| {
                (
                    range.clone(),
                    !matches!(chunk.text_type(), TextType::RawText | TextType::PlainText),
                )
            });
            span.end = span.end.max(range.end);
            if chunk.last_in_text_node()
                && let Some((range, escaped)) = pending.take()
                && !range.is_empty()
            {
                sites.borrow_mut().push(Site {
                    kind: SiteKind::StyleElement,
                    range,
                    escaped,
                });
            }
            Ok(())
        }));
    let mut rewriter = HtmlRewriter::new(settings, |_: &[u8]| {});
    rewriter
        .write(html.as_bytes())
        .and_then(|()| rewriter.end())
        .map_err(|error| error.to_string())?;
    Ok(sites.into_inner())
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
fn references(html: &str, sites: &[Site]) -> Vec<Reference> {
    let mut found = Vec::new();
    for site in sites {
        let value = Decoded::new(&html[site.range.clone()], site.escaped);
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
        for (token, url) in css_urls(&value.text) {
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
    found
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
        let bytes = read_limited(&file, ArtifactKind::Image(format))?;
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
    fn new(raw: &'a str, escaped: bool) -> Self {
        if !escaped || !raw.contains('&') {
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
                char_reference(&raw[index + 1..]).map_or(('&', 1), |(c, len)| (c, len + 1))
            } else {
                (next, next.len_utf8())
            };
            offsets.extend(std::iter::repeat_n(index, decoded.len_utf8()));
            text.push(decoded);
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

/// The character `rest` (the text after `&`) starts with and the bytes it
/// takes: numeric references and the named ones that can matter in a URL or
/// in CSS. Other names stay as written.
fn char_reference(rest: &str) -> Option<(char, usize)> {
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
        let decoded = u32::from_str_radix(&rest[start..end], radix)
            .ok()
            .and_then(char::from_u32)
            .filter(|c| *c != '\0')
            .unwrap_or('\u{FFFD}');
        let end = end + usize::from(rest.as_bytes().get(end) == Some(&b';'));
        return Some((decoded, end));
    }
    let len = rest.bytes().take_while(u8::is_ascii_alphanumeric).count();
    if rest.as_bytes().get(len) != Some(&b';') {
        return None;
    }
    let decoded = match &rest[..len] {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{A0}',
        "Tab" => '\t',
        "NewLine" => '\n',
        "excl" => '!',
        "num" => '#',
        "dollar" => '$',
        "percnt" => '%',
        "lpar" => '(',
        "rpar" => ')',
        "ast" | "midast" => '*',
        "plus" => '+',
        "comma" => ',',
        "period" => '.',
        "sol" => '/',
        "colon" => ':',
        "semi" => ';',
        "equals" => '=',
        "quest" => '?',
        "commat" => '@',
        "lsqb" | "lbrack" => '[',
        "bsol" => '\\',
        "rsqb" | "rbrack" => ']',
        "lowbar" | "UnderBar" => '_',
        "grave" | "DiacriticalGrave" => '`',
        "lcub" | "lbrace" => '{',
        "verbar" | "vert" | "VerticalLine" => '|',
        "rcub" | "rbrace" => '}',
        _ => return None,
    };
    Some((decoded, len + 1))
}

/// The `url()` tokens of a style sheet or a declaration list with their values,
/// skipping comments and strings as the CSS tokenizer does.
fn css_urls(css: &str) -> Vec<(Range<usize>, String)> {
    let bytes = css.as_bytes();
    let mut urls = Vec::new();
    let mut index = 0;
    // An escaped character continues a name, so `\"url(` is not a URL.
    let mut escape_end = None;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = css[index + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| index + 2 + end + 2);
            }
            quote @ (b'"' | b'\'') => index = css_string(css, index + 1, quote).1,
            b'\\' if matches!(bytes.get(index + 1), None | Some(b'\n' | b'\r' | b'\x0C')) => {
                index += 1;
            }
            b'\\' => {
                index = css_escape(css, index + 1).1;
                escape_end = Some(index);
            }
            _ if escape_end != Some(index) && starts_url(bytes, index) => {
                let (end, value) = url_token(css, index + 4);
                if let Some(value) = value {
                    urls.push((index..end, value));
                }
                index = end;
            }
            _ => index += 1,
        }
    }
    urls
}

/// A case-insensitive `url(` that is not the end of a longer name.
fn starts_url(bytes: &[u8], index: usize) -> bool {
    bytes
        .get(index..index + 4)
        .is_some_and(|name| name.eq_ignore_ascii_case(b"url("))
        && (index == 0 || !is_name_byte(bytes[index - 1]))
}

const fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'\\') || byte >= 0x80
}

const fn is_css_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0C')
}

fn skip_spaces(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).copied().is_some_and(is_css_space) {
        index += 1;
    }
    index
}

/// The value and end of the `url(` token whose arguments start at `start`;
/// no value for malformed tokens, which CSS ignores.
fn url_token(css: &str, start: usize) -> (usize, Option<String>) {
    let bytes = css.as_bytes();
    let start = skip_spaces(bytes, start);
    if let Some(&quote @ (b'"' | b'\'')) = bytes.get(start) {
        let (value, after) = css_string(css, start + 1, quote);
        let close = skip_spaces(bytes, after);
        return match (value, bytes.get(close)) {
            (Some(value), Some(b')')) => (close + 1, Some(value)),
            (Some(value), None) => (close, Some(value)),
            _ => (after, None),
        };
    }
    let mut value = String::new();
    let mut index = start;
    while let Some(next) = css[index..].chars().next() {
        match next {
            ')' => return (index + 1, Some(value)),
            ' ' | '\t' | '\n' | '\r' | '\x0C' => {
                let close = skip_spaces(bytes, index);
                return match bytes.get(close) {
                    Some(b')') => (close + 1, Some(value)),
                    None => (close, Some(value)),
                    Some(_) => (bad_url_end(css, close), None),
                };
            }
            '"' | '\'' | '(' | '\0'..='\x08' | '\x0B' | '\x0E'..='\x1F' | '\x7F' => {
                return (bad_url_end(css, index), None);
            }
            '\\' => {
                if matches!(bytes.get(index + 1), None | Some(b'\n' | b'\r' | b'\x0C')) {
                    return (bad_url_end(css, index), None);
                }
                let (escaped, end) = css_escape(css, index + 1);
                value.push(escaped);
                index = end;
            }
            other => {
                value.push(other);
                index += other.len_utf8();
            }
        }
    }
    (index, Some(value))
}

/// Where the remnants of a malformed `url(` token end.
fn bad_url_end(css: &str, mut index: usize) -> usize {
    let bytes = css.as_bytes();
    while index < bytes.len() {
        match bytes[index] {
            b')' => return index + 1,
            b'\\' if !matches!(bytes.get(index + 1), None | Some(b'\n' | b'\r' | b'\x0C')) => {
                index = css_escape(css, index + 1).1;
            }
            _ => index += 1,
        }
    }
    index
}

/// The value of the string whose content starts at `start` and the index after
/// it. A newline ends a string without a value, as CSS discards it.
fn css_string(css: &str, start: usize, quote: u8) -> (Option<String>, usize) {
    let bytes = css.as_bytes();
    let mut value = String::new();
    let mut index = start;
    while let Some(next) = css[index..].chars().next() {
        match next {
            _ if next == char::from(quote) => return (Some(value), index + 1),
            '\n' | '\r' | '\x0C' => return (None, index),
            '\\' => match bytes.get(index + 1) {
                None => index += 1,
                Some(b'\r') if bytes.get(index + 2) == Some(&b'\n') => index += 3,
                Some(b'\n' | b'\r' | b'\x0C') => index += 2,
                Some(_) => {
                    let (escaped, end) = css_escape(css, index + 1);
                    value.push(escaped);
                    index = end;
                }
            },
            other => {
                value.push(other);
                index += other.len_utf8();
            }
        }
    }
    (Some(value), index)
}

/// The character an escape whose backslash precedes `start` stands for, and
/// the index after it.
fn css_escape(css: &str, start: usize) -> (char, usize) {
    let bytes = css.as_bytes();
    let digits = bytes[start..]
        .iter()
        .take(6)
        .take_while(|byte| byte.is_ascii_hexdigit())
        .count();
    if digits == 0 {
        return css[start..]
            .chars()
            .next()
            .map_or(('\u{FFFD}', start), |c| (c, start + c.len_utf8()));
    }
    let mut end = start + digits;
    let decoded = u32::from_str_radix(&css[start..end], 16)
        .ok()
        .and_then(char::from_u32)
        .filter(|c| *c != '\0')
        .unwrap_or('\u{FFFD}');
    if bytes.get(end) == Some(&b'\r') && bytes.get(end + 1) == Some(&b'\n') {
        end += 2;
    } else if bytes.get(end).copied().is_some_and(is_css_space) {
        end += 1;
    }
    (decoded, end)
}

#[cfg(test)]
mod tests;
