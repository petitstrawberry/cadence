# SWS protocol dependency update

Source: `petitstrawberry/Scarlet`, commit `0639a916dfd652e9b2c1ea740cacc1c09743d9eb`,
package `user/lib/sws-protocol`. The upstream MIT license is included.

This supplies capability protocol version 13 to both ScarletUI and sws-client.
ScarletUI's exact version check then enables SGFX against the inspected server,
without changing renderer selection or Canvas painting. The older version 11
caused CPU fallback, where Canvas extensions are ignored.

Library sources retain upstream behavior and are formatted with `cargo fmt`.
The manifest uses normal `std` by default
and pins the optional legacy facade to the existing runtime dependency source.
The upstream wire-contract test's stale version-12 assertion is updated to 13.

The existing sws-client connection implementation and SAS client/protocol are
unchanged between the old runtime pin and the inspected server revision. Keep
that runtime pin shared with SGFX: updating only the client runtime introduces
two `scarlet-os` identities and a duplicate event-return trampoline at link time.
This package patch updates the protocol independently, while preserving one
runtime and one shared-image handle identity. Remove it when upstream ScarletUI
and SGFX update their runtime dependencies together.
