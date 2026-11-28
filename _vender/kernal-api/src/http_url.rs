//! Owned HTTP(S) URL handling for the client: parsing, the request target,
//! the `Host` header, origins and RFC 3986 reference resolution for redirects.

use std::io;

use super::invalid;

/// A validated absolute `http`/`https` URL without credentials or fragment.
#[derive(Clone, Debug)]
pub(super) struct Target {
    https: bool,
    /// Lowercase ASCII host as written in the authority; IPv6 keeps brackets.
    host: String,
    port: u16,
    explicit_port: bool,
    /// Origin-form request target: absolute path plus optional query.
    path_and_query: String,
}

impl Target {
    pub(super) fn parse(url: &str) -> io::Result<Self> {
        // The fragment is client-side only and never sent.
        let url = url.split_once('#').map_or(url, |(url, _)| url);
        reject_unconverted_host(url)?;
        let url = percent_encode_target(url);
        let uri: hyper::Uri = url.parse().map_err(|_| invalid("invalid HTTP URL"))?;
        let https = match uri.scheme_str() {
            Some(scheme) if scheme.eq_ignore_ascii_case("https") => true,
            Some(scheme) if scheme.eq_ignore_ascii_case("http") => false,
            _ => return Err(invalid("HTTP URL scheme or embedded credentials rejected")),
        };
        let authority = uri.authority().ok_or_else(|| invalid("invalid HTTP URL"))?;
        if authority.as_str().contains('@') {
            return Err(invalid("HTTP URL scheme or embedded credentials rejected"));
        }
        let host = authority.host();
        if host.is_empty() {
            return Err(invalid("invalid HTTP URL"));
        }
        let default_port = if https { 443 } else { 80 };
        let port = match authority.port() {
            Some(port) if !port.as_str().is_empty() => port.as_u16(),
            _ => default_port,
        };
        let path = remove_dot_segments(if uri.path().is_empty() {
            "/"
        } else {
            uri.path()
        });
        let path_and_query = match uri.query() {
            Some(query) => format!("{path}?{query}"),
            None => path,
        };
        Ok(Self {
            https,
            host: host.to_ascii_lowercase(),
            port,
            explicit_port: port != default_port,
            path_and_query,
        })
    }

    pub(super) fn is_https(&self) -> bool {
        self.https
    }

    pub(super) fn same_origin(&self, other: &Self) -> bool {
        self.https == other.https && self.host == other.host && self.port == other.port
    }

    fn scheme(&self) -> &'static str {
        if self.https {
            "https"
        } else {
            "http"
        }
    }

    /// `host[:port]`, omitting the scheme's default port.
    pub(super) fn host_header(&self) -> String {
        if self.explicit_port {
            format!("{}:{}", self.host, self.port)
        } else {
            self.host.clone()
        }
    }

    /// Absolute request URI. The client derives the pool key and wire target.
    pub(super) fn request_uri(&self) -> io::Result<hyper::Uri> {
        format!(
            "{}://{}{}",
            self.scheme(),
            self.host_header(),
            self.path_and_query
        )
        .parse()
        .map_err(|_| invalid("invalid HTTP URL"))
    }

    /// Resolve a `Location` reference against this URL (RFC 3986 5.2). The
    /// result is re-validated by [`Target::parse`] before any connection.
    pub(super) fn join(&self, reference: &str) -> String {
        let reference = reference.split_once('#').map_or(reference, |(r, _)| r);
        let origin = format!("{}://{}", self.scheme(), self.host_header());
        if has_scheme(reference) {
            reference.to_owned()
        } else if reference.starts_with("//") {
            format!("{}:{reference}", self.scheme())
        } else if reference.starts_with('/') {
            format!("{origin}{reference}")
        } else if reference.is_empty() {
            format!("{origin}{}", self.path_and_query)
        } else {
            let (path, _) = self
                .path_and_query
                .split_once('?')
                .unwrap_or((&self.path_and_query, ""));
            if reference.starts_with('?') {
                format!("{origin}{path}{reference}")
            } else {
                let directory = &path[..=path.rfind('/').unwrap_or(0)];
                format!("{origin}{directory}{reference}")
            }
        }
    }
}

/// Hosts are used exactly as written: there is no IDNA mapping and no
/// percent-decoding, so a host that would need either is rejected rather than
/// sent somewhere other than the caller meant. Pre-encoded `xn--` labels are
/// plain ASCII and accepted.
fn reject_unconverted_host(url: &str) -> io::Result<()> {
    let Some((_, rest)) = url.split_once("://") else {
        return Ok(());
    };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.is_ascii() && !host.contains('%') {
        return Ok(());
    }
    Err(invalid(
        "HTTP URL host must be ASCII; IDNA and percent-encoded hosts are not converted",
    ))
}

/// Percent-encode the path and query the way the WHATWG URL parser does for
/// `http`/`https` (non-ASCII, controls, space and its path/query sets), so a
/// caller's unencoded target is sent encoded rather than as raw bytes.
fn percent_encode_target(url: &str) -> std::borrow::Cow<'_, str> {
    let start = url.find("://").map_or(0, |scheme| {
        let authority = scheme + 3;
        url[authority..]
            .find(['/', '?'])
            .map_or(url.len(), |end| authority + end)
    });
    let (origin, target) = url.split_at(start);
    let needs = |byte: u8, query: bool| {
        !(0x21..0x7f).contains(&byte)
            || matches!(byte, b'"' | b'<' | b'>')
            || (query && byte == b'\'')
            || (!query && matches!(byte, b'`' | b'{' | b'}'))
    };
    if !target
        .bytes()
        .any(|byte| needs(byte, true) || needs(byte, false))
    {
        return std::borrow::Cow::Borrowed(url);
    }
    let mut encoded = String::with_capacity(url.len() + 16);
    encoded.push_str(origin);
    let mut query = false;
    for byte in target.bytes() {
        query |= byte == b'?';
        if needs(byte, query) {
            encoded.push_str(&format!("%{byte:02X}"));
        } else {
            encoded.push(char::from(byte));
        }
    }
    std::borrow::Cow::Owned(encoded)
}

fn has_scheme(reference: &str) -> bool {
    let Some((scheme, _)) = reference.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// RFC 3986 5.2.4 on an absolute path.
fn remove_dot_segments(path: &str) -> String {
    let mut output: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        match *segment {
            "." | ".." => {
                if *segment == ".." {
                    output.pop();
                }
                if last {
                    output.push("");
                }
            }
            segment => output.push(segment),
        }
    }
    format!("/{}", output.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_header_omits_default_ports_and_keeps_ipv6_brackets() {
        for (url, host) in [
            ("http://Example.COM/", "example.com"),
            ("http://example.com:80/", "example.com"),
            ("https://example.com:443/", "example.com"),
            ("http://example.com:443/", "example.com:443"),
            ("https://example.com:80/", "example.com:80"),
            ("http://example.com:/", "example.com"),
            ("http://[::1]/", "[::1]"),
            ("https://[::1]:8443/", "[::1]:8443"),
        ] {
            assert_eq!(Target::parse(url).unwrap().host_header(), host, "{url}");
        }
    }

    #[test]
    fn request_uri_preserves_target_and_normalizes_authority() {
        assert_eq!(
            Target::parse("https://example.com/a?b")
                .unwrap()
                .request_uri()
                .unwrap(),
            "https://example.com/a?b"
        );
        assert_eq!(
            Target::parse("http://[::1]/")
                .unwrap()
                .request_uri()
                .unwrap(),
            "http://[::1]/"
        );
    }

    #[test]
    fn origins_compare_scheme_host_and_effective_port() {
        let base = Target::parse("http://example.com/a").unwrap();
        for (other, same) in [
            ("http://EXAMPLE.com:80/b", true),
            ("http://example.com:8080/", false),
            ("https://example.com/", false),
            ("http://other.example.com/", false),
        ] {
            assert_eq!(
                base.same_origin(&Target::parse(other).unwrap()),
                same,
                "{other}"
            );
        }
    }

    #[test]
    fn dot_segments_are_removed_from_request_targets() {
        for (path, expected) in [
            ("/", "/"),
            ("/a/../b", "/b"),
            ("/a/./b/.", "/a/b/"),
            ("/a/..", "/"),
            ("/../../x", "/x"),
            ("/a//b", "/a//b"),
        ] {
            assert_eq!(remove_dot_segments(path), expected, "{path}");
        }
    }

    #[test]
    fn references_resolve_per_rfc_3986() {
        let base = Target::parse("http://a.example:8080/b/c/d;p?q").unwrap();
        for (reference, expected) in [
            ("g", "http://a.example:8080/b/c/g"),
            ("./g", "http://a.example:8080/b/c/g"),
            ("g/", "http://a.example:8080/b/c/g/"),
            ("/g", "http://a.example:8080/g"),
            ("//g.example/x", "http://g.example/x"),
            ("?y", "http://a.example:8080/b/c/d;p?y"),
            ("g?y", "http://a.example:8080/b/c/g?y"),
            ("#s", "http://a.example:8080/b/c/d;p?q"),
            ("", "http://a.example:8080/b/c/d;p?q"),
            ("..", "http://a.example:8080/b/"),
            ("../g", "http://a.example:8080/b/g"),
            ("../../../g", "http://a.example:8080/g"),
            ("https://elsewhere.example/", "https://elsewhere.example/"),
            ("g:h", "g:h"),
        ] {
            let joined = base.join(reference);
            let normalized = Target::parse(&joined).map_or(joined.clone(), |target| {
                format!(
                    "{}://{}{}",
                    target.scheme(),
                    target.host_header(),
                    target.path_and_query
                )
            });
            assert_eq!(normalized, expected, "{reference:?}");
        }
    }

    #[test]
    fn non_ascii_and_percent_encoded_hosts_are_rejected_not_converted() {
        for url in [
            "http://b\u{fc}cher.example/",
            "https://\u{4f8b}\u{3048}.jp:8443/path",
            "http://user@b\u{fc}cher.example/",
            "http://b%C3%BCcher.example/",
            "http://[fe80::1%25eth0]/",
        ] {
            let error = Target::parse(url).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{url:?}");
            assert!(error.to_string().contains("ASCII"), "{url:?}: {error}");
        }
        let punycode = Target::parse("http://xn--bcher-kva.example/").unwrap();
        assert_eq!(punycode.host_header(), "xn--bcher-kva.example");
    }

    #[test]
    fn unencoded_paths_and_queries_are_percent_encoded_like_whatwg_urls() {
        for (url, target) in [
            ("http://example.com/b\u{fc}cher", "/b%C3%BCcher"),
            ("http://example.com/a b?c d", "/a%20b?c%20d"),
            ("http://example.com/{x}`?'y'", "/%7Bx%7D%60?%27y%27"),
            ("http://example.com/q?{x}", "/q?{x}"),
            ("http://example.com/b%C3%BCcher?q=%20", "/b%C3%BCcher?q=%20"),
        ] {
            assert_eq!(
                Target::parse(url).unwrap().path_and_query,
                target,
                "{url:?}"
            );
        }
    }

    #[test]
    fn credentials_unsupported_schemes_and_missing_hosts_are_rejected() {
        for url in [
            "http://user@example.com/",
            "http://user:pass@example.com/",
            "ftp://example.com/",
            "example.com/path",
            "/path",
            "http:///path",
            "",
        ] {
            assert_eq!(
                Target::parse(url).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{url:?}"
            );
        }
    }
}
