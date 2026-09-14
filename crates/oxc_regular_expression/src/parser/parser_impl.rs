use oxc_allocator::Allocator;
use oxc_diagnostics::Result;
use oxc_span::Span;
use oxc_str::JSStr;

use crate::{
    ast, diagnostics,
    options::Options,
    parser::{flags_parser::FlagsParser, pattern_parser::PatternParser, reader::Reader},
};

/// Regular expression parser for `/literal/` usage.
/// - `pattern_text`: the text between `/` and `/`
/// - `flags_text`: the text after the second `/`
pub struct LiteralParser<'a, 'b> {
    allocator: &'a Allocator,
    pattern_text: JSStr<'a>,
    flags_text: Option<JSStr<'b>>,
    options: Options,
}

impl<'a, 'b> LiteralParser<'a, 'b> {
    pub fn new(
        allocator: &'a Allocator,
        pattern_text: &'a str,
        flags_text: Option<&'b str>,
        options: Options,
    ) -> Self {
        Self::from_value(allocator, pattern_text.into(), flags_text.map(JSStr::from), options)
    }

    /// Parse an already-decoded JavaScript pattern and flags, including lone surrogates.
    /// Spans refer to byte offsets in the decoded WTF-8 values.
    pub fn from_value(
        allocator: &'a Allocator,
        pattern_text: JSStr<'a>,
        flags_text: Option<JSStr<'b>>,
        options: Options,
    ) -> Self {
        Self { allocator, pattern_text, flags_text, options }
    }

    pub fn parse(self) -> Result<ast::Pattern<'a>> {
        let Options { pattern_span_offset, flags_span_offset } = self.options;

        let (unicode_mode, unicode_sets_mode) = if let Some(flags_text) = self.flags_text {
            // Flags are restricted to ASCII letters. A lone surrogate is an invalid flag.
            let Some(flags_utf8) = flags_text.as_str() else {
                #[expect(clippy::cast_possible_truncation, reason = "JSStr lengths fit in u32")]
                let len = flags_text.len() as u32;
                return Err(diagnostics::invalid_input(Span::new(
                    flags_span_offset,
                    flags_span_offset + len,
                )));
            };
            let reader = Reader::initialize(flags_utf8, true, false)?;

            FlagsParser::new(reader, flags_span_offset).parse()?
        } else {
            (false, false)
        };

        let pattern_text =
            if self.pattern_text.is_empty() { JSStr::from("(?:)") } else { self.pattern_text };
        let reader = Reader::from_value(pattern_text, unicode_mode);

        PatternParser::new(
            self.allocator,
            reader,
            (unicode_mode, unicode_sets_mode),
            pattern_span_offset,
        )
        .parse()
    }
}

/// Regular expression parser for `new RegExp("constrocutor")` usage.
/// - `pattern_text`: the string literal text as 1st argument of `RegExp` constructor
/// - `flags_text`: the string literal text as 2nd argument of `RegExp` constructor
///
/// String literal text should be in the form of `'...'` or `"..."` and may contain escape sequences.
pub struct ConstructorParser<'a> {
    allocator: &'a Allocator,
    pattern_text: &'a str,
    flags_text: Option<&'a str>,
    options: Options,
}

impl<'a> ConstructorParser<'a> {
    pub fn new(
        allocator: &'a Allocator,
        pattern_text: &'a str,
        flags_text: Option<&'a str>,
        options: Options,
    ) -> Self {
        Self { allocator, pattern_text, flags_text, options }
    }

    pub fn parse(self) -> Result<ast::Pattern<'a>> {
        let parse_string_literal = true;
        let Options { pattern_span_offset, flags_span_offset } = self.options;

        let (unicode_mode, unicode_sets_mode) = if let Some(flags_text) = self.flags_text {
            let reader =
                Reader::initialize(flags_text, true, parse_string_literal).map_err(|_| {
                    let span_start = flags_span_offset;
                    #[expect(clippy::cast_possible_truncation)]
                    let span_end = flags_span_offset + flags_text.len() as u32;
                    diagnostics::invalid_input(Span::new(span_start, span_end))
                })?;

            FlagsParser::new(reader, self.options.flags_span_offset).parse()?
        } else {
            (false, false)
        };

        let pattern_text = if matches!(self.pattern_text, r#""""# | "''" | "``") {
            r#""(?:)""#
        } else {
            self.pattern_text
        };
        let reader =
            Reader::initialize(pattern_text, unicode_mode, parse_string_literal).map_err(|_| {
                let span_start = pattern_span_offset;
                #[expect(clippy::cast_possible_truncation)]
                let span_end = pattern_span_offset + pattern_text.len() as u32;
                diagnostics::invalid_input(Span::new(span_start, span_end))
            })?;

        PatternParser::new(
            self.allocator,
            reader,
            (unicode_mode, unicode_sets_mode),
            pattern_span_offset,
        )
        .parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visit::{RegExpAstKind, Visit};
    use oxc_str::JSStrBuilder;

    #[derive(Default)]
    struct Characters(Vec<u32>);
    impl<'a> Visit<'a> for Characters {
        fn enter_node(&mut self, node: RegExpAstKind<'a>) {
            if let RegExpAstKind::Character(ch) = node {
                self.0.push(ch.value);
            }
        }
    }

    #[test]
    fn decoded_patterns_preserve_jsstr_characters() {
        let allocator = Allocator::new();
        for units in [&[0x61][..], &[0xD800], &[0xDC00], &[0xD800, 0xDC00], &[0x61, 0xD801, 0x62]] {
            let mut builder = JSStrBuilder::new_in(&allocator);
            builder.push_utf16(units);
            let value = builder.into_js_str();
            for flags in ["", "u", "v"] {
                let parsed = LiteralParser::from_value(
                    &allocator,
                    value,
                    Some(flags.into()),
                    Options::default(),
                )
                .parse()
                .unwrap();
                let mut characters = Characters::default();
                characters.visit_pattern(&parsed);
                let expected: Vec<_> = if flags.is_empty() {
                    units.iter().copied().map(u32::from).collect()
                } else {
                    value.chars().map(oxc_str::JSChar::to_u32).collect()
                };
                assert_eq!(characters.0, expected);
            }
        }
        // Invalid named groups and flags must return errors, never panic on WTF-8.
        let mut builder = JSStrBuilder::new_in(&allocator);
        builder.push_str("(?<");
        builder.push_utf16(&[0xD800]);
        builder.push_str(">x)");
        let invalid_group = builder.into_js_str();
        assert!(
            LiteralParser::from_value(&allocator, invalid_group, None, Options::default())
                .parse()
                .is_err()
        );
        assert!(
            LiteralParser::from_value(
                &allocator,
                "x".into(),
                Some(invalid_group),
                Options::default()
            )
            .parse()
            .is_err()
        );
    }
}
