use oxc_diagnostics::Result;
use oxc_str::{JSStr, Str};

use crate::parser::reader::{
    Options,
    ast::{CodePoint, EscapeKind},
    string_literal_parser::{
        Parser as StringLiteralParser, ast as StringLiteralAst, parse_regexp_literal,
    },
    template_literal_parser::{Parser as TemplateLiteralParser, ast as TemplateLiteralAst},
};

#[derive(Debug)]
pub struct Reader<'a> {
    source_text: JSStr<'a>,
    units: Vec<CodePoint>,
    index: usize,
    offset: u32,
}

impl<'a> Reader<'a> {
    pub fn initialize(
        source_text: &'a str,
        unicode_mode: bool,
        parse_string_or_template_literal: bool,
    ) -> Result<Self> {
        // NOTE: This must be `0`.
        // Since `source_text` here may be a slice of the original source text,
        // using `Span` for `span.source_text(source_text)` will be out of range in some cases.
        let span_offset = 0;

        let units = if parse_string_or_template_literal {
            if source_text.chars().next().is_some_and(|c| c == '`') {
                let TemplateLiteralAst::TemplateLiteral { body, .. } = TemplateLiteralParser::new(
                    source_text,
                    Options {
                        strict_mode: false,
                        span_offset,
                        combine_surrogate_pair: unicode_mode,
                    },
                )
                .parse()?;
                body
            } else {
                let StringLiteralAst::StringLiteral { body, .. } = StringLiteralParser::new(
                    source_text,
                    Options {
                        strict_mode: false,
                        span_offset,
                        combine_surrogate_pair: unicode_mode,
                    },
                )
                .parse()?;
                body
            }
        } else {
            parse_regexp_literal(source_text, span_offset, unicode_mode)
        };

        Ok(Self {
            source_text: source_text.into(),
            units,
            index: 0,
            // If `parse_string_or_template_literal` is `true`, the first character is the opening quote.
            // We need to +1 to skip it.
            offset: u32::from(parse_string_or_template_literal),
        })
    }

    /// Read a decoded pattern without interpreting JavaScript string escapes.
    pub fn from_value(value: JSStr<'a>, unicode_mode: bool) -> Self {
        if let Some(value) = value.as_str() {
            return Self::initialize(value, unicode_mode, false)
                .expect("decoded pattern requires no string-literal parsing");
        }
        let mut units = Vec::new();
        let mut offset = 0;
        for ch in value.chars() {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a WTF-8 code point is at most 4 bytes"
            )]
            let width = ch.to_char().map_or(3, char::len_utf8) as u32;
            let end = offset + width;
            StringLiteralParser::handle_code_point(
                &mut units,
                ((offset, end), ch.to_u32(), EscapeKind::None),
                0,
                unicode_mode,
            );
            offset = end;
        }
        Self { source_text: value, units, index: 0, offset: 0 }
    }

    pub fn offset(&self) -> u32 {
        self.offset
    }

    pub fn checkpoint(&self) -> (usize, u32) {
        (self.index, self.offset)
    }

    pub fn rewind(&mut self, checkpoint: (usize, u32)) {
        self.index = checkpoint.0;
        self.offset = checkpoint.1;
    }

    pub fn advance(&mut self) {
        if let Some(unit) = self.units.get(self.index) {
            self.offset = unit.span.end;
            self.index += 1;
        }
    }

    fn peek_nth(&self, n: usize) -> Option<u32> {
        let nth = self.index + n;
        self.units.get(nth).map(|cp| cp.value)
    }

    pub fn peek(&self) -> Option<u32> {
        self.peek_nth(0)
    }

    pub fn peek2(&self) -> Option<u32> {
        self.peek_nth(1)
    }

    /// Returns `true` if any code point in the whole pattern equals `ch`.
    ///
    /// Scans the already-decoded units, so it neither allocates nor re-decodes.
    /// Used to cheaply check whether a character (e.g. `(`) is present at all.
    pub fn contains(&self, ch: char) -> bool {
        self.units.iter().any(|unit| unit.value == ch as u32)
    }

    /// Returns the escape kind of the current code point.
    /// This is used to preserve information about how the character was
    /// written in source code (e.g., as `\u0301` vs literal character).
    pub fn peek_escape_kind(&self) -> EscapeKind {
        self.units.get(self.index).map_or(EscapeKind::None, |cp| cp.escape_kind)
    }

    pub fn eat(&mut self, ch: char) -> bool {
        if self.peek_nth(0) == Some(ch as u32) {
            self.advance();
            return true;
        }
        false
    }

    pub fn eat2(&mut self, ch: char, ch2: char) -> bool {
        if self.peek_nth(0) == Some(ch as u32) && self.peek_nth(1) == Some(ch2 as u32) {
            self.advance();
            self.advance();
            return true;
        }
        false
    }

    pub fn eat3(&mut self, ch: char, ch2: char, ch3: char) -> bool {
        if self.peek_nth(0) == Some(ch as u32)
            && self.peek_nth(1) == Some(ch2 as u32)
            && self.peek_nth(2) == Some(ch3 as u32)
        {
            self.advance();
            self.advance();
            self.advance();
            return true;
        }
        false
    }

    pub fn eat4(&mut self, ch: char, ch2: char, ch3: char, ch4: char) -> bool {
        if self.peek_nth(0) == Some(ch as u32)
            && self.peek_nth(1) == Some(ch2 as u32)
            && self.peek_nth(2) == Some(ch3 as u32)
            && self.peek_nth(3) == Some(ch4 as u32)
        {
            self.advance();
            self.advance();
            self.advance();
            self.advance();
            return true;
        }
        false
    }

    pub fn str(&self, start: u32, end: u32) -> Option<Str<'a>> {
        if let Some(source) = self.source_text.as_str() {
            Some(Str::from(&source[start as usize..end as usize]))
        } else {
            std::str::from_utf8(&self.source_text.as_wtf8()[start as usize..end as usize])
                .ok()
                .map(Str::from)
        }
    }
}
