// Android library wrapping the Rust core: UniFFI Kotlin bindings + the native
// .so files. Both are produced by scripts/build-android.sh before Gradle runs.
//
// AGP 9 compiles Kotlin natively, so no separate Kotlin plugin is applied.

plugins {
    id("com.android.library")
    `maven-publish`
}

val coreVersion: String = providers.gradleProperty("coreVersion").getOrElse("0.0.0-SNAPSHOT")

android {
    namespace = "app.nolva.core"
    compileSdk = 36

    defaultConfig {
        minSdk = 26
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
        }
    }
}

dependencies {
    // UniFFI's Kotlin bindings call into the native library through JNA.
    implementation("net.java.dev.jna:jna:5.18.1@aar")
}

publishing {
    publications {
        register<MavenPublication>("release") {
            groupId = "app.nolva"
            artifactId = "nolva-core"
            version = coreVersion
            afterEvaluate { from(components["release"]) }
        }
    }
    repositories {
        maven {
            name = "GitHubPackages"
            url = uri("https://maven.pkg.github.com/hpaez/nolva-core")
            credentials {
                username = providers.environmentVariable("GITHUB_ACTOR").orNull
                password = providers.environmentVariable("GITHUB_TOKEN").orNull
            }
        }
    }
}
