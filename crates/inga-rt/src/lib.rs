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

fn regions() -> &'static mut Vec<Region> {
    &mut rt().regions
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

struct Heap {
    free_lists: [*mut u8; MAX_CLASS + 1],
    ptr: *mut u8,
    end: *mut u8,
}

/// Per-thread allocator state. Each thread (the main program and every
/// spawned task) owns its own regions and RC heap — allocation stays
/// lock-free. Heap chunks are never returned to the OS, so a task's result
/// outlives its thread and the parent (exclusive owner after `await`)
/// reads and frees it safely.
struct RtState {
    regions: Vec<Region>,
    heap: Heap,
}

thread_local! {
    static RT: std::cell::UnsafeCell<RtState> = const {
        std::cell::UnsafeCell::new(RtState {
            regions: Vec::new(),
            heap: Heap {
                free_lists: [std::ptr::null_mut(); MAX_CLASS + 1],
                ptr: std::ptr::null_mut(),
                end: std::ptr::null_mut(),
            },
        })
    };
}

fn rt() -> &'static mut RtState {
    // One mutable reference at a time by construction: the runtime never
    // re-enters the allocator while a `&mut` is live on this thread.
    RT.with(|r| unsafe { &mut *r.get() })
}

fn heap() -> &'static mut Heap {
    &mut rt().heap
}

#[cold]
unsafe fn heap_refill(heap: &mut Heap) {
    // Chunks are permanent; their blocks recycle through the free lists.
    heap.ptr = malloc(HEAP_CHUNK);
    heap.end = heap.ptr.add(HEAP_CHUNK);
}

/// Allocate from the RC heap, bypassing any active arena (error boxes must
/// survive region pops). Refcount starts at 1.
#[no_mangle]
pub extern "C" fn rt_alloc_global(size: i64) -> *mut u8 {
    alloc_global_in(heap(), size)
}

fn alloc_global_in(heap: &mut Heap, size: i64) -> *mut u8 {
    let slots = 1 + (((size.max(0) as usize) + 7) >> 3); // header + payload
    unsafe {
        if slots <= MAX_CLASS {
            let head = heap.free_lists[slots];
            let p = if !head.is_null() {
                heap.free_lists[slots] = *(head as *mut *mut u8);
                head
            } else {
                let bytes = slots * 8;
                if heap.ptr.is_null() || heap.ptr.add(bytes) > heap.end {
                    heap_refill(heap);
                }
                let p = heap.ptr;
                heap.ptr = heap.ptr.add(bytes);
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
    let rt = rt();
    unsafe {
        if let Some(region) = rt.regions.last_mut() {
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
        alloc_global_in(&mut rt.heap, size)
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
    let heap = heap();
    unsafe {
        let base = (v as *mut u8).sub(8);
        let class = (*(base as *mut i64) >> CLASS_SHIFT) as usize;
        if class == HUGE {
            free(base);
        } else {
            *(base as *mut *mut u8) = heap.free_lists[class];
            heap.free_lists[class] = base;
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

thread_local! {
    // Per-thread so spawned tasks never race on the generator state.
    static RNG_STATE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// xorshift64*, seeded from the clock on first use (per thread).
#[no_mangle]
pub extern "C" fn rt_random(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    RNG_STATE.with(|state| {
        let mut s = state.get();
        if s == 0 {
            s = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x4d595df4d0f33173)
                | 1;
        }
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        state.set(s);
        ((s.wrapping_mul(0x2545F4914F6CDD1D) >> 32) % n as u64) as i64
    })
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

// The game draws in LOGICAL coordinates (the size passed to graphics.run);
// each frame renders into an offscreen target which is scaled to the real
// window with letterboxing — so every Inga game is resizable for free, and
// mouse positions are mapped back into logical space.
thread_local! {
    static LOGICAL: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new((960.0, 540.0)) };
}

/// (scale, x offset, y offset) of the logical canvas inside the window.
fn gfx_viewport() -> (f32, f32, f32) {
    let (lw, lh) = LOGICAL.with(|l| l.get());
    let (sw, sh) = (macroquad::window::screen_width(), macroquad::window::screen_height());
    let scale = (sw / lw).min(sh / lh);
    (scale, (sw - lw * scale) / 2.0, (sh - lh * scale) / 2.0)
}

#[no_mangle]
pub extern "C" fn rt_gfx_run(width: i64, height: i64, title: i64, closure: i64) {
    let title = unsafe { String::from_utf8_lossy(str_bytes(title)).into_owned() };
    let conf = macroquad::window::Conf {
        window_title: title,
        window_width: width as i32,
        window_height: height as i32,
        window_resizable: true,
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
    LOGICAL.with(|l| l.set((width as f32, height as f32)));
    macroquad::Window::from_config(conf, async move {
        let mut frame_no = 0u32;
        loop {
            // The frame draws in logical coordinates; every shim scales to
            // the live window (so text rasterizes at its real pixel size —
            // no offscreen target, no upscaling blur).
            let env = closure as *mut u8;
            let fp: FrameFn = unsafe { std::mem::transmute(*(env as *const i64)) };
            let r = unsafe { fp(env) };
            if r.err != 0 {
                eprintln!("runtime error: unhandled error escaped the frame closure");
                std::process::exit(101);
            }
            // Mask the letterbox margins.
            let (lw, lh) = LOGICAL.with(|l| l.get());
            let (scale, ox, oy) = gfx_viewport();
            let (sw, sh) = (
                macroquad::window::screen_width(),
                macroquad::window::screen_height(),
            );
            let black = macroquad::color::BLACK;
            if ox > 0.0 {
                macroquad::shapes::draw_rectangle(0.0, 0.0, ox, sh, black);
                macroquad::shapes::draw_rectangle(ox + lw * scale, 0.0, sw, sh, black);
            }
            if oy > 0.0 {
                macroquad::shapes::draw_rectangle(0.0, 0.0, sw, oy, black);
                macroquad::shapes::draw_rectangle(0.0, oy + lh * scale, sw, sh, black);
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

/// Logical x/y/length to live screen pixels.
fn sx(x: i64) -> f32 {
    let (scale, ox, _) = gfx_viewport();
    ox + x as f32 * scale
}

fn sy(y: i64) -> f32 {
    let (scale, _, oy) = gfx_viewport();
    oy + y as f32 * scale
}

fn sl(v: i64) -> f32 {
    v as f32 * gfx_viewport().0
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
    macroquad::shapes::draw_rectangle(sx(x), sy(y), sl(w), sl(h), color(r, g, b, a));
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
        sx(x),
        sy(y),
        sl(w),
        sl(h),
        sl(thickness).max(1.0),
        color(r, g, b, a),
    );
}

#[no_mangle]
pub extern "C" fn rt_gfx_circle(x: i64, y: i64, radius: i64, r: i64, g: i64, b: i64, a: i64) {
    macroquad::shapes::draw_circle(sx(x), sy(y), sl(radius), color(r, g, b, a));
}

#[no_mangle]
pub extern "C" fn rt_gfx_text(text: i64, x: i64, y: i64, size: i64, r: i64, g: i64, b: i64) {
    let text = unsafe { std::str::from_utf8_unchecked(str_bytes(text)) };
    // Glyphs are rasterized at the SCALED size, so text stays crisp at any
    // window size instead of being baked at logical size and stretched.
    macroquad::text::draw_text(text, sx(x), sy(y), sl(size), color(r, g, b, 255));
}

#[no_mangle]
pub extern "C" fn rt_gfx_text_width(text: i64, size: i64) -> i64 {
    let text = unsafe { std::str::from_utf8_unchecked(str_bytes(text)) };
    // Measure at the scaled size, report in logical units (what the caller
    // does layout in).
    let scale = gfx_viewport().0;
    let dims = macroquad::text::measure_text(text, None, sl(size) as u16, 1.0);
    (dims.width / scale) as i64
}

#[no_mangle]
pub extern "C" fn rt_gfx_mouse_x() -> i64 {
    let (scale, ox, _) = gfx_viewport();
    ((macroquad::input::mouse_position().0 - ox) / scale) as i64
}

#[no_mangle]
pub extern "C" fn rt_gfx_mouse_y() -> i64 {
    let (scale, _, oy) = gfx_viewport();
    ((macroquad::input::mouse_position().1 - oy) / scale) as i64
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
    static TEXTURES: RefCell<Vec<macroquad::texture::Texture2D>> = const { RefCell::new(Vec::new()) };
}

/// Decode PNG bytes (an Inga string is a length-prefixed byte buffer, so
/// binary bodies pass through untouched) into a texture; returns a handle,
/// or -1 when the data does not decode.
#[no_mangle]
pub extern "C" fn rt_gfx_image_new(data: i64) -> i64 {
    let bytes = unsafe { str_bytes(data) }.to_vec();
    let tex = std::panic::catch_unwind(|| {
        // Format auto-detection covers PNG/JPEG/GIF (whatever the image
        // crate build supports).
        macroquad::texture::Texture2D::from_file_with_format(&bytes, None)
    });
    match tex {
        Ok(t) => {
            // Pixel sprites stay crisp when scaled.
            t.set_filter(macroquad::texture::FilterMode::Nearest);
            TEXTURES.with(|ts| {
                ts.borrow_mut().push(t);
                ts.borrow().len() as i64 - 1
            })
        }
        Err(_) => {
            eprintln!("graphics.imageNew: could not decode image data");
            -1
        }
    }
}

/// Draw a loaded image scaled to (w, h). Unknown handles draw nothing.
#[no_mangle]
pub extern "C" fn rt_gfx_image(handle: i64, x: i64, y: i64, w: i64, h: i64) {
    TEXTURES.with(|ts| {
        if let Some(tex) = ts.borrow().get(handle as usize) {
            macroquad::texture::draw_texture_ex(
                tex,
                sx(x),
                sy(y),
                macroquad::color::WHITE,
                macroquad::texture::DrawTextureParams {
                    dest_size: Some(macroquad::math::Vec2::new(sl(w), sl(h))),
                    ..Default::default()
                },
            );
        }
    });
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

/// "assertEq failed: <a> != <b>", rendered via the type descriptor.
#[no_mangle]
pub extern "C" fn rt_assert_eq_msg(a: i64, b: i64, desc: i64) -> i64 {
    let d = unsafe { std::str::from_utf8_unchecked(str_bytes(desc)) }.to_string();
    let left = show_desc(a, &mut Desc::new(&d), true);
    let right = show_desc(b, &mut Desc::new(&d), true);
    make_str(format!("assertEq failed: {left} != {right}").as_bytes())
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

/// Carry an arena scope's result past the region it lives in: pop the
/// innermost region off the stack (keeping its memory alive), deep-copy the
/// value — the copy now allocates from the enclosing region or the RC
/// heap — then free the region wholesale.
#[no_mangle]
pub extern "C" fn rt_arena_copy_out(v: i64, desc: i64) -> i64 {
    let d = unsafe { std::str::from_utf8_unchecked(str_bytes(desc)) }.to_string();
    let Some(region) = rt().regions.pop() else { return v };
    let copied = copy_desc(v, &mut Desc::new(&d));
    unsafe {
        for (base, cap) in region.chunks {
            std::alloc::dealloc(base, std::alloc::Layout::from_size_align(cap, 8).unwrap());
        }
    }
    copied
}

// ---- tasks (spawn / await) -------------------------------------------------------
//
// `spawn(action)` runs the action on its own OS thread. The checker
// guarantees the action is self-contained (empty error and capability
// rows), so a task only touches its captured values. Captures are frozen
// (made `META_STATIC`) before the thread starts: refcount traffic from two
// threads would race, so both sides simply stop counting — frozen values
// leak, a bounded cost per spawn. Arena-allocated captures are deep-copied
// out first (the parent might pop the region while the task runs).

/// Recursively mark every block reachable from `v` as static. Returns the
/// (possibly copied) value: arena blocks are copied to the RC heap first.
fn freeze_desc(v: i64, d: &mut Desc) -> i64 {
    fn meta_of(v: i64) -> i64 {
        unsafe { *(v as *const i64).sub(1) }
    }
    fn set_static(v: i64) {
        unsafe {
            let m = (v as *mut i64).sub(1);
            // Already-static blocks may live in read-only data (literals).
            if *m >= 1 {
                *m = META_STATIC;
            }
        }
    }
    match d.peek() {
        b'i' | b'b' | b'u' | b'd' | b'f' | b'F' | b'M' => {
            d.skip();
            v
        }
        _ => {
            // Arena blocks: deep-copy the whole tree to the heap, then
            // freeze the copy (children of an arena block are arena/static).
            if v != 0 && meta_of(v) == META_ARENA {
                let start = d.pos;
                let mut cd = Desc { bytes: d.bytes, pos: start };
                let copied = copy_desc(v, &mut cd);
                d.pos = start;
                return freeze_desc(copied, d);
            }
            match d.bump() {
                b's' | b'h' => {
                    if v != 0 {
                        set_static(v);
                    }
                    v
                }
                b'O' => {
                    if v == 0 {
                        d.skip();
                        return 0;
                    }
                    let inner = unsafe { *(v as *const i64) };
                    let frozen = freeze_desc(inner, d);
                    unsafe { *(v as *mut i64) = frozen };
                    set_static(v);
                    v
                }
                b'L' => {
                    let start = d.pos;
                    let p = v as *mut i64;
                    let n = unsafe { *p } as usize;
                    for i in 0..n {
                        d.pos = start;
                        let item = unsafe { *p.add(1 + i) };
                        let frozen = freeze_desc(item, d);
                        unsafe { *p.add(1 + i) = frozen };
                    }
                    d.pos = start;
                    d.skip();
                    set_static(v);
                    v
                }
                b'T' => {
                    let n = (d.bump() - b'0') as usize;
                    let p = v as *mut i64;
                    for i in 0..n {
                        let elem = unsafe { *p.add(i) };
                        let frozen = freeze_desc(elem, d);
                        unsafe { *p.add(i) = frozen };
                    }
                    set_static(v);
                    v
                }
                b'#' => {
                    let idx: usize = d.until(b';').parse().unwrap_or(0);
                    let line = registry_line(idx).to_string();
                    let mut rd = Desc::new(&line);
                    match rd.bump() {
                        b'S' => {
                            let _ = rd.until(b'{');
                            let p = v as *mut i64;
                            let mut i = 0;
                            while rd.peek() != b'}' && rd.peek() != b'?' {
                                let _ = rd.until(b':');
                                let fv = unsafe { *p.add(i) };
                                let frozen = freeze_desc(fv, &mut rd);
                                unsafe { *p.add(i) = frozen };
                                rd.bump();
                                i += 1;
                            }
                            set_static(v);
                            v
                        }
                        b'E' => {
                            let body = unsafe {
                                std::str::from_utf8_unchecked(&rd.bytes[rd.pos..rd.bytes.len() - 1])
                            }
                            .to_string();
                            let variants = split_variants(&body);
                            if variants.iter().all(|var| !var.contains('(')) {
                                return v; // simple enums are plain ints
                            }
                            let p = v as *mut i64;
                            let vid = unsafe { *p } as usize;
                            if let Some(var) = variants.get(vid) {
                                if let Some(paren) = var.find('(') {
                                    let fields = &var[paren + 1..var.len() - 1];
                                    let mut fd = Desc::new(fields);
                                    let mut i = 1;
                                    while fd.pos < fd.bytes.len() {
                                        let _ = fd.until(b':');
                                        let fv = unsafe { *p.add(i) };
                                        let frozen = freeze_desc(fv, &mut fd);
                                        unsafe { *p.add(i) = frozen };
                                        fd.bump();
                                        i += 1;
                                    }
                                }
                            }
                            set_static(v);
                            v
                        }
                        _ => v,
                    }
                }
                _ => v,
            }
        }
    }
}

/// Freeze the value stored in a closure-environment slot (writes back the
/// copied pointer when the value had to leave an arena).
#[no_mangle]
pub extern "C" fn rt_freeze_slot(slot: i64, desc: i64) {
    let d = unsafe { std::str::from_utf8_unchecked(str_bytes(desc)) }.to_string();
    unsafe {
        let p = slot as *mut i64;
        *p = freeze_desc(*p, &mut Desc::new(&d));
    }
}

#[repr(C)]
pub struct RtPair {
    pub v: i64,
    pub e: i64,
}

// ---- fibers ----------------------------------------------------------------------
//
// Phase 1 of std/fiber: one OS thread per fiber, full API. The fiber VALUE
// is an ordinary RC heap box holding a raw Arc pointer to the record, so
// dup/release and the per-function pools work unchanged — and the drop glue
// doubles as supervision: a fiber abandoned by its forking function (pool
// drain at return) is interrupted. `Runtime(n)`'s worker count is honored
// in phase 2 (M:N); the §2 promise — identical behavior under any n except
// speed — holds trivially here.

enum FiberState {
    Pending,
    Done(i64, i64),
}

struct FiberRecord {
    state: std::sync::Mutex<FiberState>,
    cv: std::sync::Condvar,
    interrupted: std::sync::atomic::AtomicBool,
}

/// Global completion pulse for `race`: bump + notify on every completion.
static COMPLETIONS: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
static COMPLETIONS_CV: std::sync::Condvar = std::sync::Condvar::new();

fn fiber_record(boxed: i64) -> &'static FiberRecord {
    unsafe {
        let arc_raw = *(boxed as *const i64) as *const FiberRecord;
        &*arc_raw
    }
}

/// Read an environment variable: `Some(value)` boxed, or 0 for None.
#[no_mangle]
pub extern "C" fn rt_env(name: i64) -> i64 {
    let n = unsafe { std::str::from_utf8_unchecked(str_bytes(name)) };
    match std::env::var(n) {
        Ok(v) => {
            let s = make_str(v.as_bytes());
            let boxed = rt_alloc(8) as *mut i64;
            unsafe { *boxed = s };
            boxed as i64
        }
        Err(_) => 0,
    }
}

/// Freeze a single block header (its refcount stops; it is never freed) —
/// used for closure records crossing fiber boundaries.
#[no_mangle]
pub extern "C" fn rt_freeze_header(v: i64) {
    if v != 0 {
        unsafe {
            let m = (v as *mut i64).sub(1);
            if *m >= 1 {
                *m = META_STATIC;
            }
        }
    }
}

/// Build an error box `{tag, payload}` (RC-heap; error boxes are never
/// freed, so they may cross fibers freely).
#[no_mangle]
pub extern "C" fn rt_make_errbox(tag: i64, payload: i64) -> i64 {
    let p = rt_alloc_global(16) as *mut i64;
    unsafe {
        *p = tag;
        *p.add(1) = payload;
    }
    p as i64
}

/// Fork: run the thunk closure `{ fnptr, captures... }` on its own thread.
/// Returns an RC'd fiber handle box. The closure record is frozen so
/// neither side frees it under the other.
#[no_mangle]
pub extern "C" fn rt_fiber_fork(closure: i64) -> i64 {
    unsafe {
        let meta = (closure as *mut i64).sub(1);
        if *meta >= 1 {
            *meta = META_STATIC;
        }
    }
    let record = std::sync::Arc::new(FiberRecord {
        state: std::sync::Mutex::new(FiberState::Pending),
        cv: std::sync::Condvar::new(),
        interrupted: std::sync::atomic::AtomicBool::new(false),
    });
    let for_thread = record.clone();
    let env = closure as usize;
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(move || {
            let f: extern "C" fn(*const i64) -> RtPair =
                unsafe { std::mem::transmute(*(env as *const i64)) };
            let pair = f(env as *const i64);
            *for_thread.state.lock().unwrap() = FiberState::Done(pair.v, pair.e);
            for_thread.cv.notify_all();
            *COMPLETIONS.lock().unwrap() += 1;
            COMPLETIONS_CV.notify_all();
        })
        .expect("fork fiber thread");
    let boxed = rt_alloc_global(8) as *mut i64;
    unsafe { *boxed = std::sync::Arc::into_raw(record) as i64 };
    boxed as i64
}

/// Park until the fiber completes; returns its `{value, err}` pair. A
/// pending fiber that has been interrupted yields an `Interrupted` error
/// (tag passed in by the compiler); a completed fiber's result wins —
/// interruption after completion is a no-op, and joins are idempotent.
#[no_mangle]
pub extern "C" fn rt_fiber_join(boxed: i64, interrupted_tag: i64) -> RtPair {
    let rec = fiber_record(boxed);
    let mut state = rec.state.lock().unwrap();
    loop {
        if let FiberState::Done(v, e) = *state {
            return RtPair { v, e };
        }
        if rec.interrupted.load(std::sync::atomic::Ordering::Acquire) {
            return RtPair { v: 0, e: rt_make_errbox(interrupted_tag, 0) };
        }
        state = rec.cv.wait(state).unwrap();
    }
}

/// Non-blocking probe: `{some_box_or_0, err}` — `Some(value)` when done,
/// `0` (None) when still running, the error when it failed.
#[no_mangle]
pub extern "C" fn rt_fiber_poll(boxed: i64, interrupted_tag: i64) -> RtPair {
    let rec = fiber_record(boxed);
    let state = rec.state.lock().unwrap();
    match *state {
        FiberState::Done(v, e) => {
            if e != 0 {
                RtPair { v: 0, e }
            } else {
                let some = rt_alloc_global(8) as *mut i64;
                unsafe { *some = v };
                RtPair { v: some as i64, e: 0 }
            }
        }
        FiberState::Pending => {
            if rec.interrupted.load(std::sync::atomic::Ordering::Acquire) {
                RtPair { v: 0, e: rt_make_errbox(interrupted_tag, 0) }
            } else {
                RtPair { v: 0, e: 0 }
            }
        }
    }
}

/// Request cooperative cancellation; idempotent, no-op once completed.
#[no_mangle]
pub extern "C" fn rt_fiber_interrupt(boxed: i64) {
    let rec = fiber_record(boxed);
    rec.interrupted.store(true, std::sync::atomic::Ordering::Release);
    rec.cv.notify_all();
    COMPLETIONS_CV.notify_all();
}

/// Drop glue: the handle's refcount hit zero — supervision interrupts the
/// fiber (its forker is gone) and releases the record reference.
#[no_mangle]
pub extern "C" fn rt_fiber_abandon(boxed: i64) {
    let rec = fiber_record(boxed);
    rec.interrupted.store(true, std::sync::atomic::Ordering::Release);
    rec.cv.notify_all();
    unsafe {
        let arc_raw = *(boxed as *const i64) as *const FiberRecord;
        drop(std::sync::Arc::from_raw(arc_raw));
    }
}

/// Wait until either fiber completes; returns 0 or 1 (ties go left).
#[no_mangle]
pub extern "C" fn rt_fiber_race(a: i64, b: i64) -> i64 {
    let (ra, rb) = (fiber_record(a), fiber_record(b));
    let done = |r: &FiberRecord| matches!(*r.state.lock().unwrap(), FiberState::Done(..));
    let mut seen = COMPLETIONS.lock().unwrap();
    loop {
        if done(ra) {
            return 0;
        }
        if done(rb) {
            return 1;
        }
        seen = COMPLETIONS_CV.wait(seen).unwrap();
    }
}

// ---- http (std/http) -------------------------------------------------------------
//
// Blocking client over ureq/rustls — exactly right for thread-per-fiber:
// a request parks one fiber, `fiber.within` gives deadlines, `retry` gives
// backoff. Results come back as a 3-slot box the compiler unpacks:
//   { 1, payload, 0 }            success (payload depends on the call)
//   { 0, status,  message }      failure (status 0 = transport error)
// Non-2xx responses are SUCCESSES carrying their status — like fetch, the
// status is data; only transport/TLS/connect failures raise.

fn http_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(30))
            .user_agent("inga/0.3")
            .build()
    })
}

fn http_box(ok: i64, a: i64, b: i64) -> i64 {
    let p = rt_alloc_global(24) as *mut i64;
    unsafe {
        *p = ok;
        *p.add(1) = a;
        *p.add(2) = b;
    }
    p as i64
}

fn http_fail(message: String) -> i64 {
    http_box(0, 0, make_str(message.as_bytes()))
}

fn read_body(resp: ureq::Response) -> Result<(i64, i64), String> {
    use std::io::Read;
    let status = resp.status() as i64;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(64 << 20)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    Ok((status, make_str(&bytes)))
}

/// One request: method/url/body strings (body 0 = none), headers an Inga
/// list of (String, String) tuples (0 = none). Success payload is a
/// 2-slot HttpResponse struct {status, body}.
#[no_mangle]
pub extern "C" fn rt_http_send(method: i64, url: i64, body: i64, headers: i64) -> i64 {
    let m = unsafe { std::str::from_utf8_unchecked(str_bytes(method)) };
    let u = unsafe { std::str::from_utf8_unchecked(str_bytes(url)) };
    let mut req = http_agent().request(m, u);
    if headers != 0 {
        for &pair in unsafe { list_items(headers) } {
            let p = pair as *const i64;
            let (k, v) = unsafe { (*p, *p.add(1)) };
            req = req.set(
                unsafe { std::str::from_utf8_unchecked(str_bytes(k)) },
                unsafe { std::str::from_utf8_unchecked(str_bytes(v)) },
            );
        }
    }
    let result = if body != 0 {
        req.send_string(unsafe { std::str::from_utf8_unchecked(str_bytes(body)) })
    } else {
        req.call()
    };
    let resp = match result {
        Ok(r) => r,
        // Non-2xx is a response, not a failure.
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(t)) => return http_fail(t.to_string()),
    };
    match read_body(resp) {
        Ok((status, body)) => {
            let s = rt_alloc(16) as *mut i64;
            unsafe {
                *s = status;
                *s.add(1) = body;
            }
            http_box(1, s as i64, 0)
        }
        Err(e) => http_fail(e),
    }
}

// Open streams: readers parked in a registry keyed by handle. A reader is
// taken out while a chunk is read so a slow stream never blocks others.
static HTTP_STREAMS: std::sync::Mutex<Option<std::collections::HashMap<i64, Box<dyn std::io::Read + Send>>>> =
    std::sync::Mutex::new(None);
static NEXT_STREAM: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

/// GET `url` and stream the body. Success payload is the stream handle;
/// non-2xx still opens (the status is reported alongside the handle in
/// slot 2, consumed by the compiler into the HttpStream struct).
#[no_mangle]
pub extern "C" fn rt_http_open(url: i64) -> i64 {
    let u = unsafe { std::str::from_utf8_unchecked(str_bytes(url)) };
    let resp = match http_agent().get(u).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(t)) => return http_fail(t.to_string()),
    };
    let status = resp.status() as i64;
    let handle = NEXT_STREAM.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut guard = HTTP_STREAMS.lock().unwrap();
    guard
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(handle, Box::new(resp.into_reader()));
    http_box(1, handle, status)
}

/// Next chunk: payload is a string (up to 64 KB) or 0 at end-of-stream.
#[no_mangle]
pub extern "C" fn rt_http_read(handle: i64) -> i64 {
    use std::io::Read;
    let reader = HTTP_STREAMS.lock().unwrap().as_mut().and_then(|m| m.remove(&handle));
    let Some(mut reader) = reader else {
        return http_box(1, 0, 0); // closed or drained: end of stream
    };
    let mut buf = vec![0u8; 64 << 10];
    match reader.read(&mut buf) {
        Ok(0) => http_box(1, 0, 0),
        Ok(n) => {
            HTTP_STREAMS
                .lock()
                .unwrap()
                .get_or_insert_with(std::collections::HashMap::new)
                .insert(handle, reader);
            http_box(1, make_str(&buf[..n]), 0)
        }
        Err(e) => http_fail(e.to_string()),
    }
}

#[no_mangle]
pub extern "C" fn rt_http_close(handle: i64) {
    if let Some(m) = HTTP_STREAMS.lock().unwrap().as_mut() {
        m.remove(&handle);
    }
}

// ---- http server ------------------------------------------------------------------

fn http_reason(status: i64) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn http_write_response(stream: &mut std::net::TcpStream, status: i64, body: &[u8]) {
    use std::io::Write;
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        http_reason(status),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// Read one request: the request line, headers (only Content-Length is
/// honored), and the body. Returns (method, path, query, body).
fn http_read_request(
    stream: &mut std::net::TcpStream,
) -> std::io::Result<(String, String, String, Vec<u8>)> {
    use std::io::{BufRead, BufReader, Read};
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    Ok((method, path, query, body))
}

/// Serve HTTP/1.1 on `port`, one request at a time on the calling thread.
/// Returns `{0, 0, message}` when the listener fails, `{1, 0, 0}` when the
/// request budget (INGA_HTTP_SERVE_REQUESTS, a test hook) is spent, and
/// `{2, errbox, 0}` when the handler fails — that client got a 500 and the
/// error re-raises at the serve site.
#[no_mangle]
pub extern "C" fn rt_http_serve(port: i64, closure: i64) -> i64 {
    let listener = match std::net::TcpListener::bind(("0.0.0.0", port as u16)) {
        Ok(l) => l,
        Err(e) => return http_box(0, 0, make_str(e.to_string().as_bytes())),
    };
    let mut budget: Option<u64> = std::env::var("INGA_HTTP_SERVE_REQUESTS")
        .ok()
        .and_then(|v| v.parse().ok());
    let handler: extern "C" fn(*const i64, i64) -> RtPair =
        unsafe { std::mem::transmute(*(closure as *const i64)) };
    loop {
        if budget == Some(0) {
            return http_box(1, 0, 0);
        }
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(_) => continue,
        };
        let (method, path, query, body) = match http_read_request(&mut stream) {
            Ok(req) => req,
            Err(_) => {
                http_write_response(&mut stream, 400, b"bad request");
                continue;
            }
        };
        if let Some(n) = budget.as_mut() {
            *n -= 1;
        }
        let req = rt_alloc(32) as *mut i64;
        unsafe {
            *req = make_str(method.as_bytes());
            *req.add(1) = make_str(path.as_bytes());
            *req.add(2) = make_str(query.as_bytes());
            *req.add(3) = make_str(&body);
        }
        let pair = handler(closure as *const i64, req as i64);
        if pair.e != 0 {
            http_write_response(&mut stream, 500, b"internal server error");
            return http_box(2, pair.e, 0);
        }
        let resp = pair.v as *const i64;
        let (status, resp_body) = unsafe { (*resp, str_bytes(*resp.add(1))) };
        http_write_response(&mut stream, status, resp_body);
    }
}

// ---- file system -----------------------------------------------------------------
// Every fallible call returns the http-style `{ok, value, message}` box;
// codegen unpacks it and raises IoError { path, message } on failure.

fn fs_path<'a>(path: i64) -> &'a str {
    unsafe { std::str::from_utf8_unchecked(str_bytes(path)) }
}

fn fs_ok(value: i64) -> i64 {
    http_box(1, value, 0)
}

fn fs_fail(e: impl std::fmt::Display) -> i64 {
    http_box(0, 0, make_str(e.to_string().as_bytes()))
}

#[no_mangle]
pub extern "C" fn rt_fs_read(path: i64) -> i64 {
    // An Inga string is a length-prefixed byte buffer, so binary files
    // pass through untouched (same as http bodies).
    match std::fs::read(fs_path(path)) {
        Ok(bytes) => fs_ok(make_str(&bytes)),
        Err(e) => fs_fail(e),
    }
}

#[no_mangle]
pub extern "C" fn rt_fs_write(path: i64, contents: i64) -> i64 {
    match std::fs::write(fs_path(path), unsafe { str_bytes(contents) }) {
        Ok(()) => fs_ok(0),
        Err(e) => fs_fail(e),
    }
}

#[no_mangle]
pub extern "C" fn rt_fs_append(path: i64, contents: i64) -> i64 {
    use std::io::Write;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fs_path(path))
        .and_then(|mut f| f.write_all(unsafe { str_bytes(contents) }));
    match result {
        Ok(()) => fs_ok(0),
        Err(e) => fs_fail(e),
    }
}

#[no_mangle]
pub extern "C" fn rt_fs_exists(path: i64) -> i64 {
    std::path::Path::new(fs_path(path)).exists() as i64
}

#[no_mangle]
pub extern "C" fn rt_fs_list(path: i64) -> i64 {
    match std::fs::read_dir(fs_path(path)) {
        Ok(entries) => {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            names.sort();
            let items: Vec<i64> = names.iter().map(|n| make_str(n.as_bytes())).collect();
            fs_ok(make_list(&items))
        }
        Err(e) => fs_fail(e),
    }
}

#[no_mangle]
pub extern "C" fn rt_fs_remove(path: i64) -> i64 {
    let p = std::path::Path::new(fs_path(path));
    let result = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
    match result {
        Ok(()) => fs_ok(0),
        Err(e) => fs_fail(e),
    }
}

#[no_mangle]
pub extern "C" fn rt_fs_create_dir(path: i64) -> i64 {
    match std::fs::create_dir_all(fs_path(path)) {
        Ok(()) => fs_ok(0),
        Err(e) => fs_fail(e),
    }
}

// ---- stdin -------------------------------------------------------------------------

/// One line from stdin without the trailing newline; None (0) at EOF.
#[no_mangle]
pub extern "C" fn rt_read_line() -> i64 {
    use std::io::BufRead;
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => 0,
        Ok(_) => {
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            let s = make_str(line.as_bytes());
            let p = rt_alloc(8) as *mut i64;
            unsafe { *p = s };
            p as i64
        }
    }
}

// ---- process ----------------------------------------------------------------------

/// Command-line arguments after the program name.
#[no_mangle]
pub extern "C" fn rt_process_args() -> i64 {
    let items: Vec<i64> = std::env::args().skip(1).map(|a| make_str(a.as_bytes())).collect();
    make_list(&items)
}

#[no_mangle]
pub extern "C" fn rt_process_cwd() -> i64 {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    make_str(cwd.as_bytes())
}

#[no_mangle]
pub extern "C" fn rt_process_exit(code: i64) {
    std::process::exit(code as i32);
}

// ---- strings: predicates and transforms -------------------------------------------

fn str_pair<'a>(a: i64, b: i64) -> (&'a [u8], &'a [u8]) {
    unsafe { (str_bytes(a), str_bytes(b)) }
}

#[no_mangle]
pub extern "C" fn rt_str_contains(s: i64, needle: i64) -> i64 {
    let (s, n) = str_pair(s, needle);
    (n.is_empty() || s.windows(n.len().max(1)).any(|w| w == n)) as i64
}

#[no_mangle]
pub extern "C" fn rt_str_starts_with(s: i64, prefix: i64) -> i64 {
    let (s, p) = str_pair(s, prefix);
    s.starts_with(p) as i64
}

#[no_mangle]
pub extern "C" fn rt_str_ends_with(s: i64, suffix: i64) -> i64 {
    let (s, p) = str_pair(s, suffix);
    s.ends_with(p) as i64
}

#[no_mangle]
pub extern "C" fn rt_str_replace(s: i64, old: i64, new: i64) -> i64 {
    let (s, old) = str_pair(s, old);
    let new = unsafe { str_bytes(new) };
    if old.is_empty() {
        return make_str(s);
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(old) {
            out.extend_from_slice(new);
            i += old.len();
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    make_str(&out)
}

#[no_mangle]
pub extern "C" fn rt_str_upper(s: i64) -> i64 {
    let s = unsafe { std::str::from_utf8_unchecked(str_bytes(s)) };
    make_str(s.to_uppercase().as_bytes())
}

#[no_mangle]
pub extern "C" fn rt_str_lower(s: i64) -> i64 {
    let s = unsafe { std::str::from_utf8_unchecked(str_bytes(s)) };
    make_str(s.to_lowercase().as_bytes())
}

#[no_mangle]
pub extern "C" fn rt_str_join(list: i64, sep: i64) -> i64 {
    let sep = unsafe { str_bytes(sep) };
    let mut out: Vec<u8> = Vec::new();
    for (i, &item) in unsafe { list_items(list) }.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(sep);
        }
        out.extend_from_slice(unsafe { str_bytes(item) });
    }
    make_str(&out)
}

// ---- bytes -------------------------------------------------------------------------

/// The i-th byte as Some(0–255); None (0) out of bounds.
#[no_mangle]
pub extern "C" fn rt_byte_at(s: i64, i: i64) -> i64 {
    let bytes = unsafe { str_bytes(s) };
    if i < 0 || i as usize >= bytes.len() {
        return 0;
    }
    let p = rt_alloc(8) as *mut i64;
    unsafe { *p = bytes[i as usize] as i64 };
    p as i64
}

/// `n` as `width` little-endian bytes (width clamped to 1..=8).
#[no_mangle]
pub extern "C" fn rt_int_to_bytes(n: i64, width: i64) -> i64 {
    let width = width.clamp(1, 8) as usize;
    make_str(&n.to_le_bytes()[..width])
}

/// Little-endian read of `width` bytes at `offset`; bytes past the end
/// read as 0.
#[no_mangle]
pub extern "C" fn rt_bytes_to_int(s: i64, offset: i64, width: i64) -> i64 {
    let bytes = unsafe { str_bytes(s) };
    let width = width.clamp(1, 8);
    let mut out: u64 = 0;
    for i in 0..width {
        let idx = offset + i;
        let b = if idx >= 0 && (idx as usize) < bytes.len() { bytes[idx as usize] } else { 0 };
        out |= (b as u64) << (8 * i);
    }
    out as i64
}

/// [Int] (each taken mod 256) to a byte string.
#[no_mangle]
pub extern "C" fn rt_bytes_from_list(list: i64) -> i64 {
    let items = unsafe { list_items(list) };
    let bytes: Vec<u8> = items.iter().map(|&v| v as u8).collect();
    make_str(&bytes)
}

// ---- sorting -----------------------------------------------------------------------

/// kind 0 = raw i64 (Int/Bool/Duration), 1 = f64 bits, 2 = string.
#[no_mangle]
pub extern "C" fn rt_sort(list: i64, kind: i64) -> i64 {
    let mut items = unsafe { list_items(list) }.to_vec();
    match kind {
        1 => items.sort_by(|&a, &b| {
            let (x, y) = (f64::from_bits(a as u64), f64::from_bits(b as u64));
            x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
        }),
        2 => items.sort_by(|&a, &b| unsafe { str_bytes(a).cmp(str_bytes(b)) }),
        _ => items.sort(),
    }
    make_list(&items)
}

/// Stable sort by an Int key closure. Returns `{ok, list, errbox}` — a
/// failing key ends the sort and re-raises at the call site.
#[no_mangle]
pub extern "C" fn rt_sort_by(list: i64, closure: i64) -> i64 {
    let items = unsafe { list_items(list) }.to_vec();
    let key: extern "C" fn(*const i64, i64) -> RtPair =
        unsafe { std::mem::transmute(*(closure as *const i64)) };
    let mut keyed = Vec::with_capacity(items.len());
    for &v in &items {
        let pair = key(closure as *const i64, v);
        if pair.e != 0 {
            return http_box(0, 0, pair.e);
        }
        keyed.push((pair.v, v));
    }
    keyed.sort_by_key(|&(k, _)| k);
    let sorted: Vec<i64> = keyed.into_iter().map(|(_, v)| v).collect();
    http_box(1, make_list(&sorted), 0)
}

// ---- JSON encode/decode over descriptors ----------------------------------------

fn encode_desc(v: i64, d: &mut Desc, out: &mut String) {
    match d.bump() {
        b'i' | b'd' => out.push_str(&v.to_string()),
        b'f' => out.push_str(&f64::from_bits(v as u64).to_string()),
        b'b' => out.push_str(if v != 0 { "true" } else { "false" }),
        b'u' => out.push_str("null"),
        b's' => {
            let s = unsafe { std::str::from_utf8_unchecked(str_bytes(v)) };
            encode_json_str(s, out);
        }
        b'O' => {
            if v == 0 {
                d.skip();
                out.push_str("null");
            } else {
                let inner = unsafe { *(v as *const i64) };
                encode_desc(inner, d, out);
            }
        }
        b'L' => {
            out.push('[');
            let start = d.pos;
            let items = unsafe { list_items(v) };
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                d.pos = start;
                encode_desc(*item, d, out);
            }
            if items.is_empty() {
                d.pos = start;
            }
            d.pos = start;
            d.skip();
            out.push(']');
        }
        b'T' => {
            let n = (d.bump() - b'0') as usize;
            let p = v as *const i64;
            out.push('[');
            for i in 0..n {
                if i > 0 {
                    out.push(',');
                }
                encode_desc(unsafe { *p.add(i) }, d, out);
            }
            out.push(']');
        }
        b'#' => {
            let idx: usize = d.until(b';').parse().unwrap_or(0);
            let line = registry_line(idx);
            let mut rd = Desc::new(line);
            match rd.bump() {
                b'S' => {
                    let _ = rd.until(b'{');
                    let p = v as *const i64;
                    out.push('{');
                    let mut i = 0;
                    while rd.peek() != b'}' && rd.peek() != b'?' {
                        if i > 0 {
                            out.push(',');
                        }
                        let fname = rd.until(b':');
                        encode_json_str(fname, out);
                        out.push(':');
                        encode_desc(unsafe { *p.add(i) }, &mut rd, out);
                        rd.bump();
                        i += 1;
                    }
                    out.push('}');
                }
                _ => {
                    // Enums encode as their show form, quoted.
                    let shown = show_desc(v, &mut Desc::new(&format!("#{idx};")), true);
                    encode_json_str(&shown, out);
                }
            }
        }
        _ => out.push_str("null"),
    }
    fn encode_json_str(s: &str, out: &mut String) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }
}

#[no_mangle]
pub extern "C" fn rt_encode_desc(v: i64, desc: i64) -> i64 {
    let d = unsafe { std::str::from_utf8_unchecked(str_bytes(desc)) };
    let mut out = String::new();
    encode_desc(v, &mut Desc::new(d), &mut out);
    make_str(out.as_bytes())
}

// Minimal JSON value for decoding.
enum Jv {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Jv>),
    Obj(Vec<(String, Jv)>),
}

fn jparse(b: &[u8], p: &mut usize) -> Result<Jv, String> {
    fn ws(b: &[u8], p: &mut usize) {
        while matches!(b.get(*p), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            *p += 1;
        }
    }
    ws(b, p);
    match b.get(*p) {
        Some(b'n') => {
            *p += 4;
            Ok(Jv::Null)
        }
        Some(b't') => {
            *p += 4;
            Ok(Jv::Bool(true))
        }
        Some(b'f') => {
            *p += 5;
            Ok(Jv::Bool(false))
        }
        Some(b'"') => {
            *p += 1;
            let mut s = String::new();
            loop {
                match b.get(*p) {
                    None => return Err("unterminated string".into()),
                    Some(b'"') => {
                        *p += 1;
                        return Ok(Jv::Str(s));
                    }
                    Some(b'\\') => {
                        *p += 1;
                        match b.get(*p) {
                            Some(b'n') => s.push('\n'),
                            Some(b't') => s.push('\t'),
                            Some(b'r') => s.push('\r'),
                            Some(b'"') => s.push('"'),
                            Some(b'\\') => s.push('\\'),
                            Some(b'/') => s.push('/'),
                            Some(b'u') => {
                                let hex = b
                                    .get(*p + 1..*p + 5)
                                    .and_then(|h| std::str::from_utf8(h).ok())
                                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                                    .and_then(char::from_u32)
                                    .ok_or("bad \\u escape")?;
                                s.push(hex);
                                *p += 4;
                            }
                            _ => return Err("bad escape".into()),
                        }
                        *p += 1;
                    }
                    Some(_) => {
                        let start = *p;
                        *p += 1;
                        while b.get(*p).is_some_and(|c| (c & 0xC0) == 0x80) {
                            *p += 1;
                        }
                        s.push_str(&String::from_utf8_lossy(&b[start..*p]));
                    }
                }
            }
        }
        Some(b'[') => {
            *p += 1;
            let mut items = Vec::new();
            ws(b, p);
            if b.get(*p) == Some(&b']') {
                *p += 1;
                return Ok(Jv::Arr(items));
            }
            loop {
                items.push(jparse(b, p)?);
                ws(b, p);
                match b.get(*p) {
                    Some(b',') => *p += 1,
                    Some(b']') => {
                        *p += 1;
                        return Ok(Jv::Arr(items));
                    }
                    _ => return Err("expected , or ]".into()),
                }
            }
        }
        Some(b'{') => {
            *p += 1;
            let mut entries = Vec::new();
            ws(b, p);
            if b.get(*p) == Some(&b'}') {
                *p += 1;
                return Ok(Jv::Obj(entries));
            }
            loop {
                ws(b, p);
                let Jv::Str(key) = jparse(b, p)? else { return Err("expected key".into()) };
                ws(b, p);
                if b.get(*p) != Some(&b':') {
                    return Err("expected :".into());
                }
                *p += 1;
                let value = jparse(b, p)?;
                entries.push((key, value));
                ws(b, p);
                match b.get(*p) {
                    Some(b',') => *p += 1,
                    Some(b'}') => {
                        *p += 1;
                        return Ok(Jv::Obj(entries));
                    }
                    _ => return Err("expected , or }".into()),
                }
            }
        }
        Some(c) if c.is_ascii_digit() || *c == b'-' => {
            let start = *p;
            let mut float = false;
            while let Some(c) = b.get(*p) {
                match c {
                    b'0'..=b'9' | b'-' | b'+' => *p += 1,
                    b'.' | b'e' | b'E' => {
                        float = true;
                        *p += 1;
                    }
                    _ => break,
                }
            }
            let text = std::str::from_utf8(&b[start..*p]).map_err(|_| "bad number")?;
            if float {
                text.parse().map(Jv::Float).map_err(|_| "bad number".into())
            } else {
                text.parse().map(Jv::Int).map_err(|_| "bad number".into())
            }
        }
        _ => Err("unexpected character".into()),
    }
}

fn decode_value(jv: &Jv, d: &mut Desc) -> Result<i64, String> {
    match d.bump() {
        b'i' | b'd' => match jv {
            Jv::Int(n) => Ok(*n),
            _ => Err("expected a number".into()),
        },
        b'f' => match jv {
            Jv::Float(x) => Ok(x.to_bits() as i64),
            Jv::Int(n) => Ok((*n as f64).to_bits() as i64),
            _ => Err("expected a number".into()),
        },
        b'b' => match jv {
            Jv::Bool(x) => Ok(*x as i64),
            _ => Err("expected a boolean".into()),
        },
        b's' => match jv {
            Jv::Str(s) => Ok(make_str(s.as_bytes())),
            _ => Err("expected a string".into()),
        },
        b'O' => match jv {
            Jv::Null => {
                d.skip();
                Ok(0)
            }
            other => {
                let inner = decode_value(other, d)?;
                let q = rt_alloc(8) as *mut i64;
                unsafe { *q = inner };
                Ok(q as i64)
            }
        },
        b'L' => match jv {
            Jv::Arr(items) => {
                let start = d.pos;
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    d.pos = start;
                    out.push(decode_value(item, d)?);
                }
                d.pos = start;
                d.skip();
                Ok(make_list(&out))
            }
            _ => Err("expected an array".into()),
        },
        b'#' => {
            let idx: usize = d.until(b';').parse().unwrap_or(0);
            let line = registry_line(idx).to_string();
            let mut rd = Desc::new(&line);
            match rd.bump() {
                b'S' => {
                    let name = rd.until(b'{').to_string();
                    let Jv::Obj(entries) = jv else {
                        return Err(format!("expected a JSON object for `{name}`"));
                    };
                    let mut fields = Vec::new();
                    while rd.peek() != b'}' && rd.peek() != b'?' {
                        let fname = rd.until(b':');
                        match entries.iter().find(|(k, _)| k == fname) {
                            Some((_, v)) => fields.push(decode_value(v, &mut rd)?),
                            None => return Err(format!("missing field `{fname}` for `{name}`")),
                        }
                        rd.bump();
                    }
                    let q = rt_alloc(8 * fields.len().max(1) as i64) as *mut i64;
                    for (i, fv) in fields.iter().enumerate() {
                        unsafe { *q.add(i) = *fv };
                    }
                    Ok(q as i64)
                }
                _ => Err("cannot decode into this type".into()),
            }
        }
        _ => Err("cannot decode into this type".into()),
    }
}

/// Decode JSON into a value of the described type. Returns a 2-slot box
/// `{ ok, value-or-error-message }`.
#[no_mangle]
pub extern "C" fn rt_decode_desc(json: i64, desc: i64) -> i64 {
    let text = unsafe { std::str::from_utf8_unchecked(str_bytes(json)) };
    let d = unsafe { std::str::from_utf8_unchecked(str_bytes(desc)) }.to_string();
    let mut pos = 0;
    let result = jparse(text.as_bytes(), &mut pos)
        .and_then(|jv| decode_value(&jv, &mut Desc::new(&d)));
    let q = rt_alloc(16) as *mut i64;
    unsafe {
        match result {
            Ok(v) => {
                *q = 1;
                *q.add(1) = v;
            }
            Err(msg) => {
                *q = 0;
                *q.add(1) = make_str(msg.as_bytes());
            }
        }
    }
    q as i64
}
