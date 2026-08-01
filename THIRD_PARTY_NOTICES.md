# Third-party dependency notices

wscrpt's own source is licensed under the repository's [MIT license](LICENSE).
The dependency lock files are authoritative for exact revisions. This inventory
records the direct and resolved packages reviewed for the public-source audit;
it does not replace the license text shipped by each dependency.

No dependency source checkout, `node_modules` directory, Rust registry cache,
or Xcode `DerivedData` directory is tracked in this repository.

## Rust editor

Exact versions are locked in `Cargo.lock`.

| Package | License |
| --- | --- |
| `anyhow`, `base64`, `clap`, `libc`, `regex`, `serde`, `thiserror`, `toml`, `unicode-segmentation`, `unicode-width` | MIT OR Apache-2.0 |
| `crossterm`, `ropey` | MIT |
| `tempfile` (tests only) | MIT OR Apache-2.0 |

Their transitive Rust dependencies are resolved by `Cargo.lock` and are not
vendored here. `cargo package --locked` and the normal Cargo license metadata
remain part of the release review.

## Preview sidecar

Exact versions are locked in `previewd/package-lock.json`.

| Package | Resolved version | License |
| --- | --- | --- |
| `chrome-remote-interface` | 0.34.0 | MIT |
| `ws` | 8.21.0 | MIT |

The sidecar package is private and its installed dependencies are ignored.

## Native iPad client

Exact revisions and versions are locked in the Xcode workspace's
`Package.resolved`.

| Package | Resolved version | License |
| --- | --- | --- |
| SwiftTerm | 1.15.0 | MIT; includes xterm.js-derived work identified by SwiftTerm's license |
| swift-nio | 2.101.3 | Apache-2.0 |
| swift-nio-ssh | 0.15.0 | Apache-2.0 |
| swift-crypto | 4.5.1 | Apache-2.0; upstream NOTICE applies |
| swift-argument-parser | 1.8.2 | Apache-2.0 |
| swift-asn1 | 1.7.1 | Apache-2.0; upstream NOTICE applies |
| swift-atomics | 1.3.1 | Apache-2.0 |
| swift-collections | 1.6.0 | Apache-2.0 |
| swift-system | 1.7.5 | Apache-2.0 |

Before distributing a compiled iPad app, copy the exact license and NOTICE
texts from the resolved package revisions into the app's acknowledgements or
distribution bundle. The source repository and simulator build do not by
themselves close that binary-distribution gate.
