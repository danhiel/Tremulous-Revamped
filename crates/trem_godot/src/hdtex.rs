//! 4K texture packs for the enhanced graphics mode (port addition).
//!
//! `TremHdBuilder` makes the pack offline from the player's own pk3 files:
//! every world / model / sky image is padded with its own wrapped edges (so
//! tiling textures stay seamless), upscaled with Real-ESRGAN
//! (realesrgan-ncnn-vulkan, on the GPU) in one or two x4 passes, cropped,
//! resized to `target` (4096) on its longest side with Lanczos, given its
//! original alpha back, mipmapped, block-compressed (S3TC: DXT1, or DXT5
//! with alpha) and written as `<key>.hdtex` (TREMHD02: each mip level
//! zstd-compressed, lossless, trem_hdpack, dec055), with a generated normal map
//! (`<key>.hdnrm`, 1024) beside it.
//!
//! At run time `with_store` serves them by image key (the pk3 path without
//! its extension). The pack lives outside the game's search path (default
//! `user://hd`), so pure servers' pk3 checks never see it: it works on
//! vanilla 1.1.0 and 1.3 servers alike. The enhanced mode samples the HD
//! texture, vanilla the original (F10 switches both).
//! `r_hdTextures 0` turns the pack off; `r_hdMaxSize` (4096 / 2048 / 1024)
//! skips the larger mip levels to save VRAM.

use godot::classes::image::{CompressMode, Format as ImageFormat, Interpolation};
use godot::classes::{Image, ImageTexture};
use godot::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};


/// The image key of a pk3 path: lower case, '/' separators, no extension.
pub fn key_of(path: &str) -> String {
    let p = path.replace('\\', "/").trim_start_matches('/').to_lowercase();
    match p.rsplit_once('.') {
        Some((stem, ext)) if matches!(ext, "tga" | "jpg" | "jpeg" | "png") => stem.to_string(),
        _ => p,
    }
}

/// Images worth upscaling: world textures, models and skies (not the HUD,
/// menus, fonts, icons or level shots).
pub fn wanted(path: &str) -> bool {
    let p = path.to_lowercase();
    let img = p.ends_with(".tga") || p.ends_with(".jpg") || p.ends_with(".png");
    img && (p.starts_with("textures/") || p.starts_with("models/") || p.starts_with("env/"))
}

/// The interface's images (the HD UI, enhanced mode): HUD and menu art,
/// icons, the fonts' glyph pages, level shots. Upscaled x4 (Real-ESRGAN, the
/// alpha channel too: the fonts' glyphs live in it), kept uncompressed.
pub fn wanted_ui(path: &str) -> bool {
    let p = path.to_lowercase();
    let img = p.ends_with(".tga") || p.ends_with(".jpg") || p.ends_with(".png");
    img && ["ui/", "gfx/2d/", "fonts/", "icons/", "levelshots/", "menu/"].iter().any(|d| p.starts_with(d))
}

thread_local! {
    /// the UI textures (by instance) and their image keys
    static UI_KEYS: RefCell<HashMap<i64, String>> = RefCell::new(HashMap::new());
    static UI_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Library::texture: a UI image's texture and its key, for `ui`.
pub fn register_ui(tex: &Gd<godot::classes::Texture2D>, key: &str) {
    UI_KEYS.with(|m| m.borrow_mut().insert(tex.instance_id().to_i64(), key.to_string()));
}

/// The enhanced mode draws the HD UI (gfx.rs).
pub fn set_ui_enabled(on: bool) {
    UI_ON.with(|c| c.set(on));
}

/// The texture to draw for a UI texture: its HD version in the enhanced
/// mode when the pack has one, else itself (the HUD, menus and console draw
/// with normalised texture coordinates, so either fits).
pub fn ui(tex: &Gd<godot::classes::Texture2D>) -> Gd<godot::classes::Texture2D> {
    if !UI_ON.with(|c| c.get()) {
        return tex.clone();
    }
    let key = UI_KEYS.with(|m| m.borrow().get(&tex.instance_id().to_i64()).cloned());
    key.and_then(|k| get(&k)).map(|h| h.albedo.upcast()).unwrap_or_else(|| tex.clone())
}

#[cfg(test)]
fn level_bytes(fmt: ImageFormat, w: usize, h: usize) -> usize {
    trem_hdpack::level_bytes(fmt.ord() as u32, w, h)
}

/// zstd level of the files the builder writes (dec055; `hdpack pack`
/// shrinks a pack built before it the same way).
const PACK_LEVEL: i32 = 19;

/// Write an Image (with its mip chain) as a .hdtex file: TREMHD02, each
/// level zstd-compressed (trem_hdpack; lossless).
pub fn write(path: &Path, img: &Gd<Image>, mean: [f32; 3], alpha: bool) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let data = img.get_data();
    let hd = trem_hdpack::Header { w: img.get_width() as usize, h: img.get_height() as usize, fmt: img.get_format().ord() as u32, mips: img.get_mipmap_count() as usize + 1, alpha, mean };
    let bytes = trem_hdpack::encode_levels(&hd, data.as_slice(), PACK_LEVEL)?;
    std::fs::write(path, bytes)
}

/// Read `rel` of the pack at `root` (TREMHD01 / 02 or a duplicate's
/// link), skipping the mip levels larger than `max_size`.
pub fn read(root: &Path, rel: &str, max_size: usize) -> Option<(Gd<Image>, [f32; 3])> {
    let t = trem_hdpack::read(root, rel, max_size)?;
    let fmt = ImageFormat::try_from_ord(t.header.fmt as i32)?;
    let img = Image::create_from_data(t.w as i32, t.h as i32, t.levels > 1, fmt, &PackedByteArray::from(t.data.as_slice()))?;
    Some((img, t.header.mean))
}

/// An HD texture: albedo, its normal map, mean linear albedo.
#[derive(Clone)]
pub struct HdTex {
    pub albedo: Gd<ImageTexture>,
    pub normal: Option<Gd<ImageTexture>>,
    pub mean: [f32; 3],
}

pub struct HdStore {
    root: PathBuf,
    max_size: usize,
    /// each key's texture and the map (`map_gen`) that last asked for it
    cache: HashMap<String, (Option<HdTex>, u32)>,
    map_gen: u32,
}

impl HdStore {
    pub fn get(&mut self, key: &str) -> Option<HdTex> {
        if let Some((t, g)) = self.cache.get_mut(key) {
            *g = self.map_gen;
            return t.clone();
        }
        let t = read(&self.root, &format!("{key}.hdtex"), self.max_size).and_then(|(img, mean)| {
            let albedo = ImageTexture::create_from_image(&img)?;
            let normal = read(&self.root, &format!("{key}.hdnrm"), self.max_size).and_then(|(n, _)| ImageTexture::create_from_image(&n));
            Some(HdTex { albedo, normal, mean })
        });
        self.cache.insert(key.to_string(), (t.clone(), self.map_gen));
        t
    }

    /// A map starts loading: a texture the last map did not ask for, and
    /// that only this cache still holds, goes (its VRAM is freed). Without
    /// this every map visited stayed loaded. One still held elsewhere (the
    /// session's model cache, or a scene not freed yet) stays, so a later
    /// `get` cannot load a second copy of it. (kept, dropped)
    fn new_map(&mut self) -> (usize, usize) {
        let before = self.cache.len();
        let gen = self.map_gen;
        let in_use = |t: &Option<HdTex>| t.as_ref().is_some_and(|h| h.albedo.get_reference_count() > 1 || h.normal.as_ref().is_some_and(|n| n.get_reference_count() > 1));
        self.cache.retain(|_, (t, g)| *g == gen || in_use(t));
        self.map_gen = gen.wrapping_add(1);
        (self.cache.len(), before - self.cache.len())
    }
}

/// A map starts loading (game.rs begin_load / begin_client_load).
pub fn new_map() {
    let r = STORE.with(|s| s.borrow_mut().as_mut().map(|h| h.new_map()));
    if let (Some((kept, dropped)), true) = (r, std::env::var_os("TREM_HD_DEBUG").is_some()) {
        let vram = godot::classes::RenderingServer::singleton().get_rendering_info(godot::classes::rendering_server::RenderingInfo::TEXTURE_MEM_USED);
        godot_print!("trem: 4K pack: {kept} textures kept from the last map, {dropped} dropped; texture VRAM before the load {} MB", vram / (1 << 20));
    }
}

thread_local! {
    static STORE: RefCell<Option<HdStore>> = const { RefCell::new(None) };
}

/// A pack folder: `<executable folder>/<name>` when a packaged game ships
/// it there (the release folder), else `user://<name>` (where it is built).
pub fn pack_dir(name: &str) -> String {
    let exe = godot::classes::Os::singleton().get_executable_path().to_string();
    if let Some((d, _)) = exe.rsplit_once(['/', '\\']) {
        let p = format!("{d}/{name}");
        if std::path::Path::new(&p).is_dir() {
            return p;
        }
    }
    godot::classes::ProjectSettings::singleton().globalize_path(&format!("user://{name}")).to_string()
}

/// (Re)open the pack: `root` "" = the packaged game's hd folder, else
/// user://hd (pack_dir); `enabled` false drops it.
pub fn configure(enabled: bool, root: &str, max_size: usize) {
    let root = if root.is_empty() { pack_dir("hd") } else { root.to_string() };
    let root = PathBuf::from(root);
    STORE.with(|s| {
        let mut s = s.borrow_mut();
        let same = s.as_ref().is_some_and(|h| h.root == root && h.max_size == max_size);
        if !enabled || !root.is_dir() {
            *s = None;
        } else if !same {
            *s = Some(HdStore { root, max_size: max_size.clamp(64, 8192), cache: HashMap::new(), map_gen: 0 });
        }
    });
}

/// The HD texture of an image key, when the pack has it.
pub fn get(key: &str) -> Option<HdTex> {
    STORE.with(|s| s.borrow_mut().as_mut().and_then(|h| h.get(key)))
}

/// Drop the loaded pack before the engine shuts down (TremGame::exit_tree).
pub fn release() {
    STORE.with(|s| s.borrow_mut().take());
}

pub fn active() -> bool {
    STORE.with(|s| s.borrow().is_some())
}

// --- the offline builder ---------------------------------------------------

#[derive(Clone)]
struct Item {
    key: String,
    src: String,
    w: usize,
    h: usize,
    pad: usize,
    /// Real-ESRGAN x4 passes (1 or 2)
    passes: u32,
    /// an interface image (`wanted_ui`): x4, its alpha upscaled too
    ui: bool,
    has_alpha: bool,
}

/// Builds the pack (tests/hd_build.gd drives it: prepare, the Real-ESRGAN
/// runs, then finish(i) for every item).
#[derive(GodotClass)]
#[class(init, base=RefCounted)]
pub struct TremHdBuilder {
    base: Base<RefCounted>,
    pk3: Option<crate::assets::Pk3Set>,
    items: Vec<Item>,
    out: PathBuf,
    work: PathBuf,
    target: usize,
}

fn wrap_pad(rgba: &[u8], w: usize, h: usize, p: usize) -> Vec<u8> {
    pad_with(rgba, w, h, p, |v, n| v.rem_euclid(n), 0..3)
}

/// Edge-repeat padding (interface images do not tile); `chan` picks the
/// channels written as RGB (0..3 the colour, 3..4 the alpha as grey).
fn clamp_pad(rgba: &[u8], w: usize, h: usize, p: usize, chan: std::ops::Range<usize>) -> Vec<u8> {
    pad_with(rgba, w, h, p, |v, n| v.clamp(0, n - 1), chan)
}

fn pad_with(rgba: &[u8], w: usize, h: usize, p: usize, at: impl Fn(isize, isize) -> isize, chan: std::ops::Range<usize>) -> Vec<u8> {
    let (pw, ph) = (w + 2 * p, h + 2 * p);
    let mut out = vec![0u8; pw * ph * 3];
    for y in 0..ph {
        let sy = at(y as isize - p as isize, h as isize) as usize;
        for x in 0..pw {
            let sx = at(x as isize - p as isize, w as isize) as usize;
            let s = (sy * w + sx) * 4;
            let d = (y * pw + x) * 3;
            for k in 0..3 {
                out[d + k] = rgba[s + chan.start + k.min(chan.len() - 1)];
            }
        }
    }
    out
}

#[godot_api]
impl TremHdBuilder {
    /// Decode every wanted image of the pk3s not yet in `out_dir`, pad it and
    /// write the Real-ESRGAN inputs to `work_dir`/{a_in, b_in} (a: two
    /// passes, b: one). Returns the number of items.
    #[func]
    fn prepare(&mut self, pk3_paths: PackedStringArray, out_dir: GString, work_dir: GString, target: i32) -> i32 {
        let paths: Vec<String> = pk3_paths.as_slice().iter().map(|s| s.to_string()).collect();
        let mut pk3 = crate::assets::Pk3Set::open(&paths);
        self.out = PathBuf::from(out_dir.to_string());
        self.work = PathBuf::from(work_dir.to_string());
        self.target = target.max(256) as usize;
        for d in ["a_in", "a_mid", "a_out", "b_in", "b_out"] {
            // a fresh round: nothing left over from an earlier one
            let _ = std::fs::remove_dir_all(self.work.join(d));
            let _ = std::fs::create_dir_all(self.work.join(d));
        }
        let mut files: Vec<String> = pk3.files().filter(|f| wanted(f) || wanted_ui(f)).cloned().collect();
        files.sort();
        let mut seen = std::collections::HashSet::new();
        self.items.clear();
        for f in files {
            let key = key_of(&f);
            if !seen.insert(key.clone()) || self.out.join(format!("{key}.hdtex")).exists() {
                continue;
            }
            let Some(mut img) = crate::assets::load_image(&mut pk3, &f) else { continue };
            img.clear_mipmaps();
            img.convert(ImageFormat::RGBA8);
            let (w, h) = (img.get_width() as usize, img.get_height() as usize);
            if wanted_ui(&f) {
                // the interface: x4 in one pass, edges repeated, alpha too
                if w.max(h) < 8 || w.max(h) > 1024 {
                    continue;
                }
                let pad = 4;
                let data = img.get_data();
                let has_alpha = data.as_slice().chunks_exact(4).any(|p| p[3] < 250);
                let idx = self.items.len();
                let save = |rgb: Vec<u8>, name: String| {
                    if let Some(p) = Image::create_from_data((w + 2 * pad) as i32, (h + 2 * pad) as i32, false, ImageFormat::RGB8, &PackedByteArray::from(rgb.as_slice())) {
                        let _ = p.save_png(&self.work.join("b_in").join(name).to_string_lossy().to_string());
                    }
                };
                save(clamp_pad(data.as_slice(), w, h, pad, 0..3), format!("{idx:05}.png"));
                if has_alpha {
                    save(clamp_pad(data.as_slice(), w, h, pad, 3..4), format!("{idx:05}a.png"));
                }
                self.items.push(Item { key, src: f, w, h, pad, passes: 1, ui: true, has_alpha });
                continue;
            }
            if w.max(h) < 48 {
                continue;
            }
            let m = w.max(h);
            let passes = if m * 8 <= self.target { 2 } else { 1 };
            let pad = (m / 8).max(4);
            let data = img.get_data();
            let padded = wrap_pad(data.as_slice(), w, h, pad);
            let Some(pimg) = Image::create_from_data((w + 2 * pad) as i32, (h + 2 * pad) as i32, false, ImageFormat::RGB8, &PackedByteArray::from(padded.as_slice())) else { continue };
            let idx = self.items.len();
            let dir = if passes == 2 { "a_in" } else { "b_in" };
            let _ = pimg.save_png(&self.work.join(dir).join(format!("{idx:05}.png")).to_string_lossy().to_string());
            self.items.push(Item { key, src: f, w, h, pad, passes, ui: false, has_alpha: false });
        }
        self.pk3 = Some(pk3);
        godot_print!("hd: {} images to upscale ({} with two passes)", self.items.len(), self.items.iter().filter(|i| i.passes == 2).count());
        self.items.len() as i32
    }

    /// Finish item `i`: crop, resize, alpha, mipmaps, compression, write.
    /// Returns a progress line ("" when skipped).
    #[func]
    fn finish(&mut self, i: i32) -> GString {
        let Some(it) = self.items.get(i as usize).cloned() else { return GString::new() };
        let dir = if it.passes == 2 { "a_out" } else { "b_out" };
        let png = self.work.join(dir).join(format!("{i:05}.png"));
        let Some(mut up) = Image::load_from_file(&png.to_string_lossy().to_string()) else { return GString::from(format!("hd: {} missing upscaled image", it.key).as_str()) };
        let scale = 4usize.pow(it.passes);
        up.convert(ImageFormat::RGBA8);
        let crop = up.get_region(Rect2i::new(Vector2i::new((it.pad * scale) as i32, (it.pad * scale) as i32), Vector2i::new((it.w * scale) as i32, (it.h * scale) as i32)));
        let Some(mut img) = crop else { return GString::new() };
        if it.ui {
            return self.finish_ui(i, &it, img);
        }
        let m = it.w.max(it.h);
        let (fw, fh) = ((it.w * self.target / m).max(4), (it.h * self.target / m).max(4));
        img.resize_ex(fw as i32, fh as i32).interpolation(Interpolation::LANCZOS).done();
        // the original alpha, smoothly scaled
        let mut alpha = false;
        if let Some(pk3) = self.pk3.as_mut() {
            if let Some(mut src) = crate::assets::load_image(pk3, &it.src) {
                src.clear_mipmaps();
                src.convert(ImageFormat::RGBA8);
                let sd = src.get_data();
                if sd.as_slice().chunks_exact(4).any(|p| p[3] < 250) {
                    alpha = true;
                    src.resize_ex(fw as i32, fh as i32).interpolation(Interpolation::CUBIC).done();
                    let a = src.get_data();
                    let mut d = img.get_data().as_slice().to_vec();
                    for (px, sa) in d.chunks_exact_mut(4).zip(a.as_slice().chunks_exact(4)) {
                        px[3] = sa[3];
                    }
                    if let Some(n) = Image::create_from_data(fw as i32, fh as i32, false, ImageFormat::RGBA8, &PackedByteArray::from(d.as_slice())) {
                        img = n;
                    }
                }
            }
        }
        // the normal map and mean colour from a 1024 copy
        let mut small = img.duplicate_resource();
        let sm = 1024usize.min(fw.max(fh));
        small.resize_ex((fw * sm / fw.max(fh)).max(4) as i32, (fh * sm / fw.max(fh)).max(4) as i32).interpolation(Interpolation::LANCZOS).done();
        let (normal, mean) = crate::gfx::derive_maps_image(&small, !alpha);
        img.generate_mipmaps();
        let _ = img.compress(CompressMode::S3TC);
        let out = self.out.join(format!("{}.hdtex", it.key));
        if let Err(e) = write(&out, &img, mean, alpha) {
            return GString::from(format!("hd: {}: {e}", out.display()).as_str());
        }
        if let Some(mut n) = normal {
            let _ = n.compress(CompressMode::S3TC);
            let _ = write(&self.out.join(format!("{}.hdnrm", it.key)), &n, mean, false);
        }
        for d in ["a_in", "a_mid", "a_out", "b_in", "b_out"] {
            let _ = std::fs::remove_file(self.work.join(d).join(format!("{i:05}.png")));
        }
        GString::from(format!("hd: {} {}x{} -> {}x{}", it.key, it.w, it.h, fw, fh).as_str())
    }
}

impl TremHdBuilder {
    /// An interface image: x4 exactly, its alpha from the upscaled alpha
    /// image (crisp glyph edges), mipmapped, uncompressed.
    fn finish_ui(&mut self, i: i32, it: &Item, mut img: Gd<Image>) -> GString {
        let (fw, fh) = (it.w * 4, it.h * 4);
        if img.get_width() as usize != fw || img.get_height() as usize != fh {
            return GString::from(format!("hd: {} upscaled image is {}x{}, not {fw}x{fh}", it.key, img.get_width(), img.get_height()).as_str());
        }
        if it.has_alpha {
            let a_png = self.work.join("b_out").join(format!("{i:05}a.png"));
            let a = Image::load_from_file(&a_png.to_string_lossy().to_string()).and_then(|mut a| {
                a.convert(ImageFormat::RGBA8);
                a.get_region(Rect2i::new(Vector2i::new((it.pad * 4) as i32, (it.pad * 4) as i32), Vector2i::new(fw as i32, fh as i32)))
            });
            let Some(a) = a else { return GString::from(format!("hd: {} missing upscaled alpha", it.key).as_str()) };
            let ad = a.get_data();
            let mut d = img.get_data().as_slice().to_vec();
            for (px, sa) in d.chunks_exact_mut(4).zip(ad.as_slice().chunks_exact(4)) {
                px[3] = sa[0];
            }
            if let Some(n) = Image::create_from_data(fw as i32, fh as i32, false, ImageFormat::RGBA8, &PackedByteArray::from(d.as_slice())) {
                img = n;
            }
        }
        // at most 2048 on the longest side (level shots, the title art)
        let m = fw.max(fh);
        let (fw, fh) = if m > 2048 { ((fw * 2048 / m).max(1), (fh * 2048 / m).max(1)) } else { (fw, fh) };
        if fw != img.get_width() as usize {
            img.resize_ex(fw as i32, fh as i32).interpolation(Interpolation::LANCZOS).done();
        }
        // the upscale must look like the original (a blank or another
        // image's result is dropped: the next round redoes it)
        let mean = |im: &Gd<Image>| -> [f64; 2] {
            let d = im.get_data();
            let n = (d.len() / 4).max(1) as f64;
            let (mut a, mut c) = (0.0, 0.0);
            for p in d.as_slice().chunks_exact(4) {
                a += p[3] as f64;
                c += (p[0] as f64 + p[1] as f64 + p[2] as f64) / 3.0;
            }
            [a / n, c / n]
        };
        let src = self.pk3.as_mut().and_then(|pk3| crate::assets::load_image(pk3, &it.src)).map(|mut s| {
            s.clear_mipmaps();
            s.convert(ImageFormat::RGBA8);
            s.resize_ex(fw as i32, fh as i32).interpolation(Interpolation::CUBIC).done();
            s
        });
        if let Some(s) = src.as_ref() {
            let (m0, m1) = (mean(s), mean(&img));
            if (m0[0] - m1[0]).abs() > 12.0 || (m0[1] - m1[1]).abs() > 20.0 {
                return GString::from(format!("hd: {} came out unlike the original (alpha {:.0}/{:.0}, colour {:.0}/{:.0}): not written", it.key, m1[0], m0[0], m1[1], m0[1]).as_str());
            }
        }
        img.generate_mipmaps();
        let out = self.out.join(format!("{}.hdtex", it.key));
        if let Err(e) = write(&out, &img, [0.0; 3], it.has_alpha) {
            return GString::from(format!("hd: {}: {e}", out.display()).as_str());
        }
        for f in [format!("{i:05}.png"), format!("{i:05}a.png")] {
            let _ = std::fs::remove_file(self.work.join("b_in").join(&f));
            let _ = std::fs::remove_file(self.work.join("b_out").join(&f));
        }
        GString::from(format!("hd: ui {} {}x{} -> {}x{}", it.key, it.w, it.h, fw, fh).as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interface_is_upscaled_apart() {
        assert!(wanted_ui("gfx/2d/crosshair.tga"));
        assert!(wanted_ui("fonts/fontImage_0_26.tga"));
        assert!(wanted_ui("ui/assets/title.tga"));
        assert!(!wanted_ui("textures/atcs/floor.tga"));
        // edge padding repeats the border; the alpha comes out as grey
        let px = [10, 20, 30, 40, 50, 60, 70, 80];
        let p = clamp_pad(&px, 2, 1, 1, 0..3);
        assert_eq!(&p[0..3], &[10, 20, 30]);
        let a = clamp_pad(&px, 2, 1, 1, 3..4);
        assert_eq!(&a[0..3], &[40, 40, 40]);
        assert_eq!(&a[(4 + 3) * 3..(4 + 3) * 3 + 3], &[80, 80, 80]);
    }

    #[test]
    fn keys_and_scope() {
        assert_eq!(key_of("textures/ATCS/Floor.TGA"), "textures/atcs/floor");
        assert_eq!(key_of("models\\buildables\\reactor\\reactor.jpg"), "models/buildables/reactor/reactor");
        assert!(wanted("textures/atcs/floor.tga"));
        assert!(wanted("env/atcs/sky_up.jpg"));
        assert!(!wanted("gfx/2d/crosshair.tga"));
        assert!(!wanted("ui/assets/title.tga"));
        assert!(!wanted("textures/atcs/floor.shader"));
    }

    #[test]
    fn mip_chain_sizes() {
        // 4096 DXT1 top level 8 MiB, 4x4 minimum block
        assert_eq!(level_bytes(ImageFormat::DXT1, 4096, 4096), 8 * 1024 * 1024);
        assert_eq!(level_bytes(ImageFormat::DXT5, 2, 1), 16);
    }

    #[test]
    fn padding_wraps() {
        // 2x2: red green / blue white, padded by 1
        let px = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255];
        let p = wrap_pad(&px, 2, 2, 1);
        // the top-left padded texel is the bottom-right source texel
        assert_eq!(&p[0..3], &[255, 255, 255]);
        // the centre starts with the source's top-left
        assert_eq!(&p[(4 + 1) * 3..(4 + 1) * 3 + 3], &[255, 0, 0]);
    }
}
