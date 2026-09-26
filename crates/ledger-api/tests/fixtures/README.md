# Test-only signing keys

Throwaway RSA-2048 and P-256 keys generated for the `ledger-api` OIDC authenticator unit tests (local JWKS server). They are PKCS#8 PEM documents stored with a `.pkcs8` extension because the repository ignores `*.pem` and its hooks refuse credential-like file names; they are not secrets, are never used outside `cargo test`, and must never be configured on a running server. `jwks-initial.json` publishes `kid-a` (RSA) and `kid-b` (EC); `jwks-rotated.json` adds `kid-c` (RSA) and an encryption-use key that must be ignored.
