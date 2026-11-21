# nolva-core

Shared Rust core for Nolva, consumed by `nolva-ios` and `nolva-android`
through [UniFFI](https://mozilla.github.io/uniffi-rs/).

## Layout

```text
crates/
  document-engine/   Domain model and use cases (no I/O; defines ports)
  scanner-engine/    Document detection: finds the page corners in a photo
  image-engine/      Oriented decoding, perspective correction, JPEG encoding
  pdf-engine/        Multi-page PDF export (JPEGs embedded without recompression)
  storage-engine/    SQLite (metadata) + file system (images)
  core-ffi/          Public UniFFI API (lib: nolva_core)
tools/uniffi-bindgen Bindings generator pinned to the uniffi version
apple/               Swift package sources (generated bindings)
android/             Gradle module that packages the .aar
scripts/             iOS/Android builds and bindings generation
docs/decisions/      ADRs
```

Crate dependencies: `core-ffi → {storage-engine, image-engine → scanner-engine} → document-engine`.
The domain knows nothing about SQLite, image processing or the mobile
platforms; those are plugged in through ports.

## Current API (v0.4)

```swift
let core = try NolvaCore(config: CoreConfig(dataDir: appSupportPath))
let doc  = try core.createDocument(title: "Contract")
let page = try core.addPage(documentId: doc.id, imagePath: capturedFile.path)

// Find the sheet and straighten it. The original is kept untouched.
let corners = try core.detectPageCorners(pageId: page.id)?.corners ?? fullImage
let cropped = try core.cropPage(pageId: page.id, corners: corners)
show(cropped.processedPath ?? cropped.originalPath)

// Export: one page per page, cropped images preferred, JPEGs embedded losslessly.
let info = try core.exportPdf(documentId: doc.id, outputPath: pdfURL.path,
                              options: PdfOptions(paperSize: .a4, marginMm: 0))
```

```kotlin
val core = NolvaCore(CoreConfig(dataDir = context.filesDir.absolutePath))
val doc = core.createDocument("Contract")
val page = core.addPage(doc.id, capturedFile.absolutePath)
val detection = core.detectPageCorners(page.id)   // null if no sheet found
val cropped = core.cropPage(page.id, detection?.corners ?: fullImage)
val info = core.exportPdf(doc.id, pdfFile.absolutePath, PdfOptions(PaperSize.A4, marginMm = 0f))

// Manage: renames, permanent deletes (files included) and reordering.
core.renameDocument(doc.id, "Signed contract")
core.movePage(page.id, toPosition = 0u)   // returns the new order
core.deletePage(page.id)
core.deleteDocument(doc.id)
```

Corners are normalized (`0.0..=1.0`) in the coordinates of the original
image as displayed, i.e. with EXIF orientation applied.

Calls are synchronous and may block on I/O or CPU (detection and cropping
take up to ~1 s on a 12 MP photo): invoke them off the main thread
(`Dispatchers.IO` / `Dispatchers.Default` or a background `Task`).

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
