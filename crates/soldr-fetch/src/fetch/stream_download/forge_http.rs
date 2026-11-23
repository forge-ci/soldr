use std::time::Duration;

/// Client for an authenticated remote build service over HTTPS or an SSH tunnel.
pub struct RemoteBuildHttpClient {
    http: reqwest::Client,
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
        let builder = Self::builder();
        let builder = if http2 {
            builder.http2_prior_knowledge()
        } else {
            builder
        };
        let http = builder.build().map_err(std::io::Error::other)?;
        Ok(Self {
            http,
            origin: format!("http://{endpoint}"),
        })
    }

    /// Use a TLS-authenticated HTTP/2 worker origin without redirects or proxies.
    pub fn https_worker(origin: &str) -> std::io::Result<Self> {
        let origin = reqwest::Url::parse(origin).map_err(std::io::Error::other)?;
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
        let http = Self::builder()
            .https_only(true)
            .http2_prior_knowledge()
            .build()
            .map_err(std::io::Error::other)?;
        Ok(Self {
            http,
            origin: origin.origin().ascii_serialization(),
        })
    }

    fn builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
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
    ) -> std::io::Result<reqwest::Response> {
        self.get_with_encoding(path, token, query, "zstd").await
    }

    async fn get_with_encoding(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, &str)],
        accept_encoding: &str,
    ) -> std::io::Result<reqwest::Response> {
        if !path.starts_with('/') {
            return Err(std::io::Error::other("HTTP service path must begin with /"));
        }
        self.http
            .get(format!("{}{path}", self.origin))
            .query(query)
            .header("accept-encoding", accept_encoding)
            .bearer_auth(token)
            .send()
            .await
            .map_err(std::io::Error::other)
    }

    async fn read_response(
        mut response: reqwest::Response,
        max_response: usize,
    ) -> std::io::Result<Vec<u8>> {
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(std::io::Error::other)? {
            if chunk.len() > max_response - data.len() {
                return Err(std::io::Error::other(
                    "HTTP service response exceeds its size limit",
                ));
            }
            data.extend_from_slice(&chunk);
        }
        if !response.status().is_success() {
            return Err(std::io::Error::other(format!(
                "remote build {}: {}",
                response.status(),
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
    ) -> std::io::Result<reqwest::Response> {
        if !path.starts_with('/') {
            return Err(std::io::Error::other("HTTP service path must begin with /"));
        }
        self.http
            .post(format!("{}{path}", self.origin))
            .header("content-type", content_type)
            .bearer_auth(token)
            .body(body)
            .send()
            .await
            .map_err(std::io::Error::other)
    }
}

/// Bounded control-plane requests to the fixed Forge CI origin.
pub struct ForgeCiHttp(reqwest::Client);

impl ForgeCiHttp {
    /// Construct a client that never forwards Forge CI credentials through redirects.
    pub fn new() -> std::io::Result<Self> {
        super::super::net_guard::ensure_network_allowed("Forge CI")
            .map_err(std::io::Error::other)?;
        let http = RemoteBuildHttpClient::builder()
            .https_only(true)
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(forge_http_error)?;
        Ok(Self(http))
    }

    /// Start or poll a pairing, or request a worker grant.
    pub async fn post(
        &self,
        path: &str,
        credential: Option<&str>,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        self.request(reqwest::Method::POST, path, credential).await
    }

    /// Check a saved session.
    pub async fn get(&self, path: &str, credential: &str) -> std::io::Result<(u16, Vec<u8>)> {
        self.request(reqwest::Method::GET, path, Some(credential))
            .await
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        credential: Option<&str>,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        const MAX_BODY: usize = 16 * 1024;
        if !path.starts_with('/') {
            return Err(std::io::Error::other("Forge CI route must begin with /"));
        }
        let request = self
            .0
            .request(method, format!("https://forge-ci.com{path}"));
        let request = match credential {
            Some(credential) => request.bearer_auth(credential),
            None => request,
        };
        let mut response = request.send().await.map_err(forge_http_error)?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(forge_http_error)? {
            if chunk.len() > MAX_BODY - body.len() {
                return Err(std::io::Error::other(
                    "Forge CI response exceeds its size limit",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    }
}

fn forge_http_error(error: reqwest::Error) -> std::io::Error {
    std::io::Error::other(format!("Forge CI request failed: {}", error.without_url()))
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
