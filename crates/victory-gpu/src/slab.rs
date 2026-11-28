//! Command allocators shared by host lists, one recording at a time.
//!
//! An allocator of its own cost each host list 0.79 MB of resident memory of
//! the driver's even when it recorded nothing (a list on a shared allocator:
//! 0.14 MB, and ~0.05 MB more per recording; `inplace::tests::lists_cost`),
//! and ~1,100 host lists are alive in gameplay. So a list takes an allocator
//! here for each recording and gives it back when it closes; the allocator
//! is reset only once every list that recorded into it since its last reset
//! has been reset or dropped itself (which happens only after its
//! submissions ran). An allocator that has served [`RECORDINGS`] recordings
//! takes no more until then, so what it keeps stays bounded.
//! `VICTORY_SHARED_ALLOCATORS=0`: each list has an allocator of its own, as
//! before.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::{D3D12_COMMAND_LIST_TYPE_DIRECT, ID3D12CommandAllocator, ID3D12Device};

/// Recordings an allocator serves between two of its resets.
const RECORDINGS: u32 = 16;

/// Whether host lists share allocators.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SHARED_ALLOCATORS").is_none_or(|v| v != "0"))
}

struct Slab {
    allocator: ID3D12CommandAllocator,
    /// Lists that recorded into it since its last reset and are not reset
    /// yet, recordings since that reset, and whether it is in [`FREE`].
    state: Mutex<(u32, u32, bool)>,
}

// SAFETY: D3D12 objects are free-threaded; the allocator is reset and
// recorded into only by the holder of its recording [`Use`].
unsafe impl Send for Slab {}
// SAFETY: as above.
unsafe impl Sync for Slab {}

/// Allocators no list records into, that may take another recording.
static FREE: Mutex<Vec<Arc<Slab>>> = Mutex::new(Vec::new());

/// Allocators alive (for the memory line).
pub(crate) static ALIVE: AtomicU32 = AtomicU32::new(0);

/// A list's claim on an allocator: while `recording`, the only list
/// recording into it; after, that its commands may still run.
pub(crate) struct Use {
    slab: Arc<Slab>,
    recording: bool,
}

impl Use {
    pub(crate) fn allocator(&self) -> &ID3D12CommandAllocator {
        &self.slab.allocator
    }

    /// The list closed: another list may record into the allocator.
    pub(crate) fn closed(&mut self) {
        if !std::mem::replace(&mut self.recording, false) {
            return;
        }
        let back = {
            let mut state = self.slab.state.lock().unwrap_or_else(|e| e.into_inner());
            let back = state.1 < RECORDINGS && !state.2;
            state.2 |= back;
            back
        };
        if back {
            FREE.lock().unwrap_or_else(|e| e.into_inner()).push(self.slab.clone());
        }
    }
}

impl Drop for Use {
    fn drop(&mut self) {
        self.closed();
        let back = {
            let mut state = self.slab.state.lock().unwrap_or_else(|e| e.into_inner());
            state.0 -= 1;
            let back = state.0 == 0 && !state.2;
            state.2 |= back;
            back
        };
        if back {
            FREE.lock().unwrap_or_else(|e| e.into_inner()).push(self.slab.clone());
        } else if Arc::strong_count(&self.slab) == 1 {
            ALIVE.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// An allocator to record one list into, reset first if no list's commands
/// in it may still run.
pub(crate) fn take(device: &ID3D12Device) -> windows::core::Result<Use> {
    let slab = FREE.lock().unwrap_or_else(|e| e.into_inner()).pop();
    let slab = match slab {
        Some(slab) => slab,
        None => {
            // SAFETY: plain object creation.
            let allocator = unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)? };
            ALIVE.fetch_add(1, Ordering::Relaxed);
            Arc::new(Slab { allocator, state: Mutex::new((0, 0, false)) })
        }
    };
    {
        let mut state = slab.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 == 0 {
            // SAFETY: no list records into it and none of the lists that did
            // may still run (each was reset or dropped after its run).
            if let Err(e) = unsafe { slab.allocator.Reset() } {
                tracing::warn!(error = %e, "a shared command allocator did not reset");
            }
            state.1 = 0;
        }
        state.0 += 1;
        state.1 += 1;
        state.2 = false;
    }
    Ok(Use { slab, recording: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On the real card (ignored: needs one): a closed list's allocator is
    /// taken again; it is reset only once no list's use of it is left; one
    /// that served [`RECORDINGS`] waits for that and another is made.
    #[test]
    #[ignore]
    fn allocators_are_shared_and_reset_when_free() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12GraphicsCommandList};
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: plain device and list creation on the first card.
        unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap() };
        let device = device.unwrap();
        let alive = || ALIVE.load(Ordering::Relaxed);
        let start = alive();
        let mut a = take(&device).unwrap();
        // SAFETY: as above.
        let list: ID3D12GraphicsCommandList =
            unsafe { device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, a.allocator(), None).unwrap() };
        // SAFETY: an open list.
        unsafe { list.Close().unwrap() };
        a.closed();
        let mut b = take(&device).unwrap();
        assert!(Arc::ptr_eq(&a.slab, &b.slab), "a closed list's allocator is taken again");
        assert_eq!(*b.slab.state.lock().unwrap(), (2, 2, false));
        // SAFETY: a closed list, its allocator held by `b`.
        unsafe { list.Reset(b.allocator(), None).unwrap() };
        let c = take(&device).unwrap();
        assert!(!Arc::ptr_eq(&b.slab, &c.slab), "one recording at a time");
        drop(c);
        // SAFETY: an open list.
        unsafe { list.Close().unwrap() };
        b.closed();
        drop(a);
        assert_eq!(b.slab.state.lock().unwrap().0, 1);
        let slab = b.slab.clone();
        drop(b);
        assert_eq!(*slab.state.lock().unwrap(), (0, 2, true));
        let mut uses: Vec<Use> = (0..RECORDINGS).map(|_| {
            let mut u = take(&device).unwrap();
            u.closed();
            u
        }).collect();
        assert!(uses.iter().all(|u| Arc::ptr_eq(&u.slab, &slab)), "reset, it serves again");
        assert_eq!(slab.state.lock().unwrap().1, RECORDINGS);
        let other = take(&device).unwrap();
        assert!(!Arc::ptr_eq(&other.slab, &slab), "full: it waits for its lists");
        uses.clear();
        assert!(slab.state.lock().unwrap().2, "free again once its lists are reset");
        drop(other);
        assert!(alive() <= start + 2);
    }
}
