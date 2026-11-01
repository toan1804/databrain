Test-only RSA key, generated for `jwt::tests` (`openssl genrsa 2048`, then
`openssl pkcs8 -topk8`, plain and encrypted with the passphrase `databrain`).
It is not used anywhere else; `rsa.fingerprint` is its Snowflake-style
public key fingerprint.
