# routes test fixtures

Throwaway, self-signed cert/key pair used **only** by the
`route_with_valid_tls_compiles_to_some` unit test in `src/compile.rs` to
exercise the `[route.bridge.tls]` load path (`BridgeTlsConfig::from_paths`).

- `client_cert.pem` / `client_key.pem` — `CN=siphon-ai-route-tls-test`,
  RSA-2048, 100-year expiry. Not used by any binary, not a real credential,
  safe to regenerate:

  ```sh
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout client_key.pem -out client_cert.pem \
    -days 36500 -subj "/CN=siphon-ai-route-tls-test"
  ```
