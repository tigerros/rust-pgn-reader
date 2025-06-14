use std::{
    cmp::min,
    io::{self, Chain, Cursor, Read},
};

use shakmaty::{
    san::{San, SanPlus, Suffix},
    CastlingSide, Color, Outcome,
};

// use slice_deque::SliceDeque;
use crate::{
    types::{Nag, RawComment, RawTag, Skip},
    visitor::{SkipVisitor, Visitor},
};

const MIN_BUFFER_SIZE: usize = 8192;

trait ReadPgn {
    type Err;

    /// Fill the buffer. The buffer must then contain at least MIN_BUFFER_SIZE
    /// bytes or all remaining bytes until the end of the source.
    fn fill_buffer_and_peek(&mut self) -> Result<Option<u8>, Self::Err>;

    /// Returns the current buffer.
    fn buffer(&self) -> &[u8];

    /// Consume n bytes from the buffer.
    fn consume(&mut self, n: usize);

    /// Constructs a parser error.
    fn invalid_data() -> Self::Err;

    fn peek(&self) -> Option<u8> {
        self.buffer().first().cloned()
    }

    fn bump(&mut self) -> Option<u8> {
        let head = self.peek();
        if head.is_some() {
            self.consume(1);
        }
        head
    }

    fn remaining(&self) -> usize {
        self.buffer().len()
    }

    fn consume_all(&mut self) {
        let remaining = self.remaining();
        self.consume(remaining);
    }

    fn skip_bom(&mut self) -> Result<(), Self::Err> {
        self.fill_buffer_and_peek()?;
        if self.buffer().starts_with(b"\xef\xbb\xbf") {
            self.consume(3);
        }
        Ok(())
    }

    fn skip_until(&mut self, needle: u8) -> Result<(), Self::Err> {
        while self.fill_buffer_and_peek()?.is_some() {
            if let Some(pos) = memchr::memchr(needle, self.buffer()) {
                self.consume(pos);
                return Ok(());
            } else {
                self.consume_all();
            }
        }

        Ok(())
    }

    fn skip_line(&mut self) -> Result<(), Self::Err> {
        self.skip_until(b'\n')?;
        self.bump();
        Ok(())
    }

    fn skip_whitespace(&mut self) -> Result<(), Self::Err> {
        while let Some(ch) = self.fill_buffer_and_peek()? {
            match ch {
                b' ' | b'\t' | b'\r' | b'\n' => {
                    self.bump();
                }
                b'%' => {
                    self.bump();
                    self.skip_line()?;
                }
                _ => return Ok(()),
            }
        }

        Ok(())
    }

    fn skip_ket(&mut self) -> Result<(), Self::Err> {
        while let Some(ch) = self.fill_buffer_and_peek()? {
            match ch {
                b' ' | b'\t' | b'\r' | b']' => {
                    self.bump();
                }
                b'%' => {
                    self.bump();
                    self.skip_line()?;
                    return Ok(());
                }
                b'\n' => {
                    self.bump();
                    return Ok(());
                }
                _ => {
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    fn read_tags<V: Visitor>(&mut self, visitor: &mut V) -> Result<(), Self::Err> {
        while let Some(ch) = self.fill_buffer_and_peek()? {
            match ch {
                b'[' => {
                    self.bump();

                    let left_quote = match memchr::memchr3(b'"', b'\n', b']', self.buffer()) {
                        Some(left_quote) if self.buffer()[left_quote] == b'"' => left_quote,
                        Some(eol) => {
                            self.consume(eol + 1);
                            self.skip_ket()?;
                            continue;
                        }
                        None => {
                            self.consume_all();
                            self.skip_line()?;
                            return Err(Self::invalid_data());
                        }
                    };

                    let space = if left_quote > 0 && self.buffer()[left_quote - 1] == b' ' {
                        left_quote - 1
                    } else {
                        left_quote
                    };

                    let value_start = left_quote + 1;
                    let mut right_quote = value_start;
                    let consumed = loop {
                        match memchr::memchr3(b'\\', b'"', b'\n', &self.buffer()[right_quote..]) {
                            Some(delta) if self.buffer()[right_quote + delta] == b'"' => {
                                right_quote += delta;
                                break right_quote + 1;
                            }
                            Some(delta) if self.buffer()[right_quote + delta] == b'\n' => {
                                right_quote += delta;
                                break right_quote;
                            }
                            Some(delta) => {
                                // Skip escaped character.
                                right_quote = min(right_quote + delta + 2, self.remaining());
                            }
                            None => {
                                self.consume_all();
                                self.skip_line()?;
                                return Err(Self::invalid_data());
                            }
                        }
                    };

                    visitor.tag(
                        &self.buffer()[..space],
                        RawTag(&self.buffer()[value_start..right_quote]),
                    );
                    self.consume(consumed);
                    self.skip_ket()?;
                }
                b'%' => self.skip_line()?,
                _ => return Ok(()),
            }
        }

        Ok(())
    }

    fn skip_movetext(&mut self) -> Result<(), Self::Err> {
        while let Some(ch) = self.fill_buffer_and_peek()? {
            self.bump();

            match ch {
                b'{' => {
                    self.skip_until(b'}')?;
                    self.bump();
                }
                b';' => {
                    self.skip_until(b'\n')?;
                }
                b'\n' => match self.peek() {
                    Some(b'%') => self.skip_until(b'\n')?,
                    Some(b'\n') | Some(b'[') => break,
                    Some(b'\r') => {
                        self.bump();
                        if let Some(b'\n') = self.peek() {
                            break;
                        }
                    }
                    _ => continue,
                },
                _ => {
                    if let Some(consumed) = memchr::memchr3(b'\n', b'{', b';', self.buffer()) {
                        self.consume(consumed);
                    } else {
                        self.consume_all();
                    }
                }
            }
        }

        Ok(())
    }

    fn find_token_end(&mut self, start: usize) -> usize {
        let mut end = start;
        for &ch in &self.buffer()[start..] {
            match ch {
                b' ' | b'\t' | b'\n' | b'\r' | b'{' | b'}' | b'(' | b')' | b'!' | b'?' | b'$'
                | b';' | b'.' => break,
                _ => end += 1,
            }
        }
        end
    }

    fn read_movetext<V: Visitor>(&mut self, visitor: &mut V) -> Result<(), Self::Err> {
        while let Some(ch) = self.fill_buffer_and_peek()? {
            match ch {
                b'{' => {
                    self.bump();

                    let right_brace = if let Some(right_brace) = memchr::memchr(b'}', self.buffer())
                    {
                        right_brace
                    } else {
                        self.consume_all();
                        self.skip_until(b'}')?;
                        self.bump();
                        return Err(Self::invalid_data());
                    };

                    visitor.comment(RawComment(&self.buffer()[..right_brace]));
                    self.consume(right_brace + 1);
                }
                b'\n' => {
                    self.bump();

                    match self.peek() {
                        Some(b'%') => {
                            self.bump();
                            self.skip_line()?;
                        }
                        Some(b'[') | Some(b'\n') => {
                            break;
                        }
                        Some(b'\r') => {
                            self.bump();
                            if self.peek() == Some(b'\n') {
                                break;
                            }
                        }
                        _ => continue,
                    }
                }
                b';' => {
                    self.bump();
                    self.skip_until(b'\n')?;
                }
                b'1' => {
                    self.bump();
                    if self.buffer().starts_with(b"-0") {
                        self.consume(2);
                        visitor.outcome(Some(Outcome::Decisive {
                            winner: Color::White,
                        }));
                    } else if self.buffer().starts_with(b"/2-1/2") {
                        self.consume(6);
                        visitor.outcome(Some(Outcome::Draw));
                    } else {
                        let token_end = self.find_token_end(0);
                        self.consume(token_end);
                    }
                }
                b'0' => {
                    self.bump();
                    if self.buffer().starts_with(b"-1") {
                        self.consume(2);
                        visitor.outcome(Some(Outcome::Decisive {
                            winner: Color::Black,
                        }));
                    } else if self.buffer().starts_with(b"-0") {
                        // Castling notation with zeros.
                        self.consume(2);
                        let side = if self.buffer().starts_with(b"-0") {
                            self.consume(2);
                            CastlingSide::QueenSide
                        } else {
                            CastlingSide::KingSide
                        };
                        let suffix = match self.peek() {
                            Some(b'+') => Some(Suffix::Check),
                            Some(b'#') => Some(Suffix::Checkmate),
                            _ => None,
                        };
                        visitor.san(SanPlus {
                            san: San::Castle(side),
                            suffix,
                        });
                    } else {
                        let token_end = self.find_token_end(0);
                        self.consume(token_end);
                    }
                }
                b'(' => {
                    self.bump();
                    if let Skip(true) = visitor.begin_variation() {
                        self.skip_variation()?;
                    }
                }
                b')' => {
                    self.bump();
                    visitor.end_variation();
                }
                b'$' => {
                    self.bump();
                    let token_end = self.find_token_end(0);
                    if let Ok(nag) = btoi::btou(&self.buffer()[..token_end]) {
                        visitor.nag(Nag(nag));
                    }
                    self.consume(token_end);
                }
                b'!' => {
                    self.bump();
                    match self.peek() {
                        Some(b'!') => {
                            self.bump();
                            visitor.nag(Nag::BRILLIANT_MOVE);
                        }
                        Some(b'?') => {
                            self.bump();
                            visitor.nag(Nag::SPECULATIVE_MOVE);
                        }
                        _ => {
                            visitor.nag(Nag::GOOD_MOVE);
                        }
                    }
                }
                b'?' => {
                    self.bump();
                    match self.peek() {
                        Some(b'!') => {
                            self.bump();
                            visitor.nag(Nag::DUBIOUS_MOVE);
                        }
                        Some(b'?') => {
                            self.bump();
                            visitor.nag(Nag::BLUNDER);
                        }
                        _ => {
                            visitor.nag(Nag::MISTAKE);
                        }
                    }
                }
                b'*' => {
                    visitor.outcome(None);
                    self.bump();
                }
                b' ' | b'\t' | b'\r' | b'P' | b'.' => {
                    self.bump();
                }
                _ => {
                    let token_end = self.find_token_end(1);
                    if ch > b'9' || ch == b'-' {
                        if let Ok(san) = SanPlus::from_ascii(&self.buffer()[..token_end]) {
                            visitor.san(san);
                        }
                    }
                    self.consume(token_end);
                }
            }
        }

        Ok(())
    }

    fn skip_variation(&mut self) -> Result<(), Self::Err> {
        let mut depth = 0usize;

        while let Some(ch) = self.fill_buffer_and_peek()? {
            match ch {
                b'(' => {
                    depth += 1;
                    self.bump();
                }
                b')' => {
                    if let Some(d) = depth.checked_sub(1) {
                        self.bump();
                        depth = d;
                    } else {
                        break;
                    }
                }
                b'{' => {
                    self.bump();
                    self.skip_until(b'}')?;
                    self.bump();
                }
                b';' => {
                    self.bump();
                    self.skip_until(b'\n')?;
                }
                b'\n' => {
                    match self.buffer().get(1).cloned() {
                        Some(b'%') => {
                            self.consume(2);
                            self.skip_until(b'\n')?;
                        }
                        Some(b'[') | Some(b'\n') => {
                            // Do not consume the first or second line break.
                            break;
                        }
                        Some(b'\r') => {
                            // Do not consume the first or second line break.
                            if self.buffer().get(2).cloned() == Some(b'\n') {
                                break;
                            }
                        }
                        _ => {
                            self.bump();
                        }
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }

        Ok(())
    }

    fn read_game<V: Visitor>(&mut self, visitor: &mut V) -> Result<Option<V::Result>, Self::Err> {
        self.skip_bom()?;
        self.skip_whitespace()?;

        if self.fill_buffer_and_peek()?.is_none() {
            return Ok(None);
        }

        visitor.begin_tags();
        self.read_tags(visitor)?;
        if let Skip(false) = visitor.begin_movetext() {
            self.read_movetext(visitor)?;
        } else {
            self.skip_movetext()?;
        }

        self.skip_whitespace()?;
        Ok(Some(visitor.end_game()))
    }

    fn skip_game(&mut self) -> Result<bool, Self::Err> {
        self.read_game(&mut SkipVisitor).map(|r| r.is_some())
    }
}

/// Internal read ahead buffer.
#[derive(Debug, Clone)]
pub struct Buffer {
    inner: circular::Buffer,
}

impl Buffer {
    fn new() -> Buffer {
        Buffer {
            inner: circular::Buffer::with_capacity(MIN_BUFFER_SIZE * 2),
        }
    }
}

impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        self.inner.data()
    }
}

/// A buffered PGN reader.
#[derive(Debug)]
pub struct BufferedReader<R> {
    inner: R,
    buffer: Buffer,
}

impl<T: AsRef<[u8]>> BufferedReader<Cursor<T>> {
    /// Create a new reader by wrapping a byte slice in a [`Cursor`].
    ///
    /// ```
    /// use pgn_reader::BufferedReader;
    ///
    /// let pgn = b"1. e4 e5 *";
    /// let reader = BufferedReader::new_cursor(&pgn[..]);
    /// ```
    ///
    /// [`Cursor`]: https://doc.rust-lang.org/std/io/struct.Cursor.html
    pub fn new_cursor(inner: T) -> BufferedReader<Cursor<T>> {
        BufferedReader::new(Cursor::new(inner))
    }
}

impl<R: Read> BufferedReader<R> {
    /// Create a new buffered PGN reader.
    ///
    /// ```
    /// # use std::io;
    /// # fn try_main() -> io::Result<()> {
    /// use std::fs::File;
    /// use pgn_reader::BufferedReader;
    ///
    /// let file = File::open("example.pgn")?;
    /// let reader = BufferedReader::new(file);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(inner: R) -> BufferedReader<R> {
        BufferedReader {
            inner,
            buffer: Buffer::new(),
        }
    }

    /// Read a single game, if any, and returns the result produced by the
    /// visitor. Returns Ok(None) if the underlying reader is empty.
    ///
    /// # Errors
    ///
    /// * I/O error from the underlying reader.
    /// * Irrecoverable parser errors.
    pub fn read_game<V: Visitor>(&mut self, visitor: &mut V) -> io::Result<Option<V::Result>> {
        ReadPgn::read_game(self, visitor)
    }

    /// Skip a single game, if any.
    ///
    /// # Errors
    ///
    /// * I/O error from the underlying reader.
    /// * Irrecoverable parser errors.
    pub fn skip_game<V: Visitor>(&mut self) -> io::Result<bool> {
        ReadPgn::skip_game(self)
    }

    /// Read all games.
    ///
    /// # Errors
    ///
    /// * I/O error from the underlying reader.
    /// * Irrecoverable parser errors.
    pub fn read_all<V: Visitor>(&mut self, visitor: &mut V) -> io::Result<()> {
        while self.read_game(visitor)?.is_some() {}
        Ok(())
    }

    /// Create an iterator over all games.
    ///
    /// # Errors
    ///
    /// * I/O error from the underlying reader.
    /// * Irrecoverable parser errors.
    pub fn into_iter<V: Visitor>(self, visitor: &mut V) -> IntoIter<'_, V, R> {
        IntoIter {
            reader: self,
            visitor,
        }
    }

    /// Gets the remaining bytes in the buffer and the underlying reader.
    pub fn into_inner(self) -> Chain<Cursor<Buffer>, R> {
        Cursor::new(self.buffer).chain(self.inner)
    }

    /// Returns whether the reader has another game to parse, but does not
    /// actually parse it.
    ///
    /// # Errors
    ///
    /// * I/O error from the underlying reader.
    pub fn has_more(&mut self) -> io::Result<bool> {
        self.skip_bom()?;
        self.skip_whitespace()?;
        Ok(self.fill_buffer_and_peek()?.is_some())
    }
}

impl<R: Read> ReadPgn for BufferedReader<R> {
    type Err = io::Error;

    fn fill_buffer_and_peek(&mut self) -> io::Result<Option<u8>> {
        while self.buffer.inner.available_data() < MIN_BUFFER_SIZE {
            let remainder = self.buffer.inner.space();
            let size = self.inner.read(remainder)?;

            if size == 0 {
                break;
            }

            self.buffer.inner.fill(size);
        }

        Ok(self.buffer.inner.data().first().cloned())
    }

    fn invalid_data() -> io::Error {
        io::Error::from(io::ErrorKind::InvalidData)
    }

    fn buffer(&self) -> &[u8] {
        self.buffer.inner.data()
    }

    fn consume(&mut self, bytes: usize) {
        self.buffer.inner.consume(bytes);
    }

    fn peek(&self) -> Option<u8> {
        self.buffer.inner.data().first().cloned()
    }
}

/// Iterator returned by
/// [`BufferedReader::into_iter()`](struct.BufferedReader.html#method.into_iter).
#[derive(Debug)]
#[must_use]
pub struct IntoIter<'a, V: 'a, R> {
    visitor: &'a mut V,
    reader: BufferedReader<R>,
}

impl<'a, V: Visitor, R: Read> Iterator for IntoIter<'a, V, R> {
    type Item = Result<V::Result, io::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.reader.read_game(self.visitor) {
            Ok(Some(result)) => Some(Ok(result)),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct _AssertObjectSafe<R>(Box<BufferedReader<R>>);

    #[derive(Default)]
    struct GameCounter {
        count: usize,
    }

    impl Visitor for GameCounter {
        type Result = ();

        fn end_game(&mut self) {
            self.count += 1;
        }
    }

    #[test]
    fn test_empty_game() -> Result<(), io::Error> {
        let mut counter = GameCounter::default();
        let mut reader = BufferedReader::new(io::Cursor::new(b"  "));
        reader.read_game(&mut counter)?;
        assert_eq!(counter.count, 0);
        Ok(())
    }

    #[test]
    fn test_trailing_space() -> Result<(), io::Error> {
        let mut counter = GameCounter::default();
        let mut reader = BufferedReader::new(io::Cursor::new(b"1. e4 1-0\n\n\n\n\n  \n"));
        reader.read_game(&mut counter)?;
        assert_eq!(counter.count, 1);
        reader.read_game(&mut counter)?;
        assert_eq!(counter.count, 1);
        Ok(())
    }

    #[test]
    fn test_nag() -> Result<(), io::Error> {
        struct NagCollector {
            nags: Vec<Nag>,
        }

        impl Visitor for NagCollector {
            type Result = ();

            fn nag(&mut self, nag: Nag) {
                self.nags.push(nag);
            }

            fn end_game(&mut self) {}
        }

        let mut collector = NagCollector { nags: Vec::new() };
        let mut reader = BufferedReader::new(io::Cursor::new(b"1.f3! e5$71 2.g4?? Qh4#!?"));
        reader.read_game(&mut collector)?;
        assert_eq!(
            collector.nags,
            vec![Nag::GOOD_MOVE, Nag(71), Nag::BLUNDER, Nag::SPECULATIVE_MOVE]
        );
        Ok(())
    }

    #[test]
    fn test_null_moves() -> Result<(), io::Error> {
        struct SanCollector {
            sans: Vec<San>,
        }

        impl Visitor for SanCollector {
            type Result = ();

            fn san(&mut self, san: SanPlus) {
                self.sans.push(san.san);
            }

            fn end_game(&mut self) {}
        }

        let mut collector = SanCollector { sans: Vec::new() };
        let mut reader = BufferedReader::new(io::Cursor::new(b"1. e4 -- 2. Nf3 -- 3. -- e5"));
        reader.read_game(&mut collector)?;
        assert_eq!(collector.sans.len(), 6);
        assert_ne!(collector.sans[0], San::Null);
        assert_eq!(collector.sans[1], San::Null);
        assert_ne!(collector.sans[2], San::Null);
        assert_eq!(collector.sans[3], San::Null);
        assert_eq!(collector.sans[4], San::Null);
        assert_ne!(collector.sans[5], San::Null);
        Ok(())
    }
}
