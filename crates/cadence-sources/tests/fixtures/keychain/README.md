# Temporary macOS Keychain compatibility probe

`temporary-keychain-probe.c` creates a dedicated temporary Keychain at the path
provided, uses only a public synthetic string, then deletes that Keychain. It
never selects the user's default Keychain or reads existing credentials. It
prints OSStatus values and whether the device-only accessibility attribute can
be read back. It compares `kSecAttrAccessControl`, direct accessibility, and
the file-based Keychain defaults. This is an opt-in platform probe, not an
import of a real identity and not an automated application-level test.

```sh
clang -Wno-deprecated-declarations -framework Security -framework CoreFoundation \
  crates/cadence-sources/tests/fixtures/keychain/temporary-keychain-probe.c \
  -o /tmp/cadence-keychain-probe
scratch=$(mktemp -d /tmp/cadence-keychain-probe.XXXXXX)
/tmp/cadence-keychain-probe "$scratch/synthetic.keychain"
```

The probe reports the Keychain deletion status. Inspect and remove the empty
scratch directory after the run. It does not change Keychain settings.

`item-limit-probe.c` uses the same temporary-Keychain approach to attempt
noncredential zero-filled items of 0 bytes, 1 KiB and slightly over 4 MiB. It reports
only sizes and OSStatus values, and deletes each item and its Keychain.
Compile it with the same flags, changing the input C filename and output path.

`pair-probe.c` stages two pairs and an active marker in a temporary Keychain,
simulates a failure between staged writes, then verifies the old marker and a
successful replacement. It exercises real Security framework add, lookup,
update and delete calls, but the failure is injected by the test; it is not an
OS access-denied or application-level Keychain test. It deletes all synthetic
items and the temporary Keychain on normal completion.
