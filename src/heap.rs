use core::alloc::{GlobalAlloc, Layout};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};
use embedded_alloc::LlffHeap as Heap;

#[global_allocator]
pub static HEAP: DualHeap = DualHeap::empty();
const HEAP_SIZE: usize = 64 * 1024;
static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];

struct Region {
    start: AtomicUsize,
    size: AtomicUsize,
}

impl Region {
    const fn default() -> Self {
        Self {
            start: AtomicUsize::new(0),
            size: AtomicUsize::new(0),
        }
    }

    fn contains(&self, address: usize) -> bool {
        let start = self.start.load(Ordering::Relaxed);
        let end = self.start.load(Ordering::Relaxed);
        (start..start + end).contains(&address)
    }

    fn new(start: usize, size: usize) -> Self {
        Self {
            start: AtomicUsize::new(start),
            size: AtomicUsize::new(size),
        }
    }

    fn load(&self) -> (usize, usize) {
        (
            self.start.load(Ordering::SeqCst),
            self.size.load(Ordering::SeqCst),
        )
    }
}

/// This is an allocator that combines two regions of memory.
/// The intent is to use some of the directly connected RAM
/// for this, and if we find some XIP capable PSRAM, add that
/// as a secondary region.
/// Allocation from the primary region is always preferred,
/// as it is expected to be a bit faster than PSRAM.
/// FIXME: PSRAM-allocated memory isn't compatible with
/// CAS atomics, so we might need a bit of a think about this!
pub struct DualHeap {
    primary: Heap,
    primary_region: Region,
    secondary: Heap,
}

impl DualHeap {
    pub const fn empty() -> Self {
        Self {
            primary: Heap::empty(),
            primary_region: Region::default(),
            secondary: Heap::empty(),
        }
    }

    unsafe fn add_primary(&self, region: Region) {
        let (start, size) = region.load();
        unsafe {
            self.primary.init(start, size);
        }
        self.primary_region.start.store(start, Ordering::SeqCst);
        self.primary_region.size.store(size, Ordering::SeqCst);
    }

    unsafe fn add_secondary(&self, region: Region) {
        let (start, size) = region.load();
        unsafe {
            self.secondary.init(start, size);
        }
    }

    pub fn used(&self) -> usize {
        self.primary.used() + self.secondary.used()
    }

    pub fn free(&self) -> usize {
        self.primary.free() + self.secondary.free()
    }
}

unsafe impl GlobalAlloc for DualHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe {
            let ptr = self.primary.alloc(layout);
            if !ptr.is_null() {
                return ptr;
            }
            // start using secondary area when primary heap is full
            self.secondary.alloc(layout)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            let ptr_usize = ptr as usize;
            if self.primary_region.contains(ptr_usize) {
                self.primary.dealloc(ptr, layout);
            } else {
                self.secondary.dealloc(ptr, layout);
            }
        }
    }
}

pub fn init_heap() {
    let primary_start = &raw mut HEAP_MEM as usize;
    unsafe { HEAP.add_primary(Region::new(primary_start, HEAP_SIZE)) }
}

pub fn init_qmi_psram_heap(size: u32) {
    unsafe { HEAP.add_secondary(Region::new(0x11000000, size as usize)) }
}

async fn print_heap_line(name: &str, used: usize, free: usize) {
    let total = used + free;
    print!("{name:<10} {total:>10} {used:>10} {free:>10}\r\n");
}

pub async fn free_command(_args: &[&str]) {
    print!(
        "{:<10} {:>10} {:>10} {:>10}\r\n",
        "", "TOTAL", "USED", "FREE"
    );

    print_heap_line("RAM", HEAP.primary.used(), HEAP.primary.free()).await;
    print_heap_line("PSRAM (QMI)", HEAP.secondary.used(), HEAP.secondary.free()).await;
}
