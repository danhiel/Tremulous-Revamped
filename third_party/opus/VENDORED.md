libopus 1.6.1, from the official release tarball
https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz
(SHA-256 `6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1`,
as listed in https://downloads.xiph.org/releases/opus/SHA256SUMS.txt).
BSD 3-clause license (`COPYING`), Copyright Xiph.Org Foundation and the
contributors in `AUTHORS`.

Kept unmodified: `include/`, `celt/`, `silk/`, `src/`, the `*_sources.mk` /
`*_headers.mk` file lists (read by `crates/trem_opus/build.rs`), `COPYING`,
`AUTHORS`, `README`, `NEWS`, `package_version`. Left out: `dnn/` (the
optional neural features, off in this build), `tests/`, `doc/` and the
autotools / CMake / meson build systems.

To upgrade: replace these files with the same set from a newer tarball
(check its SHA-256 against xiph.org's list) and run `cargo test -p trem_opus`.
