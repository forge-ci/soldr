use kernal_api::http::{Client, Limits, Method, Protocol, Request, Response};
use std::time::Duration;

/// Client for an authenticated remote build service over HTTPS or an SSH tunnel.
pub struct RemoteBuildHttpClient {
    http: Client,
    origin: String,
}

/// Compatibility name for callers using the loopback SSH transport.
pub type LoopbackPostClient = RemoteBuildHttpClient;

impl RemoteBuildHttpClient {
    /// Use a literal loopback address and keep credentials on that endpoint.
    pub fn new(endpoint: std::net::SocketAddr) -> std::io::Result<Self> {
        Self::with_http2(endpoint, false)
    }

    /// Select cleartext HTTP/2 for a loopback endpoint reached via SSH.
    pub fn with_http2(endpoint: std::net::SocketAddr, http2: bool) -> std::io::Result<Self> {
        if !endpoint.ip().is_loopback() {
            return Err(std::io::Error::other(
                "Remote build endpoint must be a loopback SSH tunnel.",
            ));
        }
        let http = remote_client(http2)?;
        Ok(Self {
            http,
            origin: format!("http://{endpoint}"),
        })
    }

    /// Use a TLS-authenticated HTTP/2 worker origin without redirects or proxies.
    pub fn https_worker(origin: &str) -> std::io::Result<Self> {
        Self::https_origin(origin, true)
    }

    /// Use authenticated HTTPS with ordinary HTTP version negotiation.
    pub fn https_service(origin: &str) -> std::io::Result<Self> {
        Self::https_origin(origin, false)
    }

    fn https_origin(origin: &str, require_http2: bool) -> std::io::Result<Self> {
        let origin = url::Url::parse(origin).map_err(std::io::Error::other)?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || origin.port_or_known_default() != Some(443)
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(std::io::Error::other(
                "Remote build worker must be an HTTPS origin.",
            ));
        }
        let http = remote_client(require_http2)?;
        Ok(Self {
            http,
            origin: origin.origin().ascii_serialization(),
        })
    }

    /// Post a frame and return its response with a bounded body size.
    pub async fn post(
        &self,
        path: &str,
        token: &str,
        content_type: &str,
        body: Vec<u8>,
        max_response: usize,
    ) -> std::io::Result<Vec<u8>> {
        let response = self.post_stream(path, token, content_type, body).await?;
        Self::read_response(response, max_response).await
    }

    /// Fetch a bounded response with the specified encoding and return its raw bytes.
    pub async fn get(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, &str)],
        max_response: usize,
        accept_encoding: &str,
    ) -> std::io::Result<Vec<u8>> {
        let response = self
            .get_with_encoding(path, token, query, accept_encoding)
            .await?;
        Self::read_response(response, max_response).await
    }

    /// Issue an authenticated GET and return its streaming response body.
    pub async fn get_stream(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, &str)],
    ) -> std::io::Result<Response> {
        self.get_with_encoding(path, token, query, "zstd").await
    }

    /// Open an authenticated streaming GET with an explicit response encoding.
    pub async fn get_with_encoding(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, &str)],
        accept_encoding: &str,
    ) -> std::io::Result<Response> {
        if !path.starts_with('/') {
            return Err(std::io::Error::other("HTTP service path must begin with /"));
        }
        let mut url =
            url::Url::parse(&format!("{}{path}", self.origin)).map_err(std::io::Error::other)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        let authorization = format!("Bearer {token}");
        self.http
            .execute(Request {
                headers: &[
                    ("accept-encoding", accept_encoding),
                    ("authorization", &authorization),
                ],
                ..Request::get(url.as_str())
            })
            .await
    }

    async fn read_response(response: Response, max_response: usize) -> std::io::Result<Vec<u8>> {
        let status = response.status();
        let data = super::read_forge_body(
            response,
            max_response,
            "HTTP service response exceeds its size limit",
            std::io::Error::other,
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(std::io::Error::other(format!(
                "remote build {}: {}",
                status,
                String::from_utf8_lossy(&data)
            )));
        }
        Ok(data)
    }

    /// Post a request and leave response streaming to the caller.
    pub async fn post_stream(
        &self,
        path: &str,
        token: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> std::io::Result<Response> {
        if !path.starts_with('/') {
            return Err(std::io::Error::other("HTTP service path must begin with /"));
        }
        let url = format!("{}{path}", self.origin);
        let authorization = format!("Bearer {token}");
        self.http
            .execute(Request {
                method: Method::Post,
                url: &url,
                headers: &[
                    ("content-type", content_type),
                    ("authorization", &authorization),
                ],
                body: &body,
            })
            .await
    }
}

fn remote_client(http2: bool) -> std::io::Result<Client> {
    let protocol = if http2 {
        Protocol::Http2Only
    } else {
        Protocol::Auto
    };
    Client::with_protocol(
        Limits {
            max_request_bytes: usize::MAX,
            max_body_bytes: u64::MAX,
            total_timeout: Duration::from_secs(6 * 60 * 60),
            read_timeout: Duration::from_secs(6 * 60 * 60),
            ..Limits::default()
        },
        protocol,
    )
}

/// Bounded control-plane requests to the fixed Forge CI origin.
pub struct ForgeCiHttp(Client);

impl ForgeCiHttp {
    /// Construct a client that never forwards Forge CI credentials through redirects.
    pub fn new() -> std::io::Result<Self> {
        super::super::net_guard::ensure_network_allowed("Forge CI")
            .map_err(std::io::Error::other)?;
        let http = Client::new(Limits {
            max_body_bytes: 16 * 1024,
            total_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(15),
            ..Limits::default()
        })
        .map_err(forge_http_error)?;
        Ok(Self(http))
    }

    /// Start or poll a pairing, or request a worker grant.
    pub async fn post(
        &self,
        path: &str,
        credential: Option<&str>,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        self.request(Method::Post, path, credential).await
    }

    /// Check a saved session.
    pub async fn get(&self, path: &str, credential: &str) -> std::io::Result<(u16, Vec<u8>)> {
        self.request(Method::Get, path, Some(credential)).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        credential: Option<&str>,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        const MAX_BODY: usize = 16 * 1024;
        if !path.starts_with('/') {
            return Err(std::io::Error::other("Forge CI route must begin with /"));
        }
        let url = format!("https://forge-ci.com{path}");
        let authorization = credential.map(|token| format!("Bearer {token}"));
        let headers = authorization
            .as_deref()
            .map(|value| [("authorization", value)]);
        let response = self
            .0
            .execute(Request {
                method,
                headers: headers.as_ref().map_or(&[], |header| header.as_slice()),
                ..Request::get(&url)
            })
            .await
            .map_err(forge_http_error)?;
        let status = response.status();
        let body = super::read_forge_body(
            response,
            MAX_BODY,
            "Forge CI response exceeds its size limit",
            forge_http_error,
        )
        .await?;
        Ok((status, body))
    }
}

fn forge_http_error(error: std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("Forge CI request failed: {error}"))
}

#[cfg(test)]
mod remote_worker_transport_tests {
    use super::RemoteBuildHttpClient;

    #[test]
    fn worker_connections_require_an_https_origin() {
        assert!(RemoteBuildHttpClient::https_worker("https://build.forge-ci.com").is_ok());
        for invalid in [
            "http://build.forge-ci.com",
            "https://build.forge-ci.com:8443",
            "https://user@build.forge-ci.com",
            "https://build.forge-ci.com/api",
            "https://build.forge-ci.com/?next=worker",
        ] {
            assert!(RemoteBuildHttpClient::https_worker(invalid).is_err());
        }
    }
}
