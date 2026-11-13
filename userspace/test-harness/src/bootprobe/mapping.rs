//! The mapping path: copies from pages not faulted in, windows, gaps, and maps made at once.

use super::*;
use core::sync::atomic::{AtomicU32, Ordering};
use libkern::KError;
use libos::thread;

const LARGE: &str = "/system/large.bin";
const MIB: usize = 1 << 20;

fn refusal<T>(r: &libos::Result<T>) -> Option<KError> {
    r.as_ref().err().map(|e| e.code())
}

/// **A kernel copy from a page not yet faulted in is served where nothing has to wait**: an untouched
/// stack page, a file's page another mapping made resident; one not resident is refused at once.
/// **`large.bin`'s first test**, so no fault, nor its read-ahead, has brought its pages in.
pub(super) fn copy_from_pages_not_faulted_in_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: a copy from pages not faulted in FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    let Ok(file) = lookup::<File, MapRead, _>(root, LARGE, Rights::MAP_READ) else {
        return fail(b"no large.bin");
    };
    let (Ok(a), Ok(b)) = (file.map(LARGE_FILE_BYTES), file.map(LARGE_FILE_BYTES)) else {
        return fail(b"no mapping");
    };
    // SAFETY: a page of a read-only mapping held to the end of this function, and any bytes are a
    // `Msg`; nothing here reads it, so its first touch is the kernel's.
    let page = |m: &Mapping<MapRead>, i: usize| unsafe { &*(m.as_ptr().add(i * PAGE as usize) as *const Msg) };
    let unread = refusal(&mine.send(page(&b, 6), &mut Carried::new()));
    let mut one = [0u8];
    let read = a.read_into(PAGE as usize, &mut one).is_ok() && one[0] == fill_byte(PAGE as usize);
    let resident = refusal(&mine.send(page(&b, 1), &mut Carried::new()));
    let here = 0u8;
    let below = (core::ptr::addr_of!(here) as usize - 6 * MIB) & !(PAGE as usize - 1);
    // SAFETY: a page of this thread's stack, eight MiB reserved and faulted in as it is touched, six
    // below where it runs: never touched, so it reads zeroes, an empty message; nothing writes it.
    let stack = mine.send(unsafe { &*(below as *const Msg) }, &mut Carried::new());
    let mut msg = Msg::new();
    let came = its.recv(&mut msg).is_ok() && msg.payload().is_empty();
    if unread != Some(KError::FaultFromUser) {
        return fail(b"a page not resident was not refused at once");
    }
    if !read || resident != Some(KError::InvalidArgument) {
        return fail(b"a resident page another mapping read was not copied");
    }
    if stack.is_err() || !came {
        return fail(b"an untouched page of the stack was not copied");
    }
    log::write(b"boot-probe: a kernel copy faults in a stack page and a resident file page, not one unread ok\n");
    true
}

/// **A window of an object maps from its offset**, to its last page and no further.
pub(super) fn map_window_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: a window of an object FAIL: ").s(why).end();
        false
    };
    let p = PAGE as usize;
    let Ok(file) = lookup::<File, MapRead, _>(root, LARGE, Rights::MAP_READ) else {
        return fail(b"no large.bin");
    };
    let byte = |m: &Mapping<MapRead>, at: usize| {
        let mut b = [0u8];
        m.read_into(at, &mut b).ok().map(|_| b[0])
    };
    let Ok(window) = file.map_range(3 * p, 2 * p) else {
        return fail(b"a file's window was not mapped");
    };
    if byte(&window, 0) != Some(fill_byte(3 * p)) || byte(&window, p + 5) != Some(fill_byte(4 * p + 5)) {
        return fail(b"a file's window is not its pages from the offset");
    }
    let last = file.map_range(7 * p, p).is_ok();
    let past = refusal(&file.map_range(7 * p, 2 * p));
    let inside = refusal(&file.map_range(p / 2, p));
    if !last || past != Some(KError::InvalidArgument) || inside != Some(KError::InvalidArgument) {
        return fail(b"its last page refused, or a page past it or an offset inside a page mapped");
    }
    let Ok(mem) = Handle::<Memory, MapReadWrite>::create(3 * p) else {
        return fail(b"no memory object");
    };
    let wrote = mem.map(3 * p).is_ok_and(|mut whole| whole.write_from(2 * p, b"E6").is_ok());
    let mut got = [0u8; 2];
    let read = mem.map_range(2 * p, p).is_ok_and(|third| third.read_into(0, &mut got).is_ok());
    if !wrote || !read || got != *b"E6" {
        return fail(b"a memory object's window is not its page from the offset");
    }
    log::write(b"boot-probe: a window of a file and of a memory object maps from its offset ok\n");
    true
}

static REFUSED: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);

/// Map a page of a memory object of its own 64 times, held at once, counting each refusal.
extern "C" fn mapper(_: u64, _: u64, _: u64, _: u64) -> ! {
    let mut held: [Option<Mapping<MapReadWrite>>; 64] = [const { None }; 64];
    match Handle::<Memory, MapReadWrite>::create(PAGE as usize) {
        Ok(mem) => {
            for slot in held.iter_mut() {
                match mem.map(PAGE as usize) {
                    Ok(m) => *slot = Some(m),
                    Err(_) => _ = REFUSED.fetch_add(1, Ordering::Relaxed),
                }
            }
        }
        Err(_) => _ = REFUSED.fetch_add(64, Ordering::Relaxed),
    }
    drop(held);
    DONE.fetch_add(1, Ordering::Release);
    thread::exit(0)
}

/// **An anywhere mapping leaves the page above the one below it free**, which a hinted map may take;
/// **two threads mapping at once are never refused**.
pub(super) fn anywhere_maps_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: anywhere mappings FAIL: ").s(why).end();
        false
    };
    let Ok(mem) = Handle::<Memory, MapReadWrite>::create(PAGE as usize) else {
        return fail(b"no memory object");
    };
    let (Ok(one), Ok(two)) = (mem.map(PAGE as usize), mem.map(PAGE as usize)) else {
        return fail(b"not mapped");
    };
    let low = (one.as_ptr() as u64).min(two.as_ptr() as u64);
    if refused::authority::map_asking(&mem, low + PAGE, PAGE, Rights::MAP_READ.bits()) <= 0 {
        return fail(b"the page above the lower of two is mapped");
    }
    REFUSED.store(0, Ordering::Relaxed);
    DONE.store(0, Ordering::Relaxed);
    let started =
        (0..2).all(|_| thread::Stack::new(4 * PAGE as usize).and_then(|s| thread::start(mapper, s, 0)).is_ok());
    let deadline = clock_ns() + 10_000 * MS;
    while started && DONE.load(Ordering::Acquire) < 2 && clock_ns() < deadline {
        nap(MS);
    }
    if !started || DONE.load(Ordering::Acquire) < 2 {
        return fail(b"the two mapping threads did not finish");
    }
    if REFUSED.load(Ordering::Relaxed) != 0 {
        return fail(b"a map was refused while another thread mapped");
    }
    log::write(b"boot-probe: an anywhere mapping keeps a page free above, and two threads map at once ok\n");
    true
}
