# Synthetic mutual TLS fixtures

These certificates and private keys were generated solely for local automated
tests. They are public test data, never a real user's identity. Do not deploy or
trust this CA outside tests. The PKCS#12 password is `test-fixture-only`.

The root is a self-signed RSA 2048-bit CA. Its two RSA 2048-bit leaf certificates
use clientAuth and serverAuth respectively; the server SAN is localhost and
127.0.0.1. They were generated on 2026-10-28 with a 36500-day validity period so
tests do not expire during normal development. `client.p12` contains its client
private key, leaf certificate and root certificate. `server-key.der` is PKCS#8.
`client-v1.p12` is a separate self-signed synthetic X.509 v1 client identity;
its password is `synthetic-test-only`. It is not trusted outside tests.
`client-nested.p12` contains the same synthetic identity in a nested
safeContentsBag, without encryption or a MAC, to document unsupported nesting.

The tests never read or write Keychain and never contact a non-loopback server.
