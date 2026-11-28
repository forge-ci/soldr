use std::io;

/// Pull-driven form-query decoding. Each nonempty `&`-separated field yields
/// one name/value pair; a missing `=` means an empty value. Duplicate names and
/// ordering are preserved for application policy. `+` is a space in queries.
/// Malformed escapes and non-UTF-8 fields yield `InvalidData`, not replacement
/// characters. Errors consume just that field. No decoded pair queue is built.
#[derive(Debug)]
pub struct QueryPairs<'a> {
    remaining: std::str::Split<'a, char>,
}

impl<'a> QueryPairs<'a> {
    pub(super) fn new(query: &'a str) -> Self {
        Self {
            remaining: query.split('&'),
        }
    }
}

impl Iterator for QueryPairs<'_> {
    type Item = io::Result<(String, String)>;
    fn next(&mut self) -> Option<Self::Item> {
        let field = self.remaining.find(|field| !field.is_empty())?;
        let (name, value) = field.split_once('=').unwrap_or((field, ""));
        Some(decode(name, true).and_then(|name| Ok((name, decode(value, true)?))))
    }
}

pub(super) fn decode(input: &str, plus_space: bool) -> io::Result<String> {
    let mut output = Vec::with_capacity(input.len());
    let mut bytes = input.bytes();
    while let Some(byte) = bytes.next() {
        output.push(match byte {
            b'%' => {
                let high = bytes.next().and_then(hex).ok_or_else(invalid)?;
                let low = bytes.next().and_then(hex).ok_or_else(invalid)?;
                (high << 4) | low
            }
            b'+' if plus_space => b' ',
            byte => byte,
        });
    }
    String::from_utf8(output).map_err(|_| invalid())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid UTF-8 percent-encoded target component",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_query_fields_do_not_hide_errors_or_consume_next_pair() {
        for bad in ["%", "%A", "%GG", "%FF", "%C0%AF"] {
            let query = format!("bad={bad}&next=ok");
            let mut pairs = QueryPairs::new(&query);
            assert_eq!(
                pairs.next().unwrap().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(pairs.next().unwrap().unwrap(), ("next".into(), "ok".into()));
            assert!(pairs.next().is_none());
        }
    }

    #[test]
    fn path_decoding_is_once_only_and_does_not_normalize_security_policy() {
        assert_eq!(
            decode("/%252e%252e/%2f+%5c", false).unwrap(),
            "/%2e%2e//+\\"
        );
        assert_eq!(decode("/%E2%98%83", false).unwrap(), "/☃");
        assert_eq!(decode("/a%00b", false).unwrap(), "/a\0b");
        assert!(decode("/%FF", false).is_err());
    }
}
