use phf::{Map, phf_map};

use crate::err::ProcessingResult;
use crate::proc::{Processor, ProcessorRange};
use crate::spec::codepoint::{is_digit, is_whitespace};
use crate::unit::entity::{EntityType, parse_entity};

fn is_double_quote(c: u8) -> bool {
    c == b'"'
}

fn is_single_quote(c: u8) -> bool {
    c == b'\''
}

// Valid attribute quote characters.
// See https://html.spec.whatwg.org/multipage/introduction.html#intro-early-example for spec.
fn is_attr_quote(c: u8) -> bool {
    // Backtick is not a valid quote character according to spec.
    is_double_quote(c) || is_single_quote(c)
}

// Valid unquoted attribute value characters.
// See https://html.spec.whatwg.org/multipage/syntax.html#unquoted for spec.
fn is_unquoted_val_char(c: u8) -> bool {
    !(is_whitespace(c) || c == b'"' || c == b'\'' || c == b'=' || c == b'<' || c == b'>' || c == b'`')
}

fn is_not_unquoted_val_char(c: u8) -> bool {
    !is_unquoted_val_char(c)
}

fn entity_requires_semicolon(next_char: u8) -> bool {
    is_digit(next_char) || next_char == b';'
}

// See comment in `process_attr_value` for full description of why these intentionally do not have semicolons.
static ENCODED: Map<u8, &'static [u8]> = phf_map! {
    b'\'' => b"&#39",
    b'"' => b"&#34",
    b'>' => b"&gt",
    // Whitespace characters as defined by spec in crate::spec::codepoint::is_whitespace.
    b'\x09' => b"&#9",
    b'\x0a' => b"&#10",
    b'\x0c' => b"&#12",
    b'\x0d' => b"&#13",
    b'\x20' => b"&#32",
};

#[derive(Clone, Copy)]
enum CharType {
    Start,
    End,
    Entity(EntityType),
    // Normal needs associated character to be able to write it.
    Normal(u8),
    // Whitespace needs associated character to determine cost of encoding it.
    Whitespace(u8),
    SingleQuote,
    DoubleQuote,
}

impl CharType {
    fn from_char(c: u8) -> CharType {
        match c {
            b'"' => CharType::DoubleQuote,
            b'\'' => CharType::SingleQuote,
            c => if is_whitespace(c) { CharType::Whitespace(c) } else { CharType::Normal(c) },
        }
    }

    fn is_start_or_end(&self) -> bool {
        match self {
            CharType::Start | CharType::End => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum DelimiterType {
    Double,
    Single,
    Unquoted,
}

struct Metrics {
    count_double_quotation: usize,
    // Some encoded double quotes may require semicolons, so lengths vary.
    total_double_quote_encoded_length: usize,
    count_single_quotation: usize,
    // Some encoded double quotes may require semicolons, so lengths vary.
    total_single_quote_encoded_length: usize,
    // NOTE: This count is amount after any trimming and collapsing of whitespace.
    count_whitespace: usize,
    // Since whitespace characters have varying encoded lengths, also calculate total length if all of them had to be encoded.
    total_whitespace_encoded_length: usize,
}

impl Metrics {
    fn unquoted_len(&self, raw_val: &[u8]) -> usize {
        // TODO VERIFY (including control characters and Unicode noncharacters) Browsers seem to simply consider any characters until whitespace part of an unquoted attribute value, despite the spec (and hyperbuild) having more restrictions on allowed characters.
        // Costs for encoding first and last characters if going with unquoted attribute value.
        // NOTE: Don't need to consider whitespace for either as all whitespace will be encoded and counts as part of `total_whitespace_encoded_length`.
        // Need to consider semicolon in any encoded entity in case first char is followed by semicolon or digit.
        let first_char_encoded_semicolon = raw_val.get(1).filter(|&&c| entity_requires_semicolon(c)).is_some() as usize;
        let first_char_encoding_cost = match raw_val.first() {
            Some(b'"') => ENCODED[&b'"'].len() + first_char_encoded_semicolon,
            Some(b'\'') => ENCODED[&b'\''].len() + first_char_encoded_semicolon,
            _ => 0,
        };
        let last_char_encoding_cost = match raw_val.last() {
            Some(b'>') => ENCODED[&b'>'].len(),
            _ => 0,
        };

        // Replace all whitespace chars with encoded versions.
        let raw_len = raw_val.len() - self.count_whitespace + self.total_whitespace_encoded_length;
        // Replace first char with encoded version if necessary.
        let raw_len = raw_len - (first_char_encoding_cost > 0) as usize + first_char_encoding_cost;
        // Replace last char with encoded version if necessary.
        let raw_len = raw_len - (last_char_encoding_cost > 0) as usize + last_char_encoding_cost;
        raw_len
    }

    fn single_quoted_len(&self, raw_len: usize) -> usize {
        // Replace all single quote chars with encoded version.
        let raw_len = raw_len - self.count_single_quotation + self.total_single_quote_encoded_length;
        // Delimiter quotes.
        let raw_len = raw_len + 2;
        raw_len
    }

    fn double_quoted_len(&self, raw_len: usize) -> usize {
        // Replace all double quote chars with encoded version.
        let raw_len = raw_len - self.count_double_quotation + self.total_double_quote_encoded_length;
        // Delimiter quotes.
        let raw_len = raw_len + 2;
        raw_len
    }

    fn get_optimal_delimiter_type(&self, raw_val: &[u8]) -> (DelimiterType, usize) {
        // When all equal, prefer double quotes to all and single quotes to unquoted.
        let mut min = (DelimiterType::Double, self.double_quoted_len(raw_val.len()));

        let single = (DelimiterType::Single, self.single_quoted_len(raw_val.len()));
        if single.1 < min.1 {
            min = single;
        };

        let unquoted = (DelimiterType::Unquoted, self.unquoted_len(raw_val));
        if unquoted.1 < min.1 {
            min = unquoted;
        };

        min
    }
}

pub fn skip_attr_value(proc: &mut Processor) -> ProcessingResult<()> {
    let src_delimiter = chain!(proc.match_pred(is_attr_quote).discard().maybe_char());
    let delim_pred = match src_delimiter {
        Some(b'"') => is_double_quote,
        Some(b'\'') => is_single_quote,
        None => is_not_unquoted_val_char,
        _ => unreachable!(),
    };
    chain!(proc.match_while_not_pred(delim_pred).discard());
    if let Some(c) = src_delimiter {
        chain!(proc.match_char(c).require_with_reason("attribute value closing delimiter quote")?.discard());
    };
    Ok(())
}

pub struct ProcessedAttrValue {
    pub delimiter: DelimiterType,
    pub value: Option<ProcessorRange>,
}

fn handle_whitespace_char_type(c: u8, proc: &mut Processor, metrics: &mut Metrics) -> () {
    proc.write(c);
    metrics.count_whitespace += 1;
    metrics.total_whitespace_encoded_length += ENCODED[&c].len();
}

// Minifying attribute value in place (i.e. without using extra memory) is tricky.
// To do in place, the read position must always be greater than write.
// When processing left to right, read must always be >= write.
// When processing right to left, read must always be <= write.
// Three ideas that do not work:
// 1. Write right to left, and start from processed end.
// 2. Write right to left, and start from source end, and then do a memory move at the end.
// 3. Write left to right, and start from source start.
// We can't always use option 1, as we expect the processed attribute value to be smaller than source.
// We can't always use option 2 or 3, as we might encode something early on which would cause write position to overtake read position and overwrite unread source code.
// We could use option 2 or 3 if we shift everything down every time we write more than 1 character, but this is not always possible as the code slice might have not enough room; it would also be very slow.
// None of the above even considers trimming whitespace.
// Current working strategy:
// Read left to right, writing an unquoted value with all entities decoded (including special chars like quotes and whitespace).
// The resulting written value would have the minimum possible value length.
// Since the actual processed value would have a length equal or greater to it (e.g. it might be quoted, or some characters might get encoded), we can then read minimum value right to left and start writing from actual processed value length (which is calculated), quoting/encoding as necessary.
pub fn process_attr_value(proc: &mut Processor, should_collapse_and_trim_ws: bool) -> ProcessingResult<ProcessedAttrValue> {
    let start = proc.checkpoint();
    let src_delimiter = chain!(proc.match_pred(is_attr_quote).discard().maybe_char());
    let delim_pred = match src_delimiter {
        Some(b'"') => is_double_quote,
        Some(b'\'') => is_single_quote,
        None => is_not_unquoted_val_char,
        _ => unreachable!(),
    };

    // Stage 1: read and collect metrics on attribute value characters.
    let mut metrics = Metrics {
        count_double_quotation: 0,
        total_double_quote_encoded_length: 0,
        count_single_quotation: 0,
        total_single_quote_encoded_length: 0,
        count_whitespace: 0,
        total_whitespace_encoded_length: 0,
    };
    // Set to true when one or more immediately previous characters were whitespace and deferred for processing after the contiguous whitespace.
    // NOTE: Only used if `should_collapse_and_trim_ws`.
    let mut currently_in_whitespace = false;
    // TODO Comment.
    let uep = &mut proc.start_preventing_unintentional_entities();

    let mut last_char_type: CharType = CharType::Start;
    loop {
        let char_type = if chain!(proc.match_pred(delim_pred).matched()) {
            // DO NOT BREAK HERE. More processing is done afterwards upon reaching end.
            CharType::End
        } else if chain!(proc.match_char(b'&').matched()) {
            // Don't write entity here; wait until any previously ignored whitespace has been handled.
            match parse_entity(proc, true)? {
                EntityType::Ascii(c) => CharType::from_char(c),
                entity => CharType::Entity(entity),
            }
        } else {
            CharType::from_char(proc.skip()?)
        };

        if should_collapse_and_trim_ws {
            if let CharType::Whitespace(_) = char_type {
                // Ignore this whitespace character, but mark the fact that we are currently in contiguous whitespace.
                currently_in_whitespace = true;
                continue;
            };

            // Now past whitespace (e.g. moved to non-whitespace char or end of attribute value). Either:
            // - ignore contiguous whitespace (i.e. do nothing) if we are currently at beginning or end of value; or
            // - collapse contiguous whitespace (i.e. count as one whitespace char) otherwise.
            if currently_in_whitespace && !char_type.is_start_or_end() {
                // Collect current collapsed contiguous whitespace that was ignored previously.
                // Update `last_char_type` as this space character will become the new "previous character", important later when checking if previous character as an entity requires semicolon.
                last_char_type = CharType::Whitespace(b' ');
                handle_whitespace_char_type(b' ', proc, &mut metrics);
            };
            currently_in_whitespace = false;
        };

        match char_type {
            CharType::Start => unreachable!(),
            CharType::End => {
                break;
            }
            CharType::Entity(e) => {
                e.keep(proc);
            }
            CharType::Whitespace(c) => {
                handle_whitespace_char_type(c, proc, &mut metrics);
            }
            CharType::SingleQuote => {
                proc.write(b'\'');
                metrics.count_single_quotation += 1;
                metrics.total_single_quote_encoded_length += ENCODED[&b'\''].len();
            }
            CharType::DoubleQuote => {
                proc.write(b'\"');
                metrics.count_double_quotation += 1;
                metrics.total_double_quote_encoded_length += ENCODED[&b'"'].len();
            }
            CharType::Normal(c) => {
                proc.write(c);
                // If the last char written was a quote or whitespace, and this character would require the previous character, encoded as an entity, to have a semicolon, then add one more character to encoded length in metrics.
                if entity_requires_semicolon(c) {
                    match last_char_type {
                        CharType::SingleQuote => metrics.total_single_quote_encoded_length += 1,
                        CharType::DoubleQuote => metrics.total_double_quote_encoded_length += 1,
                        CharType::Whitespace(_) => metrics.total_whitespace_encoded_length += 1,
                        _ => {}
                    };
                };
            }
        };
        proc.update(uep);
        last_char_type = char_type;
    };
    if let Some(c) = src_delimiter {
        chain!(proc.match_char(c).require_with_reason("attribute value closing delimiter quote")?.discard());
    };
    proc.end(uep);
    let minimum_value = proc.written_range(start);
    // If minimum value is empty, return now before trying to read out of range later.
    // (Reading starts at one character before end of minimum value.)
    if minimum_value.empty() {
        return Ok(ProcessedAttrValue {
            delimiter: DelimiterType::Unquoted,
            value: None,
        });
    };

    // Stage 2: optimally minify attribute value using metrics.
    // TODO Optimise: don't do anything if minimum is already optimal.
    let (optimal_delimiter, optimal_len) = metrics.get_optimal_delimiter_type(&proc[minimum_value]);
    let optimal_delimiter_char = match optimal_delimiter {
        DelimiterType::Double => Some(b'"'),
        DelimiterType::Single => Some(b'\''),
        _ => None,
    };

    proc.reserve_output(optimal_len - minimum_value.len());
    let optimal_slice = &mut proc[start.get_written_range_since(optimal_len)];
    let mut write = optimal_slice.len() - 1;
    // Write opening delimiter, if any.
    if let Some(c) = optimal_delimiter_char {
        optimal_slice[write] = c;
        write -= 1;
    };
    for read in (0..minimum_value.len()).rev() {
        // First and last should always be based on minimum_read_next.
        // First is not always when optimal_write_next at zero.
        let is_first = read == 0;
        let is_last = read == minimum_value.len() - 1;
        let c = optimal_slice[read];
        // TODO Comment is_first and is_last could both be true,
        let should_encode = match (c, optimal_delimiter, is_first, is_last) {
            (b'>', DelimiterType::Unquoted, _, true) => true,
            (c, DelimiterType::Unquoted, true, _) => is_attr_quote(c),
            (c, DelimiterType::Unquoted, _, _) => is_whitespace(c),
            (b'\'', DelimiterType::Single, _, _) => true,
            (b'"', DelimiterType::Double, _, _) => true,
            _ => false,
        };
        if should_encode {
            // Encoded entities do not have a semicolon by default, and a `;` is only added if required to prevent any following characters from unintentionally being part of an entity.
            // This is done to save space, and to prevent overwriting source code. Why? Because it's possible for a entity without a semicolon to decode to a character that would later be encoded. If the output entity always has a semicolon, this might cause written code to be longer than source code.
            // For example, consider `<div class=&gt>`.
            // Numeric entities simply need to check if the following character is a base 10 digit.
            // The last character encoded as an entity never needs a semicolon:
            // - For quoted values, it's always a quote and will never be encoded.
            // - Unquoted attribute values are only ever followed by a space (written by hyperbuild) or the opening tag delimiter ('>').
            // '&gt' is always safe as it's only used for any '>' as the last character of an unquoted value.
            let should_add_semicolon = !is_last && entity_requires_semicolon(optimal_slice[write + 1]);
            let encoded = ENCODED[&c];
            // Make extra room for entity (only have room for 1 char currently).
            write -= encoded.len() + should_add_semicolon as usize - 1;
            optimal_slice[write..write + encoded.len()].copy_from_slice(encoded);
            if should_add_semicolon {
                optimal_slice[write + encoded.len()] = b';';
            };
        } else {
            optimal_slice[write] = c;
        };

        // Break before decrementing to prevent underflow.
        if is_first {
            break;
        };

        write -= 1;
    };
    // Write closing delimiter, if any.
    if let Some(c) = optimal_delimiter_char {
        // Don't use `write` as index, as it will not have decremented on last iteration of previous loop to zero if quoted.
        optimal_slice[0] = c;
    };

    Ok(ProcessedAttrValue {
        delimiter: optimal_delimiter,
        value: Some(proc.written_range(start)).filter(|r| !r.empty()),
    })
}
