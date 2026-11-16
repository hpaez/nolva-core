# nolva-core

Shared Rust core for Nolva, consumed by `nolva-ios` and `nolva-android`
through [UniFFI](https://mozilla.github.io/uniffi-rs/).

## Layout

```text
crates/
  document-engine/   Domain model and use cases (no I/O; defines ports)
  storage-engine/    SQLite (metadata) + file system (images)
  core-ffi/          Public UniFFI API (lib: nolva_core)
tools/uniffi-bindgen Bindings generator pinned to the uniffi version
apple/               Swift package sources (generated bindings)
android/             Gradle module that packages the .aar
scripts/             iOS/Android builds and bindings generation
docs/decisions/      ADRs
```

Crate dependencies: `core-ffi → storage-engine → document-engine`.
The domain knows nothing about SQLite or the mobile platforms.

## Current API (v0.1)

```swift
let core = try NolvaCore(config: CoreConfig(dataDir: appSupportPath))
let doc  = try core.createDocument(title: "Contract")
let page = try core.addPage(documentId: doc.id, imagePath: capturedFile.path)
let docs = try core.listDocuments()
```

```kotlin
val core = NolvaCore(CoreConfig(dataDir = context.filesDir.absolutePath))
val doc = core.createDocument("Contract")
val page = core.addPage(doc.id, capturedFile.absolutePath)
```

Calls are synchronous and may block on I/O: invoke them off the main thread
(`Dispatchers.IO` or a background `Task`).

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

- **iOS** (requires macOS + Xcode): `scripts/build-ios.sh` produces
  `build/apple/NolvaCoreFFI.xcframework`. To make the app use the local build
  instead of the released one, open the project with `NOLVA_CORE_LOCAL=1`.
- **Android** (requires NDK r28+ and `cargo install cargo-ndk`):
  `ANDROID_NDK_HOME=... scripts/build-android.sh`, then
  `gradle -p android :nolva-core:publishToMavenLocal`.

## Releasing

1. Bump `version` in `[workspace.package]` of the root `Cargo.toml` and merge to `main`.
2. In Actions, run **Release** with that version.
3. The workflow publishes the XCFramework (GitHub Release + `Package.swift` at
   tag `vX.Y.Z`) and the `.aar` (`app.nolva:nolva-core:X.Y.Z` on GitHub Packages).

## Consuming from the apps (private repo)

**iOS:** add the package `https://github.com/hpaez/nolva-core` pinned to a version.
The binary is downloaded through the GitHub API, so you need a token with
access to the repo in `~/.netrc`:

```text
machine api.github.com
  login hpaez
  password <TOKEN>
```

**Android:** add the Maven repository `https://maven.pkg.github.com/hpaez/nolva-core`
with credentials (`gpr.user` / `gpr.token` in `~/.gradle/gradle.properties`; the
token needs `read:packages`) and the dependency `app.nolva:nolva-core:X.Y.Z`.

The rationale for this setup is in [ADR 0001](docs/decisions/0001-core-distribution.md).
