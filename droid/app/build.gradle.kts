plugins {
    id("com.android.application")
}

val rustLibs = layout.buildDirectory.dir("rustJniLibs").get().asFile

android {
    namespace = "top.dns7.feedstr"
    compileSdk = 36
    ndkVersion = "28.2.13676358"

    defaultConfig {
        applicationId = "top.dns7.feedstr"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
    }

    sourceSets {
        getByName("main") {
            jniLibs.directories.add(rustLibs.path)
        }
    }
}

// The app itself is Rust (the feedstr-droid crate in this folder), built for
// phones (arm64-v8a) and the emulator (x86_64) with cargo-ndk.
val ndk = androidComponents.sdkComponents.ndkDirectory
tasks.register<Exec>("cargoBuild") {
    workingDir = rootProject.projectDir
    commandLine(
        "cargo", "ndk", "-t", "arm64-v8a", "-t", "x86_64", "-o", rustLibs.path,
        "build", "--profile", "android", "-p", "feedstr-droid",
    )
    doFirst {
        environment("ANDROID_NDK_HOME", ndk.get().asFile.path)
    }
}

tasks.named("preBuild") {
    dependsOn("cargoBuild")
}
