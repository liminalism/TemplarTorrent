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
