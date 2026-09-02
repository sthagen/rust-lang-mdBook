use self::take_lines::{
    take_anchored_lines, take_lines, take_rustdoc_include_anchored_lines,
    take_rustdoc_include_lines,
};
use anyhow::{Context, Result};
use mdbook_core::book::{Book, BookItem};
use mdbook_core::static_regex;
use mdbook_core::utils::fs;
use mdbook_preprocessor::{Preprocessor, PreprocessorContext};
use regex::{CaptureMatches, Captures};
use std::ops::{Bound, Range, RangeBounds, RangeFrom, RangeFull, RangeTo};
use std::path::{Path, PathBuf};
use tracing::{error, warn};

mod take_lines;

const ESCAPE_CHAR: char = '\\';
const MAX_LINK_NESTED_DEPTH: usize = 10;

/// A preprocessor for expanding helpers in a chapter. Supported helpers are:
///
/// - `{{# include}}` - Insert an external file of any type. Include the whole file, only particular
///   lines, or only between the specified anchors.
/// - `{{# rustdoc_include}}` - Insert an external Rust file, showing the particular lines
///   specified or the lines between specified anchors, and include the rest of the file behind `#`.
///   This hides the lines from initial display but shows them when the reader expands the code
///   block and provides them to Rustdoc for testing.
/// - `{{# playground}}` - Insert runnable Rust files
/// - `{{# title}}` - Override \<title\> of a webpage.
#[derive(Default)]
#[non_exhaustive]
pub struct LinkPreprocessor;

impl LinkPreprocessor {
    /// Name of this preprocessor.
    pub const NAME: &'static str = "links";

    /// Create a new `LinkPreprocessor`.
    pub fn new() -> Self {
        LinkPreprocessor
    }
}

impl Preprocessor for LinkPreprocessor {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn run(&self, ctx: &PreprocessorContext, mut book: Book) -> Result<Book> {
        let src_dir = ctx.root.join(&ctx.config.book.src);

        book.for_each_mut(|section: &mut BookItem| {
            if let BookItem::Chapter(ref mut ch) = *section {
                if let Some(ref chapter_path) = ch.path {
                    let base = chapter_path
                        .parent()
                        .map(|dir| src_dir.join(dir))
                        .expect("All book items have a parent");

                    let mut chapter_title = ch.name.clone();
                    let content =
                        replace_all(&ch.content, base, chapter_path, 0, &mut chapter_title);
                    ch.content = content;
                    if chapter_title != ch.name {
                        ctx.chapter_titles
                            .borrow_mut()
                            .insert(chapter_path.clone(), chapter_title);
                    }
                }
            }
        });

        Ok(book)
    }
}

fn replace_all<P1, P2>(
    s: &str,
    path: P1,
    source: P2,
    depth: usize,
    chapter_title: &mut String,
) -> String
where
    P1: AsRef<Path>,
    P2: AsRef<Path>,
{
    // When replacing one thing in a string by something with a different length,
    // the indices after that will not correspond,
    // we therefore have to store the difference to correct this
    let path = path.as_ref();
    let source = source.as_ref();
    let mut previous_end_index = 0;
    let mut replaced = String::new();

    for link in find_links(s) {
        replaced.push_str(&s[previous_end_index..link.start_index]);

        match link.render_with_path(path, chapter_title) {
            Ok(new_content) => {
                if depth < MAX_LINK_NESTED_DEPTH {
                    // use split('\n') instead of lines because we DON'T
                    // want the last \n to be removed
                    // Otherwise includes starting a new line would be prefixed
                    // by the preceding line
                    let prefix = replaced.split('\n').last().unwrap_or("");
                    let raw_new_content = if let Some(rel_path) = link.link_type.relative_path(path)
                    {
                        replace_all(&new_content, rel_path, source, depth + 1, chapter_title)
                    } else {
                        new_content
                    };
                    // use lines instead of split('\n') because we DO
                    // want the last \n to be removed
                    // Otherwise inlined includes would fail
                    let prefixed_new_content = raw_new_content
                        .lines()
                        .collect::<Vec<_>>()
                        .join(&format!("\n{prefix}"));
                    replaced.push_str(&prefixed_new_content);
                } else {
                    error!(
                        "Stack depth exceeded in {}. Check for cyclic includes",
                        source.display()
                    );
                }
                previous_end_index = link.end_index;
            }
            Err(e) => {
                error!("Error updating \"{}\", {}", link.link_text, e);
                for cause in e.chain().skip(1) {
                    warn!("Caused By: {}", cause);
                }

                // This should make sure we include the raw `{{# ... }}` snippet
                // in the page content if there are any errors.
                previous_end_index = link.start_index;
            }
        }
    }

    replaced.push_str(&s[previous_end_index..]);
    replaced
}

#[derive(PartialEq, Debug, Clone)]
enum LinkType<'a> {
    Escaped,
    Include(PathBuf, RangeOrAnchor),
    Playground(PathBuf, Vec<&'a str>),
    RustdocInclude(PathBuf, RangeOrAnchor),
    Title(&'a str),
    Invalid(String),
}

#[derive(PartialEq, Debug, Clone)]
enum RangeOrAnchor {
    Range(LineRange),
    Anchor(String),
}

// A range of lines specified with some include directive.
#[derive(PartialEq, Debug, Clone)]
enum LineRange {
    Range(Range<usize>),
    RangeFrom(RangeFrom<usize>),
    RangeTo(RangeTo<usize>),
    RangeFull(RangeFull),
}

impl RangeBounds<usize> for LineRange {
    fn start_bound(&self) -> Bound<&usize> {
        match self {
            LineRange::Range(r) => r.start_bound(),
            LineRange::RangeFrom(r) => r.start_bound(),
            LineRange::RangeTo(r) => r.start_bound(),
            LineRange::RangeFull(r) => r.start_bound(),
        }
    }

    fn end_bound(&self) -> Bound<&usize> {
        match self {
            LineRange::Range(r) => r.end_bound(),
            LineRange::RangeFrom(r) => r.end_bound(),
            LineRange::RangeTo(r) => r.end_bound(),
            LineRange::RangeFull(r) => r.end_bound(),
        }
    }
}

impl From<Range<usize>> for LineRange {
    fn from(r: Range<usize>) -> LineRange {
        LineRange::Range(r)
    }
}

impl From<RangeFrom<usize>> for LineRange {
    fn from(r: RangeFrom<usize>) -> LineRange {
        LineRange::RangeFrom(r)
    }
}

impl From<RangeTo<usize>> for LineRange {
    fn from(r: RangeTo<usize>) -> LineRange {
        LineRange::RangeTo(r)
    }
}

impl From<RangeFull> for LineRange {
    fn from(r: RangeFull) -> LineRange {
        LineRange::RangeFull(r)
    }
}

impl<'a> LinkType<'a> {
    fn relative_path<P: AsRef<Path>>(self, base: P) -> Option<PathBuf> {
        let base = base.as_ref();
        match self {
            LinkType::Escaped | LinkType::Invalid(_) => None,
            LinkType::Include(p, _) => Some(return_relative_path(base, &p)),
            LinkType::Playground(p, _) => Some(return_relative_path(base, &p)),
            LinkType::RustdocInclude(p, _) => Some(return_relative_path(base, &p)),
            LinkType::Title(_) => None,
        }
    }
}
fn return_relative_path<P: AsRef<Path>>(base: P, relative: P) -> PathBuf {
    base.as_ref()
        .join(relative)
        .parent()
        .expect("Included file should not be /")
        .to_path_buf()
}

fn parse_range_or_anchor(parts: Option<&str>) -> RangeOrAnchor {
    let mut parts = parts.unwrap_or("").splitn(3, ':').fuse();

    let next_element = parts.next();
    let start = if let Some(value) = next_element.and_then(|s| s.parse::<usize>().ok()) {
        // subtract 1 since line numbers usually begin with 1
        Some(value.saturating_sub(1))
    } else if let Some("") = next_element {
        None
    } else if let Some(anchor) = next_element {
        return RangeOrAnchor::Anchor(String::from(anchor));
    } else {
        None
    };

    let end = parts.next();
    // If `end` is empty string or any other value that can't be parsed as a usize, treat this
    // include as a range with only a start bound. However, if end isn't specified, include only
    // the single line specified by `start`.
    let end = end.map(|s| s.parse::<usize>());

    match (start, end) {
        (Some(start), Some(Ok(end))) => RangeOrAnchor::Range(LineRange::from(start..end)),
        (Some(start), Some(Err(_))) => RangeOrAnchor::Range(LineRange::from(start..)),
        (Some(start), None) => RangeOrAnchor::Range(LineRange::from(start..start + 1)),
        (None, Some(Ok(end))) => RangeOrAnchor::Range(LineRange::from(..end)),
        (None, None) | (None, Some(Err(_))) => RangeOrAnchor::Range(LineRange::from(RangeFull)),
    }
}

fn parse_quoted_path(input: &str) -> Result<(PathBuf, &str), &'static str> {
    // Caller must strip leading whitespace and verify the opening quote.
    let after_quote = input.strip_prefix('"').ok_or("expected opening quote")?;
    let mut path = String::new();
    let mut chars = after_quote.char_indices();
    let mut closed = false;
    let mut end_index = 0;

    while let Some((idx, ch)) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                // Only `\"` and `\\` are escape sequences.
                Some((_, '"')) => path.push('"'),
                Some((_, '\\')) => path.push('\\'),
                // Any other `\X` (e.g. `\t`, `\n`, Windows `\U`) preserves
                // the backslash as a literal path character.
                Some((_, other)) => {
                    path.push('\\');
                    path.push(other);
                }
                None => return Err("unclosed quote in path"),
            }
        } else if ch == '"' {
            closed = true;
            end_index = idx + 1;
            break;
        } else {
            path.push(ch);
        }
    }

    if closed {
        Ok((PathBuf::from(path), &after_quote[end_index..]))
    } else {
        Err("unclosed quote in path")
    }
}

/// Parse a path that may be quoted (for paths with spaces) or unquoted
/// (backward-compatible split on whitespace).
///
/// The `constructor` closure maps the parsed `PathBuf` and `RangeOrAnchor`
/// to the appropriate `LinkType` variant, eliminating duplication between
/// `parse_include_path` and `parse_rustdoc_include_path`.
fn parse_quoted_or_plain<F>(raw: &str, constructor: F) -> Option<LinkType<'static>>
where
    F: FnOnce(PathBuf, RangeOrAnchor) -> LinkType<'static>,
{
    let trimmed = raw.trim();
    if trimmed.starts_with('"') {
        match parse_quoted_path(trimmed) {
            Ok((path, after)) => {
                let after = after.trim_start();
                let range_or_anchor = parse_range_or_anchor(after.strip_prefix(':'));
                Some(constructor(path, range_or_anchor))
            }
            Err(err) => Some(LinkType::Invalid(err.to_string())),
        }
    } else {
        let mut path_props = trimmed.split_whitespace();
        let file_arg = path_props.next()?;
        let mut parts = file_arg.splitn(2, ':');
        let path = parts.next().unwrap().into();
        let range_or_anchor = parse_range_or_anchor(parts.next());
        Some(constructor(path, range_or_anchor))
    }
}

fn parse_include_path(path: &str) -> Option<LinkType<'static>> {
    parse_quoted_or_plain(path, LinkType::Include)
}

fn parse_rustdoc_include_path(path: &str) -> Option<LinkType<'static>> {
    parse_quoted_or_plain(path, LinkType::RustdocInclude)
}

fn parse_playground_args<'a>(rest: &'a str) -> Option<LinkType<'a>> {
    let trimmed = rest.trim_start();
    if trimmed.starts_with('"') {
        match parse_quoted_path(trimmed) {
            Ok((path, after)) => {
                let props: Vec<&'a str> = after.split_whitespace().collect();
                Some(LinkType::Playground(path, props))
            }
            Err(err) => Some(LinkType::Invalid(err.to_string())),
        }
    } else {
        let mut path_props = trimmed.split_whitespace();
        let file_arg = path_props.next()?;
        let props: Vec<&'a str> = path_props.collect();
        Some(LinkType::Playground(file_arg.into(), props))
    }
}

#[derive(PartialEq, Debug, Clone)]
struct Link<'a> {
    start_index: usize,
    end_index: usize,
    link_type: LinkType<'a>,
    link_text: &'a str,
}

impl<'a> Link<'a> {
    fn from_capture(cap: Captures<'a>) -> Option<Link<'a>> {
        let link_type = match (cap.get(0), cap.get(1), cap.get(2)) {
            (_, Some(link_kind), Some(title)) if link_kind.as_str() == "title" => {
                Some(LinkType::Title(title.as_str()))
            }
            (_, Some(link_kind), Some(rest)) if link_kind.as_str() == "include" => {
                parse_include_path(rest.as_str())
            }
            (_, Some(link_kind), Some(rest)) if link_kind.as_str() == "rustdoc_include" => {
                parse_rustdoc_include_path(rest.as_str())
            }
            (_, Some(link_kind), Some(rest)) if link_kind.as_str() == "playground" => {
                parse_playground_args(rest.as_str())
            }
            (_, Some(link_kind), Some(rest)) if link_kind.as_str() == "playpen" => {
                warn!(
                    "the {{{{#playpen}}}} expression has been \
                    renamed to {{{{#playground}}}}, \
                    please update your book to use the new name"
                );
                parse_playground_args(rest.as_str())
            }
            (Some(mat), None, None) if mat.as_str().starts_with(ESCAPE_CHAR) => {
                Some(LinkType::Escaped)
            }
            _ => None,
        };

        link_type.and_then(|lnk_type| {
            cap.get(0).map(|mat| Link {
                start_index: mat.start(),
                end_index: mat.end(),
                link_type: lnk_type,
                link_text: mat.as_str(),
            })
        })
    }

    fn render_with_path<P: AsRef<Path>>(
        &self,
        base: P,
        chapter_title: &mut String,
    ) -> Result<String> {
        let base = base.as_ref();
        match self.link_type {
            LinkType::Invalid(ref msg) => anyhow::bail!("{msg}"),
            // omit the escape char
            LinkType::Escaped => Ok(self.link_text[1..].to_owned()),
            LinkType::Include(ref pat, ref range_or_anchor) => {
                let target = base.join(pat);

                fs::read_to_string(&target)
                    .map(|s| match range_or_anchor {
                        RangeOrAnchor::Range(range) => take_lines(&s, range.clone()),
                        RangeOrAnchor::Anchor(anchor) => take_anchored_lines(&s, anchor),
                    })
                    .with_context(|| {
                        format!(
                            "Could not read file for link {} ({})",
                            self.link_text,
                            target.display(),
                        )
                    })
            }
            LinkType::RustdocInclude(ref pat, ref range_or_anchor) => {
                let target = base.join(pat);

                fs::read_to_string(&target)
                    .map(|s| match range_or_anchor {
                        RangeOrAnchor::Range(range) => {
                            take_rustdoc_include_lines(&s, range.clone())
                        }
                        RangeOrAnchor::Anchor(anchor) => {
                            take_rustdoc_include_anchored_lines(&s, anchor)
                        }
                    })
                    .with_context(|| {
                        format!(
                            "Could not read file for link {} ({})",
                            self.link_text,
                            target.display(),
                        )
                    })
            }
            LinkType::Playground(ref pat, ref attrs) => {
                let target = base.join(pat);

                let mut contents = fs::read_to_string(&target).with_context(|| {
                    format!(
                        "Could not read file for link {} ({})",
                        self.link_text,
                        target.display()
                    )
                })?;
                let ftype = if !attrs.is_empty() { "rust," } else { "rust" };
                if !contents.ends_with('\n') {
                    contents.push('\n');
                }
                Ok(format!(
                    "```{}{}\n{}```\n",
                    ftype,
                    attrs.join(","),
                    contents
                ))
            }
            LinkType::Title(title) => {
                *chapter_title = title.to_owned();
                Ok(String::new())
            }
        }
    }
}

struct LinkIter<'a>(CaptureMatches<'a, 'a>);

impl<'a> Iterator for LinkIter<'a> {
    type Item = Link<'a>;
    fn next(&mut self) -> Option<Link<'a>> {
        for cap in &mut self.0 {
            if let Some(inc) = Link::from_capture(cap) {
                return Some(inc);
            }
        }
        None
    }
}

fn find_links(contents: &str) -> LinkIter<'_> {
    static_regex!(
        LINK,
        r"(?x)              # insignificant whitespace mode
        \\\{\{\#.*\}\}      # match escaped link
        |                   # or
        \{\{\s*             # link opening parens and whitespace
        \#([a-zA-Z0-9_]+)   # link type
        \s+                 # separating whitespace
        ([^}]+)             # link target path and space separated properties
        \}\}                # link closing parens"
    );

    LinkIter(LINK.captures_iter(contents))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_all_escaped() {
        let start = r"
        Some text over here.
        ```hbs
        \{{#include file.rs}} << an escaped link!
        ```";
        let end = r"
        Some text over here.
        ```hbs
        {{#include file.rs}} << an escaped link!
        ```";
        let mut chapter_title = "test_replace_all_escaped".to_owned();
        assert_eq!(replace_all(start, "", "", 0, &mut chapter_title), end);
    }

    #[test]
    fn test_set_chapter_title() {
        let start = r"{{#title My Title}}
        # My Chapter
        ";
        let end = r"
        # My Chapter
        ";
        let mut chapter_title = "test_set_chapter_title".to_owned();
        assert_eq!(replace_all(start, "", "", 0, &mut chapter_title), end);
        assert_eq!(chapter_title, "My Title");
    }

    #[test]
    fn test_find_links_no_link() {
        let s = "Some random text without link...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
    }

    #[test]
    fn test_find_links_partial_link() {
        let s = "Some random text with {{#playground...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
        let s = "Some random text with {{#include...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
        let s = "Some random text with \\{{#include...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
    }

    #[test]
    fn test_find_links_empty_link() {
        let s = "Some random text with {{#playground}} and {{#playground   }} {{}} {{#}}...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
    }

    #[test]
    fn test_find_links_unknown_link_type() {
        let s = "Some random text with {{#playgroundz ar.rs}} and {{#incn}} {{baz}} {{#bar}}...";
        assert!(find_links(s).collect::<Vec<_>>() == vec![]);
    }

    #[test]
    fn test_find_links_simple_link() {
        let s = "Some random text with {{#playground file.rs}} and {{#playground test.rs }}...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");

        assert_eq!(
            res,
            vec![
                Link {
                    start_index: 22,
                    end_index: 45,
                    link_type: LinkType::Playground(PathBuf::from("file.rs"), vec![]),
                    link_text: "{{#playground file.rs}}",
                },
                Link {
                    start_index: 50,
                    end_index: 74,
                    link_type: LinkType::Playground(PathBuf::from("test.rs"), vec![]),
                    link_text: "{{#playground test.rs }}",
                },
            ]
        );
    }

    #[test]
    fn test_find_links_with_special_characters() {
        let s = "Some random text with {{#playground foo-bar\\baz/_c++.rs}}...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");

        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 57,
                link_type: LinkType::Playground(PathBuf::from("foo-bar\\baz/_c++.rs"), vec![]),
                link_text: "{{#playground foo-bar\\baz/_c++.rs}}",
            },]
        );
    }

    #[test]
    fn test_find_links_with_range() {
        let s = "Some random text with {{#include file.rs:10:20}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 48,
                link_type: LinkType::Include(
                    PathBuf::from("file.rs"),
                    RangeOrAnchor::Range(LineRange::from(9..20))
                ),
                link_text: "{{#include file.rs:10:20}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_line_number() {
        let s = "Some random text with {{#include file.rs:10}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 45,
                link_type: LinkType::Include(
                    PathBuf::from("file.rs"),
                    RangeOrAnchor::Range(LineRange::from(9..10))
                ),
                link_text: "{{#include file.rs:10}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_from_range() {
        let s = "Some random text with {{#include file.rs:10:}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 46,
                link_type: LinkType::Include(
                    PathBuf::from("file.rs"),
                    RangeOrAnchor::Range(LineRange::from(9..))
                ),
                link_text: "{{#include file.rs:10:}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_to_range() {
        let s = "Some random text with {{#include file.rs::20}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 46,
                link_type: LinkType::Include(
                    PathBuf::from("file.rs"),
                    RangeOrAnchor::Range(LineRange::from(..20))
                ),
                link_text: "{{#include file.rs::20}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_full_range() {
        let s = "Some random text with {{#include file.rs::}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 44,
                link_type: LinkType::Include(
                    PathBuf::from("file.rs"),
                    RangeOrAnchor::Range(LineRange::from(..))
                ),
                link_text: "{{#include file.rs::}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_no_range_specified() {
        let s = "Some random text with {{#include file.rs}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 42,
                link_type: LinkType::Include(
                    PathBuf::from("file.rs"),
                    RangeOrAnchor::Range(LineRange::from(..))
                ),
                link_text: "{{#include file.rs}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_space_in_path() {
        let s = "Some random text with {{#include \"fila a.md\"}}...";
        let res = find_links(s).collect::<Vec<_>>();
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 46,
                link_type: LinkType::Include(
                    PathBuf::from("fila a.md"),
                    RangeOrAnchor::Range(LineRange::from(..)),
                ),
                link_text: "{{#include \"fila a.md\"}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_space_in_path_and_range() {
        let s = "Some random text with {{#include \"fila a.md\":1:2}}...";
        let res = find_links(s).collect::<Vec<_>>();
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 50,
                link_type: LinkType::Include(
                    PathBuf::from("fila a.md"),
                    RangeOrAnchor::Range(LineRange::from(0..2)),
                ),
                link_text: "{{#include \"fila a.md\":1:2}}",
            }]
        );
    }

    #[test]
    fn test_find_links_rustdoc_include_with_space_in_path() {
        let s = "Some random text with {{#rustdoc_include \"fila a.rs\"}}...";
        let res = find_links(s).collect::<Vec<_>>();
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 54,
                link_type: LinkType::RustdocInclude(
                    PathBuf::from("fila a.rs"),
                    RangeOrAnchor::Range(LineRange::from(..)),
                ),
                link_text: "{{#rustdoc_include \"fila a.rs\"}}",
            }]
        );
    }

    #[test]
    fn test_find_links_rustdoc_include_with_space_in_path_and_range() {
        let s = "Some random text with {{#rustdoc_include \"fila a.rs\":1:5}}...";
        let res = find_links(s).collect::<Vec<_>>();
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 58,
                link_type: LinkType::RustdocInclude(
                    PathBuf::from("fila a.rs"),
                    RangeOrAnchor::Range(LineRange::from(0..5)),
                ),
                link_text: "{{#rustdoc_include \"fila a.rs\":1:5}}",
            }]
        );
    }

    #[test]
    fn test_find_links_playground_with_space_in_path() {
        let s = "Some random text with {{#playground \"fila a.rs\" editable no_run}}...";
        let res = find_links(s).collect::<Vec<_>>();
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 65,
                link_type: LinkType::Playground(
                    PathBuf::from("fila a.rs"),
                    vec!["editable", "no_run"],
                ),
                link_text: "{{#playground \"fila a.rs\" editable no_run}}",
            }]
        );
    }

    #[test]
    fn test_find_links_unclosed_quote() {
        let s = "Some random text with {{#include \"unclosed.md}}...";
        let res = find_links(s).collect::<Vec<_>>();
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 47,
                link_type: LinkType::Invalid("unclosed quote in path".to_string()),
                link_text: "{{#include \"unclosed.md}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_escaped_quotes_in_path() {
        let s = "Some random text with {{#include \"file \\\"name\\\".md\"}}...";
        let res = find_links(s).collect::<Vec<_>>();
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 53,
                link_type: LinkType::Include(
                    PathBuf::from("file \"name\".md"),
                    RangeOrAnchor::Range(LineRange::from(..)),
                ),
                link_text: "{{#include \"file \\\"name\\\".md\"}}",
            }]
        );
    }

    #[test]
    fn test_find_links_with_anchor() {
        let s = "Some random text with {{#include file.rs:anchor}}...";
        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");
        assert_eq!(
            res,
            vec![Link {
                start_index: 22,
                end_index: 49,
                link_type: LinkType::Include(
                    PathBuf::from("file.rs"),
                    RangeOrAnchor::Anchor(String::from("anchor"))
                ),
                link_text: "{{#include file.rs:anchor}}",
            }]
        );
    }

    #[test]
    fn test_find_links_escaped_link() {
        let s = "Some random text with escaped playground \\{{#playground file.rs editable}} ...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");

        assert_eq!(
            res,
            vec![Link {
                start_index: 41,
                end_index: 74,
                link_type: LinkType::Escaped,
                link_text: "\\{{#playground file.rs editable}}",
            }]
        );
    }

    #[test]
    fn test_find_playgrounds_with_properties() {
        let s = "Some random text with escaped playground {{#playground file.rs editable }} and some \
                 more\n text {{#playground my.rs editable no_run should_panic}} ...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");
        assert_eq!(
            res,
            vec![
                Link {
                    start_index: 41,
                    end_index: 74,
                    link_type: LinkType::Playground(PathBuf::from("file.rs"), vec!["editable"]),
                    link_text: "{{#playground file.rs editable }}",
                },
                Link {
                    start_index: 95,
                    end_index: 145,
                    link_type: LinkType::Playground(
                        PathBuf::from("my.rs"),
                        vec!["editable", "no_run", "should_panic"],
                    ),
                    link_text: "{{#playground my.rs editable no_run should_panic}}",
                },
            ]
        );
    }

    #[test]
    fn test_find_all_link_types() {
        let s = "Some random text with escaped playground {{#include file.rs}} and \\{{#contents are \
                 insignifficant in escaped link}} some more\n text  {{#playground my.rs editable \
                 no_run should_panic}} ...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {res:?}\n");
        assert_eq!(res.len(), 3);
        assert_eq!(
            res[0],
            Link {
                start_index: 41,
                end_index: 61,
                link_type: LinkType::Include(
                    PathBuf::from("file.rs"),
                    RangeOrAnchor::Range(LineRange::from(..))
                ),
                link_text: "{{#include file.rs}}",
            }
        );
        assert_eq!(
            res[1],
            Link {
                start_index: 66,
                end_index: 115,
                link_type: LinkType::Escaped,
                link_text: "\\{{#contents are insignifficant in escaped link}}",
            }
        );
        assert_eq!(
            res[2],
            Link {
                start_index: 133,
                end_index: 183,
                link_type: LinkType::Playground(
                    PathBuf::from("my.rs"),
                    vec!["editable", "no_run", "should_panic"]
                ),
                link_text: "{{#playground my.rs editable no_run should_panic}}",
            }
        );
    }

    #[test]
    fn parse_without_colon_includes_all() {
        let link_type = parse_include_path("arbitrary").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(RangeFull))
            )
        );
    }

    #[test]
    fn parse_with_nothing_after_colon_includes_all() {
        let link_type = parse_include_path("arbitrary:").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(RangeFull))
            )
        );
    }

    #[test]
    fn parse_with_two_colons_includes_all() {
        let link_type = parse_include_path("arbitrary::").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(RangeFull))
            )
        );
    }

    #[test]
    fn parse_with_garbage_after_two_colons_includes_all() {
        let link_type = parse_include_path("arbitrary::NaN").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(RangeFull))
            )
        );
    }

    #[test]
    fn parse_with_one_number_after_colon_only_that_line() {
        let link_type = parse_include_path("arbitrary:5").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(4..5))
            )
        );
    }

    #[test]
    fn parse_with_one_based_start_becomes_zero_based() {
        let link_type = parse_include_path("arbitrary:1").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(0..1))
            )
        );
    }

    #[test]
    fn parse_with_zero_based_start_stays_zero_based_but_is_probably_an_error() {
        let link_type = parse_include_path("arbitrary:0").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(0..1))
            )
        );
    }

    #[test]
    fn parse_start_only_range() {
        let link_type = parse_include_path("arbitrary:5:").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(4..))
            )
        );
    }

    #[test]
    fn parse_start_with_garbage_interpreted_as_start_only_range() {
        let link_type = parse_include_path("arbitrary:5:NaN").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(4..))
            )
        );
    }

    #[test]
    fn parse_end_only_range() {
        let link_type = parse_include_path("arbitrary::5").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(..5))
            )
        );
    }

    #[test]
    fn parse_start_and_end_range() {
        let link_type = parse_include_path("arbitrary:5:10").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(4..10))
            )
        );
    }

    #[test]
    fn parse_with_negative_interpreted_as_anchor() {
        let link_type = parse_include_path("arbitrary:-5").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Anchor("-5".to_string())
            )
        );
    }

    #[test]
    fn parse_with_floating_point_interpreted_as_anchor() {
        let link_type = parse_include_path("arbitrary:-5.7").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Anchor("-5.7".to_string())
            )
        );
    }

    #[test]
    fn parse_with_anchor_followed_by_colon() {
        let link_type = parse_include_path("arbitrary:some-anchor:this-gets-ignored").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Anchor("some-anchor".to_string())
            )
        );
    }

    #[test]
    fn parse_with_more_than_three_colons_ignores_everything_after_third_colon() {
        let link_type = parse_include_path("arbitrary:5:10:17:anything:").unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary"),
                RangeOrAnchor::Range(LineRange::from(4..10))
            )
        );
    }

    #[test]
    fn parse_quoted_path_without_colon_includes_all() {
        let link_type = parse_include_path(r#""arbitrary file name.md""#).unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary file name.md"),
                RangeOrAnchor::Range(LineRange::from(RangeFull))
            )
        );
    }

    #[test]
    fn parse_quoted_path_with_range() {
        let link_type = parse_include_path(r#""arbitrary file name.md":5:10"#).unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary file name.md"),
                RangeOrAnchor::Range(LineRange::from(4..10))
            )
        );
    }

    #[test]
    fn parse_quoted_path_with_anchor() {
        let link_type = parse_include_path(r#""arbitrary file name.md":some-anchor"#).unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("arbitrary file name.md"),
                RangeOrAnchor::Anchor("some-anchor".to_string())
            )
        );
    }

    #[test]
    fn parse_quoted_path_with_escapes() {
        let link_type = parse_include_path(r#""file \"name\".md""#).unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("file \"name\".md"),
                RangeOrAnchor::Range(LineRange::from(RangeFull))
            )
        );
    }

    #[test]
    fn parse_quoted_path_preserves_whitespace() {
        let link_type = parse_include_path(r#""  spaces   ""#).unwrap();
        assert_eq!(
            link_type,
            LinkType::Include(
                PathBuf::from("  spaces   "),
                RangeOrAnchor::Range(LineRange::from(RangeFull))
            )
        );
    }

    #[test]
    fn parse_quoted_path_unclosed_returns_error() {
        assert_eq!(
            parse_include_path(r#""unclosed.md"#),
            Some(LinkType::Invalid("unclosed quote in path".to_string()))
        );
        assert_eq!(
            parse_rustdoc_include_path(r#""unclosed.rs"#),
            Some(LinkType::Invalid("unclosed quote in path".to_string()))
        );
        assert_eq!(
            parse_playground_args(r#""unclosed.rs"#),
            Some(LinkType::Invalid("unclosed quote in path".to_string()))
        );
    }

    #[test]
    fn test_replace_all_unclosed_quote() {
        let start = "Some text with {{#include \"unclosed.md}} here.";
        let mut chapter_title = "test".to_owned();
        let res = replace_all(start, "", "", 0, &mut chapter_title);
        assert_eq!(res, start);
    }

    // ========================================================================
    // DRY / ARCHITECTURE: A/B unit-tests
    // PR mdBook#3163 — reviewer GuillaumeGomez flagged copy-paste between
    // parse_include_path and parse_rustdoc_include_path. These tests
    // verify the architectural property that all three parsers behave
    // consistently and quantify the duplication.
    // ========================================================================

    // --- 1. Consistency: parse_include_path vs parse_rustdoc_include_path ---

    #[test]
    fn arch_consistency_quoted_plain_path() {
        let input = r#""my file.md""#;
        let inc = parse_include_path(input).unwrap();
        let rust = parse_rustdoc_include_path(input).unwrap();

        match (&inc, &rust) {
            (LinkType::Include(p1, r1), LinkType::RustdocInclude(p2, r2)) => {
                assert_eq!(p1, p2, "paths should match for identical quoted input");
                assert_eq!(r1, r2, "range/anchor should match");
            }
            _ => panic!("expected Include and RustdocInclude, got {inc:?} vs {rust:?}"),
        }
    }

    #[test]
    fn arch_consistency_quoted_path_with_range() {
        let input = r#""my file.md":5:10"#;
        let inc = parse_include_path(input).unwrap();
        let rust = parse_rustdoc_include_path(input).unwrap();

        match (&inc, &rust) {
            (LinkType::Include(p1, r1), LinkType::RustdocInclude(p2, r2)) => {
                assert_eq!(p1, p2);
                assert_eq!(r1, r2);
            }
            _ => panic!("type mismatch: {inc:?} vs {rust:?}"),
        }
    }

    #[test]
    fn arch_consistency_quoted_path_with_anchor() {
        let input = r#""my file.md":my-anchor"#;
        let inc = parse_include_path(input).unwrap();
        let rust = parse_rustdoc_include_path(input).unwrap();

        match (&inc, &rust) {
            (LinkType::Include(p1, r1), LinkType::RustdocInclude(p2, r2)) => {
                assert_eq!(p1, p2);
                assert_eq!(r1, r2);
            }
            _ => panic!("type mismatch: {inc:?} vs {rust:?}"),
        }
    }

    #[test]
    fn arch_consistency_unquoted_path() {
        let input = "plain_path.md:5:10";
        let inc = parse_include_path(input).unwrap();
        let rust = parse_rustdoc_include_path(input).unwrap();

        match (&inc, &rust) {
            (LinkType::Include(p1, r1), LinkType::RustdocInclude(p2, r2)) => {
                assert_eq!(p1, p2);
                assert_eq!(r1, r2);
            }
            _ => panic!("type mismatch: {inc:?} vs {rust:?}"),
        }
    }

    /// Property test: for any input, parse_include_path and
    /// parse_rustdoc_include_path must return the same variant *shape*.
    /// The only allowed difference is the variant tag.
    #[test]
    fn arch_consistency_variant_shape_across_diverse_inputs() {
        let inputs = [
            r#""quoted file.md""#,
            r#""quoted file.md":1:5"#,
            r#""quoted file.md":anchor""#,
            r#""unclosed.md"#,
            "plain.md:1:5",
            "plain.md",
            "",
            "   ",
            r#""file with \"escaped\" quotes.md""#,
        ];

        for input in &inputs {
            let inc = parse_include_path(input);
            let rust = parse_rustdoc_include_path(input);

            assert_eq!(
                inc.is_some(),
                rust.is_some(),
                "Some/None mismatch for input: {input:?}"
            );

            if let (Some(inc_v), Some(rust_v)) = (inc, rust) {
                let inc_is_invalid = matches!(inc_v, LinkType::Invalid(_));
                let rust_is_invalid = matches!(rust_v, LinkType::Invalid(_));
                assert_eq!(
                    inc_is_invalid, rust_is_invalid,
                    "Invalid/non-Invalid mismatch for input: {input:?}"
                );

                if inc_is_invalid {
                    if let (LinkType::Invalid(e1), LinkType::Invalid(e2)) = (inc_v, rust_v) {
                        assert_eq!(e1, e2, "error message differs for input: {input:?}");
                    }
                }
            }
        }
    }

    // --- 2. Playground: quoted path parsing with props ---

    #[test]
    fn arch_playground_quoted_path_with_props() {
        let input = r#""my file.rs" prop1 prop2"#;
        let result = parse_playground_args(input).unwrap();

        match result {
            LinkType::Playground(path, props) => {
                assert_eq!(path, PathBuf::from("my file.rs"));
                assert_eq!(props, vec!["prop1", "prop2"]);
            }
            _ => panic!("expected Playground variant, got {result:?}"),
        }
    }

    #[test]
    fn arch_playground_quoted_path_no_props() {
        let input = r#""my file.rs""#;
        let result = parse_playground_args(input).unwrap();

        match result {
            LinkType::Playground(path, props) => {
                assert_eq!(path, PathBuf::from("my file.rs"));
                assert!(props.is_empty(), "props should be empty, got {props:?}");
            }
            _ => panic!("expected Playground variant, got {result:?}"),
        }
    }

    #[test]
    fn arch_playground_quoted_path_with_escapes_and_props() {
        let input = r#""file \"name\".rs" editable"#;
        let result = parse_playground_args(input).unwrap();

        match result {
            LinkType::Playground(path, props) => {
                assert_eq!(path, PathBuf::from(r#"file "name".rs"#));
                assert_eq!(props, vec!["editable"]);
            }
            _ => panic!("expected Playground variant, got {result:?}"),
        }
    }

    // --- 3. Unclosed quote: all three functions return LinkType::Invalid ---

    #[test]
    fn arch_unclosed_quote_all_three_parsers_return_invalid() {
        let inputs = [
            r#""unclosed"#,
            r#""unclosed with spaces.md"#,
            r#""unclosed\"escaped.rs"#,
            r#""unclosed trailing backslash\"#,
        ];

        for input in &inputs {
            let inc = parse_include_path(input);
            let rust = parse_rustdoc_include_path(input);
            let play = parse_playground_args(input);

            let assert_invalid = |lt: Option<LinkType<'_>>, label: &str| match lt {
                Some(LinkType::Invalid(msg)) => {
                    assert!(
                        msg.contains("unclosed"),
                        "{label}: error should mention 'unclosed', got: {msg}"
                    );
                }
                other => {
                    panic!("{label}: expected Some(Invalid), got {other:?} for input {input:?}")
                }
            };

            assert_invalid(inc.clone(), "parse_include_path");
            assert_invalid(rust.clone(), "parse_rustdoc_include_path");
            assert_invalid(play.clone(), "parse_playground_args");

            if let (
                Some(LinkType::Invalid(e1)),
                Some(LinkType::Invalid(e2)),
                Some(LinkType::Invalid(e3)),
            ) = (inc.clone(), rust.clone(), play.clone())
            {
                assert_eq!(e1, e2, "include vs rustdoc error mismatch for {input:?}");
                assert_eq!(e2, e3, "rustdoc vs playground error mismatch for {input:?}");
            }
        }
    }

    // --- 4. Unquoted fallback: backward compat for all three ---

    #[test]
    fn arch_unquoted_fallback_include() {
        let result = parse_include_path("plain.md:1:5").unwrap();
        match result {
            LinkType::Include(path, RangeOrAnchor::Range(range)) => {
                assert_eq!(path, PathBuf::from("plain.md"));
                assert_eq!(range, LineRange::from(0..5));
            }
            _ => panic!("expected Include with Range, got {result:?}"),
        }
    }

    #[test]
    fn arch_unquoted_fallback_rustdoc_include() {
        let result = parse_rustdoc_include_path("plain.rs:1:5").unwrap();
        match result {
            LinkType::RustdocInclude(path, RangeOrAnchor::Range(range)) => {
                assert_eq!(path, PathBuf::from("plain.rs"));
                assert_eq!(range, LineRange::from(0..5));
            }
            _ => panic!("expected RustdocInclude with Range, got {result:?}"),
        }
    }

    #[test]
    fn arch_unquoted_fallback_playground() {
        let result = parse_playground_args("plain.rs editable no_run").unwrap();
        match result {
            LinkType::Playground(path, props) => {
                assert_eq!(path, PathBuf::from("plain.rs"));
                assert_eq!(props, vec!["editable", "no_run"]);
            }
            _ => panic!("expected Playground, got {result:?}"),
        }
    }

    #[test]
    fn arch_unquoted_fallback_empty_input_returns_none() {
        assert_eq!(parse_include_path(""), None);
        assert_eq!(parse_rustdoc_include_path(""), None);
        assert_eq!(parse_playground_args(""), None);
    }

    #[test]
    fn arch_unquoted_fallback_whitespace_only_returns_none() {
        assert_eq!(parse_include_path("   "), None);
        assert_eq!(parse_rustdoc_include_path("   "), None);
        assert_eq!(parse_playground_args("   "), None);
    }

    /// Cross-parser unquoted consistency: for a plain filename without colon,
    /// Include, RustdocInclude, and Playground must all produce the same path.
    #[test]
    fn arch_unquoted_fallback_cross_parser_no_colon() {
        let input = "plain_file.md";
        let inc = parse_include_path(input).unwrap();
        let rust = parse_rustdoc_include_path(input).unwrap();
        let play = parse_playground_args(input).unwrap();

        match (&inc, &rust, &play) {
            (
                LinkType::Include(pi, _),
                LinkType::RustdocInclude(pr, _),
                LinkType::Playground(pp, props),
            ) => {
                assert_eq!(pi, pr, "Include/Rustdoc path mismatch");
                assert_eq!(pi, pp, "Include/Playground path mismatch");
                assert!(
                    props.is_empty(),
                    "Playground props should be empty for {input:?}"
                );
            }
            _ => panic!("unexpected variant combination: {inc:?}, {rust:?}, {play:?}"),
        }
    }

    // --- 5. Copy-paste detector: architectural assertion ---

    /// Returns the number of lines that differ between two function bodies,
    /// excluding lines that differ only in the variant name or function name.
    fn count_meaningful_diffs(a: &str, b: &str) -> usize {
        let a_lines: Vec<&str> = a.lines().collect();
        let b_lines: Vec<&str> = b.lines().collect();

        let max_len = a_lines.len().max(b_lines.len());
        let mut diffs = 0;

        for i in 0..max_len {
            let la = a_lines.get(i).copied().unwrap_or("");
            let lb = b_lines.get(i).copied().unwrap_or("");

            if la == lb {
                continue;
            }

            let normalize = |s: &str| {
                s.replace("parse_rustdoc_include_path", "PARSE_FN")
                    .replace("parse_include_path", "PARSE_FN")
                    .replace("RustdocInclude", "VARIANT")
                    .replace("Include", "VARIANT")
            };

            if normalize(la) == normalize(lb) {
                continue;
            }

            diffs += 1;
        }

        diffs
    }

    #[test]
    fn arch_copy_paste_detector_include_vs_rustdoc() {
        let include_src = stringify!(parse_include_path);
        let rustdoc_src = stringify!(parse_rustdoc_include_path);

        let diffs = count_meaningful_diffs(include_src, rustdoc_src);

        assert!(
            diffs <= 3,
            "parse_include_path and parse_rustdoc_include_path have \
             {diffs} meaningful line differences (threshold: 3). These \
             functions should share a common helper to avoid copy-paste \
             duplication."
        );
    }

    /// Companion: all three parsers share parse_quoted_path — behavioral
    /// verification that the same quoted string yields the same extracted
    /// path regardless of which top-level parser is used.
    #[test]
    fn arch_shared_quoted_path_helper_consistency() {
        let inputs = [
            r#""path with spaces.md""#,
            r#""path with spaces.md":1:10"#,
            r#""path with spaces.md":anchor""#,
            r#""escaped \"quote\".md""#,
        ];

        for input in &inputs {
            let inc_path = extract_path_from_link_type(parse_include_path(input));
            let rust_path = extract_path_from_link_type(parse_rustdoc_include_path(input));
            let play_path = extract_path_from_link_type(parse_playground_args(input));

            assert_eq!(
                inc_path, rust_path,
                "Include vs Rustdoc path mismatch for {input:?}"
            );
            assert_eq!(
                inc_path, play_path,
                "Include vs Playground path mismatch for {input:?}"
            );
        }
    }

    /// Helper: extract the PathBuf from any LinkType variant that carries one.
    fn extract_path_from_link_type(lt: Option<LinkType<'_>>) -> Option<PathBuf> {
        match lt? {
            LinkType::Include(p, _) => Some(p),
            LinkType::RustdocInclude(p, _) => Some(p),
            LinkType::Playground(p, _) => Some(p),
            LinkType::Invalid(_) => None,
            LinkType::Escaped => None,
            LinkType::Title(_) => None,
        }
    }

    // ========================================================================
    // ESCAPE SEMANTICS — A/B architectural unit-tests for parse_quoted_path
    // Bug: backslash was a universal escape. Only `\\` and `\"` should be
    // escape sequences; all other `\X` must preserve the backslash.
    // ========================================================================

    #[test]
    fn escape_semantics_double_backslash() {
        // A (bug):  `\\` -> `\` (correct result, wrong rule — blind consumption)
        // B (correct): `\\` -> `\` (explicit escape-sequence rule)
        let result = parse_quoted_path(r#""folder\\file.md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"folder\file.md"));
    }

    #[test]
    fn escape_semantics_backslash_t_preserves_backslash() {
        // A (bug):  `\t` -> `t`  (backslash dropped)
        // B (correct): `\t` -> `\t` (backslash + 't', NOT a tab)
        let result = parse_quoted_path(r#""path\to\target.md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"path\to\target.md"));
        let path_str = path.to_str().unwrap();
        assert!(
            !path_str.contains('\t'),
            "backslash-t must not become a tab"
        );
        assert!(
            path_str.contains(r"\t"),
            "backslash before 't' must be preserved"
        );
    }

    #[test]
    fn escape_semantics_backslash_n_preserves_backslash() {
        // A (bug):  `\n` -> `n`  (backslash dropped)
        // B (correct): `\n` -> `\n` (backslash + 'n', NOT a newline)
        let result = parse_quoted_path(r#""dir\new\file.md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"dir\new\file.md"));
        let path_str = path.to_str().unwrap();
        assert!(
            !path_str.contains('\n'),
            "backslash-n must not become a newline"
        );
        assert!(
            path_str.contains(r"\n"),
            "backslash before 'n' must be preserved"
        );
    }

    #[test]
    fn escape_semantics_backslash_c_preserves_backslash() {
        // A (bug):  `\c` -> `c` — the original Windows bug: `C:\folder` -> `C:folder`
        // B (correct): `\c` -> `\c` (backslash preserved)
        let result = parse_quoted_path(r#""C:\code\test.md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"C:\code\test.md"));
        let path_str = path.to_str().unwrap();
        assert!(
            path_str.contains(r"\c"),
            "backslash before 'c' must be preserved"
        );
    }

    #[test]
    fn escape_semantics_escaped_quote() {
        // A (bug):  `\"` -> `"` (accidentally correct via blind consumption)
        // B (correct): `\"` -> `"` (explicitly defined escape sequence)
        let result = parse_quoted_path(r#""file \"name\".md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r#"file "name".md"#));
    }

    #[test]
    fn escape_semantics_escaped_quote_does_not_close_string() {
        // A (bug): `\"` consumed as escape -> `"` pushed, scanning continues
        // B (correct): `\"` -> `"`, embedded quote must NOT close the string
        let result = parse_quoted_path(r#""a\"b.md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r#"a"b.md"#));
    }

    #[test]
    fn escape_semantics_trailing_backslash_is_error() {
        // A (bug):  `"path\"` — `\` consumes closing `"`, scanning continues -> error
        // B (correct): `"path\"` — `\"` is escaped quote, string not closed -> error
        let result = parse_quoted_path(r#""path\""#);
        assert!(
            result.is_err(),
            "trailing backslash must not silently produce a path"
        );
        assert_eq!(result.unwrap_err(), "unclosed quote in path");
    }

    #[test]
    fn escape_semantics_trailing_doubled_backslash_closes() {
        // A (bug):  `"path\\"` — `\\` -> `\`, then `"` closes. Path = `path\`.
        // B (correct): same result, but via principled rule
        let result = parse_quoted_path(r#""path\\"\"#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"path\"));
    }

    #[test]
    fn escape_semantics_backslash_unicode_preserves_both() {
        // A (bug):  `\α` -> `α` (backslash dropped)
        // B (correct): `\α` -> `\α` (both backslash and Unicode char preserved)
        let result = parse_quoted_path(r#""\α\β.md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"\α\β.md"));
        let path_str = path.to_str().unwrap();
        assert!(
            path_str.starts_with('\\'),
            "backslash before Unicode must be preserved"
        );
        assert!(
            path_str.contains('α'),
            "Unicode character must be preserved"
        );
        assert!(
            path_str.contains('β'),
            "Unicode character must be preserved"
        );
    }

    #[test]
    fn escape_semantics_backslash_emoji_preserves_both() {
        // A (bug):  `\🦀` -> `🦀` (backslash dropped)
        // B (correct): `\🦀` -> `\🦀` (backslash + emoji both preserved)
        let result = parse_quoted_path(r#""\🦀\test.md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"\🦀\test.md"));
        let path_str = path.to_str().unwrap();
        assert!(
            path_str.starts_with('\\'),
            "backslash before emoji must be preserved"
        );
        assert!(path_str.contains('🦀'), "emoji must be preserved");
    }

    #[test]
    fn escape_semantics_empty_quotes_yield_empty_path() {
        // A (bug):  `""` -> empty path (accidentally correct, loop exits on `"`)
        // B (correct): `""` -> empty path (valid edge case)
        let result = parse_quoted_path(r#""""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(""));
    }

    #[test]
    fn escape_semantics_mixed_escapes_only_defined_sequences() {
        // A (bug):  all backslashes consumed -> `a\bcnd\e"f`
        // B (correct): only `\\`->`\` and `\"`->`"`; `\t`->`\t`, `\n`->`\n` preserved
        let result = parse_quoted_path(r#""a\\b\tc\nd\\e\"f""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r#"a\b\tc\nd\e"f"#));
        let path_str = path.to_str().unwrap();
        assert!(!path_str.contains('\t'), r"no tab from \t");
        assert!(!path_str.contains('\n'), r"no newline from \n");
        assert!(path_str.contains(r"\t"), "literal backslash-t preserved");
        assert!(path_str.contains(r"\n"), "literal backslash-n preserved");
        assert!(path_str.contains('"'), "escaped quote became literal quote");
    }

    #[test]
    fn escape_semantics_windows_absolute_path() {
        // A (bug):  every `\X` drops backslash -> `C:Userstestfile.md`
        // B (correct): all single backslashes preserved -> `C:\Users\test\file.md`
        let result = parse_quoted_path(r#""C:\Users\test\file.md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"C:\Users\test\file.md"));
        let path_str = path.to_str().unwrap();
        assert!(path_str.contains(r"\U"), "backslash before 'U' preserved");
        assert!(path_str.contains(r"\t"), "backslash before 't' preserved");
        assert!(path_str.contains(r"\f"), "backslash before 'f' preserved");
    }

    #[test]
    fn escape_semantics_consecutive_unknown_escapes() {
        // A (bug):  `\a\b\c\d` -> `abcd` (all backslashes dropped)
        // B (correct): `\a\b\c\d` -> `\a\b\c\d` (all backslashes preserved)
        let result = parse_quoted_path(r#""\a\b\c\d.md""#);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"\a\b\c\d.md"));
        let path_str = path.to_str().unwrap();
        assert_eq!(
            path_str.matches('\\').count(),
            4,
            "all four backslashes must be preserved for non-defined escapes"
        );
    }

    #[test]
    fn escape_semantics_remaining_input_after_close() {
        // A (bug):  wrong end_index after escaped-quote consumption
        // B (correct): `\\`->`\` then `"` closes, remainder = `:5:10`
        let result = parse_quoted_path(r#""path\\file.md":5:10"#);
        assert!(result.is_ok());
        let (path, rest) = result.unwrap();
        assert_eq!(path, PathBuf::from(r"path\file.md"));
        assert_eq!(rest, ":5:10");
    }

    #[test]
    fn escape_semantics_escaped_quote_in_middle_preserves_rest() {
        // A (bug):  `\"` consumes next char, potential shift of end_index
        // B (correct): `\"` -> `"`, scanning continues, rest correct
        let result = parse_quoted_path(r#""a\"b\\c.md":2:4"#);
        assert!(result.is_ok());
        let (path, rest) = result.unwrap();
        assert_eq!(path, PathBuf::from(r#"a"b\c.md"#));
        assert_eq!(rest, ":2:4");
    }

    // ========================================================================
    // CROSS-PLATFORM PATHS — A/B architectural unit-tests
    // ========================================================================

    #[test]
    fn cross_platform_windows_absolute_path() {
        let input = r#""C:\Users\name\file.md""#;
        let buggy = PathBuf::from("C:Usersnamefile.md");
        let correct = PathBuf::from(r"C:\Users\name\file.md");
        let (parsed, rest) = parse_quoted_path(input).expect("should parse");
        assert_ne!(parsed, buggy, "A — buggy: backslashes swallowed");
        assert_eq!(parsed, correct, "B — correct: backslashes preserved");
        assert_eq!(rest, "");
    }

    #[test]
    fn cross_platform_unix_path_with_spaces() {
        let input = r#""/usr/local/my docs/file.md""#;
        let correct = PathBuf::from("/usr/local/my docs/file.md");
        let (parsed, rest) = parse_quoted_path(input).expect("should parse");
        assert_eq!(parsed, correct);
        assert_eq!(rest, "");
    }

    #[test]
    fn cross_platform_mixed_separators() {
        let input = r#""C:/Users/name\docs/file.md""#;
        let buggy = PathBuf::from("C:/Users/namedocs/file.md");
        let correct = PathBuf::from(r"C:/Users/name\docs/file.md");
        let (parsed, _) = parse_quoted_path(input).expect("should parse mixed separators");
        assert_ne!(parsed, buggy, "A — buggy: backslash eaten in mixed path");
        assert_eq!(parsed, correct, "B — correct: both separators preserved");
    }

    #[test]
    fn cross_platform_colons_in_name() {
        let input = r#""file:with:colons.md""#;
        let correct = PathBuf::from("file:with:colons.md");
        let (parsed, rest) = parse_quoted_path(input).expect("should parse colons in quotes");
        assert_eq!(parsed, correct, "colons inside quotes are literal");
        assert_eq!(rest, "", "no trailing range/anchor");
    }

    #[test]
    fn cross_platform_relative_windows_path() {
        let input = r#"".\docs\my file.md""#;
        let buggy = PathBuf::from(".docsmy file.md");
        let correct = PathBuf::from(r".\docs\my file.md");
        let (parsed, _) = parse_quoted_path(input).expect("should parse relative Windows path");
        assert_ne!(parsed, buggy, "A — buggy: relative backslashes eaten");
        assert_eq!(
            parsed, correct,
            "B — correct: relative backslashes + space preserved"
        );
    }

    #[test]
    fn cross_platform_unicode_cyrillic_path() {
        let input = r#""папка\файл.md""#;
        let buggy = PathBuf::from("папкафайл.md");
        let correct = PathBuf::from(r"папка\файл.md");
        let (parsed, _) = parse_quoted_path(input).expect("should parse Cyrillic path");
        assert_ne!(parsed, buggy, "A — buggy: backslash eaten in Cyrillic path");
        assert_eq!(
            parsed, correct,
            "B — correct: Cyrillic + backslash preserved"
        );
    }

    #[test]
    fn cross_platform_escaped_quote_inside_path() {
        let input = r#""my\"quoted\file.md""#;
        let buggy = PathBuf::from(r#"my"quotedfile.md"#);
        let correct = PathBuf::from(r#"my"quoted\file.md"#);
        let (parsed, _) = parse_quoted_path(input).expect("should parse escaped quote");
        assert_ne!(parsed, buggy, "A — buggy: trailing backslash eaten");
        assert_eq!(
            parsed, correct,
            "B — correct: escaped quote + backslash separator"
        );
    }

    #[test]
    fn cross_platform_escaped_backslash_in_path() {
        let input = r#""C:\\Users\\file.md""#;
        let correct = PathBuf::from(r"C:\Users\file.md");
        let (parsed, _) = parse_quoted_path(input).expect("should parse escaped backslashes");
        assert_eq!(parsed, correct, "escaped \\\\ -> \\");
    }

    #[test]
    fn cross_platform_quoted_path_with_trailing_range() {
        let input = r#""my docs\file.md":10:20"#;
        let buggy_path = PathBuf::from("my docsfile.md");
        let correct_path = PathBuf::from(r"my docs\file.md");
        let (parsed, rest) = parse_quoted_path(input).expect("should parse quoted path + range");
        assert_ne!(
            parsed, buggy_path,
            "A — buggy: backslash eaten before range"
        );
        assert_eq!(parsed, correct_path, "B — correct: backslash preserved");
        assert_eq!(rest, ":10:20", "range preserved after closing quote");
    }
}
