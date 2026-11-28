//! Which of the process's private memory is buffers of ours in the PC's
//! memory (upload and shared buffers, by the line that made them) and which
//! is the driver's or the runtime's (diagnostics builds): each buffer is
//! noted with its mapping when made and forgotten when the host frees it (an
//! object it holds as private data is released then), and [`census`] walks
//! the address space.

use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::Location;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::core::{GUID, HRESULT, IUnknown, Interface};
use windows_sys::Win32::System::Memory as mem;

/// By mapping: bytes asked for and where.
static LIVE: Mutex<Option<HashMap<usize, (u64, &'static Location<'static>)>>> = Mutex::new(None);

const KEY: GUID = GUID::from_u128(0x3c1f6a2e_9d4b_4c7e_a1f0_5b2e8d7c4a19);

#[repr(C)]
struct Gone {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    cpu: usize,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}
static GONE: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    HRESULT(0x8000_4002_u32 as i32)
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Gone>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Gone>()).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Gone>());
            if let Some(live) = LIVE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                live.remove(&gone.cpu);
            }
        }
        left
    }
}

/// `buffer`, mapped at `cpu`, was made at `at`.
pub(crate) fn track(buffer: &ID3D12Resource, cpu: *mut u8, bytes: u64, at: &'static Location<'static>) {
    let cpu = cpu as usize;
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_default().insert(cpu, (bytes, at));
    let gone = Box::into_raw(Box::new(Gone { methods: &GONE.0, references: AtomicU32::new(1), cpu }));
    // SAFETY: a COM object of ours with one reference, handed to the buffer
    // (which takes its own); ours goes when `object` drops.
    unsafe {
        let object = IUnknown::from_raw(gone.cast());
        if buffer.SetPrivateDataInterface(&KEY, &object).is_err() {
            LIVE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_default().remove(&cpu);
        }
    }
}

/// Bytes of `[at, at + len)` in the working set, one page in sixteen asked about.
fn resident(at: usize, len: usize) -> u64 {
    use windows_sys::Win32::System::ProcessStatus::K32QueryWorkingSetEx;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const STEP: usize = 64 << 10;
    let mut asked: Vec<[usize; 2]> = (at..at + len).step_by(STEP).map(|a| [a, 0]).collect();
    // SAFETY: our own process, a buffer of (address, attributes) pairs.
    let ok = unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), asked.as_mut_ptr().cast(), (asked.len() * 16) as u32) } != 0;
    if !ok {
        return 0;
    }
    let step = STEP.min(len) as u64;
    asked.iter().filter(|a| a[1] & 1 != 0).count() as u64 * step
}

/// Logs the private memory outside the title's range: ours by the line that
/// made it, the rest by the size of its allocations.
pub(crate) fn census() {
    // Committed bytes and whether write-combined, by allocation base.
    let mut allocations: HashMap<usize, (u64, bool, u64)> = HashMap::new();
    // The title's range is not counted, and not walked either: its page
    // views are ~250,000 regions (0.9 s of questions, each holding the
    // process's memory map: the title's page mappings waited behind them,
    // up to 114 ms a question in run tl1's streaming).
    const TITLE: std::ops::Range<usize> = 0x1_0000_0000..0x110_0000_0000;
    // Every region walked (start, end, allocation base), in order: our
    // buffers' allocations are looked up here, not asked about one by one
    // (two questions per buffer, ~20,000, each waiting behind the title's
    // page mappings: 20 ms a frame of the meter thread in run tl2).
    let mut regions: Vec<(usize, usize, usize)> = Vec::new();
    let mut at = 0usize;
    loop {
        // SAFETY: plain data the query fills in.
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { mem::VirtualQuery(at as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let base = info.BaseAddress as usize;
        if info.State != mem::MEM_FREE {
            regions.push((base, base.saturating_add(info.RegionSize), info.AllocationBase as usize));
        }
        if info.State == mem::MEM_COMMIT && info.Type == mem::MEM_PRIVATE && !TITLE.contains(&base) {
            let e = allocations.entry(info.AllocationBase as usize).or_default();
            e.0 += info.RegionSize as u64;
            e.1 |= info.Protect & mem::PAGE_WRITECOMBINE != 0;
            e.2 += resident(base, info.RegionSize);
        }
        at = base.saturating_add(info.RegionSize);
        if TITLE.contains(&at) {
            at = TITLE.end;
        }
        if at >= 0x7fff_ffff_0000 {
            break;
        }
    }
    let base_of = |cpu: usize| {
        let i = regions.partition_point(|r| r.0 <= cpu).checked_sub(1)?;
        let (start, end, base) = regions[i];
        (start..end).contains(&cpu).then_some(base)
    };
    let live: Vec<(usize, u64, &'static Location<'static>)> = LIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|l| l.iter().map(|(&c, &(b, at))| (c, b, at)).collect())
        .unwrap_or_default();
    // By line: buffers, bytes asked for, bytes of the allocations they lie in.
    let mut ours: HashMap<String, (u64, u64, u64)> = HashMap::new();
    // Allocations holding our buffers, by base: how many buffers and lines.
    let mut holders: HashMap<usize, (u32, std::collections::BTreeSet<String>)> = HashMap::new();
    let mut owned: HashMap<usize, String> = HashMap::new();
    for (cpu, bytes, at) in live {
        let line = format!("{}:{}", at.file().rsplit(['/', '\\']).next().unwrap_or(""), at.line());
        let e = ours.entry(line.clone()).or_default();
        e.0 += 1;
        e.1 += bytes;
        if let Some(base) = base_of(cpu) {
            let h = holders.entry(base).or_default();
            h.0 += 1;
            h.1.insert(line.clone());
        }
        if let Some(base) = base_of(cpu)
            && let Some(&(_, _, resident)) = allocations.get(&base)
            && owned.insert(base, line).is_none()
        {
            e.2 += resident;
        }
    }
    let mut ours: Vec<_> = ours.into_iter().collect();
    ours.sort_by_key(|(_, e)| std::cmp::Reverse(e.2));
    let ours_text: Vec<String> = ours.iter().map(|(l, e)| format!("{l} {} buffers {} MB, {} MB resident", e.0, e.1 >> 20, e.2 >> 20)).collect();
    // The allocations holding several of our buffers, or buffers of several lines.
    let mut shared: Vec<(u64, u64, u32, String)> = holders
        .iter()
        .filter(|(_, h)| h.0 > 1)
        .filter_map(|(base, h)| {
            let &(committed, _, resident) = allocations.get(base)?;
            Some((resident, committed, h.0, h.1.iter().cloned().collect::<Vec<_>>().join("+")))
        })
        .collect();
    shared.sort_by_key(|x| std::cmp::Reverse(x.0));
    let shared_text: Vec<String> =
        shared.iter().take(8).map(|(r, c, n, l)| format!("{} KiB ({} resident) holds {n} of {l}", c >> 10, r >> 20)).collect();
    tracing::info!(single = holders.values().filter(|h| h.0 == 1).count(), "census: allocations holding more than one buffer of ours: {}", shared_text.join("; "));
    // The rest, by kind and exact size.
    // The process heap's segments our blocks were seen in.
    let heap: std::collections::HashSet<usize> =
        crate::copystat::HEAP_BASES.iter().map(|b| b.load(Ordering::Relaxed) as usize).filter(|&b| b != 0).collect();
    let mut heap_total = [0u64; 3];
    for base in &heap {
        if let Some(&(committed, _, resident)) = allocations.get(base) {
            heap_total[0] += 1;
            heap_total[1] += committed;
            heap_total[2] += resident;
        }
    }
    let mut rest: HashMap<(bool, u64), (u64, u64, u64)> = HashMap::new();
    let (mut rest_total, mut ours_total) = ([0u64; 2], [0u64; 2]);
    for (base, &(committed, wc, resident)) in &allocations {
        if owned.contains_key(base) {
            ours_total[0] += committed;
            ours_total[1] += resident;
            continue;
        }
        rest_total[0] += committed;
        rest_total[1] += resident;
        let e = rest.entry((wc, committed)).or_default();
        e.0 += 1;
        e.1 += committed;
        e.2 += resident;
    }
    let mut rest: Vec<_> = rest.into_iter().collect();
    rest.sort_by_key(|(_, e)| std::cmp::Reverse(e.2));
    let rest_text: Vec<String> = rest
        .iter()
        .take(16)
        .map(|((wc, size), e)| format!("{} {} KiB x{} = {} MB, {} resident", if *wc { "wc" } else { "wb" }, size >> 10, e.0, e.1 >> 20, e.2 >> 20))
        .collect();
    tracing::info!(
        idle_lists = crate::list::idle_lists(),
        heap_segments_with_our_blocks = format_args!("{} allocations, {} MB, {} MB resident", heap_total[0], heap_total[1] >> 20, heap_total[2] >> 20),
        "census of private memory outside the title's range: buffers of ours {} MB committed, {} MB resident ({}); the rest {} MB, {} MB resident, by allocation size: {}",
        ours_total[0] >> 20,
        ours_total[1] >> 20,
        ours_text.join(", "),
        rest_total[0] >> 20,
        rest_total[1] >> 20,
        rest_text.join(", ")
    );
}
