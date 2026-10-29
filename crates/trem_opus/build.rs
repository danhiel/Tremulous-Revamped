//! Builds libopus 1.6.1 (third_party/opus, the xiph.org release tarball's
//! sources) with the `cc` crate, so it also cross-compiles
//! (x86_64-pc-windows-gnu with mingw). The file lists are upstream's own
//! (`celt_sources.mk`, `silk_sources.mk`, `opus_sources.mk`), read here.
//! Floating point, portable C only (no SIMD / run-time CPU detection), no
//! neural features (DRED, OSCE, deep PLC: their `dnn/` is not vendored),
//! hardening on: the defaults of upstream's CMake build, minus intrinsics.

use std::path::Path;

/// The variables of upstream's makefile fragments that a floating-point
/// build without intrinsics or DNN compiles.
const LISTS: &[(&str, &str)] = &[
    ("celt_sources.mk", "CELT_SOURCES"),
    ("silk_sources.mk", "SILK_SOURCES"),
    ("silk_sources.mk", "SILK_SOURCES_FLOAT"),
    ("opus_sources.mk", "OPUS_SOURCES"),
    ("opus_sources.mk", "OPUS_SOURCES_FLOAT"),
];

/// The files of `VAR = \` ... (backslash-continued lines) in a .mk file.
fn mk_list(mk: &str, var: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = mk.lines();
    while let Some(l) = lines.next() {
        let Some((name, rest)) = l.split_once('=') else { continue };
        if name.trim() != var {
            continue;
        }
        let mut cont = rest.trim_end().ends_with('\\');
        out.extend(rest.split_whitespace().filter(|w| *w != "\\").map(str::to_owned));
        while cont {
            let Some(l) = lines.next() else { break };
            cont = l.trim_end().ends_with('\\');
            out.extend(l.split_whitespace().filter(|w| *w != "\\").map(str::to_owned));
        }
        return out;
    }
    panic!("{var} not found");
}

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // the workspace root, without ".." (it is cut from __FILE__ below)
    let root = manifest.parent().and_then(Path::parent).unwrap().to_path_buf();
    let opus = root.join("third_party").join("opus");
    let version = std::fs::read_to_string(opus.join("package_version")).expect("third_party/opus/package_version");
    let version = version.trim().trim_start_matches("PACKAGE_VERSION=").trim_matches('"');
    let mut b = cc::Build::new();
    b.warnings(false)
        .define("OPUS_BUILD", None)
        .define("HAVE_LRINTF", None)
        .define("FLOATING_POINT", None)
        .define("USE_ALLOCA", None)
        .define("ENABLE_HARDENING", None)
        .define("PACKAGE_VERSION", Some(format!("\"{version}\"").as_str()))
        .include(opus.join("include"))
        .include(opus.join("celt"))
        .include(opus.join("silk"))
        .include(opus.join("silk/float"))
        .include(opus.join("src"))
        .flag_if_supported("-fno-fast-math")
        .opt_level(2);
    // hardening reports failures with __FILE__: the paths relative to the
    // workspace, not the build machine's folders (user names included)
    if b.get_compiler().is_like_msvc() {
        b.flag(format!("/d1trimfile:{}\\", root.display()));
    } else {
        b.flag_if_supported(format!("-ffile-prefix-map={}=.", root.display()));
    }
    for (file, var) in LISTS {
        let mk = std::fs::read_to_string(opus.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        for s in mk_list(&mk, var) {
            assert!(Path::new(&s).is_relative() && !s.contains(".."), "{file}: {s}");
            b.file(opus.join(&s));
        }
    }
    b.compile("opus");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={}", opus.display());
}
