//! Native runtime for compiled Inga programs.
//!
//! The LLVM backend (`inga-codegen`) emits calls to these `extern "C"`
//! functions. Conventions shared with the backend:
//!
//! - Every Inga value is one `i64`. Ints/Bools/Durations are raw integers;
//!   strings, structs, maps, lists, closures are pointers cast to `i64`.
//! - A string is `{ i64 byte_len, bytes... }`.
//! - A list is `{ i64 len, item0, item1, ... }`.
//! - `Option` is `0` for `None`, else a pointer to one boxed value.
//! - Memory is bump-allocated and never freed (no GC in v0.2; programs are
//!   short-lived processes).

use std::io::Write;
use std::time::Instant;

thread_local! {
    static EPOCH: Instant = Instant::now();
}

// ---- allocator ---------------------------------------------------------------
//
// Raw bump allocator. Compiled Inga programs are single-threaded, allocations
// are 8-aligned, never freed, and codegen initializes every slot it
// allocates, so this can be a two-pointer fast path (~3 ns).

const CHUNK: usize = 1 << 22; // 4 MiB

static mut BUMP_PTR: *mut u8 = std::ptr::null_mut();
static mut BUMP_END: *mut u8 = std::ptr::null_mut();

#[cold]
unsafe fn bump_refill(size: usize) {
    let cap = CHUNK.max(size);
    let layout = std::alloc::Layout::from_size_align(cap, 8).unwrap();
    let chunk = std::alloc::alloc(layout); // leaked by design
    BUMP_PTR = chunk;
    BUMP_END = chunk.add(cap);
}

/// Allocate `size` bytes, 8-aligned, uninitialized. Never freed.
#[no_mangle]
pub extern "C" fn rt_alloc(size: i64) -> *mut u8 {
    unsafe {
        let size = ((size.max(0) as usize) + 7) & !7;
        if BUMP_PTR.is_null() || BUMP_PTR.add(size) > BUMP_END {
            bump_refill(size);
        }
        let p = BUMP_PTR;
        BUMP_PTR = BUMP_PTR.add(size);
        p
    }
}

// ---- strings -------------------------------------------------------------------

unsafe fn str_bytes<'a>(s: i64) -> &'a [u8] {
    let p = s as *const u8;
    let len = *(p as *const i64) as usize;
    std::slice::from_raw_parts(p.add(8), len)
}

fn new_str(bytes: &[u8]) -> i64 {
    let p = rt_alloc(8 + bytes.len() as i64);
    unsafe {
        *(p as *mut i64) = bytes.len() as i64;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(8), bytes.len());
    }
    p as i64
}

#[no_mangle]
pub extern "C" fn rt_str_concat(a: i64, b: i64) -> i64 {
    unsafe {
        let (x, y) = (str_bytes(a), str_bytes(b));
        let p = rt_alloc(8 + (x.len() + y.len()) as i64);
        *(p as *mut i64) = (x.len() + y.len()) as i64;
        std::ptr::copy_nonoverlapping(x.as_ptr(), p.add(8), x.len());
        std::ptr::copy_nonoverlapping(y.as_ptr(), p.add(8 + x.len()), y.len());
        p as i64
    }
}

#[no_mangle]
pub extern "C" fn rt_int_to_str(n: i64) -> i64 {
    let mut buf = [0u8; 20];
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    let _ = write!(cursor, "{n}");
    let len = cursor.position() as usize;
    new_str(&buf[..len])
}

/// Number of decimal characters of `n` (the compiler's len-of-interpolation fold).
#[no_mangle]
pub extern "C" fn rt_int_digits(mut n: i64) -> i64 {
    let mut digits = 1i64;
    if n < 0 {
        digits += 1;
        n = -(n / 10); // avoid overflow on i64::MIN by dividing first
        if n == 0 {
            return digits;
        }
    }
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

#[no_mangle]
pub extern "C" fn rt_float_to_str(bits: i64) -> i64 {
    let f = f64::from_bits(bits as u64);
    let text =
        if f.fract() == 0.0 && f.is_finite() { format!("{f:.1}") } else { format!("{f}") };
    new_str(text.as_bytes())
}

#[no_mangle]
pub extern "C" fn rt_bool_to_str(b: i64) -> i64 {
    new_str(if b != 0 { b"true" } else { b"false" })
}

#[no_mangle]
pub extern "C" fn rt_duration_to_str(ms: i64) -> i64 {
    let text = if ms % 3_600_000 == 0 && ms != 0 {
        format!("{}.hours", ms / 3_600_000)
    } else if ms % 60_000 == 0 && ms != 0 {
        format!("{}.minutes", ms / 60_000)
    } else if ms % 1000 == 0 && ms != 0 {
        format!("{}.seconds", ms / 1000)
    } else {
        format!("{ms}.millis")
    };
    new_str(text.as_bytes())
}

/// Character count (Inga's `len` on strings counts chars, not bytes).
#[no_mangle]
pub extern "C" fn rt_str_chars(s: i64) -> i64 {
    unsafe { std::str::from_utf8_unchecked(str_bytes(s)).chars().count() as i64 }
}

#[no_mangle]
pub extern "C" fn rt_str_eq(a: i64, b: i64) -> i64 {
    unsafe { (str_bytes(a) == str_bytes(b)) as i64 }
}

/// -1 / 0 / 1 for string ordering.
#[no_mangle]
pub extern "C" fn rt_str_cmp(a: i64, b: i64) -> i64 {
    unsafe {
        match str_bytes(a).cmp(str_bytes(b)) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }
}

#[no_mangle]
pub extern "C" fn rt_show_list_int(list: i64) -> i64 {
    unsafe {
        let p = list as *const i64;
        let len = *p;
        let mut out = String::from("[");
        for i in 0..len {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&(*p.add(1 + i as usize)).to_string());
        }
        out.push(']');
        new_str(out.as_bytes())
    }
}

#[no_mangle]
pub extern "C" fn rt_show_list_str(list: i64) -> i64 {
    unsafe {
        let p = list as *const i64;
        let len = *p;
        let mut out = String::from("[");
        for i in 0..len {
            if i > 0 {
                out.push_str(", ");
            }
            out.push('"');
            out.push_str(std::str::from_utf8_unchecked(str_bytes(*p.add(1 + i as usize))));
            out.push('"');
        }
        out.push(']');
        new_str(out.as_bytes())
    }
}

// ---- I/O, time, panic -------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rt_print(s: i64) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(unsafe { str_bytes(s) });
}

#[no_mangle]
pub extern "C" fn rt_println(s: i64) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(unsafe { str_bytes(s) });
    let _ = lock.write_all(b"\n");
}

#[no_mangle]
pub extern "C" fn rt_now_millis() -> i64 {
    EPOCH.with(|e| e.elapsed().as_millis() as i64)
}

#[no_mangle]
pub extern "C" fn rt_now_micros() -> i64 {
    EPOCH.with(|e| e.elapsed().as_micros() as i64)
}

#[no_mangle]
pub extern "C" fn rt_sleep_millis(ms: i64) {
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    }
}

#[no_mangle]
pub extern "C" fn rt_panic(msg: i64) -> ! {
    let text = unsafe { String::from_utf8_lossy(str_bytes(msg)).into_owned() };
    eprintln!("runtime error: {text}");
    std::process::exit(101);
}

// ---- maps ----------------------------------------------------------------------
//
// Open-addressing hash map, linear probing, fibonacci hashing. Keys are raw
// i64s (Int keys) or string pointers (String keys — hashed/compared by
// content); a map only ever sees one key kind because Inga's types are static.

const EMPTY: u8 = 0;
const FULL: u8 = 1;
const TOMB: u8 = 2;

struct RtMap {
    keys: Vec<i64>,
    vals: Vec<i64>,
    state: Vec<u8>,
    len: usize,
    mask: usize,
}

impl RtMap {
    fn with_cap(cap: usize) -> RtMap {
        RtMap {
            keys: vec![0; cap],
            vals: vec![0; cap],
            state: vec![EMPTY; cap],
            len: 0,
            mask: cap - 1,
        }
    }

    fn grow_if_needed(&mut self, hash_of: impl Fn(i64) -> u64) {
        if (self.len + 1) * 4 < (self.mask + 1) * 3 {
            return;
        }
        let mut next = RtMap::with_cap((self.mask + 1) * 2);
        for i in 0..self.keys.len() {
            if self.state[i] == FULL {
                next.insert_raw(self.keys[i], self.vals[i], hash_of(self.keys[i]));
            }
        }
        *self = next;
    }

    fn insert_raw(&mut self, key: i64, val: i64, hash: u64) {
        let mut i = (hash as usize) & self.mask;
        loop {
            if self.state[i] != FULL {
                self.state[i] = FULL;
                self.keys[i] = key;
                self.vals[i] = val;
                self.len += 1;
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
}

#[inline]
fn int_hash(k: i64) -> u64 {
    (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[inline]
fn str_hash(s: i64) -> u64 {
    // FNV-1a over the bytes.
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in unsafe { str_bytes(s) } {
        h = (h ^ b as u64).wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn map_ref<'a>(m: i64) -> &'a mut RtMap {
    unsafe { &mut *(m as *mut RtMap) }
}

#[no_mangle]
pub extern "C" fn rt_map_new() -> i64 {
    Box::into_raw(Box::new(RtMap::with_cap(16))) as i64
}

fn box_value(v: i64) -> i64 {
    let p = rt_alloc(8) as *mut i64;
    unsafe { *p = v };
    p as i64
}

macro_rules! map_ops {
    ($set:ident, $get:ident, $get_or:ident, $del:ident, $hash:expr, $eq:expr) => {
        #[no_mangle]
        pub extern "C" fn $set(m: i64, key: i64, val: i64) {
            let map = map_ref(m);
            map.grow_if_needed($hash);
            let hash = $hash(key);
            let mut i = (hash as usize) & map.mask;
            let mut first_tomb = usize::MAX;
            loop {
                match map.state[i] {
                    FULL if $eq(map.keys[i], key) => {
                        map.vals[i] = val;
                        return;
                    }
                    FULL => {}
                    TOMB => {
                        if first_tomb == usize::MAX {
                            first_tomb = i;
                        }
                    }
                    _ => {
                        let slot = if first_tomb != usize::MAX { first_tomb } else { i };
                        map.state[slot] = FULL;
                        map.keys[slot] = key;
                        map.vals[slot] = val;
                        map.len += 1;
                        return;
                    }
                }
                i = (i + 1) & map.mask;
            }
        }

        /// Returns an Option: 0 for None, else a pointer to the boxed value.
        #[no_mangle]
        pub extern "C" fn $get(m: i64, key: i64) -> i64 {
            let map = map_ref(m);
            let hash = $hash(key);
            let mut i = (hash as usize) & map.mask;
            loop {
                match map.state[i] {
                    FULL if $eq(map.keys[i], key) => return box_value(map.vals[i]),
                    EMPTY => return 0,
                    _ => {}
                }
                i = (i + 1) & map.mask;
            }
        }

        /// `getOrElse(map.get(k), default)` fused by the compiler: no Option box.
        #[no_mangle]
        pub extern "C" fn $get_or(m: i64, key: i64, default: i64) -> i64 {
            let map = map_ref(m);
            let hash = $hash(key);
            let mut i = (hash as usize) & map.mask;
            loop {
                match map.state[i] {
                    FULL if $eq(map.keys[i], key) => return map.vals[i],
                    EMPTY => return default,
                    _ => {}
                }
                i = (i + 1) & map.mask;
            }
        }

        #[no_mangle]
        pub extern "C" fn $del(m: i64, key: i64) {
            let map = map_ref(m);
            let hash = $hash(key);
            let mut i = (hash as usize) & map.mask;
            loop {
                match map.state[i] {
                    FULL if $eq(map.keys[i], key) => {
                        map.state[i] = TOMB;
                        map.len -= 1;
                        return;
                    }
                    EMPTY => return,
                    _ => {}
                }
                i = (i + 1) & map.mask;
            }
        }
    };
}

map_ops!(rt_map_set_int, rt_map_get_int, rt_map_get_or_int, rt_map_del_int, int_hash, |a: i64,
                                                                                       b: i64| {
    a == b
});
map_ops!(rt_map_set_str, rt_map_get_str, rt_map_get_or_str, rt_map_del_str, str_hash, |a: i64,
                                                                                       b: i64| {
    rt_str_eq(a, b) != 0
});

#[no_mangle]
pub extern "C" fn rt_map_size(m: i64) -> i64 {
    map_ref(m).len as i64
}

// ---- range / random -------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rt_range(n: i64) -> i64 {
    let n = n.max(0);
    let p = rt_alloc(8 * (n + 1)) as *mut i64;
    unsafe {
        *p = n;
        for i in 0..n {
            *p.add(1 + i as usize) = i;
        }
    }
    p as i64
}

static mut RNG_STATE: u64 = 0;

/// xorshift64*, seeded from the clock on first use.
#[no_mangle]
pub extern "C" fn rt_random(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    unsafe {
        if RNG_STATE == 0 {
            RNG_STATE = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x4d595df4d0f33173)
                | 1;
        }
        RNG_STATE ^= RNG_STATE << 13;
        RNG_STATE ^= RNG_STATE >> 7;
        RNG_STATE ^= RNG_STATE << 17;
        ((RNG_STATE.wrapping_mul(0x2545F4914F6CDD1D) >> 32) % n as u64) as i64
    }
}

// ---- graphics (GL-backed via miniquad/macroquad) ----------------------------------
//
// `rt_gfx_run` owns the window and event loop and invokes the Inga frame
// closure once per frame, so Inga programs never need an unbounded game-loop
// recursion. The closure uses the standard closure ABI:
// `{ fnptr, captures... }`, `fnptr(env) -> { value, err }`.

#[repr(C)]
pub struct RetPair {
    pub value: i64,
    pub err: i64,
}

type FrameFn = unsafe extern "C" fn(*mut u8) -> RetPair;

#[no_mangle]
pub extern "C" fn rt_gfx_run(width: i64, height: i64, title: i64, closure: i64) {
    let title = unsafe { String::from_utf8_lossy(str_bytes(title)).into_owned() };
    let conf = macroquad::window::Conf {
        window_title: title,
        window_width: width as i32,
        window_height: height as i32,
        high_dpi: true,
        ..Default::default()
    };
    // Debug/CI hook: INGA_GFX_SHOT=<path.png> renders INGA_GFX_SHOT_FRAME
    // frames (default 30), saves a screenshot of the framebuffer, and exits.
    let shot = std::env::var("INGA_GFX_SHOT").ok();
    let shot_frame: u32 = std::env::var("INGA_GFX_SHOT_FRAME")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    macroquad::Window::from_config(conf, async move {
        let mut frame_no = 0u32;
        loop {
            let env = closure as *mut u8;
            let fp: FrameFn = unsafe { std::mem::transmute(*(env as *const i64)) };
            let r = unsafe { fp(env) };
            if r.err != 0 {
                eprintln!("runtime error: unhandled error escaped the frame closure");
                std::process::exit(101);
            }
            frame_no += 1;
            if let Some(path) = &shot {
                if frame_no == shot_frame {
                    macroquad::texture::get_screen_data().export_png(path);
                    std::process::exit(0);
                }
            }
            macroquad::window::next_frame().await;
        }
    });
}

fn color(r: i64, g: i64, b: i64, a: i64) -> macroquad::color::Color {
    macroquad::color::Color::from_rgba(r as u8, g as u8, b as u8, a as u8)
}

#[no_mangle]
pub extern "C" fn rt_gfx_clear(r: i64, g: i64, b: i64) {
    macroquad::window::clear_background(color(r, g, b, 255));
}

#[no_mangle]
pub extern "C" fn rt_gfx_rect(x: i64, y: i64, w: i64, h: i64, r: i64, g: i64, b: i64, a: i64) {
    macroquad::shapes::draw_rectangle(
        x as f32,
        y as f32,
        w as f32,
        h as f32,
        color(r, g, b, a),
    );
}

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn rt_gfx_rect_lines(
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    thickness: i64,
    r: i64,
    g: i64,
    b: i64,
    a: i64,
) {
    macroquad::shapes::draw_rectangle_lines(
        x as f32,
        y as f32,
        w as f32,
        h as f32,
        thickness as f32,
        color(r, g, b, a),
    );
}

#[no_mangle]
pub extern "C" fn rt_gfx_circle(x: i64, y: i64, radius: i64, r: i64, g: i64, b: i64, a: i64) {
    macroquad::shapes::draw_circle(x as f32, y as f32, radius as f32, color(r, g, b, a));
}

#[no_mangle]
pub extern "C" fn rt_gfx_text(text: i64, x: i64, y: i64, size: i64, r: i64, g: i64, b: i64) {
    let text = unsafe { std::str::from_utf8_unchecked(str_bytes(text)) };
    macroquad::text::draw_text(
        text,
        x as f32,
        y as f32,
        size as f32,
        color(r, g, b, 255),
    );
}

#[no_mangle]
pub extern "C" fn rt_gfx_text_width(text: i64, size: i64) -> i64 {
    let text = unsafe { std::str::from_utf8_unchecked(str_bytes(text)) };
    let dims = macroquad::text::measure_text(text, None, (size as f32) as u16, 1.0);
    dims.width as i64
}

#[no_mangle]
pub extern "C" fn rt_gfx_mouse_x() -> i64 {
    macroquad::input::mouse_position().0 as i64
}

#[no_mangle]
pub extern "C" fn rt_gfx_mouse_y() -> i64 {
    macroquad::input::mouse_position().1 as i64
}

#[no_mangle]
pub extern "C" fn rt_gfx_mouse_pressed() -> i64 {
    macroquad::input::is_mouse_button_pressed(macroquad::input::MouseButton::Left) as i64
}

// ---- shaders ---------------------------------------------------------------------
//
// Fragment shaders are written in Inga source as GLSL strings; the runtime
// pairs them with a standard vertex shader and exposes two uniforms set
// automatically on use: `iTime` (seconds) and `iRes` (drawable size).

pub const GFX_VERTEX_SHADER: &str = r#"#version 100
attribute vec3 position;
attribute vec2 texcoord;
attribute vec4 color0;
varying lowp vec4 color;
varying vec2 uv;
uniform mat4 Model;
uniform mat4 Projection;
void main() {
    gl_Position = Projection * Model * vec4(position, 1);
    color = color0 / 255.0;
    uv = texcoord;
}"#;

use std::cell::RefCell;

thread_local! {
    static MATERIALS: RefCell<Vec<macroquad::material::Material>> = const { RefCell::new(Vec::new()) };
}

/// Compile a fragment shader; returns a handle, or -1 on compile error.
#[no_mangle]
pub extern "C" fn rt_gfx_shader_new(fragment: i64) -> i64 {
    use macroquad::miniquad::{UniformDesc, UniformType};
    let fragment = unsafe { String::from_utf8_lossy(str_bytes(fragment)).into_owned() };
    let result = macroquad::material::load_material(
        macroquad::miniquad::ShaderSource::Glsl {
            vertex: GFX_VERTEX_SHADER,
            fragment: &fragment,
        },
        macroquad::material::MaterialParams {
            uniforms: vec![
                UniformDesc::new("iTime", UniformType::Float1),
                UniformDesc::new("iRes", UniformType::Float2),
            ],
            ..Default::default()
        },
    );
    match result {
        Ok(material) => MATERIALS.with(|m| {
            m.borrow_mut().push(material);
            m.borrow().len() as i64 - 1
        }),
        Err(e) => {
            eprintln!("Gfx.shaderNew: shader failed to compile: {e}");
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn rt_gfx_shader_use(handle: i64) {
    MATERIALS.with(|m| {
        if let Some(material) = m.borrow().get(handle as usize) {
            let time = EPOCH.with(|e| e.elapsed().as_secs_f32());
            material.set_uniform("iTime", time);
            material.set_uniform(
                "iRes",
                macroquad::math::Vec2::new(
                    macroquad::window::screen_width(),
                    macroquad::window::screen_height(),
                ),
            );
            macroquad::material::gl_use_material(material);
        }
    });
}

#[no_mangle]
pub extern "C" fn rt_gfx_shader_off() {
    macroquad::material::gl_use_default_material();
}
