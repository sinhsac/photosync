import org.jetbrains.kotlin.gradle.tasks.KotlinCompile

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

/**
 * ABIs to build the Rust engine for.
 *
 * One by default. Each extra ABI is a full extra Rust release build, and the only
 * device this is tested on is arm64. Override for an emulator:
 *
 *     gradlew assembleDebug -Ppsync.abis=arm64-v8a,x86_64
 */
val abis: List<String> = (findProperty("psync.abis") as String? ?: "arm64-v8a")
    .split(",")
    .map { it.trim() }
    .filter { it.isNotEmpty() }

val repoRoot: File = rootProject.projectDir.parentFile
val jniLibsDir: File = file("src/main/jniLibs")

/**
 * Builds `crates/android` into `src/main/jniLibs/<abi>/libphotosync_android.so`.
 *
 * Wired into the build rather than left as a manual step on purpose: a stale `.so`
 * fails silently — JNI resolves the old symbols and the app runs yesterday's engine
 * with no warning anywhere. That has already cost real debugging time on the
 * command-line harness, and it is much harder to spot from inside a GUI.
 *
 * `--platform 24` is not optional. `getifaddrs`/`freeifaddrs` were only added to
 * bionic in API 24; below that the link fails outright, and the interface
 * enumeration §7.2 depends on has nothing to call.
 */
val cargoNdk = tasks.register<Exec>("cargoNdkBuild") {
    group = "build"
    description = "Compiles the Rust engine for ${abis.joinToString(", ")}"

    workingDir = repoRoot

    val args = mutableListOf("ndk")
    abis.forEach { args += listOf("-t", it) }
    args += listOf(
        "--platform", "24",
        "-o", jniLibsDir.absolutePath,
        "build", "-p", "photosync-android", "--release",
    )

    // `cargo` is resolved through PATH; on Windows that needs a shell because the
    // real file is cargo.exe and Exec does not append the extension itself.
    if (System.getProperty("os.name").startsWith("Windows", ignoreCase = true)) {
        commandLine(listOf("cmd", "/c", "cargo") + args)
    } else {
        commandLine(listOf("cargo") + args)
    }

    // cargo-ndk finds the toolchain through these. The local SDK layout is not
    // guaranteed anywhere else, so fall back to it rather than failing obscurely
    // inside the linker.
    val ndk = System.getenv("ANDROID_NDK_HOME")
        ?: System.getenv("NDK_HOME")
        ?: file("${android.sdkDirectory}/ndk").listFiles()
            ?.filter { it.isDirectory }
            ?.maxByOrNull { it.name }
            ?.absolutePath
    if (ndk != null) environment("ANDROID_NDK_HOME", ndk)
    environment("ANDROID_HOME", android.sdkDirectory.absolutePath)

    // Rebuild when any Rust source or manifest changes, and skip otherwise.
    inputs.files(fileTree(repoRoot.resolve("crates")) { include("**/*.rs", "**/Cargo.toml") })
    inputs.file(repoRoot.resolve("Cargo.lock"))
    inputs.property("abis", abis)
    outputs.dir(jniLibsDir)

    doFirst { jniLibsDir.mkdirs() }
}

android {
    namespace = "app.photosync"

    // 34 rather than 36: AGP 8.2.0 is what is in the local Gradle cache, and it
    // does not know about newer platforms. Nothing in this app needs a 35+ API.
    compileSdk = 34

    defaultConfig {
        applicationId = "app.photosync"

        // 24 is a hard floor, not a preference. See the note on `--platform` above.
        minSdk = 24
        targetSdk = 34
        versionCode = 1
        versionName = "0.1-bringup"

        // Only ship the ABIs the engine was actually built for. Without this a
        // device of another architecture installs happily and then dies in
        // System.loadLibrary.
        ndk { abiFilters += abis }
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
        }
        release {
            // No shrinking. There is no product release yet, and R8 with a JNI
            // entry surface needs keep rules that would only be guesswork now.
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    sourceSets {
        getByName("main") {
            java.srcDirs("src/main/kotlin")
            jniLibs.srcDirs(jniLibsDir)
        }
    }

    packaging {
        jniLibs {
            // Extract to the filesystem instead of loading from inside the APK.
            // The engine opens file descriptors it owns and expects a normal
            // library layout; uncompressed in-APK loading is an optimisation this
            // app has no need to risk.
            useLegacyPackaging = true
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    // Lint has opinions about a screen that is explicitly a bring-up harness
    // (hardcoded strings, no content descriptions). Those get fixed when §19
    // replaces this, not before.
    lint {
        abortOnError = false
    }
}

// 17, not 21. The installed JDK is 21, but AGP 8.2.0's desugaring and dexing are
// only validated against 17 bytecode. No toolchain block: asking Gradle for a JDK 17
// toolchain would make it go looking for one that is not installed.
tasks.withType<KotlinCompile>().configureEach {
    kotlinOptions.jvmTarget = "17"
}

dependencies {
    // appcompat only. It brings androidx.core transitively, which is where
    // ContextCompat and ActivityCompat live, so nothing else is needed.
    implementation("androidx.appcompat:appcompat:1.6.1")
}

// Make sure the engine exists before anything tries to package it.
tasks.named("preBuild") { dependsOn(cargoNdk) }
