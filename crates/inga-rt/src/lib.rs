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
//! - Memory is Perceus-style ARC by default (8-byte refcount header per
//!   object, non-atomic — compiled Inga is single-threaded; the compiler
//!   emits type-directed drop glue), with optional `provide Arena(n)`
//!   regions whose allocations are bump-allocated and freed wholesale at
//!   scope end.

use std::io::Write;
use std::time::Instant;

thread_local! {
    static EPOCH: Instant = Instant::now();
}

// ---- allocator ---------------------------------------------------------------
//
// Every allocation carries an 8-byte header word immediately before the
// payload pointer:
//
//   meta >= 1  — RC-heap object; meta is the (non-atomic) refcount
//   meta == -1 — static constant (string literals); dup/release are no-ops
//   meta == -2 — arena object; freed wholesale when its region is popped
//
// The compiler emits type-directed drop glue: `rt_release` decs; when the
// count hits zero the glue releases heap-typed children and calls
// `rt_free`. `provide Arena(n)` pushes a region; allocations in its dynamic
// extent are bump-allocated (overflow chains chunks) and freed together.

const META_STATIC: i64 = -1;
const META_ARENA: i64 = -2;

struct Region {
    /// Chunks owned by this region: (base ptr, capacity).
    chunks: Vec<(*mut u8, usize)>,
    cursor: *mut u8,
    end: *mut u8,
}

static mut REGIONS: Vec<Region> = Vec::new();

#[allow(static_mut_refs)]
fn regions() -> &'static mut Vec<Region> {
    // Single-threaded by construction (compiled Inga has no threads).
    unsafe { &mut REGIONS }
}

unsafe fn raw_chunk(cap: usize) -> *mut u8 {
    std::alloc::alloc(std::alloc::Layout::from_size_align(cap, 8).unwrap())
}

/// Push an arena region of `bytes` capacity (overflow chains more chunks).
#[no_mangle]
pub extern "C" fn rt_arena_push(bytes: i64) {
    let cap = ((bytes.max(4096) as usize) + 7) & !7;
    unsafe {
        let base = raw_chunk(cap);
        regions().push(Region { chunks: vec![(base, cap)], cursor: base, end: base.add(cap) });
    }
}

/// Pop the innermost region, freeing all its allocations at once.
#[no_mangle]
pub extern "C" fn rt_arena_pop() {
    if let Some(region) = regions().pop() {
        unsafe {
            for (base, cap) in region.chunks {
                std::alloc::dealloc(base, std::alloc::Layout::from_size_align(cap, 8).unwrap());
            }
        }
    }
}

#[cold]
unsafe fn region_grow(region: &mut Region, need: usize) {
    let cap = need.max(region.chunks.last().map(|(_, c)| *c).unwrap_or(4096));
    let base = raw_chunk(cap);
    region.chunks.push((base, cap));
    region.cursor = base;
    region.end = base.add(cap);
}

extern "C" {
    fn malloc(size: usize) -> *mut u8;
    fn free(p: *mut u8);
}

// The RC heap: segregated free lists for small objects (1..=MAX_CLASS i64
// slots, including the header), refilled from bump chunks; larger objects
// go to libc malloc. The header packs `class << CLASS_SHIFT | refcount`, so
// `rt_free` knows which list a dead object returns to — freed memory is
// reused at bump-allocator speed (the practical payoff of Perceus reuse).

const CLASS_SHIFT: u32 = 48;
const RC_MASK: i64 = (1 << CLASS_SHIFT) - 1;
const MAX_CLASS: usize = 16; // up to 16 slots = 120 payload bytes
const HUGE: usize = 0;
const HEAP_CHUNK: usize = 1 << 20;

static mut FREE_LISTS: [*mut u8; MAX_CLASS + 1] = [std::ptr::null_mut(); MAX_CLASS + 1];
static mut HEAP_PTR: *mut u8 = std::ptr::null_mut();
static mut HEAP_END: *mut u8 = std::ptr::null_mut();

#[cold]
unsafe fn heap_refill() {
    // Chunks are permanent; their blocks recycle through the free lists.
    HEAP_PTR = malloc(HEAP_CHUNK);
    HEAP_END = HEAP_PTR.add(HEAP_CHUNK);
}

/// Allocate from the RC heap, bypassing any active arena (error boxes must
/// survive region pops). Refcount starts at 1.
#[no_mangle]
pub extern "C" fn rt_alloc_global(size: i64) -> *mut u8 {
    let slots = 1 + (((size.max(0) as usize) + 7) >> 3); // header + payload
    unsafe {
        if slots <= MAX_CLASS {
            let head = FREE_LISTS[slots];
            let p = if !head.is_null() {
                FREE_LISTS[slots] = *(head as *mut *mut u8);
                head
            } else {
                let bytes = slots * 8;
                if HEAP_PTR.is_null() || HEAP_PTR.add(bytes) > HEAP_END {
                    heap_refill();
                }
                let p = HEAP_PTR;
                HEAP_PTR = HEAP_PTR.add(bytes);
                p
            };
            *(p as *mut i64) = ((slots as i64) << CLASS_SHIFT) | 1;
            p.add(8)
        } else {
            let p = malloc(slots * 8);
            *(p as *mut i64) = ((HUGE as i64) << CLASS_SHIFT) | 1;
            p.add(8)
        }
    }
}

/// Allocate `size` bytes, 8-aligned, uninitialized, preceded by a header.
/// Inside an arena scope the bytes come from the innermost region;
/// otherwise from the RC heap.
#[no_mangle]
pub extern "C" fn rt_alloc(size: i64) -> *mut u8 {
    unsafe {
        if let Some(region) = regions().last_mut() {
            let size = ((size.max(0) as usize) + 7) & !7;
            let need = 8 + size;
            if region.cursor.add(need) > region.end {
                region_grow(region, need);
            }
            let p = region.cursor;
            region.cursor = region.cursor.add(need);
            *(p as *mut i64) = META_ARENA;
            return p.add(8);
        }
        rt_alloc_global(size)
    }
}

/// Bump a refcount (no-op for static and arena objects). Null-safe.
#[no_mangle]
pub extern "C" fn rt_dup(v: i64) -> i64 {
    if v != 0 {
        unsafe {
            let meta = (v as *mut i64).sub(1);
            if *meta >= 1 {
                *meta += 1; // rc lives in the low bits; the class is untouched
            }
        }
    }
    v
}

/// Drop one reference; returns 1 when the object just hit zero and the
/// caller (compiler-emitted drop glue) must release children and free it.
#[no_mangle]
pub extern "C" fn rt_release(v: i64) -> i64 {
    if v == 0 {
        return 0;
    }
    unsafe {
        let meta = (v as *mut i64).sub(1);
        if *meta < 1 {
            debug_assert!(*meta == META_STATIC || *meta == META_ARENA);
            return 0;
        }
        *meta -= 1;
        (*meta & RC_MASK == 0) as i64
    }
}

/// Free an object whose refcount already reached zero (drop glue only):
/// small classes recycle through their free list, huge ones go back to the
/// system allocator.
#[no_mangle]
pub extern "C" fn rt_free(v: i64) {
    unsafe {
        let base = (v as *mut u8).sub(8);
        let class = (*(base as *mut i64) >> CLASS_SHIFT) as usize;
        if class == HUGE {
            free(base);
        } else {
            *(base as *mut *mut u8) = FREE_LISTS[class];
            FREE_LISTS[class] = base;
        }
    }
}

// ---- strings / lists ------------------------------------------------------------

fn make_str(bytes: &[u8]) -> i64 {
    let p = rt_alloc(8 + bytes.len() as i64);
    unsafe {
        *(p as *mut i64) = bytes.len() as i64;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(8), bytes.len());
    }
    p as i64
}

fn make_list(items: &[i64]) -> i64 {
    let p = rt_alloc(8 * (1 + items.len()) as i64) as *mut i64;
    unsafe {
        *p = items.len() as i64;
        for (i, v) in items.iter().enumerate() {
            *p.add(1 + i) = *v;
        }
    }
    p as i64
}

#[no_mangle]
pub extern "C" fn rt_str_split(s: i64, sep: i64) -> i64 {
    let (s, sep) = unsafe {
        (
            std::str::from_utf8_unchecked(str_bytes(s)),
            std::str::from_utf8_unchecked(str_bytes(sep)),
        )
    };
    let parts: Vec<i64> = if sep.is_empty() {
        s.chars().map(|c| make_str(c.to_string().as_bytes())).collect()
    } else {
        s.split(sep).map(|p| make_str(p.as_bytes())).collect()
    };
    make_list(&parts)
}

#[no_mangle]
pub extern "C" fn rt_str_slice(s: i64, a: i64, b: i64) -> i64 {
    let s = unsafe { std::str::from_utf8_unchecked(str_bytes(s)) };
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let lo = a.clamp(0, n) as usize;
    let hi = b.clamp(0, n) as usize;
    let out: String = if lo < hi { chars[lo..hi].iter().collect() } else { String::new() };
    make_str(out.as_bytes())
}

#[no_mangle]
pub extern "C" fn rt_str_index_of(s: i64, needle: i64) -> i64 {
    let (s, needle) = unsafe {
        (
            std::str::from_utf8_unchecked(str_bytes(s)),
            std::str::from_utf8_unchecked(str_bytes(needle)),
        )
    };
    match s.find(needle) {
        Some(byte) => s[..byte].chars().count() as i64,
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn rt_str_trim(s: i64) -> i64 {
    let s = unsafe { std::str::from_utf8_unchecked(str_bytes(s)) };
    make_str(s.trim().as_bytes())
}

#[no_mangle]
pub extern "C" fn rt_parse_int(s: i64) -> i64 {
    let s = unsafe { std::str::from_utf8_unchecked(str_bytes(s)) };
    match s.trim().parse::<i64>() {
        Ok(n) => {
            let p = rt_alloc(8) as *mut i64;
            unsafe { *p = n };
            p as i64
        }
        Err(_) => 0,
    }
}

unsafe fn list_items<'a>(l: i64) -> &'a [i64] {
    let p = l as *const i64;
    std::slice::from_raw_parts(p.add(1), *p as usize)
}

#[no_mangle]
pub extern "C" fn rt_list_concat(xs: i64, ys: i64) -> i64 {
    unsafe {
        let mut out: Vec<i64> = list_items(xs).to_vec();
        out.extend_from_slice(list_items(ys));
        make_list(&out)
    }
}

#[no_mangle]
pub extern "C" fn rt_list_reverse(xs: i64) -> i64 {
    unsafe {
        let mut out: Vec<i64> = list_items(xs).to_vec();
        out.reverse();
        make_list(&out)
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

/// Drop glue for MutMap: frees the map's own storage. Values the map still
/// holds keep the references they were given by `set` (a known leak).
#[no_mangle]
pub extern "C" fn rt_map_free(m: i64) {
    if m != 0 {
        unsafe { drop(Box::from_raw(m as *mut RtMap)) };
    }
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

// ---- type descriptors ---------------------------------------------------------
//
// The compiler serializes every value type into a compact descriptor; one
// runtime interpreter implements show/display, structural equality, JSON
// encode/decode, and deep copy over them. Grammar (prefix, self-delimiting):
//   i f b s u d h F M ?         primitives / opaque
//   O<desc>  L<desc>            option, list
//   T<n><desc...>               tuple of n (single digit)
//   #<idx>;                     named type, by registry index
// Registry lines (set once at startup, before any task threads):
//   S<name>{f:<desc>;...}       struct
//   E<name>{Variant(f:<desc>;...);...}  enum (variant order = ids)

static mut TYPES: Vec<String> = Vec::new();

#[allow(static_mut_refs)]
fn types() -> &'static Vec<String> {
    unsafe { &TYPES }
}

#[no_mangle]
pub extern "C" fn rt_types_init(table: i64) {
    let text = unsafe { std::str::from_utf8_unchecked(str_bytes(table)) };
    unsafe {
        #[allow(static_mut_refs)]
        if TYPES.is_empty() {
            TYPES = text.lines().map(|l| l.to_string()).collect();
        }
    }
}

struct Desc<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Desc<'a> {
    fn new(s: &'a str) -> Desc<'a> {
        Desc { bytes: s.as_bytes(), pos: 0 }
    }

    fn peek(&self) -> u8 {
        self.bytes.get(self.pos).copied().unwrap_or(b'?')
    }

    fn bump(&mut self) -> u8 {
        let b = self.peek();
        self.pos += 1;
        b
    }

    fn until(&mut self, stop: u8) -> &'a str {
        let start = self.pos;
        while self.pos < self.bytes.len() && self.bytes[self.pos] != stop {
            self.pos += 1;
        }
        let s = unsafe { std::str::from_utf8_unchecked(&self.bytes[start..self.pos]) };
        self.pos += 1; // consume the stop byte
        s
    }

    /// Skip one complete descriptor.
    fn skip(&mut self) {
        match self.bump() {
            b'O' | b'L' => self.skip(),
            b'T' => {
                let n = (self.bump() - b'0') as usize;
                for _ in 0..n {
                    self.skip();
                }
            }
            b'#' => {
                self.until(b';');
            }
            _ => {}
        }
    }
}

fn registry_line(idx: usize) -> &'static str {
    types().get(idx).map(String::as_str).unwrap_or("S?{}")
}

fn show_desc(v: i64, d: &mut Desc, quote_str: bool) -> String {
    match d.bump() {
        b'i' => v.to_string(),
        b'd' => {
            let ms = v;
            if ms % 3_600_000 == 0 && ms != 0 {
                format!("{}.hours", ms / 3_600_000)
            } else if ms % 60_000 == 0 && ms != 0 {
                format!("{}.minutes", ms / 60_000)
            } else if ms % 1000 == 0 && ms != 0 {
                format!("{}.seconds", ms / 1000)
            } else {
                format!("{ms}.millis")
            }
        }
        b'f' => {
            let x = f64::from_bits(v as u64);
            if x.fract() == 0.0 && x.is_finite() {
                format!("{x:.1}")
            } else {
                x.to_string()
            }
        }
        b'b' => (v != 0).to_string(),
        b's' => {
            let s = unsafe { std::str::from_utf8_unchecked(str_bytes(v)) };
            if quote_str {
                format!("{s:?}")
            } else {
                s.to_string()
            }
        }
        b'u' => "()".to_string(),
        b'h' => {
            let p = v as *const i64;
            let (kind, base, max) = unsafe { (*p, *p.add(1), *p.add(2)) };
            let kind = if kind == 0 { "exponential" } else { "fixed" };
            let base = show_desc(base, &mut Desc::new("d"), false);
            if max >= 0 {
                format!("schedule.{kind}({base}) |> schedule.upTo({max})")
            } else {
                format!("schedule.{kind}({base})")
            }
        }
        b'F' => "<lambda>".to_string(),
        b'M' => "MutMap".to_string(),
        b'O' => {
            if v == 0 {
                d.skip();
                "None".to_string()
            } else {
                let inner = unsafe { *(v as *const i64) };
                format!("Some({})", show_desc(inner, d, true))
            }
        }
        b'L' => {
            let start = d.pos;
            let items = unsafe { list_items(v) };
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                d.pos = start;
                parts.push(show_desc(*item, d, true));
            }
            if items.is_empty() {
                d.pos = start;
                d.skip();
            }
            format!("[{}]", parts.join(", "))
        }
        b'T' => {
            let n = (d.bump() - b'0') as usize;
            let p = v as *const i64;
            let mut parts = Vec::with_capacity(n);
            for i in 0..n {
                let elem = unsafe { *p.add(i) };
                parts.push(show_desc(elem, d, true));
            }
            format!("({})", parts.join(", "))
        }
        b'#' => {
            let idx: usize = d.until(b';').parse().unwrap_or(0);
            let line = registry_line(idx);
            let mut rd = Desc::new(line);
            match rd.bump() {
                b'S' => {
                    let name = rd.until(b'{');
                    let p = v as *const i64;
                    let mut parts = Vec::new();
                    let mut i = 0;
                    while rd.peek() != b'}' && rd.peek() != b'?' {
                        let fname = rd.until(b':');
                        let fv = unsafe { *p.add(i) };
                        parts.push(format!("{fname}: {}", show_desc(fv, &mut rd, true)));
                        rd.bump(); // ';'
                        i += 1;
                    }
                    format!("{name}({})", parts.join(", "))
                }
                b'E' => {
                    let _name = rd.until(b'{');
                    // Collect variant sections.
                    let body = unsafe {
                        std::str::from_utf8_unchecked(&rd.bytes[rd.pos..rd.bytes.len() - 1])
                    };
                    let variants: Vec<&str> = split_variants(body);
                    let simple = variants.iter().all(|v| !v.contains('('));
                    let (vid, base) = if simple {
                        (v as usize, std::ptr::null::<i64>())
                    } else {
                        let p = v as *const i64;
                        (unsafe { *p } as usize, unsafe { p.add(1) })
                    };
                    let Some(var) = variants.get(vid) else { return "?".to_string() };
                    match var.find('(') {
                        None => var.to_string(),
                        Some(paren) => {
                            let vname = &var[..paren];
                            let fields = &var[paren + 1..var.len() - 1];
                            let mut fd = Desc::new(fields);
                            let mut parts = Vec::new();
                            let mut i = 0;
                            while fd.pos < fd.bytes.len() {
                                let fname = fd.until(b':');
                                let fv = unsafe { *base.add(i) };
                                parts.push(format!("{fname}: {}", show_desc(fv, &mut fd, true)));
                                fd.bump(); // ';'
                                i += 1;
                            }
                            if parts.is_empty() {
                                vname.to_string()
                            } else {
                                format!("{vname}({})", parts.join(", "))
                            }
                        }
                    }
                }
                _ => "?".to_string(),
            }
        }
        _ => "?".to_string(),
    }
}

/// Split an enum body "A;B(f:i;);C" into variant sections (parens nest).
fn split_variants(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, b) in body.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b';' if depth == 0 => {
                out.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < body.len() {
        out.push(&body[start..]);
    }
    out
}

#[no_mangle]
pub extern "C" fn rt_show_desc(v: i64, desc: i64) -> i64 {
    let d = unsafe { std::str::from_utf8_unchecked(str_bytes(desc)) };
    make_str(show_desc(v, &mut Desc::new(d), true).as_bytes())
}

#[no_mangle]
pub extern "C" fn rt_display_desc(v: i64, desc: i64) -> i64 {
    let d = unsafe { std::str::from_utf8_unchecked(str_bytes(desc)) };
    make_str(show_desc(v, &mut Desc::new(d), false).as_bytes())
}

fn eq_desc(a: i64, b: i64, d: &mut Desc) -> bool {
    match d.bump() {
        b'i' | b'b' | b'u' | b'd' | b'F' | b'M' | b'h' => a == b,
        b'f' => f64::from_bits(a as u64) == f64::from_bits(b as u64),
        b's' => unsafe { str_bytes(a) == str_bytes(b) },
        b'O' => {
            if a == 0 || b == 0 {
                d.skip();
                a == b
            } else {
                let (x, y) = unsafe { (*(a as *const i64), *(b as *const i64)) };
                eq_desc(x, y, d)
            }
        }
        b'L' => {
            let (xs, ys) = unsafe { (list_items(a), list_items(b)) };
            let start = d.pos;
            if xs.len() != ys.len() {
                d.skip();
                return false;
            }
            for (x, y) in xs.iter().zip(ys.iter()) {
                d.pos = start;
                if !eq_desc(*x, *y, d) {
                    return false;
                }
            }
            if xs.is_empty() {
                d.pos = start;
                d.skip();
            }
            true
        }
        b'T' => {
            let n = (d.bump() - b'0') as usize;
            let (pa, pb) = (a as *const i64, b as *const i64);
            for i in 0..n {
                let (x, y) = unsafe { (*pa.add(i), *pb.add(i)) };
                if !eq_desc(x, y, d) {
                    // consume the remaining element descriptors
                    for _ in i + 1..n {
                        d.skip();
                    }
                    return false;
                }
            }
            true
        }
        b'#' => {
            let idx: usize = d.until(b';').parse().unwrap_or(0);
            let line = registry_line(idx);
            let mut rd = Desc::new(line);
            match rd.bump() {
                b'S' => {
                    let _ = rd.until(b'{');
                    let (pa, pb) = (a as *const i64, b as *const i64);
                    let mut i = 0;
                    while rd.peek() != b'}' && rd.peek() != b'?' {
                        let _ = rd.until(b':');
                        let (x, y) = unsafe { (*pa.add(i), *pb.add(i)) };
                        if !eq_desc(x, y, &mut rd) {
                            return false;
                        }
                        rd.bump();
                        i += 1;
                    }
                    true
                }
                b'E' => {
                    let _ = rd.until(b'{');
                    let body = unsafe {
                        std::str::from_utf8_unchecked(&rd.bytes[rd.pos..rd.bytes.len() - 1])
                    };
                    let variants = split_variants(body);
                    let simple = variants.iter().all(|v| !v.contains('('));
                    if simple {
                        return a == b;
                    }
                    let (pa, pb) = (a as *const i64, b as *const i64);
                    let (va, vb) = unsafe { (*pa, *pb) };
                    if va != vb {
                        return false;
                    }
                    let Some(var) = variants.get(va as usize) else { return false };
                    let Some(paren) = var.find('(') else { return true };
                    let fields = &var[paren + 1..var.len() - 1];
                    let mut fd = Desc::new(fields);
                    let mut i = 1;
                    while fd.pos < fd.bytes.len() {
                        let _ = fd.until(b':');
                        let (x, y) = unsafe { (*pa.add(i), *pb.add(i)) };
                        if !eq_desc(x, y, &mut fd) {
                            return false;
                        }
                        fd.bump();
                        i += 1;
                    }
                    true
                }
                _ => a == b,
            }
        }
        _ => a == b,
    }
}

#[no_mangle]
pub extern "C" fn rt_eq_desc(a: i64, b: i64, desc: i64) -> i64 {
    let d = unsafe { std::str::from_utf8_unchecked(str_bytes(desc)) };
    eq_desc(a, b, &mut Desc::new(d)) as i64
}

fn copy_desc(v: i64, d: &mut Desc) -> i64 {
    match d.bump() {
        b'i' | b'b' | b'u' | b'd' | b'f' | b'F' | b'M' => v, // scalars; F/M shared by reference
        b's' => {
            let bytes = unsafe { str_bytes(v) }.to_vec();
            make_str(&bytes)
        }
        b'h' => {
            let p = v as *const i64;
            let q = rt_alloc(24) as *mut i64;
            unsafe {
                *q = *p;
                *q.add(1) = *p.add(1);
                *q.add(2) = *p.add(2);
            }
            q as i64
        }
        b'O' => {
            if v == 0 {
                d.skip();
                0
            } else {
                let inner = unsafe { *(v as *const i64) };
                let copied = copy_desc(inner, d);
                let q = rt_alloc(8) as *mut i64;
                unsafe { *q = copied };
                q as i64
            }
        }
        b'L' => {
            let items: Vec<i64> = unsafe { list_items(v) }.to_vec();
            let start = d.pos;
            let mut copied = Vec::with_capacity(items.len());
            for item in items {
                d.pos = start;
                copied.push(copy_desc(item, d));
            }
            if copied.is_empty() {
                d.pos = start;
                d.skip();
            }
            make_list(&copied)
        }
        b'T' => {
            let n = (d.bump() - b'0') as usize;
            let p = v as *const i64;
            let q = rt_alloc(8 * n as i64) as *mut i64;
            for i in 0..n {
                let elem = unsafe { *p.add(i) };
                let c = copy_desc(elem, d);
                unsafe { *q.add(i) = c };
            }
            q as i64
        }
        b'#' => {
            let idx: usize = d.until(b';').parse().unwrap_or(0);
            let line = registry_line(idx).to_string();
            let mut rd = Desc::new(&line);
            match rd.bump() {
                b'S' => {
                    let _ = rd.until(b'{');
                    let p = v as *const i64;
                    let mut copied = Vec::new();
                    let mut i = 0;
                    while rd.peek() != b'}' && rd.peek() != b'?' {
                        let _ = rd.until(b':');
                        let fv = unsafe { *p.add(i) };
                        copied.push(copy_desc(fv, &mut rd));
                        rd.bump();
                        i += 1;
                    }
                    let q = rt_alloc(8 * copied.len().max(1) as i64) as *mut i64;
                    for (i, c) in copied.iter().enumerate() {
                        unsafe { *q.add(i) = *c };
                    }
                    q as i64
                }
                b'E' => {
                    let _ = rd.until(b'{');
                    let body_owned;
                    let body = {
                        body_owned = unsafe {
                            std::str::from_utf8_unchecked(&rd.bytes[rd.pos..rd.bytes.len() - 1])
                        }
                        .to_string();
                        body_owned.as_str()
                    };
                    let variants = split_variants(body);
                    let simple = variants.iter().all(|v| !v.contains('('));
                    if simple {
                        return v;
                    }
                    let p = v as *const i64;
                    let vid = unsafe { *p } as usize;
                    let mut copied = vec![vid as i64];
                    if let Some(var) = variants.get(vid) {
                        if let Some(paren) = var.find('(') {
                            let fields = &var[paren + 1..var.len() - 1];
                            let mut fd = Desc::new(fields);
                            let mut i = 1;
                            while fd.pos < fd.bytes.len() {
                                let _ = fd.until(b':');
                                let fv = unsafe { *p.add(i) };
                                copied.push(copy_desc(fv, &mut fd));
                                fd.bump();
                                i += 1;
                            }
                        }
                    }
                    let q = rt_alloc(8 * copied.len() as i64) as *mut i64;
                    for (i, c) in copied.iter().enumerate() {
                        unsafe { *q.add(i) = *c };
                    }
                    q as i64
                }
                _ => v,
            }
        }
        _ => v,
    }
}

/// Deep-copy a value (into the current thread/arena allocator) — used by
/// task results and arena copy-out.
#[no_mangle]
pub extern "C" fn rt_copy_desc(v: i64, desc: i64) -> i64 {
    let d = unsafe { std::str::from_utf8_unchecked(str_bytes(desc)) }.to_string();
    copy_desc(v, &mut Desc::new(&d))
}
