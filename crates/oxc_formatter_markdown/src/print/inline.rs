//! Phrasing content.
//!
//! A paragraph is ONE `fill`: every whitespace that may break is a separator,
//! everything else (words, markers, code spans, hard breaks) is content.
//! [`Parts`] collects that shape; the node printers push into it.

use std::borrow::Cow;

use cow_utils::CowUtils;

use oxc_allocator::{Allocator, ArenaVec, StringBuilder};
use oxc_formatter_core::{
    Buffer, Format,
    builders::{
        dedent_to_root, hard_line_break, literal_line_break, mark_as_root, soft_line_break,
        soft_line_break_or_space, text,
    },
    write,
};
use oxc_markdown_parser::{
    Constructs, Span,
    ast::{CodeSpan, Emphasis, HardBreakKind, Inline, LinkKind, Strong},
    lexical, unicode,
};

use crate::{
    context::{MarkdownFormatContext, Raw},
    options::ProseWrap,
    print::{Mark, pairing},
};

use super::{
    MarkdownFormatter, backticks, block, format_with, join_pieces, link, text as words, with_depth,
};

/// A piece of fill content.
#[derive(Clone, Copy)]
pub enum Atom<'a> {
    Str(&'a str),
    /// A forced break inside content (backslash hard break, HTML comment lines), not a separator:
    /// a fill measures an item only up to a hard break, so the first two words after one always share a line,
    /// whatever the width (Prettier does the same).
    HardLine,
    /// A line break that keeps trailing whitespace and resumes at the current indention
    /// (two-space hard break).
    LiteralLine,
    /// A line break inside a verbatim inline construct (HTML, liquid, code span):
    /// the next line resumes at the container's content column, the only indention the parser strips.
    VerbatimLine,
}

impl<'a> Format<'a, MarkdownFormatContext<'a>> for Atom<'a> {
    fn fmt(&self, f: &mut MarkdownFormatter<'_, 'a>) {
        match self {
            Atom::Str(s) => write!(f, text(s)),
            Atom::LiteralLine => write!(f, mark_as_root(&literal_line_break())),
            // Containers mark their content column as root; a list item's extra alignment
            // (checkbox, tab width) is not stripped by the parser and must not be printed here
            Atom::VerbatimLine => write!(f, dedent_to_root(&hard_line_break())),
            Atom::HardLine => write!(f, hard_line_break()),
        }
    }
}

/// A fill separator.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sep {
    /// A break or nothing (a line break between CJ characters).
    SoftLine,
    Line,
    HardLine,
}

impl<'a> Format<'a, MarkdownFormatContext<'a>> for Sep {
    fn fmt(&self, f: &mut MarkdownFormatter<'_, 'a>) {
        match self {
            Sep::SoftLine => write!(f, soft_line_break()),
            Sep::Line => write!(f, soft_line_break_or_space()),
            Sep::HardLine => write!(f, hard_line_break()),
        }
    }
}

#[derive(Clone, Copy)]
enum Item<'a> {
    Atom(Atom<'a>),
    Sep(Sep),
}

/// The alternating content / separator list of one fill, flat (no per-word allocation);
/// runs of atoms are grouped when the fill is written.
pub struct Parts<'a> {
    items: ArenaVec<'a, Item<'a>>,
    /// Bytes of `flat` pushed so far: the printed offset of what comes next.
    len: u32,
    /// The delimiter nodes, opaque text and link texts pushed, by printed offset (`pairing`).
    pub marks: Vec<Mark>,
}

impl<'a> Item<'a> {
    /// The item as the parser will read it: a break is a newline, a soft line between CJ letters nothing.
    fn flat(&self) -> &'a str {
        match self {
            Item::Atom(Atom::Str(s)) => s,
            Item::Atom(_) | Item::Sep(Sep::HardLine) => "\n",
            Item::Sep(Sep::Line) => " ",
            Item::Sep(Sep::SoftLine) => "",
        }
    }
}

impl<'a> Parts<'a> {
    pub fn new(allocator: &'a Allocator) -> Self {
        Self { items: ArenaVec::new_in(&allocator), len: 0, marks: Vec::new() }
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn push_str(&mut self, s: &'a str) {
        if !s.is_empty() {
            self.push_atom(Atom::Str(s));
        }
    }

    pub fn push_atom(&mut self, atom: Atom<'a>) {
        self.push(Item::Atom(atom));
    }

    /// Adjacent separators collapse to the strongest (a text edge next to a soft break).
    pub fn push_sep(&mut self, sep: Sep) {
        if let Some(Item::Sep(existing)) = self.items.last_mut() {
            *existing = (*existing).max(sep);
        } else {
            self.push(Item::Sep(sep));
        }
    }

    fn push(&mut self, item: Item<'a>) {
        self.len += u32::try_from(item.flat().len()).unwrap_or(u32::MAX);
        self.items.push(item);
    }

    pub fn mark(&mut self, mark: Mark) {
        self.marks.push(mark);
    }

    /// The content as the parser will read it.
    fn flat(&self, allocator: &'a Allocator) -> &'a str {
        let mut out = StringBuilder::with_capacity_in(self.len as usize, allocator);
        for item in &self.items {
            out.push_str(item.flat());
        }
        out.into_str()
    }

    /// `value` split into lines, `separator` between them.
    pub fn push_lines(&mut self, value: &'a str, separator: Atom<'a>) {
        for (i, line) in value.split('\n').enumerate() {
            if i > 0 {
                self.push_atom(separator);
            }
            self.push_str(line);
        }
    }

    /// The content on one line (table cells): every separator is a space.
    pub fn into_str(self, allocator: &'a Allocator) -> &'a str {
        if let [Item::Atom(Atom::Str(s))] = self.items.as_slice() {
            return s;
        }
        let mut out = StringBuilder::new_in(allocator);
        for item in &self.items {
            match item {
                Item::Atom(Atom::Str(s)) => out.push_str(s),
                _ => out.push(' '),
            }
        }
        out.into_str()
    }

    pub fn write_fill(self, f: &mut MarkdownFormatter<'_, 'a>) {
        let mut fill = f.fill();
        let mut sep = Sep::Line;
        let mut run_start = 0;
        for (i, item) in self.items.iter().enumerate() {
            if let Item::Sep(next) = item {
                // A fill alternates content / separator;
                // a separator with no content before it gets an empty entry.
                let run = &self.items[run_start..i];
                fill.entry(&sep, &format_with(|f| write_atoms(run, f)));
                sep = *next;
                run_start = i + 1;
            }
        }
        if !self.items.is_empty() {
            let run = &self.items[run_start..];
            fill.entry(&sep, &format_with(|f| write_atoms(run, f)));
        }
        fill.finish();
    }
}

fn write_atoms<'a>(items: &[Item<'a>], f: &mut MarkdownFormatter<'_, 'a>) {
    for item in items {
        if let Item::Atom(atom) = item {
            atom.fmt(f);
        }
    }
}

/// Where the inline children hang; a few rules depend on the immediate parent.
#[derive(Clone, Copy)]
pub struct InlineParent {
    /// The children are a paragraph's (its first / last text gets trimmed).
    pub paragraph: bool,
    /// The parent is an `Emphasis` / `Strong`: its marker character as printed (`*` / `_`),
    /// the neighbor of the content's edge words.
    pub delimiter: Option<u8>,
    /// `Some(has_word_neighbor)` when the parent is a `Strong` (nested emphasis reads it).
    pub strong_neighbor: Option<bool>,
    /// The paragraph is the first block of a blockquote
    /// (`[!NOTE]` on its first line is an alert marker).
    pub first_in_container: bool,
    /// The paragraph follows a task list checkbox on its line: its first line is not a line start.
    pub after_checkbox: bool,
    /// Nothing follows the parent node on its source line (its last child ends the line).
    pub ends_line: bool,
}

impl Default for InlineParent {
    fn default() -> Self {
        Self {
            paragraph: false,
            delimiter: None,
            strong_neighbor: None,
            first_in_container: false,
            after_checkbox: false,
            ends_line: true,
        }
    }
}

/// Paragraph, heading or table-cell content as one fill.
pub fn write_inlines<'a>(
    children: &'a [Inline<'a>],
    parent: InlineParent,
    f: &mut MarkdownFormatter<'_, 'a>,
) {
    let raw = {
        let mut opaque = f.context().opaque_spans().borrow_mut();
        opaque.clear();
        collect_opaque_spans(children, &mut opaque);
        code_span_literal_runs(
            children,
            &opaque,
            &mut f.context().code_span_literal_runs().borrow_mut(),
            f,
        );
        parent.paragraph && wiki_link_risk(children, &opaque, f)
    };
    let collect = |initial: Raw, f: &mut MarkdownFormatter<'_, 'a>| {
        let mut parts = Parts::new(f.allocator());
        f.context().raw_text().set(initial);
        collect_inlines(children, parent, &mut parts, f);
        f.context().raw_text().set(Raw::No);
        parts
    };
    let consistent = |parts: &Parts<'a>, f: &MarkdownFormatter<'_, 'a>| {
        !parts.marks.iter().any(|m| matches!(m, Mark::Delimiter { .. }))
            || pairing::consistent(parts.flat(f.allocator()), &parts.marks)
    };
    let initial = if raw { Raw::Text } else { Raw::No };
    let mut parts = collect(initial, f);
    // Normalized markers must pair as the source's did; otherwise the markers are printed as written,
    // and if joined lines still pair differently, the text as written too
    if !consistent(&parts, f) {
        f.context().literal_markers().set(true);
        parts = collect(initial, f);
        if !consistent(&parts, f) {
            parts = collect(Raw::Text, f);
        }
        f.context().literal_markers().set(false);
    }
    parts.write_fill(f);
}

/// For every code span, the backtick run lengths (bit `n - 1` for a run of `n`) that occur before it
/// in the content as literal text (outside code spans, HTML, autolinks, math, liquid, whose backticks
/// never delimit): a printed fence of such a length would pair with the run and move the span.
fn code_span_literal_runs<'a>(
    children: &'a [Inline<'a>],
    opaque: &[(Span, bool)],
    out: &mut Vec<(u32, u64)>,
    f: &MarkdownFormatter<'_, 'a>,
) {
    out.clear();
    let (Some(first), Some(last)) = (children.first(), children.last()) else { return };
    let source = f.context().source_text();
    if !source.bytes_contain(first.span().start, last.span().end, b'`') {
        return;
    }
    let mut mask = 0;
    let mut at = first.span().start;
    for &(span, is_code) in opaque {
        if span.start > at {
            mask |= run_mask(source.slice_range(at, span.start).as_bytes(), b'`');
        }
        if is_code {
            out.push((span.start, mask));
        }
        at = at.max(span.end);
    }
}

/// Spans whose backticks are not code span delimiters, in source order; `true` marks code spans.
fn collect_opaque_spans<'a>(children: &'a [Inline<'a>], out: &mut Vec<(Span, bool)>) {
    for child in children {
        match child {
            Inline::CodeSpan(c) => out.push((c.span, true)),
            Inline::HtmlInline(_)
            | Inline::Autolink(_)
            | Inline::AutolinkLiteral(_)
            | Inline::MathSpan(_)
            | Inline::Liquid(_)
            | Inline::WikiLink(_) => out.push((child.span(), false)),
            Inline::Emphasis(e) => collect_opaque_spans(&e.children, out),
            Inline::Strong(s) => collect_opaque_spans(&s.children, out),
            Inline::Strikethrough(s) => collect_opaque_spans(&s.children, out),
            // Link text is inline content; a reference label or a title is not
            Inline::Link(l) => {
                collect_opaque_spans(&l.children, out);
                collect_link_kind_spans(&l.kind, out);
            }
            Inline::Image(i) => {
                collect_opaque_spans(&i.children, out);
                collect_link_kind_spans(&i.kind, out);
            }
            _ => {}
        }
    }
}

fn collect_link_kind_spans(kind: &LinkKind<'_>, out: &mut Vec<(Span, bool)>) {
    let pieces = match kind {
        LinkKind::Reference { label, .. } => label,
        LinkKind::Inline { title: Some(title), .. } => title,
        LinkKind::Inline { title: None, .. } => return,
    };
    out.extend(pieces.iter().map(|piece| (piece.span, false)));
}

/// The emphasis marker as printed: `_` unless a word touches it (`1*2*3` is emphasis, `1_2_3` is not),
/// the source marker around an autolink, on a raw line, and when a literal run of the target
/// character before it could pair with it.
fn emphasis_style<'a>(
    e: &'a Emphasis<'a>,
    children: &'a [Inline<'a>],
    i: usize,
    parent: InlineParent,
    f: &MarkdownFormatter<'_, 'a>,
) -> &'static str {
    let source = if e.marker == b'_' { "_" } else { "*" };
    if markers_as_written(f)
        || matches!(e.children.first(), Some(Inline::Autolink(_) | Inline::AutolinkLiteral(_)))
    {
        return source;
    }
    let word_neighbor = has_word_neighbor(children, i, f)
        || parent.strong_neighbor == Some(true)
        || f.context().emphasis_depth().get() > 0;
    if word_neighbor { "*" } else { "_" }
}

/// `**`; `__` stays as written.
fn strong_style<'a>(s: &'a Strong<'a>, f: &MarkdownFormatter<'_, 'a>) -> &'static str {
    if f.context().slice(s.span).as_bytes()[0] == b'_' && markers_as_written(f) {
        "__"
    } else {
        "**"
    }
}

/// On a raw line, or when normalized markers would pair differently (`pairing`).
fn markers_as_written(f: &MarkdownFormatter<'_, '_>) -> bool {
    f.context().raw_text().get() == Raw::Line || f.context().literal_markers().get()
}

/// Under `proseWrap: preserve`, whether any soft break of this paragraph survives printing
/// (one before a block-start-looking word is joined away; one next to a dialect shape line stays).
pub fn keeps_a_line_break<'a>(children: &'a [Inline<'a>], f: &MarkdownFormatter<'_, 'a>) -> bool {
    let last = children.len().wrapping_sub(1);
    children.iter().enumerate().any(|(i, child)| {
        matches!(child, Inline::SoftBreak(_))
            && i != 0
            && i != last
            && (line_shape(children, i + 1, false, false, f)
                || !words::prevents_break(
                    true,
                    next_word_of(children, i, InlineParent::default(), f),
                    ProseWrap::Preserve,
                ))
    })
}

/// Dialect line shapes (see AGENTS.md "Dialects"): a source line starting with one of these
/// keeps both its line boundaries and is never re-wrapped, whatever `proseWrap` says.
/// `alert`: the line is the first of a blockquote's first paragraph (`[!NOTE]`).
fn line_shape<'a>(
    children: &'a [Inline<'a>],
    j: usize,
    first_line: bool,
    alert: bool,
    f: &MarkdownFormatter<'_, 'a>,
) -> bool {
    let Some(inline) = children.get(j) else { return false };
    if matches!(inline, Inline::SoftBreak(_) | Inline::HardBreak(_)) {
        return false;
    }
    // The whole source line from this node on (a liquid tag, a tag, text: whatever starts it)
    let (base, raw) = source_line_at(children, inline.span().start, f);
    // The shapes, an alert, and a line that is not a block only because of what follows on it
    // (`[label]: dest text`, ``` ```a `b` ```, `$$x $y$`, `</span> text` on a first line):
    // a wrap inside it would leave the block behind.
    // (A first line that opens a block as a whole is escaped instead, or printed after its definition;
    // a continuation line's start is guarded where it stays, after a kept break.)
    words::is_line_shape_start(raw)
        || (alert && raw.starts_with("[!"))
        || is_unfinished_block_shape(raw, matches!(inline, Inline::Text(_)), first_line)
        || (first_line
            && !line_opens_block(raw, false)
            && line_prefix_opens_block(raw, base, false, f))
}

/// The source line starting at offset `start` (nodes after it on the line included),
/// up to the content's end, without the leading whitespace the printer drops;
/// with the offset it then starts at.
fn source_line_at<'a>(
    children: &[Inline<'_>],
    start: u32,
    f: &MarkdownFormatter<'_, 'a>,
) -> (u32, &'a str) {
    let end = children.last().map_or(start, |last| last.span().end);
    let raw = f.context().source_text().slice_range(start, end.max(start));
    let line = raw.trim_start_matches(HTML_WHITESPACE);
    let base = start + u32::try_from(raw.len() - line.len()).unwrap_or(0);
    (base, line.split('\n').next().unwrap_or(line))
}

/// Whether the line, or any prefix wrapping may cut it to, opens a block.
fn line_or_prefix_opens_block(
    line: &str,
    base: u32,
    in_paragraph: bool,
    f: &MarkdownFormatter<'_, '_>,
) -> bool {
    line_opens_block(line, in_paragraph) || line_prefix_opens_block(line, base, in_paragraph, f)
}

/// Whether a proper prefix of the line, cut at a whitespace outside opaque nodes
/// (a code span is one atom), opens a block. `base`: the line's source offset.
fn line_prefix_opens_block(
    line: &str,
    base: u32,
    in_paragraph: bool,
    f: &MarkdownFormatter<'_, '_>,
) -> bool {
    if !line.as_bytes().first().is_some_and(|&b| words::may_open_block(b)) {
        return false;
    }
    let opaque = f.context().opaque_spans().borrow();
    line.match_indices(words::is_split_whitespace).any(|(at, _)| {
        let offset = base + u32::try_from(at).unwrap_or(u32::MAX);
        !opaque.iter().any(|(span, _)| span.contains(offset))
            && line_opens_block(&line[..at], in_paragraph)
    })
}

/// A paragraph whose first line, printed at a block start,
/// opens a block that no escape can prevent (a tag: `<Badge />`, `</span>`).
/// Such a paragraph is the rest of one a definition was split from;
/// printed right after the definition, as in the source, its line stays a continuation.
pub fn first_line_opens_block(children: &[Inline<'_>], f: &MarkdownFormatter<'_, '_>) -> bool {
    match children.first() {
        None | Some(Inline::Text(_)) => false,
        Some(first) => line_opens_block(source_line_at(children, first.span().start, f).1, false),
    }
}

/// After a kept line break before node `i`:
/// whether a line the printer may produce from there opens a block.
/// Under `preserve` (and on a raw line) that is the source line.
/// Otherwise the source lines up to the next kept break are joined,
/// and the printed line is that text (`never`) or any prefix of it ending at a word boundary (`always`);
/// each is a candidate, which keeps the decision the same on every pass
/// (`| - | - | a` may wrap to a delimiter row `| - | - |`, `:-` may join to `:- ]`).
fn printed_line_opens_block<'a>(
    children: &'a [Inline<'a>],
    i: usize,
    break_kept: &dyn Fn(usize, &MarkdownFormatter<'_, 'a>) -> bool,
    f: &MarkdownFormatter<'_, 'a>,
) -> bool {
    // The source line runs to the line end
    // (through a backslash hard break: `---\` is no thematic break)
    let (_, line) = source_line_at(children, children[i].span().start, f);
    if !line.as_bytes().first().is_some_and(|&b| words::may_open_block(b)) {
        return false;
    }
    let prose_wrap = f.options().prose_wrap;
    if prose_wrap == ProseWrap::Preserve || f.context().raw_text().get() != Raw::No {
        return line_opens_block(line, true);
    }
    let start = children[i].span().start;
    let end = children
        .iter()
        .enumerate()
        .skip(i + 1)
        .find_map(|(j, c)| match c {
            Inline::HardBreak(_) => Some(c.span().start),
            Inline::SoftBreak(_) if break_kept(j, f) => Some(c.span().start),
            _ => None,
        })
        .unwrap_or_else(|| children[children.len() - 1].span().end);
    let segment = f.context().source_text().slice_range(start, end);
    let trimmed = segment.trim_start_matches(HTML_WHITESPACE);
    let joined = trimmed.cow_replace('\n', " ");
    if prose_wrap == ProseWrap::Always {
        let base = start + u32::try_from(segment.len() - trimmed.len()).unwrap_or(0);
        line_or_prefix_opens_block(&joined, base, true, f)
    } else {
        line_opens_block(&joined, true)
    }
}

/// The line, printed at column 0, would open a block:
/// as a paragraph continuation (`in_paragraph`, where setext underlines and table delimiter rows also count)
/// or as a paragraph's first line.
/// A bare `:::` run only closes, and a `|` row is inert without a table.
fn line_opens_block(line: &str, in_paragraph: bool) -> bool {
    match lexical::line_start(&Constructs::markdown(), line, in_paragraph) {
        None | Some(lexical::LineStart::TableRow | lexical::LineStart::DirectiveCloser) => false,
        Some(_) => true,
    }
}

/// A line that is not a block only because of what follows on it:
/// a fence or math opener whose info has a backtick or a `$` (they interrupt a paragraph, so any line counts),
/// a liquid tag, and on a paragraph's first line a definition (`[label]: dest text`; one cannot interrupt).
/// `is_text`: the line starts with text (a code span or math span is one atom and never wraps inside).
fn is_unfinished_block_shape(raw: &str, is_text: bool, first_line: bool) -> bool {
    (is_text
        && ((first_line
            && raw.starts_with('[')
            && raw.find(']').is_some_and(|i| raw[i + 1..].starts_with(':')))
            || raw.starts_with("```")
            || raw.starts_with("~~~")
            || raw.starts_with("$$")))
        || raw.starts_with("{%")
        || raw.starts_with("{{")
}

/// Prettier's `riskyParagraphPositions`: a `[[` in text, followed by `]]` (text or a wiki link's).
/// Wrapping such a paragraph could merge `[[foo\n[[wiki link]]` into one link,
/// so its text is printed as written.
/// Checked on the source, where a bracket pair split across nodes (`[` + `[link](u)`) still counts;
/// a `[[` inside an opaque span (a wiki link's own, a code span, HTML) does not.
fn wiki_link_risk<'a>(
    children: &'a [Inline<'a>],
    opaque: &[(Span, bool)],
    f: &MarkdownFormatter<'_, 'a>,
) -> bool {
    let (Some(first), Some(last)) = (children.first(), children.last()) else { return false };
    let start = first.span().start;
    let raw = f.context().source_text().slice_range(start, last.span().end);
    let Some(last_close) = raw.rfind("]]") else { return false };
    raw[..last_close].match_indices("[[").any(|(at, _)| {
        let abs = start + u32::try_from(at).unwrap_or(u32::MAX);
        !opaque.iter().any(|(span, _)| span.contains(abs))
    })
}

pub fn collect_inlines<'a>(
    children: &'a [Inline<'a>],
    parent: InlineParent,
    parts: &mut Parts<'a>,
    f: &mut MarkdownFormatter<'_, 'a>,
) {
    let last = children.len().wrapping_sub(1);
    // Raw text is switched per source line of a top-level paragraph:
    // every line when wiki links are at risk, else the lines starting with a dialect shape.
    // Nested content (emphasis, links) inherits the line's setting through the context.
    let raw_paragraph = parent.paragraph && f.context().raw_text().get() != Raw::No;
    let raw_for = |shape: bool| {
        if shape {
            Raw::Line
        } else if raw_paragraph {
            Raw::Text
        } else {
            Raw::No
        }
    };
    let raw_line = |j: usize, f: &MarkdownFormatter<'_, 'a>| -> Raw {
        raw_for(
            parent.paragraph
                && line_shape(children, j, j == 0, j == 0 && parent.first_in_container, f),
        )
    };
    if parent.paragraph {
        f.context().raw_text().set(raw_line(0, f));
    }
    // The soft break at `j` stays a line break: either side of it is raw
    let break_kept = |j: usize, f: &MarkdownFormatter<'_, 'a>| -> bool {
        f.context().raw_text().get() != Raw::No || raw_line(j + 1, f) != Raw::No
    };
    // Prettier's sentence is a run of texts joined by soft breaks; its CJ spacing style is one statistic.
    let mut sentence_cj_spaces: Option<Option<bool>> = None;
    // The previous sibling was a line break that stays in the output
    let mut last_break_kept = false;
    for (i, child) in children.iter().enumerate() {
        if !matches!(child, Inline::Text(_) | Inline::SoftBreak(_)) {
            sentence_cj_spaces = None;
        }
        let after_kept_break = last_break_kept;
        if !matches!(child, Inline::SoftBreak(_) | Inline::HardBreak(_)) {
            last_break_kept = false;
        }
        // After a kept line break, a line that would open a block at column 0 stays paragraph text
        // behind four spaces (indented code cannot interrupt a paragraph)
        if after_kept_break
            && matches!(child, Inline::Text(_) | Inline::HtmlInline(_) | Inline::Liquid(_))
            && printed_line_opens_block(children, i, &break_kept, f)
        {
            parts.push_str("    ");
        }
        match child {
            Inline::Text(t) => {
                let mut raw = f.context().slice(t.span);
                // CommonMark keeps a paragraph's edge `\f`; HTML rendering drops it
                if parent.paragraph {
                    if i == 0 {
                        raw = raw.trim_start_matches(HTML_WHITESPACE);
                    }
                    if i == last {
                        raw = raw.trim_end_matches(HTML_WHITESPACE);
                    }
                }
                if f.context().raw_text().get() != Raw::No {
                    // A line ending with `\` would be a hard break
                    let before_break = matches!(children.get(i + 1), Some(Inline::SoftBreak(_)));
                    let mut lines = raw.split('\n').peekable();
                    while let Some(line) = lines.next() {
                        parts.push_str(line);
                        let more = lines.peek().is_some();
                        if (more || before_break) && words::ends_with_unescaped_backslash(line) {
                            parts.push_str("\\");
                        }
                        if more {
                            parts.push_sep(Sep::HardLine);
                        }
                    }
                    continue;
                }
                // Edge characters matter only inside emphasis (escaping)
                let (edge_prev, edge_next) = if f.context().delimiter_depth().get() > 0 {
                    edge_chars(children, i, parent, f)
                } else {
                    (None, None)
                };
                let cj_spaces = *sentence_cj_spaces
                    .get_or_insert_with(|| sentence_cj_spaces_at(children, i, f));
                // A paragraph's first line that opens a block
                // (the rest of a paragraph a definition was split from) is escaped: `\- x`, `1\. x`
                if i == 0
                    && parent.paragraph
                    && !parent.after_checkbox
                    && line_opens_block(source_line_at(children, t.span.start, f).1, false)
                {
                    let digits = raw.bytes().take_while(u8::is_ascii_digit).count();
                    if digits > 0 && matches!(raw.as_bytes().get(digits), Some(b'.' | b')')) {
                        parts.push_str(&raw[..digits]);
                        raw = &raw[digits..];
                    }
                    parts.push_str("\\");
                }
                // Trailing punctuation after an autolink literal is where the link may still extend
                let autolink_stretch = if matches!(
                    children.get(i.wrapping_sub(1)),
                    Some(Inline::AutolinkLiteral(_))
                ) {
                    raw.find(|c: char| unicode::is_whitespace(c) || c == '<').unwrap_or(raw.len())
                } else {
                    0
                };
                let cx = words::TextContext {
                    autolink_stretch,
                    first_of_delimiter: parent.delimiter.filter(|_| i == 0),
                    last_of_delimiter: parent.delimiter.filter(|_| i == last),
                    edge_prev,
                    edge_next,
                    after_soft_break: matches!(
                        children.get(i.wrapping_sub(1)),
                        Some(Inline::SoftBreak(_))
                    ),
                    before_soft_break: matches!(children.get(i + 1), Some(Inline::SoftBreak(_))),
                    ends_line: ends_line(children, i, parent.ends_line),
                    next_word: next_word_of(children, i, parent, f),
                    glued_last_word: glued_last_word(children, i, raw, f),
                    cj_spaces,
                    ..words::TextContext::default()
                };
                words::push_text(raw, cx, parts, f);
            }
            // A soft break at a paragraph edge (a task checkbox followed by a newline) is stripped
            Inline::SoftBreak(_) if parent.paragraph && (i == 0 || i == last) => {}
            Inline::SoftBreak(_) => {
                let keep = break_kept(i, f);
                if parent.paragraph {
                    f.context().raw_text().set(raw_line(i + 1, f));
                }
                last_break_kept = keep;
                if keep {
                    // A separator, not content: the next line is measured on its own
                    parts.push_sep(Sep::HardLine);
                } else {
                    let cj_spaces = *sentence_cj_spaces
                        .get_or_insert_with(|| sentence_cj_spaces_at(children, i, f));
                    let cx = words::TextContext {
                        next_word: next_word_of(children, i, parent, f),
                        // Only the CJK rules look back (a trailing `\` was escaped by the text)
                        prev_word: if cj_spaces.is_some() {
                            prev_word_of(children, i, f)
                        } else {
                            None
                        },
                        cj_spaces,
                        ..words::TextContext::default()
                    };
                    words::push_whitespace(true, &cx, parts, f);
                }
            }
            Inline::HardBreak(b) => {
                match b.kind {
                    HardBreakKind::Spaces => {
                        parts.push_str("  ");
                        parts.push_atom(Atom::LiteralLine);
                    }
                    HardBreakKind::Backslash => {
                        parts.push_str("\\");
                        parts.push_atom(Atom::HardLine);
                    }
                }
                if parent.paragraph {
                    f.context().raw_text().set(raw_line(i + 1, f));
                }
                last_break_kept = true;
            }
            Inline::Emphasis(e) => {
                let style = emphasis_style(e, children, i, parent, f);
                let open = parts.len();
                parts.push_str(style);
                let inner = InlineParent {
                    delimiter: Some(style.as_bytes()[0]),
                    ends_line: ends_line(children, i, parent.ends_line),
                    ..InlineParent::default()
                };
                with_depth(f, MarkdownFormatContext::emphasis_depth, |f| {
                    with_depth(f, MarkdownFormatContext::delimiter_depth, |f| {
                        collect_inlines(&e.children, inner, parts, f);
                    });
                });
                mark_delimiter(open, parts, false);
                parts.push_str(style);
            }
            Inline::Strong(s) => {
                let style = strong_style(s, f);
                let open = parts.len();
                parts.push_str(style);
                let inner = InlineParent {
                    delimiter: Some(style.as_bytes()[0]),
                    strong_neighbor: Some(has_word_neighbor(children, i, f)),
                    ends_line: ends_line(children, i, parent.ends_line),
                    ..InlineParent::default()
                };
                with_depth(f, MarkdownFormatContext::delimiter_depth, |f| {
                    collect_inlines(&s.children, inner, parts, f);
                });
                mark_delimiter(open, parts, true);
                parts.push_str(style);
            }
            Inline::Strikethrough(s) => {
                let open = parts.len();
                parts.push_str("~~");
                let inner = InlineParent {
                    ends_line: ends_line(children, i, parent.ends_line),
                    ..InlineParent::default()
                };
                collect_inlines(&s.children, inner, parts, f);
                mark_delimiter(open, parts, false);
                parts.push_str("~~");
            }
            Inline::Link(l) => link::collect_link(l, parts, f),
            // Opaque to delimiter pairing (an image's alt text too: it is printed as written)
            _ => {
                let start = parts.len();
                // A code span joins its lines: no newline in its printed text
                let mut code_span_joined = false;
                match child {
                    Inline::CodeSpan(c) => {
                        let printed = print_code_span(c, f);
                        code_span_joined = !printed.contains('\n');
                        parts.push_str(printed);
                    }
                    Inline::Image(img) => link::collect_image(img, parts, f),
                    Inline::HtmlInline(h) => {
                        // Verbatim, continuation indentation included (mdast's `html` value is the source slice)
                        let value = join_pieces(&h.pieces, f);
                        let separator = if block::is_html_comment(value) {
                            Atom::HardLine
                        } else {
                            Atom::VerbatimLine
                        };
                        parts.push_lines(value, separator);
                    }
                    Inline::WikiLink(w) => {
                        let raw = f.context().slice(w.span);
                        let inner = &raw[2..raw.len() - 2];
                        parts.push_str("[[");
                        if f.options().prose_wrap == ProseWrap::Preserve
                            || !inner.contains(['\t', '\n'])
                        {
                            parts.push_str(inner);
                        } else {
                            parts.push_str(collapse_tabs_and_newlines(inner, f));
                        }
                        parts.push_str("]]");
                    }
                    Inline::Liquid(l) => {
                        parts.push_lines(join_pieces(&l.pieces, f), Atom::VerbatimLine);
                    }
                    // Printed as written.
                    // Math spans too: remark-math trims them, but the match may be accidental.
                    _ => parts.push_str(f.context().slice(child.span())),
                }
                parts.mark(Mark::Opaque { start, end: parts.len() });
                // A newline inside the node starts a source line with the node's tail; what follows
                // on that line may be cut by wrapping (`$$\n$$ text`: `$$` alone opens math),
                // so such a line stays as written
                if parent.paragraph
                    && !code_span_joined
                    && let Some(newline) = f.context().slice(child.span()).rfind('\n')
                {
                    let line_start = child.span().start + u32::try_from(newline).unwrap_or(0) + 1;
                    let (base, line) = source_line_at(children, line_start, f);
                    let risky = words::is_line_shape_start(line)
                        || line_or_prefix_opens_block(line, base, true, f);
                    f.context().raw_text().set(raw_for(risky));
                }
            }
        }
    }
}

/// Records a delimiter node whose opening marker was pushed at `open` and whose closing marker
/// is about to be pushed.
fn mark_delimiter(open: u32, parts: &mut Parts<'_>, strong: bool) {
    parts.mark(Mark::Delimiter { open, close: parts.len(), strong });
}

/// The smallest run length of `ch` absent from `text`.
fn min_absent_run(text: &str, ch: u8) -> usize {
    run_mask(text.as_bytes(), ch).trailing_ones() as usize + 1
}

/// Bit `n - 1` set: a run of exactly `n` `ch`s occurs in `bytes` (runs past 64 count as 64).
fn run_mask(bytes: &[u8], ch: u8) -> u64 {
    let mut present: u64 = 0;
    let mut run = 0usize;
    for &b in bytes.iter().chain(std::iter::once(&0)) {
        if b == ch {
            run += 1;
        } else if run > 0 {
            present |= 1u64 << (run.min(64) - 1);
            run = 0;
        }
    }
    present
}

/// HTML whitespace: `\t\n\f\r` and space.
const HTML_WHITESPACE: [char; 5] = ['\t', '\n', '\u{c}', '\r', ' '];

/// Runs of tabs / newlines become one space (wiki link contents under wrapping).
fn collapse_tabs_and_newlines<'a>(s: &str, f: &MarkdownFormatter<'_, 'a>) -> &'a str {
    let mut out = oxc_allocator::StringBuilder::with_capacity_in(s.len(), f.allocator());
    let mut in_run = false;
    for c in s.chars() {
        if c == '\t' || c == '\n' {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out.into_str()
}

/// The first word of the next sibling when it is a text, for the whitespace that ends this one.
fn next_word_of<'a>(
    children: &'a [Inline<'a>],
    i: usize,
    parent: InlineParent,
    f: &MarkdownFormatter<'_, 'a>,
) -> Option<words::NextWord<'a>> {
    // Any node wrapped to a line start may open a block from its source text
    // (`<!--`, `<div>`, `$$`, `{% t %}`, `[[toc]]`)
    let next = children.get(i + 1)?;
    if matches!(next, Inline::SoftBreak(_) | Inline::HardBreak(_)) {
        return None;
    }
    // The word runs across nodes (`[^1]:` is a footnote reference and a text)
    let (word, alone_on_line) = first_word_at(children, next.span().start, f)?;
    // A text's leading run after a soft break is escaped inside emphasis
    // (as `push_text` decides: a space before it, and after it the text's next character or the node edge)
    let escaped = matches!(next, Inline::Text(_))
        && f.context().delimiter_depth().get() > 0
        && !f.context().literal_markers().get()
        && words::leading_run_escaped(
            word,
            Some(' '),
            edge_char(children, i + 1, parent, false, f),
        );
    Some(words::NextWord { word, alone_on_line, escaped })
}

/// The first word of the source line starting at `start`, and whether it is the line's only one.
fn first_word_at<'a>(
    children: &[Inline<'_>],
    start: u32,
    f: &MarkdownFormatter<'_, 'a>,
) -> Option<(&'a str, bool)> {
    let (_, line) = source_line_at(children, start, f);
    let mut it = line.split(words::is_split_whitespace).filter(|w| !w.is_empty());
    let word = it.next()?;
    Some((word, it.next().is_none()))
}

/// Nothing follows node `i` on its source line (a node glued after it would be part of the line);
/// past the last sibling, whether the parent ends its line.
fn ends_line(children: &[Inline<'_>], i: usize, parent_ends_line: bool) -> bool {
    match children.get(i + 1) {
        None => parent_ends_line,
        Some(Inline::SoftBreak(_) | Inline::HardBreak(_)) => true,
        Some(_) => false,
    }
}

/// Both edge characters of the text at `i`.
fn edge_chars<'a>(
    children: &'a [Inline<'a>],
    i: usize,
    parent: InlineParent,
    f: &MarkdownFormatter<'_, 'a>,
) -> (Option<char>, Option<char>) {
    (edge_char(children, i, parent, true, f), edge_char(children, i, parent, false, f))
}

/// The character right before (`before`) or after the text at `i`, as it will be printed:
/// a space for a line break, the neighbor node's edge character, the enclosing emphasis marker;
/// `None` at the edge of a paragraph.
fn edge_char<'a>(
    children: &'a [Inline<'a>],
    i: usize,
    parent: InlineParent,
    before: bool,
    f: &MarkdownFormatter<'_, 'a>,
) -> Option<char> {
    let neighbor =
        if before { i.checked_sub(1).map(|j| &children[j]) } else { children.get(i + 1) };
    match neighbor {
        Some(Inline::SoftBreak(_) | Inline::HardBreak(_)) => Some(' '),
        // An autolink's extent depends on what follows it: escaping there changes the URL
        Some(Inline::Autolink(_) | Inline::AutolinkLiteral(_)) => None,
        Some(node) => {
            let raw = f.context().slice(node.span());
            if before { raw.chars().next_back() } else { raw.chars().next() }
        }
        // A paragraph edge is a boundary, whitespace to the flanking rules
        None => parent.delimiter.map_or(Some(' '), |m| Some(char::from(m))),
    }
}

/// The last word of text `i` (its printed `raw`) extended by the nodes glued after it
/// on its source line (a backslash hard break's `\` included), and whether it is the line's last word.
fn glued_last_word<'a>(
    children: &'a [Inline<'a>],
    i: usize,
    raw: &str,
    f: &MarkdownFormatter<'_, 'a>,
) -> Option<(&'a str, bool)> {
    // Nothing glues after whitespace, a line end, or a two-space hard break
    match children.get(i + 1) {
        None | Some(Inline::SoftBreak(_)) => return None,
        Some(Inline::HardBreak(b)) if b.kind == HardBreakKind::Spaces => return None,
        _ if raw.ends_with(words::is_split_whitespace) => return None,
        _ => {}
    }
    let last = raw.rsplit(words::is_split_whitespace).next()?;
    let start = children[i].span().end - u32::try_from(last.len()).unwrap_or(0);
    first_word_at(children, start, f).filter(|(word, _)| word.len() != last.len())
}

/// The last word of the text before node `i`.
fn prev_word_of<'a>(
    children: &'a [Inline<'a>],
    i: usize,
    f: &MarkdownFormatter<'_, 'a>,
) -> Option<&'a str> {
    let Some(Inline::Text(t)) = children.get(i.checked_sub(1)?) else { return None };
    f.context().slice(t.span).rsplit(words::is_split_whitespace).find(|w| !w.is_empty())
}

/// `usesCJSpaces` of the sentence (run of texts and soft breaks) containing node `i`;
/// `None` without CJK text, which is the common case and skips the scan and the CJK rules.
fn sentence_cj_spaces_at<'a>(
    children: &'a [Inline<'a>],
    i: usize,
    f: &MarkdownFormatter<'_, 'a>,
) -> Option<bool> {
    let in_sentence = |c: &Inline<'a>| matches!(c, Inline::Text(_) | Inline::SoftBreak(_));
    let start = (0..i).rev().take_while(|&j| in_sentence(&children[j])).last().unwrap_or(i);
    let end = (i..children.len()).take_while(|&j| in_sentence(&children[j])).last().unwrap_or(i);
    let texts = children[start..=end].iter().filter_map(|c| match c {
        Inline::Text(t) => Some(t),
        _ => None,
    });
    if !texts.clone().any(|t| t.contains_cjk) {
        return None;
    }
    Some(words::uses_cj_spaces(texts.map(|t| f.context().slice(t.span))))
}

/// A word character directly before or after node `i`.
fn has_word_neighbor<'a>(
    children: &'a [Inline<'a>],
    i: usize,
    f: &MarkdownFormatter<'_, 'a>,
) -> bool {
    let is_word_char = |c: char| !unicode::is_whitespace(c) && !unicode::is_punctuation(c);
    let before = i > 0
        && matches!(&children[i - 1], Inline::Text(t)
            if f.context().slice(t.span).chars().next_back().is_some_and(is_word_char));
    let after = matches!(children.get(i + 1), Some(Inline::Text(t))
        if f.context().slice(t.span).chars().next().is_some_and(is_word_char));
    before || after
}

/// Backtick fence of the shortest length the content permits that no literal backtick run of the
/// paragraph shares (a fence equal to a stray run would pair with it and move the span:
/// Prettier's content-only rule, prettier/prettier#6035).
/// A space pads content that starts / ends with a backtick or is surrounded by spaces (CommonMark strips one).
/// Inside a table cell a `|` is `\|` in the source and stays so
/// (the raw slice already carries the escape Prettier re-adds to its decoded value).
fn print_code_span<'a>(code: &'a CodeSpan<'a>, f: &MarkdownFormatter<'_, 'a>) -> &'a str {
    let joined = join_pieces(&code.pieces, f);
    let value: Cow<'_, str> = if f.options().prose_wrap == ProseWrap::Preserve {
        Cow::Borrowed(joined)
    } else {
        joined.cow_replace('\n', " ")
    };
    let runs = f.context().code_span_literal_runs().borrow();
    let literal_runs =
        runs.binary_search_by_key(&code.span.start, |&(start, _)| start).map_or(0, |i| runs[i].1);
    let mut len = min_absent_run(&value, b'`');
    while len <= 64 && literal_runs & (1u64 << (len - 1)) != 0 {
        len += 1;
    }
    let fence = backticks(len);
    let is_space_or_newline = |c: char| c == ' ' || c == '\n';
    let padding = value.starts_with('`')
        || value.ends_with('`')
        || (value.starts_with(is_space_or_newline)
            && value.ends_with(is_space_or_newline)
            && value.chars().any(|c| !is_space_or_newline(c)));
    let padding = if padding { " " } else { "" };
    f.allocator().alloc_concat_strs_array([&fence, padding, &value, padding, &fence])
}
