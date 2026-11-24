//! Indirect execution's arguments, bounded before the GPU reads them. The
//! title's own compute passes write most of them; a pass we drop or
//! translate wrongly leaves whatever the memory held, and a draw of four
//! billion vertices or a dispatch past the host's limits runs until the
//! host resets the display driver. So every record is clamped on the GPU,
//! just before its execution, by a compute shader of ours (HLSL compiled by
//! the system's compiler as a 5.1 shader; its root signature is built here,
//! since Wine's compiler does not embed one).
//!
//! Each list also has a budget (2^27 vertex instances or threads, groups
//! counted as 64 threads, mesh groups as 128): records past it run empty,
//! so a list of thousands of garbage records cannot run for seconds
//! either.
//!
//! shortcut: the bounds (2^24 vertices, 2^26 vertex instances, 65,535
//! groups per dimension and 2^20 per dispatch, one-dimensional mesh
//! dispatches, the budget) cut legitimate work beyond them; raise them if
//! the title needs more.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use crate::host::{HostList, host, transition};

// Written for both compilers `D3DCompile` may be: Microsoft's and, under
// Wine, vkd3d-shader's, which has no `RWByteAddressBuffer` methods. So the
// buffers are structured buffers of words (a root UAV takes either).
const SOURCE: &str = r#"
cbuffer Record : register(b0) { uint kind; uint stride; uint records; uint unused; };
RWStructuredBuffer<uint> args : register(u0);
// The list's work so far (vertex instances, threads): past the budget,
// records run empty.
RWStructuredBuffer<uint> budget : register(u1);
static const uint BUDGET = 1u << 27;
// Groups of one dispatch; more run empty.
static const uint GROUPS = 1u << 20;
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= records) return;
    uint at = id.x * (stride / 4);
    uint cost;
    if (kind <= 1) {
        // Draws: vertex or index count, then instances.
        uint count = min(args[at], 1u << 24);
        uint instances = min(args[at + 1], (1u << 26) / max(count, 1u));
        cost = count * instances;
        args[at] = count;
        args[at + 1] = instances;
    } else if (kind == 2) {
        uint3 g = min(uint3(args[at], args[at + 1], args[at + 2]), uint3(65535, 65535, 65535));
        // x * y fits 32 bits; the whole count may not.
        if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
            g = uint3(0, 0, 0);
        }
        cost = g.x * g.y * g.z * 64;
        args[at] = g.x;
        args[at + 1] = g.y;
        args[at + 2] = g.z;
    } else {
        // Mesh dispatches: the console's are one-dimensional.
        uint3 g = uint3(min(args[at], 65535u), min(args[at + 1], 1u), min(args[at + 2], 1u));
        cost = g.x * g.y * g.z * 128;
        args[at] = g.x;
        args[at + 1] = g.y;
        args[at + 2] = g.z;
    }
    uint before;
    InterlockedAdd(budget[0], cost, before);
    if (before >= BUDGET || before + cost > BUDGET) {
        // Draws keep their layout with no vertices; groups become zero.
        args[at] = 0;
    }
}
"#;

struct Clamp {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Clamp {}
// SAFETY: as above.
unsafe impl Sync for Clamp {}

fn clamp() -> Option<&'static Clamp> {
    static CLAMP: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    CLAMP
        .get_or_init(|| match make() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no argument clamp: indirect execution dropped");
                None
            }
        })
        .as_ref()
}

fn make() -> windows::core::Result<Clamp> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("clamp"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize())
            };
            tracing::warn!("argument clamp: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    let root = root_signature(device)?;
    // SAFETY: the blob's own bytes.
    unsafe {
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso })
    }
}

/// Constants b0 (four words: kind, stride, records, unused), then the
/// arguments as root UAV u0 and the budget as root UAV u1.
fn root_signature(device: &ID3D12Device) -> windows::core::Result<ID3D12RootSignature> {
    let uav = |register| D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: register, RegisterSpace: 0 },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    let params = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 4 },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        uav(0),
        uav(1),
    ];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: params.len() as u32,
        pParameters: params.as_ptr(),
        NumStaticSamplers: 0,
        pStaticSamplers: std::ptr::null(),
        Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };
    // SAFETY: the description points at a live array; the blob is read in
    // full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)?;
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes = std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        device.CreateRootSignature(0, bytes)
    }
}

/// Records, in `list`, the clamp of `records` records of `kind` (PC
/// argument numbering) `stride` bytes apart at host address `gpu` in
/// `buffer`, and leaves the buffer ready to be read as arguments. Replaces
/// the list's compute root signature and pipeline (callers bind theirs
/// after). False if the clamp is not available.
pub(crate) fn bound(
    list: &HostList,
    buffer: &ID3D12Resource,
    gpu: u64,
    (kind, stride, records): (u32, u32, u32),
) -> bool {
    let (Some(c), Some(budget)) = (clamp(), list.budget()) else {
        return false;
    };
    if !gpu.is_multiple_of(4) || !stride.is_multiple_of(4) {
        return false;
    }
    let l = &list.list;
    let constants = [kind, stride, records, 0];
    // SAFETY: an open list; our root signature, pipeline and a live buffer
    // the caller's list holds.
    unsafe {
        // Earlier shader writes (the title's argument passes) first.
        l.ResourceBarrier(&[uav_barrier()]);
        l.SetComputeRootSignature(&c.root);
        l.SetPipelineState(&c.pso);
        l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
        l.SetComputeRootUnorderedAccessView(1, gpu);
        l.SetComputeRootUnorderedAccessView(2, budget);
        l.Dispatch(records.div_ceil(64), 1, 1);
        l.ResourceBarrier(&[
            uav_barrier(),
            transition(
                buffer,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            ),
        ]);
    }
    true
}

/// After the execution: the buffer back to unordered access, where the
/// title's passes keep using it.
pub(crate) fn done(list: &HostList, buffer: &ID3D12Resource) {
    // SAFETY: an open list and a live buffer.
    unsafe {
        list.list.ResourceBarrier(&[transition(
            buffer,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )])
    };
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
        },
    }
}

#[cfg(test)]
mod tests {
    /// The clamp compiles with the system's compiler and the host takes it.
    #[test]
    fn argument_clamp_builds() {
        if super::host().is_none() {
            return; // no host GPU here
        }
        assert!(super::clamp().is_some());
    }

    /// The clamp bounds a draw, a dispatch and a mesh dispatch on the GPU.
    #[test]
    fn arguments_are_clamped_on_the_gpu() {
        clamp_on_the_gpu(false);
    }

    /// The same in a list with the title's kind of descriptor heaps bound
    /// (under vkd3d-proton the clamp then faulted on address 0).
    #[test]
    fn arguments_are_clamped_with_heaps_bound() {
        clamp_on_the_gpu(true);
    }

    fn clamp_on_the_gpu(heaps: bool) {
        use super::*;
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let (buffer, data) = host.shared_buffer(256).unwrap();
        let words = data.cast::<u32>();
        let records: [(usize, [u32; 4]); 3] = [
            (0, [u32::MAX, 5, 0, 0]), // draw: vertices, instances
            (16, [70_000, 2, 3, 0]),  // dispatch
            (32, [100_000, 7, 9, 0]), // mesh dispatch
        ];
        for (at, w) in records {
            // SAFETY: inside the mapped 256 bytes.
            unsafe { std::ptr::copy_nonoverlapping(w.as_ptr(), words.add(at / 4), 4) };
        }
        let list = host.new_list().unwrap();
        if heaps {
            let heap = |kind, count| -> ID3D12DescriptorHeap {
                // SAFETY: plain heap creation.
                unsafe {
                    host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
                }
                .unwrap()
            };
            let views = heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1024);
            let samplers = heap(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, 16);
            // SAFETY: an open list and shader-visible heaps it keeps alive.
            unsafe { list.list.SetDescriptorHeaps(&[Some(views.clone()), Some(samplers.clone())]) };
            list.keep(&views);
            list.keep(&samplers);
        }
        // SAFETY: a live buffer and an open list.
        let gpu = unsafe {
            list.list.ResourceBarrier(&[transition(
                &buffer,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
            buffer.GetGPUVirtualAddress()
        };
        for (kind, at, stride) in [(0, 0, 16), (2, 16, 12), (3, 32, 12)] {
            assert!(bound(&list, &buffer, gpu + at, (kind, stride, 1)));
            done(&list, &buffer);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: the list ran (execute waits); inside the mapped bytes.
        let read = |at: usize| unsafe { std::slice::from_raw_parts(words.add(at / 4), 3).to_vec() };
        assert_eq!(read(0)[..2], [1 << 24, 4]);
        assert_eq!(read(16), [65_535, 2, 3]);
        assert_eq!(read(32), [65_535, 1, 1]);
    }
}
