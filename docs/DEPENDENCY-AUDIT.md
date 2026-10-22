# Dependency and source-language audit

## Release blocker: strict Rust-only dependency requirement

The delivered application/reference source is Rust; the source-language claim does
not automatically extend to every transitive dependency. The reviewed rbit 0.2.2
manifest uses `reqwest 0.12` with `default-features=false` and `rustls-tls`.
Its published lockfile resolves rustls and rustls-webpki through `ring`; ring's own
documentation describes Rust, C and assembly. This is a concrete reason not to
claim that an unmodified rbit HTTPS build is strictly 100% Rust implementation code.

A direct application dependency on reqwest with fewer features cannot subtract a
feature enabled by rbit; Cargo features are additive. `default-features=false` on
rbit itself is not a magic fix for its unconditional child dependency feature.

Recommended fix direction: a narrow rbit fork/patch that makes HTTP client/TLS
provider injection possible and features separately selectable. Then choose and
audit a mature pure-Rust TLS/crypto implementation with necessary algorithms,
certificate verification, roots and platform interoperability. This is an
integration/review gate, not justification for inventing cryptography or disabling
certificate validation. Keep HTTPS support; silently removing it fails legitimate
tracker/feed use cases.

The examined `rustls-rustcrypto` provider is 0.0.2-alpha and explicitly says not to
use it in production. It is not an acceptable unqualified release substitution.
An experimental pure-Rust profile may be useful to evaluate the boundary, but must
be labeled experimental and cannot close this gate by itself.

Reproduce on the implementation machine after dependency resolution:

```sh
cargo tree --manifest-path integration/rbit-adapter/Cargo.toml -e features
cargo tree --manifest-path integration/rbit-adapter/Cargo.toml -i ring
cargo metadata --manifest-path integration/rbit-adapter/Cargo.toml --format-version 1
cargo tree --manifest-path integration/pixelkit-preview/Cargo.toml -e features
```

Audit both target graphs, Cargo build scripts, bundled native sources and link flags.
Search for native implementations such as ring/aws-lc, OpenSSL, C/C++ codecs,
libtorrent, Qt/GTK runtime additions and compiler/build helpers. A `cc` entry is a
review signal, not proof by itself of what was linked. Rust OS bindings, the OS
kernel, GPU/display drivers and standard platform libraries are different from
shipping a bundled non-Rust torrent engine. Document the accepted boundary.

Sources:
https://docs.rs/crate/rbit/0.2.2/source/Cargo.toml.orig
https://docs.rs/crate/rbit/0.2.2/source/Cargo.lock
https://docs.rs/ring/latest/ring/
https://docs.rs/rustls-rustcrypto/latest/rustls_rustcrypto/

## Status 2026-10-22: rbit forked in-tree, ring removed from the graph

The owner chose the fork direction. `vendor/rbit` is the published rbit 0.2.2
source with `reqwest` made optional behind an off-by-default `http-client`
feature (see `vendor/rbit/TEMPLARTORRENT-FORK.md`); the workspace applies it
through `[patch.crates-io]`. The adapter announces through its own bounded
transport (`integration/rbit-adapter/src/announce.rs`): one connection to the
already-classified address, no redirects, 64 KiB cap, `Connection: close`,
bencode parsed into rbit's `AnnounceResponse`. HTTPS uses rustls 0.23.46.

Consequences, stated rather than hidden:

- `cargo tree -i ring` and `-i reqwest` on the workspace graph report nothing.
  rustls is back, by choice: the owner selected the 2026-10-14 rustls release
  (0.23.46) for HTTPS tracker announces. Default crate features are off.
  Enabled features are `std`, `tls12` and `aws_lc_rs`, the cryptography
  provider that release turns on by default. `ring` is not a feature.
  `cargo tree -i ring` prints nothing, so ring is not compiled. Cargo.lock
  still names ring because rustls-webpki declares it as an unused optional
  dependency. `cargo tree -i aws-lc-sys` reports aws-lc-sys. aws-lc is C,
  built with cmake. That is an accepted native boundary for tracker TLS,
  not a pure-Rust stack, and the strict all-Rust claim does not cover it.
- `https://` announces go through the same bounded client as plain HTTP:
  one connection to the already-classified address, no redirects, 64 KiB cap,
  rustls certificate verification against the webpki public roots, and the
  name from the tracker URL. Brotli certificate decompression is not enabled
  (0.23.46 advisory GHSA-h2v8-394m-p7vv). The examined `rustls-rustcrypto`
  provider is still not used. rbit's WebSeed and UPnP modules are unavailable
  without the `http-client` feature and were never used.
- Remaining non-Rust boundary: `aws-lc-sys` for tracker TLS, OS bindings
  (`libc`, Wayland/X11 shell crates used by PixelKit), and build scripts
  (`cmake` and `cc` for aws-lc, plus build scripts of pure-Rust crates).
  Review the package list for `-sys` crates each time the lockfile changes.

## PixelKit Windows integration gate

The reviewed README supports Linux/macOS clipboard transport and reports other
platforms unavailable. Implement Windows clipboard handling in PixelKit's shell
boundary with Rust Windows bindings and its existing asynchronous worker interface.
Test Unicode magnets, ownership/lifetime, unavailable clipboard, large input,
Ctrl+V, Ctrl+C and focus changes. Do not make an application-specific fake clipboard
that appears to work only within the same process.

## Reproducibility and licenses

The release agent must generate and commit Cargo.lock after checking all resolved
features; no lockfile has been fabricated here. Pin all PixelKit crates to one
revision. Record licenses and attribution for rbit, PixelKit and transitive crates.
Production fonts, GeoIP data and flag graphics need independently reviewed
redistribution rights and notices; none are included in this handoff.
