use futures::{Poll, Stream};

use mime::{self, Mime};

use std::{io, str};

use server::{Multipart, BodyChunk, StreamError, httparse, twoway};
use server::boundary::BoundaryFinder;

use self::httparse::{EMPTY_HEADER, Status};

use helpers::*;

const MAX_BUF_LEN: usize = 1024;
const MAX_HEADERS: usize = 4;

#[derive(Clone, Default, Debug)]
pub struct FieldHeaders {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<Mime>,
}

impl FieldHeaders {
    /// `true` if `content_type` is `None` or `text/*` (such as `text/plain`).
    ///
    /// **Note**: this does not guarantee that it is compatible with `FieldData::read_text()`;
    /// if you intend to support more than just ASCII/UTF-8 you will want to look into a more robust
    /// implementation.
    pub fn is_text(&self) -> bool {
        self.content_type.as_ref().map_or(true, |ct| ct.type_() == mime::TEXT)
    }

    /* TODO: when https://github.com/hyperium/mime/pull/60 is merged
    /// The character set of this field, if provided.
    pub fn charset(&self) -> Option<&str> {
        self.content_type.as_ref().and_then(|ct| ct.get_param(mime::CHARSET)).map(Into::into)
    }
    */
}

#[derive(Debug, Default)]
pub struct ReadHeaders {
    accumulator: Vec<u8>
}

impl ReadHeaders {
    pub fn read_headers<S: Stream>(&mut self, stream: &mut BoundaryFinder<S>) -> PollOpt<FieldHeaders, S::Error>
    where S::Item: BodyChunk, S::Error: StreamError {
        loop {
            let chunk = match try_ready!(stream.poll()) {
                Some(chunk) => chunk,
                None => return if !self.accumulator.is_empty() {
                    error("unexpected end of stream")
                } else {
                    ready(None)
                },
            };

            // End of the headers section is signalled by a double-CRLF
            if let Some(header_end) = twoway::find_bytes(chunk.as_slice(), b"\r\n\r\n") {
                // Split after the double-CRLF because we don't want to yield it and httparse expects it
                let (headers, rem) = chunk.split_at(header_end + 4);
                stream.push_chunk(rem);

                if !self.accumulator.is_empty() {
                    self.accumulator.extend_from_slice(headers.as_slice());
                    let headers = parse_headers(&self.accumulator)?;
                    self.accumulator.clear();

                    return ready(Some(headers));
                } else {
                    return ready(Some(parse_headers(headers.as_slice())?));
                }
            } else if let Some(split_idx) = header_end_split(&self.accumulator, chunk.as_slice()) {
                let (head, tail) = chunk.split_at(split_idx);
                self.accumulator.extend_from_slice(head.as_slice());
                stream.push_chunk(tail);
                continue;
            }

            if self.accumulator.len().saturating_add(chunk.len()) > MAX_BUF_LEN {
                return error("headers section too long or trailing double-CRLF missing");
            }

            self.accumulator.extend_from_slice(chunk.as_slice());
        }
    }
}

const CRLF2: &[u8] = b"\r\n\r\n";

/// Check if the double-CRLF falls between chunk boundaries, and if so, the split index of
/// the second boundary
fn header_end_split(first: &[u8], second: &[u8]) -> Option<usize> {
    fn split_subcheck(start: usize, first: &[u8], second: &[u8]) -> bool {
        first.len() >= start && first[first.len() - start ..].iter().chain(second).take(4).eq(CRLF2)
    }

    if split_subcheck(3, first, second) {
        Some(1)
    } else if split_subcheck(2, first, second) {
        Some(2)
    } else if split_subcheck(1, first, second) {
        Some(3)
    } else {
        None
    }
}

fn parse_headers<E: StreamError>(bytes: &[u8]) -> Result<FieldHeaders, E> {
    debug_assert!(bytes.ends_with(b"\r\n\r\n"),
                  "header byte sequence does not end with `\\r\\n\\r\\n`: {}",
                  show_bytes(bytes));

    let mut header_buf = [EMPTY_HEADER; MAX_HEADERS];

    let headers = match httparse::parse_headers(bytes, &mut header_buf) {
        Ok(Status::Complete((_, headers))) => headers,
        Ok(Status::Partial) => ret_err!("field headers incomplete: {}", show_bytes(bytes)),
        Err(e) => ret_err!("error parsing headers: {}; from buffer: {}", e, show_bytes(bytes)),
    };

    let mut out_headers = FieldHeaders::default();

    for header in headers {
        let str_val = str::from_utf8(header.value)
            .or_else(|_| error("multipart field headers must be UTF-8 encoded"))?
            .trim();

        match header.name {
            "Content-Disposition" => parse_cont_disp_val(str_val, &mut out_headers)?,
            "Content-Type" => out_headers.content_type = Some(str_val.parse::<Mime>()
                 .or_else(|_| ret_err!("could not parse MIME type from {:?}", str_val))?),
            _ => (),
        }
    }

    Ok(out_headers)
}

fn parse_cont_disp_val<E: StreamError>(val: &str, out: &mut FieldHeaders) -> Result<(), E> {
    // Only take the first section, the rest can be in quoted strings that we want to handle
    let mut sections = val.splitn(1, ';').map(str::trim);

    match sections.next() {
        Some("form-data") => (),
        Some(other) => ret_err!("unexpected multipart field Content-Disposition: {}", other),
        None => return error("each multipart field requires a Content-Disposition: form-data header"),
    }

    let mut rem = sections.next().unwrap_or("");

    while let Some((key, val, rest)) = parse_keyval(rem) {
        rem = rest;

        match key {
            "name" => out.name = val.to_string(),
            "filename" => out.filename = Some(val.to_string()),
            _ => debug!("unknown key-value pair in Content-Disposition: {:?} = {:?}", key, val),
        }
    }

    if out.name.is_empty() {
        return ret_err!("expected 'name' attribute in Content-Disposition: {}", val);
    }

    Ok(())
}

fn parse_keyval(input: &str) -> Option<(&str, &str, &str)> {
    let (name, rest) = try_opt!(param_name(input));
    let (val, rest) = try_opt!(param_val(rest));

    Some((name, val, rest))
}

fn param_name(input: &str) -> Option<(&str, &str)> {
    let mut splits = input.trim_left_matches(&[' ', ';'][..]).splitn(1, '=');

    let name = try_opt!(splits.next()).trim();
    let rem = splits.next().unwrap_or("");

    Some((name, rem))
}

fn param_val(input: &str) -> Option<(&str, &str)> {
    let pat: &[char] = &['"'];
    let mut splits = input.splitn(2, pat);

    let token = try_opt!(splits.next()).trim();

    // the value doesn't have to be in quotes if it doesn't contain forbidden chars like `;`
    if !token.is_empty() {
        let mut splits = token.splitn(1, ';');
        let token = try_opt!(splits.next()).trim();
        let rem = splits.next().unwrap_or("");

        return Some((token, rem));
    }

    let qstr = try_opt!(splits.next()).trim();
    let rem = splits.next().unwrap_or_else(|| { warn!("unterminated quote: {:?}", qstr); "" });

    Some((qstr, rem))
}

#[test]
fn test_header_end_split() {
    assert_eq!(header_end_split(b"\r\n\r", b"\n"), Some(1));
    assert_eq!(header_end_split(b"\r\n", b"\r\n"), Some(2));
    assert_eq!(header_end_split(b"\r", b"\n\r\n"), Some(3));
    assert_eq!(header_end_split(b"\r\n\r\n", b"FOOBAR"), None);
    assert_eq!(header_end_split(b"FOOBAR", b"\r\n\r\n"), None);
}