//! Builds speech.cpp (the `speech-static` CMake target and everything it depends on) and generates
//! the Rust bindings for `include/speech.h`.
//!
//! The source tree is `$SPEECH_CPP_DIR` when that variable is set, else the `speech.cpp` git
//! submodule beside this file. A build script may panic: every failure here is a message for the
//! developer, not an error the application can handle.

// A build script is not production code (G6 applies to the app): a clear panic is its only way to
// stop the build with a message.
#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The static libraries of the `speech-static` target in link order: dependents before their
/// dependencies. ld64 does not need the order, GNU ld does, and a fixed list keeps the link line
/// the same on every build.
const LINK_ORDER: &[&str] = &[
    "speech-static",
    "qwen3tts",
    "irodoritts",
    "drama3",
    "fastconformer",
    "qwen3asr",
    "nemotrontranscribe",
    "silerovad",
    "sortformer",
    "speechcommon",
    "ggml",
    "ggml-metal",
    "ggml-blas",
    "ggml-cpu",
    "ggml-base",
];

/// The CPU baseline for a macOS arm64 build that must run on every Apple Silicon Mac.
///
/// ggml compiles its CPU kernels for the features the compiler defines, so the flag decides which
/// Macs the binary runs on. The reference is clang's own model of the M1 (`-mcpu=apple-m1`): it
/// defines `__ARM_FEATURE_DOTPROD` and `__ARM_FEATURE_FP16_VECTOR_ARITHMETIC` and leaves out
/// `__ARM_FEATURE_MATMUL_INT8` (i8mm), `__ARM_FEATURE_BF16` (M2 and later) and `__ARM_FEATURE_SME`
/// (M4 and later). `-mcpu=native`, ggml's default, would enable BF16 and SME on the build machine
/// and the binary would crash with SIGILL on an M1. `GGML_CPU_ARM_ARCH` is passed as `-march=`, so
/// the same feature set is spelled as an architecture: armv8.4-a (which the M1 implements) with
/// dotprod and fp16 made explicit. i8mm is left out because the compiler's M1 model does not
/// enable it; it only speeds up the CPU matmul kernels, which the GPU path uses for decoding alone.
const APPLE_SILICON_BASELINE: &str = "armv8.4-a+dotprod+fp16";

fn main() {
    let manifest_dir = PathBuf::from(env_or_panic("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env_or_panic("OUT_DIR"));
    let target_os = env_or_panic("CARGO_CFG_TARGET_OS");
    let target_arch = env_or_panic("CARGO_CFG_TARGET_ARCH");

    println!("cargo:rerun-if-env-changed=SPEECH_CPP_DIR");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

    let source = source_dir(&manifest_dir);
    for relative in [
        "VERSION",
        "CMakeLists.txt",
        "include/speech.h",
        "src",
        "ggml/CMakeLists.txt",
        "ggml/src",
    ] {
        println!("cargo:rerun-if-changed={}", source.join(relative).display());
    }

    let build_dir = build_speech(&source, &target_os, &target_arch);
    link(&build_dir, &target_os);
    generate_bindings(&source, &out_dir, &target_os);
}

fn env_or_panic(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("speech-sys: cargo did not set {name}"))
}

/// The speech.cpp source tree, checked for the one file every step needs.
fn source_dir(manifest_dir: &Path) -> PathBuf {
    let source = match env::var_os("SPEECH_CPP_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => manifest_dir.join("speech.cpp"),
    };
    let header = source.join("include").join("speech.h");
    assert!(
        header.is_file(),
        "speech-sys: {} is not a speech.cpp source tree (no include/speech.h). Run \
         `git submodule update --init --recursive` or set SPEECH_CPP_DIR.",
        source.display()
    );
    assert!(
        source.join("ggml").join("CMakeLists.txt").is_file(),
        "speech-sys: the ggml submodule of speech.cpp is missing at {}. Run \
         `git submodule update --init --recursive` inside it.",
        source.join("ggml").display()
    );
    source
}

/// Configures and builds the `speech-static` target, Release, static ggml, and returns the CMake
/// build directory that holds the archives.
fn build_speech(source: &Path, target_os: &str, target_arch: &str) -> PathBuf {
    let mut config = cmake::Config::new(source);
    config
        // Cargo's debug profile would otherwise build a Debug ggml, ten times slower.
        .profile("Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        // Never -mcpu=native: the binary is distributed.
        .define("GGML_NATIVE", "OFF")
        // Apple clang has no OpenMP; a Homebrew libomp found on a build machine would add a dylib
        // the app cannot ship. ggml threads with its own pool instead.
        .define("GGML_OPENMP", "OFF")
        .define("GGML_BUILD_TESTS", "OFF")
        .define("GGML_BUILD_EXAMPLES", "OFF")
        // Only the library target is built: the CLI, the checks and the timing tools are skipped.
        .build_target("speech-static");

    if target_os == "macos" {
        config
            // The cmake crate does not forward MACOSX_DEPLOYMENT_TARGET. Without it CMake compiles
            // for the SDK's own version, and the archives would carry a newer minimum than the Rust
            // code they are linked into.
            .define(
                "CMAKE_OSX_DEPLOYMENT_TARGET",
                macos_deployment_target(target_arch),
            )
            .define("GGML_METAL", "ON")
            // The Metal shaders go into the archive as a data section, so the app bundle needs no
            // default.metallib beside the executable.
            .define("GGML_METAL_EMBED_LIBRARY", "ON")
            .define("GGML_ACCELERATE", "ON")
            .define("GGML_BLAS", "ON")
            .define("GGML_BLAS_VENDOR", "Apple");
        if target_arch == "aarch64" {
            config.define("GGML_CPU_ARM_ARCH", APPLE_SILICON_BASELINE);
        }
    } else {
        config.define("GGML_METAL", "OFF");
    }

    let dst = config.build();
    // With a build target instead of `install`, the archives stay in the CMake build tree.
    dst.join("build")
}

/// The minimum macOS the archives are compiled for: `MACOSX_DEPLOYMENT_TARGET`, which rustc and
/// cc honour as well, or rustc's own default for the target when it is not set, so that the
/// archives and the Rust code agree.
fn macos_deployment_target(target_arch: &str) -> String {
    if let Ok(target) = env::var("MACOSX_DEPLOYMENT_TARGET")
        && !target.is_empty()
    {
        return target;
    }
    if target_arch == "aarch64" {
        "11.0".to_owned()
    } else {
        "10.12".to_owned()
    }
}

/// Emits the link line: every archive of the build tree in `LINK_ORDER`, the system frameworks
/// and the C++ runtime.
fn link(build_dir: &Path, target_os: &str) {
    let mut archives = Vec::new();
    collect_archives(build_dir, &mut archives);
    assert!(
        !archives.is_empty(),
        "speech-sys: no static library under {}",
        build_dir.display()
    );

    let mut dirs = BTreeSet::new();
    let mut names = BTreeSet::new();
    for archive in &archives {
        if let Some(dir) = archive.parent() {
            dirs.insert(dir.to_path_buf());
        }
        if let Some(name) = archive_name(archive) {
            names.insert(name);
        }
    }
    for dir in &dirs {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }

    let mut ordered = Vec::new();
    for lib in LINK_ORDER {
        if names.remove(*lib) {
            ordered.push((*lib).to_owned());
        } else if *lib == "ggml-metal" || *lib == "ggml-blas" {
            // Platform backends are absent where they are switched off.
        } else {
            panic!(
                "speech-sys: the build produced no lib{lib}.a under {}; speech.cpp's CMake targets \
                 changed, update LINK_ORDER",
                build_dir.display()
            );
        }
    }
    // A backend this list does not know yet is linked after the ones it does.
    for extra in names {
        println!("cargo:warning=speech-sys: linking lib{extra}.a, which LINK_ORDER does not list");
        ordered.push(extra);
    }
    for lib in &ordered {
        println!("cargo:rustc-link-lib=static={lib}");
    }

    if target_os == "macos" {
        for framework in ["Foundation", "Metal", "MetalKit", "Accelerate"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
        println!("cargo:rustc-link-lib=objc");
        println!("cargo:rustc-link-lib=c++");
        link_clang_runtime();
    } else {
        println!("cargo:rustc-link-lib=stdc++");
        println!("cargo:rustc-link-lib=pthread");
        println!("cargo:rustc-link-lib=m");
    }
}

/// ggml-metal's Objective-C code uses `@available(macOS ...)`, which compiles to calls to
/// `__isPlatformVersionAtLeast` from clang's compiler runtime. rustc links with the system `cc` but
/// not that runtime, so a release build (LTO, no dead code left to hide it) fails to link without
/// it. The runtime directory comes from the same clang that compiled speech.cpp.
fn link_clang_runtime() {
    let output = Command::new("clang").arg("--print-runtime-dir").output();
    let dir = match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        _ => {
            println!(
                "cargo:warning=speech-sys: clang --print-runtime-dir failed; not linking clang_rt.osx"
            );
            return;
        }
    };
    if Path::new(&dir).join("libclang_rt.osx.a").is_file() {
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
    } else {
        println!("cargo:warning=speech-sys: libclang_rt.osx.a not found in {dir}");
    }
}

fn collect_archives(dir: &Path, archives: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("speech-sys: cannot read {}: {error}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_archives(&path, archives);
        } else if path.extension().is_some_and(|extension| extension == "a") {
            archives.push(path);
        }
    }
}

/// `libfoo.a` -> `foo`.
fn archive_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.strip_prefix("lib").map(str::to_owned)
}

/// Generates `bindings.rs` for `include/speech.h`: the `speech_*` functions and types and the
/// `SPEECH_*` constants, enums as integer constants.
fn generate_bindings(source: &Path, out_dir: &Path, target_os: &str) {
    hint_libclang_path(target_os);

    let include_dir = source.join("include");
    let header = include_dir.join("speech.h");
    let mut builder = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .clang_arg(format!("-I{}", include_dir.display()))
        .allowlist_function("speech_.*")
        .allowlist_type("speech_.*")
        .allowlist_var("SPEECH_.*")
        // Integer constants: a status or option value the header does not know is still a valid
        // integer, where a Rust enum with an unknown discriminant would be undefined behaviour.
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .generate_comments(true)
        .layout_tests(false)
        .derive_debug(true)
        .derive_copy(true);

    if target_os == "macos" {
        // `<stdint.h>` of clang's builtin headers `#include_next`s the SDK's copy, which libclang
        // finds only with the sysroot.
        if let Some(sdk) = macos_sdk_path() {
            builder = builder.clang_arg(format!("-isysroot{sdk}"));
        }
    }

    let bindings = builder.generate().unwrap_or_else(|error| {
        panic!(
            "speech-sys: bindgen failed on {}: {error}",
            header.display()
        )
    });
    let out = out_dir.join("bindings.rs");
    bindings
        .write_to_file(&out)
        .unwrap_or_else(|error| panic!("speech-sys: cannot write {}: {error}", out.display()));
}

/// The macOS SDK root, as `xcrun` reports it.
fn macos_sdk_path() -> Option<String> {
    if let Ok(root) = env::var("SDKROOT")
        && !root.is_empty()
    {
        return Some(root);
    }
    let output = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| path.to_owned())
}

/// Points clang-sys at Xcode's libclang when `LIBCLANG_PATH` is not set and no libclang is in the
/// places clang-sys searches by itself, so that a machine with Xcode alone builds without setup.
fn hint_libclang_path(target_os: &str) {
    if target_os != "macos" || env::var_os("LIBCLANG_PATH").is_some() {
        return;
    }
    let Ok(output) = Command::new("xcode-select").arg("-p").output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let developer = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let lib_dir = Path::new(&developer)
        .join("Toolchains")
        .join("XcodeDefault.xctoolchain")
        .join("usr")
        .join("lib");
    if lib_dir.join("libclang.dylib").is_file() {
        // SAFETY: a build script runs its main thread alone at this point; no other thread reads
        // the environment while it is set.
        unsafe { env::set_var("LIBCLANG_PATH", &lib_dir) };
    }
}
