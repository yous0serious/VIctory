//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, View, view_at};
use crate::host::host;
use crate::shadow::Shadow;

pub(crate) struct HostHeap {
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Dynamic slots past `count` (none in sampler heaps): [`DYNAMIC`]
    /// where the host takes them, else [`MIN_DYNAMIC`].
    dynamic: u32,
    sampler: bool,
    /// The record (view index and generation) each slot was last made from.
    made: Mutex<Vec<u64>>,
    /// What each slot views, held while the slot does: shaders index heaps
    /// directly, so a slot the title has not rewritten since it released the
    /// resource may still be read, and a freed host resource read there is
    /// a GPU page fault (the console reads stale memory). A buffer slot's
    /// copy is also the one its draws bring up to date and copy back
    /// ([`HostHeap::copy_at`]).
    kept: Mutex<Vec<Kept>>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, Vec<bool>)>,
}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!("{:#x} past its end (slot {})", va - start - bytes, (va - start) / step)
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw). A list holds the slots it
/// took until it is reset (its submissions have run) or released.
/// 2^16 ran out seconds into the world (every world run on an RX 9070 XT,
/// each a device removal shortly after; #59).
const DYNAMIC: u32 = 1 << 18;

/// The dynamic slots of a heap whose host would not take [`DYNAMIC`] more
/// (a capped heap, NVIDIA): the title's slots keep their room first.
const MIN_DYNAMIC: u32 = 1 << 16;

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        // The larger dynamic range only where the host takes it on top of
        // every title slot; else the smaller one, and the cap after that.
        let (heap, count, extra) = if sampler {
            match make(count) {
                Ok(heap) => (heap, count, 0),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else if let Ok(heap) = make(count + DYNAMIC) {
            (heap, count, DYNAMIC)
        } else {
            match make(count + MIN_DYNAMIC) {
                Ok(heap) => (heap, count, MIN_DYNAMIC),
                Err(e) if count + MIN_DYNAMIC > PORTABLE_VIEWS => {
                    // shortcut: descriptors past the host's limit are left
                    // out; a title that reaches them needs remapping.
                    tracing::warn!(error = %e, count, "host view heap capped at {PORTABLE_VIEWS}");
                    (make(PORTABLE_VIEWS).ok()?, PORTABLE_VIEWS - MIN_DYNAMIC, MIN_DYNAMIC)
                }
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        };
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(count + extra) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let h = HostHeap {
            heap,
            cpu,
            step,
            count,
            dynamic: extra,
            sampler,
            made: Mutex::new(vec![0; count as usize]),
            kept: Mutex::new(vec![Kept::Nothing; (count + extra) as usize]),
            next: Mutex::new((0, vec![false; extra as usize])),
        };
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, count + extra) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(ms = started.elapsed().as_millis(), "host descriptor heap made null");
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: BLOCK,
                    ..Default::default()
                })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler(start);
            } else {
                null_view(start);
            }
            let mut filled = 1;
            while filled < BLOCK {
                let n = filled.min(BLOCK - filled);
                host.device.CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = BLOCK.min(total - done);
                host.device.CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler(self.cpu(index));
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let n = n as usize;
        // Search from `next` on, then from the start.
        let starts = (*next as usize..self.dynamic as usize).chain(0..*next as usize);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let Some(first) = found else {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(n, "no free dynamic descriptor slots: a table is not made"));
            return None;
        };
        taken[first..first + n].fill(true);
        *next = ((first + n) % self.dynamic as usize) as u32;
        Some(self.count + first as u32)
    }

    /// The title slots this heap has host slots for; fewer than the title's
    /// when the host caps its size.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned).
    ///
    /// [`dynamic`]: HostHeap::dynamic
    pub(crate) fn release(&self, first: u32, n: u32) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let end = (at + n).min(self.dynamic) as usize;
        if let Some(slots) = guard.1.get_mut(at as usize..end) {
            slots.fill(false);
        }
    }

    /// Makes slot `index` (a dynamic one) from `view`, a null view if the
    /// host cannot, and returns the copy of title buffer memory it views.
    pub(crate) fn make_at(&self, index: u32, view: &View) -> Option<Arc<Shadow>> {
        let kept = make(view, self.cpu(index)).unwrap_or_else(|| {
            null_for(view, self.cpu(index));
            Kept::Nothing
        });
        let copy = kept.copy();
        self.keep(index, kept);
        copy
    }

    /// The copy of title buffer memory slot `index` views, if any: the one
    /// to bring up to date for it and to copy its writes back from. Looking
    /// a copy up again from the view's range can find another, overlapping
    /// title buffer's, or the buffer's copy where the view has its own.
    pub(crate) fn copy_at(&self, index: u32) -> Option<Arc<Shadow>> {
        self.kept
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)?
            .copy()
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
        self.keep(index, Kept::Nothing);
    }

    /// Slot `index` now views `kept`.
    fn keep(&self, index: u32, kept: Kept) {
        let mut slots = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = slots.get_mut(index as usize) {
            *slot = kept;
        }
    }

    /// The shader-visible handle of slot `index`.
    pub(crate) fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: a live shader-visible heap.
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart().ptr };
        D3D12_GPU_DESCRIPTOR_HANDLE { ptr: start + u64::from(index) * u64::from(self.step) }
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view of `len` bytes
    /// of `buffer` from byte `offset` (4-byte units, rounded down), holding
    /// the buffer while the slot does.
    pub(crate) fn raw_srv_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: (len / 4).clamp(1, u64::from(u32::MAX)) as u32,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a live buffer and a slot of our heap; the range lies in it.
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), self.cpu(index))
        };
        self.keep(index, Some(buffer.clone()));
    }

    /// Makes slot `index` (a dynamic one) a constant buffer view of host
    /// memory (the list's own, which it holds).
    pub(crate) fn constant_at(&self, index: u32, gpu: u64, size: u32) {
        self.keep(index, Kept::Nothing);
        let Some(host) = host() else { return };
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: size.next_multiple_of(256),
        };
        // SAFETY: a slot of our heap; the memory is the caller's.
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.cpu(index))
        };
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there and, for
    /// draws, the copy of title buffer memory the slot views. Buffer views
    /// are made only for draws (`for_draw`), which also copy their data: a
    /// host copy of a buffer is made on first use.
    pub(crate) fn mirror(
        &self,
        index: u32,
        handle: u64,
        for_draw: bool,
    ) -> Option<(View, Option<Arc<Shadow>>)> {
        if index >= self.count {
            // Diagnostic (NVIDIA heap cap): the highest title slot past the
            // host heap seen so far.
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        let Some(view) = view else {
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            if made[index as usize] != 0 {
                made[index as usize] = 0;
                self.make_null(index);
            }
            return None;
        };
        let copy = if made[index as usize] != key {
            let later = !for_draw && view.is_buffer();
            made[index as usize] = if later { 0 } else { key };
            let kept = if later {
                None
            } else {
                make(&view, self.cpu(index))
            };
            let kept = kept.unwrap_or_else(|| {
                if !self.sampler {
                    null_for(&view, self.cpu(index));
                }
                Kept::Nothing
            });
            let copy = kept.copy();
            self.keep(index, kept);
            copy
        } else if for_draw {
            self.copy_at(index)
        } else {
            None
        };
        Some((view, copy))
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = *self.made.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = self.copy_at(index).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (address, _, unordered, words) = buffer_parts(view)?;
    let (from, bytes, _) = extent(address, unordered, words);
    Some((from, bytes))
}

/// A buffer view as [`make`] takes it: where its resource's memory starts and
/// how long it is (its host copy is found by these), whether it is unordered
/// access, and its descriptor's words.
fn buffer_parts(view: &View) -> Option<(u64, u64, bool, &[u32; 10])> {
    let registered = |r: usize| {
        crate::resource::buffer_bytes(r as crate::object::This)
            .map(|(data, len)| (data as u64, len))
    };
    match view {
        View::Shader(r, Some(desc)) => registered(*r).map(|(at, len)| (at, len, false, desc)),
        View::Unordered(r, _, Some(desc)) => registered(*r).map(|(at, len)| (at, len, true, desc)),
        View::Placed(unordered, resource, desc) if resource[0] == 1 => Some((
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            // The resource description's width, after the alignment.
            u64::from(resource[4]) | u64::from(resource[5]) << 32,
            *unordered,
            desc[..10].try_into().ok()?,
        )),
        _ => None,
    }
}

/// A buffer view's own range in the resource at `address` and the bytes of
/// one of its elements (0 for a format of unknown size).
fn extent(address: u64, unordered: bool, words: &[u32; 10]) -> (u64, u64, u64) {
    if unordered {
        unordered_extent(address, &uav_desc(words))
    } else {
        shader_extent(address, &srv_desc(words))
    }
}

/// [`extent`] of a shader view.
fn shader_extent(address: u64, desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> (u64, u64, u64) {
    let (from, bytes) = view_range_of(address, desc);
    (from, bytes, element_size(desc))
}

/// [`extent`] of an unordered access view.
fn unordered_extent(address: u64, desc: &D3D12_UNORDERED_ACCESS_VIEW_DESC) -> (u64, u64, u64) {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    (
        address.saturating_add(buffer.FirstElement * element),
        u64::from(buffer.NumElements) * element,
        element,
    )
}

/// What a slot made by [`make`] views, kept while the slot is.
#[derive(Clone)]
pub(crate) enum Kept {
    /// Nothing of the host's (a sampler, a null view, a list's memory).
    Nothing,
    /// A host resource: a texture, or the copy a constant buffer view is of.
    Resource { _resource: ID3D12Resource },
    /// A copy of title buffer memory a shader or unordered access view is
    /// of: its buffer's copy, or a copy of just the view's bytes.
    Copy(Arc<Shadow>),
}

impl Kept {
    /// The copy of title buffer memory, for a buffer view.
    fn copy(&self) -> Option<Arc<Shadow>> {
        match self {
            Kept::Copy(copy) => Some(copy.clone()),
            _ => None,
        }
    }
}

/// The title memory a buffer shader view reads, from where its descriptor puts
/// it in the resource at `address`: its first element and the bytes its
/// elements take.
fn view_range_of(address: u64, desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> (u64, u64) {
    let element = element_size(desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    (
        address.saturating_add(buffer.FirstElement * element),
        u64::from(buffer.NumElements) * element,
    )
}

/// The host copy a buffer view of the resource at `address` (`len` bytes)
/// uses, the element its start is, and whether that copy holds only the
/// view's bytes. `extent` is the view's range and element size.
///
/// [`Copy::OwnBytes`] is for a view of memory the title placed inside another
/// object (`View::Placed`: methods 0x230 and 0x238), whose start need not be a
/// multiple of its stride. Its copy is a host copy like any other, so its own
/// refresh brings it up to date from title memory, what shaders write in it
/// goes back there after the submission, and a shader sees what the rest of
/// the pipeline sees.
fn view_copy(
    address: u64,
    len: u64,
    (from, bytes, element): (u64, u64, u64),
) -> Result<(Arc<Shadow>, u64, bool), &'static str> {
    // The view's own range is also the key `aligned` caches its copy by (the
    // one `buffer_range` gives `readable`).
    let buffer = crate::shadow::find(address, len).ok_or("no host copy of its buffer")?;
    match plan(bytes, element, buffer.offset(from)) {
        Copy::Element { first } => Ok((buffer, first, false)),
        Copy::OwnBytes => {
            let copy = crate::shadow::aligned(from, bytes).ok_or("no copy of the view")?;
            Ok((copy, 0, true))
        }
        Copy::None { why } => Err(why),
    }
}

/// What a buffer shader view reads.
enum Copy {
    /// The buffer's copy, at this element.
    Element { first: u64 },
    /// A copy of just the view's bytes, starting at the view.
    OwnBytes,
    /// Nothing, for the reason given.
    None { why: &'static str },
}

/// Where a buffer shader view's bytes come from: the buffer's copy when the
/// view starts on one of its elements (`element` bytes each, at `offset` in
/// the copy), else a copy of the view's own bytes.
fn plan(len: u64, element: u64, offset: u64) -> Copy {
    // Elements of a format whose size is not known: left null, as before.
    if element == 0 {
        return Copy::None {
            why: "elements of unknown size",
        };
    }
    if offset.is_multiple_of(element) {
        return Copy::Element {
            first: offset / element,
        };
    }
    if len == 0 {
        return Copy::None {
            why: "a view of no bytes",
        };
    }
    Copy::OwnBytes
}

/// Strides from `VICTORY_NULL_VIEW_STRIDES`: decimal or `0x` hex, comma
/// separated; anything else is named and skipped.
#[cfg(feature = "diagnostics")]
fn parsed_strides(value: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match part
            .strip_prefix("0x")
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .or_else(|| part.parse().ok())
        {
            Some(n) => out.push(n),
            None => tracing::warn!(
                part,
                "VICTORY_NULL_VIEW_STRIDES expects strides in decimal or 0x hex"
            ),
        }
    }
    out
}

/// With `VICTORY_NULL_VIEW_STRIDES` (diagnostics): buffer views of these
/// strides are left null, to confirm in the game which ones a pass needs.
/// Decimal or `0x` hex strides, comma separated.
fn nulled_stride(stride: u32) -> bool {
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = stride;
        false
    }
    #[cfg(feature = "diagnostics")]
    {
        if stride == 0 {
            return false;
        }
        static NULLED: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
        let nulled = NULLED.get_or_init(|| {
            parsed_strides(&std::env::var("VICTORY_NULL_VIEW_STRIDES").unwrap_or_default())
        });
        nulled.contains(&stride)
    }
}

fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    placed: bool,
) -> Option<Kept> {
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let stride = unsafe { desc.Anonymous.Buffer }.StructureByteStride;
    if nulled_stride(stride) {
        once!(
            stride,
            "a buffer shader view left null (VICTORY_NULL_VIEW_STRIDES)"
        );
        return None;
    }
    let (shadow, first, own) = view_copy(address, len, shader_extent(address, &desc))
        .map_err(|why| {
            once!(
                view = kind(placed),
                address = format_args!("{address:#x}"),
                len,
                element,
                stride,
                why,
                "a buffer shader view left null"
            );
        })
        .ok()?;
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    // `first` is the element the view's own start is from the copy's start,
    // so it replaces the title's first element rather than adding to it: on a
    // copy of just the view's bytes that is 0, since the copy starts there.
    buffer.FirstElement = if own { 0 } else { first };
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    Some(Kept::Copy(shadow))
}

/// How a buffer view was placed, as its warning names it: a registered
/// resource's views start at the resource and so on an element; the title
/// places a view of its own memory, which names its own start and can land off
/// an element.
fn kind(placed: bool) -> &'static str {
    if placed { "placed" } else { "registered" }
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; none if the copy cannot be
/// written by shaders (upload memory). What shaders write goes back to title
/// memory after the submission (`readable` declares the view's range).
fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    placed: bool,
) -> Option<Kept> {
    let extent = unordered_extent(address, &desc);
    let (shadow, first, own) = view_copy(address, len, extent)
        .map_err(|why| {
            once!(
                view = kind(placed),
                address = format_args!("{address:#x}"),
                len,
                element = extent.2,
                why,
                "an unordered buffer view left null"
            );
        })
        .ok()?;
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    // As for a shader view: the element the view starts at in the copy.
    buffer.FirstElement = if own { 0 } else { first };
    buffer.CounterOffsetInBytes = 0;
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateUnorderedAccessView(&shadow.buffer, None::<&ID3D12Resource>, Some(&desc), at)
    };
    Some(Kept::Copy(shadow))
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in another format than a
/// typed resource's. A view of a typed texture in another format of its
/// family takes the texture's format.
/// shortcut: such a view (sRGB of UNORM, say) reads without its
/// conversion; cast with castable formats if colors come out wrong.
fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    if family(view) != family(own) {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && !typeless
    {
        *format = d.Format;
    }
    true
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot.
fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    let host = host()?;
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            let shadow = crate::shadow::find(address, u64::from(size))?;
            let gpu = shadow.gpu(address);
            if gpu % 256 != 0 {
                once!(
                    address = format_args!("{address:#x}"),
                    "constant buffer not 256-byte aligned on the host"
                );
                return None;
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: size.next_multiple_of(256),
            };
            // SAFETY: a slot of our heap; the range lies in the host copy.
            unsafe { device.CreateConstantBufferView(Some(&desc), at) };
            Some(Kept::Resource {
                _resource: shadow.buffer.clone(),
            })
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(texture) = crate::resource::host_resource(resource) {
                let mut desc = desc.as_ref().map(srv_desc);
                if !fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return None;
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some(Kept::Resource {
                    _resource: texture.clone(),
                })
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                // A registered resource's copy starts at the resource, so its
                // views always start on an element.
                buffer_srv(data as u64, len, srv_desc(desc), at, false)
            } else {
                None
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(texture) = crate::resource::host_resource(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(data as u64, len, uav_desc(desc), at, false)
                    }
                    _ => None,
                };
            };
            let mut desc = desc.as_ref().map(uav_desc);
            if !fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return None;
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some(Kept::Resource {
                _resource: texture.clone(),
            })
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(Kept::Nothing)
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(Kept::Nothing)
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(view) = placed_texture(view) {
                return make(&view, at);
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                // shortcut: unordered access to a texture the title placed
                // itself is not mirrored; give it a resource as shader
                // views of such textures get when a dispatch writes one.
                once!(dimension = resource[0], "placed unordered texture view not mirrored");
                return None;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            // The view names its own start: it can land off an element.
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at, true)
            } else {
                buffer_srv(address, len, srv_desc(words), at, true)
            }
        }
    }
}

/// A shader view of a texture the title placed in memory itself, as a view
/// of the resource made for that memory (on first use).
pub(crate) fn placed_texture(view: &View) -> Option<View> {
    const BUFFER: u32 = 1;
    let View::Placed(false, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let texture = crate::resource::texture_at(address, resource)?;
    Some(View::Shader(texture as usize, desc[..10].try_into().ok()))
}

/// A null view of the kind `view` is, for a slot whose view the host
/// cannot make: unordered access (to a buffer or a 2D texture) or a shader
/// view. Writes to it are dropped, reads give zeros.
fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    // Unordered views: whether a buffer, and the title's view dimension.
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        // Shader views of buffers read nothing through a null buffer view:
        // a null 2D view where the shader expects a buffer is a mismatch
        // the hardware does not survive in general (GPU-based validation
        // reports it).
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        // Textures: a null view of the title's dimension (1D, arrays, 3D,
        // cubes), not always 2D.
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        View::Constant(..) => {
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
            // SAFETY: a slot of our heap; a zero location makes a null view.
            return unsafe { host.device.CreateConstantBufferView(Some(&desc), at) };
        }
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A null texture UAV of the title's view dimension (1D, 1D or 2D arrays,
/// 3D), 2D if it names none: a 2D null view where the shader writes a 3D
/// texture is a mismatch GPU-based validation reports (the world's wind
/// field, CS_CopyWindFieldBeforeUpdateWithSmooth, 2026-11-24).
fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

/// A buffer view that reads zeros (raw, as translated shaders mostly read
/// buffers).
fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A texture view that reads zeros with the dimension (and mip and array
/// counts) of the title's description `words`; 2D if it names none.
fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    // One mip, one slice or cube, plane 0: fields a null view still checks.
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A view that reads zeros, for slots whose view the host cannot make.
/// shortcut: slots with no view at all (kind unknown) get this null 2D
/// view; null buffer views there did not stop the world's low-address
/// page faults (2026-11-24, run w10), so they are not the cause.
fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    /// A view on an element of its buffer's copy reads that copy; one off its
    /// elements reads a copy of just its bytes, which starts at the view.
    #[test]
    fn a_view_on_its_copy_reads_it_and_one_off_it_gets_its_own() {
        // 56-byte elements at offset 0: the first and third elements.
        assert!(matches!(
            plan(56, 56, 0),
            Copy::Element { first } if first == 0
        ));
        assert!(matches!(
            plan(56, 56, 112),
            Copy::Element { first } if first == 2
        ));
        // The hit-detection SRV: 56-byte elements, 48 bytes in (offset % 56).
        assert!(matches!(plan(3136, 56, 48), Copy::OwnBytes));
        // PreSkinSimNodes: 336-byte elements, 32 bytes in.
        assert!(matches!(plan(18_289_152, 336, 32), Copy::OwnBytes));
        // A raw view (4-byte elements) off by 2 bytes.
        assert!(matches!(plan(64, 4, 2), Copy::OwnBytes));
        // Elements of a format the host has no size for: as before, nothing.
        assert!(matches!(plan(64, 0, 0), Copy::None { .. }));
        // A view of no bytes has nothing to copy.
        assert!(matches!(plan(0, 56, 48), Copy::None { .. }));
    }

    /// An unordered view off an element of its buffer's copy writes a copy of
    /// just its bytes, which a shader view of the same bytes shares; one on
    /// an element writes the buffer's copy. The device takes all of them.
    #[test]
    fn an_unordered_view_off_an_element_writes_its_own_copy() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        // A title buffer the views' memory lies in (its copy is made on the
        // host; title memory is not read).
        let (address, len) = (0x7d00_0000_0000_u64, 0x1_0000_u64);
        crate::shadow::register(usize::MAX - 2, address, len);
        // Memory placed `start` bytes into it, of four 56-byte elements.
        let placed = |unordered: bool, start: u64| {
            let mut resource = [0u32; 14];
            resource[0] = 1; // a buffer
            resource[4] = 4 * 56;
            let mut desc = [0u32; 16];
            desc[1] = 1; // buffer dimension
            if unordered {
                // NumElements, StructureByteStride
                (desc[4], desc[5]) = (4, 56);
            } else {
                desc[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
                (desc[6], desc[7]) = (4, 56);
            }
            let at = address + start;
            (desc[10], desc[11]) = (at as u32, (at >> 32) as u32);
            View::Placed(unordered, resource, desc)
        };
        let buffer = heap.make_at(0, &placed(true, 112));
        let buffer = buffer.expect("on an element: the buffer's copy");
        // The hit-detection offset: 48 bytes in.
        heap.make_at(1, &placed(true, 48));
        heap.make_at(2, &placed(false, 48));
        let uav = heap.copy_at(1);
        let uav = uav.expect("an unordered view off an element: its own copy");
        let srv = heap.copy_at(2);
        let srv = srv.expect("a shader view off an element: its own copy");
        assert!(Arc::ptr_eq(&uav, &srv), "one copy of the same bytes");
        assert!(!Arc::ptr_eq(&uav, &buffer), "not the buffer's copy");
        // SAFETY: a plain device query.
        let removed = unsafe { host.device.GetDeviceRemovedReason() };
        assert!(removed.is_ok(), "device removed: {removed:?}");
        crate::shadow::unregister(usize::MAX - 2, address);
    }

    /// A slot hands its draws the copy its view is of: a view of an outer
    /// title buffer whose bytes lie in an inner registered one keeps the
    /// outer buffer's copy, where looking the view's range up again finds
    /// the inner one's.
    #[test]
    fn a_slot_keeps_the_copy_its_view_is_of() {
        if host().is_none() {
            return; // no host GPU
        }
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let (outer, inner) = (0x7c00_0000_0000_u64, 0x7c00_0000_1000_u64);
        crate::shadow::register(usize::MAX - 3, outer, 0x1_0000);
        crate::shadow::register(usize::MAX - 4, inner, 0x100);
        // A raw view of the outer buffer's memory: 64 elements from element
        // 0x400, which are the inner buffer's bytes.
        let mut resource = [0u32; 14];
        resource[0] = 1; // a buffer
        resource[4] = 0x1_0000;
        let mut desc = [0u32; 16];
        desc[0] = DXGI_FORMAT_R32_TYPELESS.0 as u32;
        desc[1] = 1; // buffer dimension
        desc[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        // FirstElement, NumElements, Flags
        (desc[4], desc[6], desc[8]) = (0x400, 64, D3D12_BUFFER_SRV_FLAG_RAW.0 as u32);
        (desc[10], desc[11]) = (outer as u32, (outer >> 32) as u32);
        let view = View::Placed(false, resource, desc);
        let kept = heap.make_at(0, &view).expect("the slot's copy");
        assert_eq!(buffer_range(&view), Some((inner, 0x100)));
        let outer_copy = crate::shadow::find(outer, 0x1_0000).unwrap();
        let inner_copy = crate::shadow::find(inner, 0x100).unwrap();
        assert!(Arc::ptr_eq(&kept, &outer_copy), "the outer buffer's copy");
        assert!(!Arc::ptr_eq(&kept, &inner_copy), "not the inner one's");
        crate::shadow::unregister(usize::MAX - 4, inner);
        crate::shadow::unregister(usize::MAX - 3, outer);
    }

    /// A nulled-stride list parses decimal and hex, and ignores junk.
    #[cfg(feature = "diagnostics")]
    #[test]
    fn nulled_strides_parse_decimal_and_hex() {
        assert_eq!(parsed_strides("56, 336,0x20"), vec![56, 336, 32]);
        assert_eq!(parsed_strides(" nope , 56"), vec![56]);
        assert!(parsed_strides("").is_empty());
    }

    /// A view's range is its own elements where its descriptor puts them, and
    /// `buffer_range` (what `readable` refreshes) gives the same one, so the
    /// copy `buffer_srv` or `buffer_uav` makes is found again for the refresh.
    #[test]
    fn a_views_range_is_the_one_its_copy_is_keyed_by() {
        // Whatever the words mean, both paths read them the same way: the
        // copy's key and the range `readable` refreshes cannot diverge.
        for words in [
            [0u32, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0u32, 4, 2, 0, 1, 56, 0, 0, 0, 0],
            [0u32, 4, 0, 0, 64, 336, 0, 0, 0, 0],
        ] {
            let mut resource = [0u32; 14];
            resource[0] = 1; // a buffer
            let mut placed = [0u32; 16];
            placed[..10].copy_from_slice(&words);
            placed[10] = 0x1000;
            let range = |(from, bytes, _): (u64, u64, u64)| Some((from, bytes));
            assert_eq!(
                buffer_range(&View::Placed(false, resource, placed)),
                range(shader_extent(0x1000, &srv_desc(&words)))
            );
            assert_eq!(
                buffer_range(&View::Placed(true, resource, placed)),
                range(unordered_extent(0x1000, &uav_desc(&words)))
            );
        }
    }
}
