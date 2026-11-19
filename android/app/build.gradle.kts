plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "dev.dbdelve.android"
    compileSdk = 35

    defaultConfig {
        applicationId = "dev.dbdelve.android"
        // Android 15 only: nothing older to stay compatible with.
        minSdk = 35
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
        ndk {
            // 64-bit ARM only. No x86_64, so no emulator: test on a phone.
            abiFilters += listOf("arm64-v8a")
        }
    }

    // The release key comes from the environment (CI's secrets, see the
    // README), so it never lands in the repository. Without it, the debug
    // key: the build works, but every machine's debug key differs, so its
    // APK cannot update one built elsewhere.
    signingConfigs {
        System.getenv("DBDELVE_SIGNING_STORE")?.let { store ->
            create("release") {
                storeFile = file(store)
                storePassword = System.getenv("DBDELVE_SIGNING_STORE_PASSWORD")
                keyAlias = System.getenv("DBDELVE_SIGNING_KEY_ALIAS")
                keyPassword = System.getenv("DBDELVE_SIGNING_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            // R8 shrinks, optimises and obfuscates; proguard-rules.pro keeps
            // what it cannot see being used (JNA's reflection into the core)
            // and strips logging, so a release carries no debug code.
            isMinifyEnabled = true
            isShrinkResources = true
            isDebuggable = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        compose = true
    }
    sourceSets["main"].java.srcDir(layout.buildDirectory.dir("generated/uniffi"))
    sourceSets["main"].jniLibs.srcDir(layout.buildDirectory.dir("generated/jniLibs"))
    sourceSets["main"].assets.srcDir(layout.buildDirectory.dir("generated/changelog"))
}

val coreDir = rootProject.file("core")
val jniLibsDir = layout.buildDirectory.dir("generated/jniLibs")
val bindingsDir = layout.buildDirectory.dir("generated/uniffi")

// The Rust core, cross-compiled per ABI by cargo-ndk (`cargo install
// cargo-ndk`, and the NDK that ANDROID_NDK_HOME or the SDK names).
val cargoNdk by tasks.registering(Exec::class) {
    workingDir = coreDir
    commandLine(
        "cargo", "ndk",
        "-t", "arm64-v8a",
        "-p", "35",
        "-o", jniLibsDir.get().asFile.absolutePath,
        "build", "--release", "--lib",
    )
    inputs.dir(coreDir.resolve("src"))
    inputs.file(coreDir.resolve("Cargo.toml"))
    inputs.file(coreDir.resolve("Cargo.lock"))
    outputs.dir(jniLibsDir)
}

// Kotlin bindings read from the built library itself, so they can never
// describe a different core from the one packaged.
val uniffiBindings by tasks.registering(Exec::class) {
    dependsOn(cargoNdk)
    workingDir = coreDir
    commandLine(
        "cargo", "run", "--quiet", "--bin", "uniffi-bindgen", "--",
        "generate",
        // The copy in Cargo's own target directory: cargo-ndk strips the one
        // it writes to jniLibs, and that is right for the APK but leaves
        // bindgen nothing to read.
        "--library", coreDir.resolve("target/aarch64-linux-android/release/libdbdelve_core.so").absolutePath,
        "--language", "kotlin",
        "--no-format",
        "--out-dir", bindingsDir.get().asFile.absolutePath,
    )
    inputs.dir(jniLibsDir)
    outputs.dir(bindingsDir)
    // A stripped library makes bindgen write nothing and exit happily, and
    // the build then fails far away, on every type it names.
    doLast {
        check(bindingsDir.get().asFile.walk().any { it.extension == "kt" }) {
            "uniffi-bindgen generated no Kotlin: was it given a stripped libdbdelve_core.so?"
        }
    }
}

// The changelog the app shows under "What's new": the repository's own.
val changelog by tasks.registering(Copy::class) {
    from(rootProject.file("CHANGELOG.md"))
    into(layout.buildDirectory.dir("generated/changelog"))
}

tasks.named("preBuild") { dependsOn(uniffiBindings, changelog) }

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.12.01")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-core")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("androidx.biometric:biometric:1.1.0")
    implementation("androidx.fragment:fragment-ktx:1.8.5")
    implementation("net.java.dev.jna:jna:5.14.0@aar")
}
