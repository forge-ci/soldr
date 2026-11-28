# Local HTTP TLS fixture

This is public, test-only key material. Never use this identity for a deployed
service or install its certificate in an operating-system trust store.
`identity.p12.hex` is a hex-encoded PKCS#12 identity with password `fixture`.
The certificate is self-signed, valid from September 16, 2026 through December
14, 2028, and has only the `localhost` DNS subject alternative name. Renew it
before expiry; an IP-address URL deliberately fails hostname verification.

**Keep the validity period at or under 825 days.** macOS refuses a TLS server
certificate whose validity exceeds that, failing the handshake with *"The
validity period in the certificate exceeds the maximum allowed"* (Security
framework, −67901), so a longer window makes the tests fail on macOS and
nowhere else. The previous fixture used 3650 days and did exactly that; it went
unnoticed because no lane ran these tests on macOS. `-days 820` leaves margin
below the limit while keeping renewal infrequent.

The Rust unit tests decode the identity in memory and add the certificate only
to their explicitly trusting client. The ordinary public constructor must reject
it. No external website, Python process, or OpenSSL executable is needed to run
the tests. The test server uses the same native TLS library already selected by
the HTTP backend; its direct dependency is test-only.

To regenerate in a temporary directory with OpenSSL:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem \
  -days 820 -subj /CN=kernal-http-test-only \
  -addext subjectAltName=DNS:localhost \
  -addext basicConstraints=critical,CA:TRUE \
  -addext keyUsage=critical,digitalSignature,keyEncipherment,keyCertSign \
  -addext extendedKeyUsage=serverAuth
openssl pkcs12 -export -inkey key.pem -in cert.pem -out identity.p12 \
  -passout pass:fixture -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES \
  -macalg sha1
od -An -v -tx1 identity.p12
```

Copy the resulting certificate and hex bytes into these fixtures and update the
validity dates above. The legacy PKCS#12 envelope is for native importer
compatibility, not a cryptographic recommendation; the RSA certificate and
negotiated TLS connection use the platform's normal verification defaults.

## CA-signed leaf (`chain-root.pem`, `chain-leaf.p12.hex`)

rustls refuses a CA certificate presented as an end-entity certificate, so the
self-signed fixture above can only prove that refusal under
`http-client-rustls`. The chain fixture is what both backends accept:
`chain-root.pem` is a test-only CA (`CN=kernal-http-test-only-root`) and
`chain-leaf.p12.hex` is a PKCS#12 identity (password `fixture`) for a
`CA:FALSE` leaf with only the `localhost` DNS name, bundled with that root.
Both are valid from November 8, 2026 through February 5, 2029; keep the same
820-day window. The HTTP/2 test uses `chain-leaf.pem`, which holds the same leaf
certificate and private key in PEM form. To regenerate:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 820 \
  -subj /CN=kernal-http-test-only-root \
  -addext basicConstraints=critical,CA:TRUE \
  -addext keyUsage=critical,keyCertSign,cRLSign
openssl req -newkey rsa:2048 -nodes -keyout leaf.key -out leaf.csr -subj /CN=localhost
printf 'subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n' > ext.cnf
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out leaf.pem -days 820 -extfile ext.cnf
openssl pkcs12 -export -inkey leaf.key -in leaf.pem -certfile ca.pem \
  -out leaf.p12 -passout pass:fixture -keypbe PBE-SHA1-3DES \
  -certpbe PBE-SHA1-3DES -macalg sha1
od -An -v -tx1 leaf.p12
cat leaf.pem leaf.key
```
