//! Tests of the methods in `list_work.rs`, recorded through the list's vtable.

use crate::d3d12::command::*;
use crate::d3d12::list::{recorded, test_support::*};
use std::ffi::c_void;

unsafe fn raw_barrier(kind: u32, flags: u32, a: u64, b: u64, c: u64) -> [u8; 32] {
    let mut raw = [0u8; 32];
    raw[0..4].copy_from_slice(&kind.to_le_bytes());
    raw[4..8].copy_from_slice(&flags.to_le_bytes());
    raw[8..16].copy_from_slice(&a.to_le_bytes());
    raw[16..24].copy_from_slice(&b.to_le_bytes());
    raw[24..32].copy_from_slice(&c.to_le_bytes());
    raw
}

#[test]
fn draws_and_dispatches() {
    unsafe {
        let l = new_test_list();
        call!(l, 12, (u32, u32, u32, u32), 3, 2, 1, 0);
        call!(l, 13, (u32, u32, u32, i32, u32), 6, 1, 2, -4, 9);
        call!(l, 14, (u32, u32, u32), 1, 2, 3);
        call!(l, 80, (u32, u32, u32, u32), 4, 5, 6, 7);
        call!(l, 129, (u32, u32, u32), 8, 1, 1);
        call!(l, 100, (u32, u64, u64, u64), 2, 3, 4, 5);
        call!(
            l,
            61,
            (*mut c_void, u32, *mut c_void, u64, *mut c_void, u64),
            0x10 as *mut c_void,
            8,
            0x20 as *mut c_void,
            64,
            std::ptr::null_mut(),
            0
        );
        assert_eq!(
            recorded(l),
            vec![
                Command::Draw {
                    vertices: 3,
                    instances: 2,
                    first_vertex: 1,
                    first_instance: 0
                },
                Command::DrawIndexed {
                    indices: 6,
                    instances: 1,
                    first_index: 2,
                    base_vertex: -4,
                    first_instance: 9
                },
                Command::Dispatch {
                    x: 1,
                    y: 2,
                    z: 3,
                    flags: 0
                },
                Command::Dispatch {
                    x: 4,
                    y: 5,
                    z: 6,
                    flags: 7
                },
                Command::DispatchMesh { x: 8, y: 1, z: 1 },
                Command::DispatchMeshX {
                    x: 2,
                    flags: 3,
                    indirect: 4,
                    payload: 5
                },
                Command::ExecuteIndirect {
                    signature: 0x10,
                    max: 8,
                    arguments: 0x20,
                    offset: 64,
                    count: 0,
                    count_offset: 0
                },
            ]
        );
    }
}

#[test]
fn copies_and_memory_operations() {
    unsafe {
        let l = new_test_list();
        let r = 0x11 as *mut c_void;
        let s = 0x22 as *mut c_void;
        call!(
            l,
            15,
            (*mut c_void, u64, *mut c_void, u64, u64),
            r,
            1,
            s,
            2,
            3
        );
        call!(l, 17, (*mut c_void, *mut c_void), r, s);
        call!(l, 86, (*mut c_void, *mut c_void, u32), r, s, 5);
        call!(
            l,
            19,
            (*mut c_void, u32, *mut c_void, u32, u32),
            r,
            1,
            s,
            2,
            28
        );
        call!(l, 62, (u64, u64, u64, u32), 0x1000, 0x2000, 16, 1);
        call!(l, 63, (u64, u64, u32), 0x1000, 8, 0xdead_beef);
        call!(l, 64, (u64, u64, u64), 0x1000, 16, 0x0102_0304_0506_0708);
        let v = [7u8; 16];
        call!(l, 65, (u64, u64, *const [u8; 16]), 0x1000, 32, &v);
        call!(
            l,
            65,
            (u64, u64, *const [u8; 16]),
            0x1000,
            32,
            std::ptr::null()
        );
        call!(l, 71, (u64, u32, u32, u32), 0x40, 9, 1, 0);
        call!(l, 72, (u64, u64, u32, u32), 0x48, 10, 0, 1);
        call!(l, 73, (u64, u32, u32, u32, u32), 0x50, 2, 11, 0xff, 0);
        call!(l, 74, (u64, u32, u64, u32), 0x58, 3, 12, 0);

        let mut loc = [0u8; 48];
        loc[0..8].copy_from_slice(&0x33u64.to_le_bytes());
        loc[8..12].copy_from_slice(&1u32.to_le_bytes());
        loc[16..24].copy_from_slice(&0x400u64.to_le_bytes());
        for (i, v) in [28u32, 64, 32, 1, 256].iter().enumerate() {
            loc[24 + 4 * i..28 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        let mut sub = [0u8; 48];
        sub[0..8].copy_from_slice(&0x44u64.to_le_bytes());
        sub[16..20].copy_from_slice(&3u32.to_le_bytes());
        let mut bx = [1u32, 2, 3, 4, 5, 6];
        call!(
            l,
            16,
            (*const u8, u32, u32, u32, *const u8, *const [u32; 6]),
            sub.as_ptr(),
            1,
            2,
            3,
            loc.as_ptr(),
            &bx
        );
        bx[0] = 0;
        std::hint::black_box(&bx);
        loc[0] = 0;
        std::hint::black_box(&loc);
        call!(
            l,
            85,
            (*const u8, u32, u32, u32, *const u8, *const [u32; 6], u32),
            sub.as_ptr(),
            0,
            0,
            0,
            sub.as_ptr(),
            std::ptr::null(),
            9
        );
        // A missing location records nothing.
        call!(
            l,
            16,
            (*const u8, u32, u32, u32, *const u8, *const [u32; 6]),
            std::ptr::null(),
            0,
            0,
            0,
            sub.as_ptr(),
            std::ptr::null()
        );

        let rec = recorded(l);
        assert_eq!(
            rec.len(),
            14,
            "the copy without a destination records nothing"
        );
        assert_eq!(
            rec[0],
            Command::CopyBufferRegion {
                dst: 0x11,
                dst_offset: 1,
                src: 0x22,
                src_offset: 2,
                len: 3
            }
        );
        assert_eq!(
            rec[1],
            Command::CopyResource {
                dst: 0x11,
                src: 0x22,
                flags: 0
            }
        );
        assert_eq!(
            rec[2],
            Command::CopyResource {
                dst: 0x11,
                src: 0x22,
                flags: 5
            }
        );
        assert_eq!(
            rec[3],
            Command::ResolveSubresource {
                dst: 0x11,
                dst_sub: 1,
                src: 0x22,
                src_sub: 2,
                format: 28
            }
        );
        assert_eq!(
            rec[4],
            Command::CopyMemory {
                dst: 0x1000,
                src: 0x2000,
                len: 16,
                flags: 1
            }
        );
        let mut p = [0u8; 16];
        p[..4].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        assert_eq!(
            rec[5],
            Command::FillMemory {
                address: 0x1000,
                size: 8,
                pattern: p,
                width: 4
            }
        );
        assert!(matches!(
            rec[6],
            Command::FillMemory {
                width: 8,
                size: 16,
                ..
            }
        ));
        assert_eq!(
            rec[7],
            Command::FillMemory {
                address: 0x1000,
                size: 32,
                pattern: v,
                width: 16
            }
        );
        assert_eq!(
            rec[8],
            Command::WriteBottom {
                address: 0x40,
                value: 9,
                wide: false,
                flush: 1,
                flags: 0
            }
        );
        assert_eq!(
            rec[9],
            Command::WriteBottom {
                address: 0x48,
                value: 10,
                wide: true,
                flush: 0,
                flags: 1
            }
        );
        assert_eq!(
            rec[10],
            Command::WaitValue {
                address: 0x50,
                comparison: 2,
                reference: 11,
                mask: 0xff,
                wide: false
            }
        );
        assert_eq!(
            rec[11],
            Command::WaitValue {
                address: 0x58,
                comparison: 3,
                reference: 12,
                mask: u64::MAX,
                wide: true
            }
        );
        assert_eq!(
            rec[12],
            Command::CopyTextureRegion {
                dst: CopyLocation::Subresource {
                    resource: 0x44,
                    index: 3
                },
                x: 1,
                y: 2,
                z: 3,
                src: CopyLocation::Footprint {
                    resource: 0x33,
                    offset: 0x400,
                    format: 28,
                    width: 64,
                    height: 32,
                    depth: 1,
                    row_pitch: 256
                },
                source_box: Some([1, 2, 3, 4, 5, 6]),
                flags: 0,
            }
        );
        assert!(matches!(
            rec[13],
            Command::CopyTextureRegion {
                source_box: None,
                flags: 9,
                ..
            }
        ));
    }
}

#[test]
fn barriers_clears_discards_and_queries() {
    unsafe {
        let l = new_test_list();
        let mut b = [
            raw_barrier(0, 1, 0x11, 3 | (4 << 32), 0x80),
            raw_barrier(1, 0, 0x22, 0x33, 0),
            raw_barrier(2, 0, 0x44, 0, 0),
            raw_barrier(9, 2, 0, 0, 0),
        ];
        assert_eq!(
            call!(l, 26, (u32, *const [u8; 32]) -> u32, 4, b.as_ptr()),
            0
        );
        b[0][8] = 0;
        std::hint::black_box(&b);
        call!(l, 26, (u32, *const [u8; 32]) -> u32, 0, std::ptr::null());

        let mut rects = [[1i32, 2, 3, 4]];
        let color = [1.0f32, 0.5, 0.25, 0.0];
        call!(
            l,
            52,
            (u64, *const [f32; 4], u32, *const Rect),
            0x70,
            &color,
            1,
            rects.as_ptr()
        );
        rects[0][0] = 0;
        std::hint::black_box(&rects);
        call!(
            l,
            52,
            (u64, *const [f32; 4], u32, *const Rect),
            0x70,
            std::ptr::null(),
            0,
            std::ptr::null()
        );
        call!(
            l,
            51,
            (u64, u32, f32, u8, u32, *const Rect),
            0x78,
            3,
            1.0,
            7,
            0,
            std::ptr::null()
        );
        let vals = [1u32, 2, 3, 4];
        call!(
            l,
            53,
            (u64, u64, *mut c_void, *const [u32; 4], u32, *const Rect),
            0x1,
            0x2,
            0x3 as *mut c_void,
            &vals,
            0,
            std::ptr::null()
        );
        call!(
            l,
            54,
            (u64, u64, *mut c_void, *const [u32; 4], u32, *const Rect),
            0x1,
            0x2,
            0x3 as *mut c_void,
            std::ptr::null(),
            0,
            std::ptr::null()
        );
        call!(
            l,
            54,
            (u64, u64, *mut c_void, *const [u32; 4], u32, *const Rect),
            0x1,
            0x2,
            0x3 as *mut c_void,
            &vals,
            0,
            std::ptr::null()
        );

        let r2 = [[5i32, 6, 7, 8]];
        let region: [usize; 3] = [1, r2.as_ptr() as usize, 2 | (3 << 32)];
        call!(
            l,
            55,
            (*mut c_void, *const [usize; 3]),
            0x55 as *mut c_void,
            &region
        );
        call!(
            l,
            55,
            (*mut c_void, *const [usize; 3]),
            0x56 as *mut c_void,
            std::ptr::null()
        );

        let h = 0x60 as *mut c_void;
        call!(l, 28, (*mut c_void, u32, u32), h, 0, 4);
        call!(l, 29, (*mut c_void, u32, u32), h, 0, 4);
        call!(
            l,
            30,
            (*mut c_void, u32, u32, u32, *mut c_void, u64),
            h,
            2,
            1,
            2,
            0x61 as *mut c_void,
            16
        );
        // Markers, events and hints do nothing.
        call!(l, 56, (u32, *const u8, u32), 0, std::ptr::null(), 0);
        call!(l, 57, (u32, *const u8, u32), 0, std::ptr::null(), 0);
        call!(l, 58, (),);
        call!(l, 75, (u32,), 1);
        call!(l, 126, (u32, *const u32), 0, std::ptr::null());
        call!(l, 142, (u64,), 0);
        assert_eq!(call!(l, 101, (u64, u64) -> u64, 1, 2), 0);

        let rec = recorded(l);
        assert_eq!(rec.len(), 11);
        assert_eq!(
            rec[0],
            Command::Barriers(vec![
                Barrier::Transition {
                    flags: 1,
                    resource: 0x11,
                    subresource: 3,
                    before: 4,
                    after: 0x80
                },
                Barrier::Aliasing {
                    flags: 0,
                    before: 0x22,
                    after: 0x33
                },
                Barrier::Uav {
                    flags: 0,
                    resource: 0x44
                },
                Barrier::Other { kind: 9, flags: 2 },
            ])
        );
        assert_eq!(rec[1], Command::Barriers(vec![]));
        assert_eq!(
            rec[2],
            Command::ClearRenderTarget {
                handle: 0x70,
                color,
                rects: vec![[1, 2, 3, 4]]
            }
        );
        assert_eq!(
            rec[3],
            Command::ClearDepthStencil {
                handle: 0x78,
                flags: 3,
                depth: 1.0,
                stencil: 7,
                rects: vec![]
            }
        );
        assert_eq!(
            rec[4],
            Command::ClearUav {
                float: false,
                gpu_handle: 1,
                cpu_handle: 2,
                resource: 3,
                values: vals,
                rects: vec![]
            }
        );
        assert!(matches!(rec[5], Command::ClearUav { float: true, .. }));
        assert_eq!(
            rec[6],
            Command::Discard {
                resource: 0x55,
                rects: vec![[5, 6, 7, 8]],
                first_subresource: 2,
                subresources: 3
            }
        );
        assert_eq!(
            rec[7],
            Command::Discard {
                resource: 0x56,
                rects: vec![],
                first_subresource: 0,
                subresources: 0
            }
        );
        assert_eq!(
            rec[8],
            Command::BeginQuery {
                heap: 0x60,
                kind: 0,
                index: 4
            }
        );
        assert_eq!(
            rec[9],
            Command::EndQuery {
                heap: 0x60,
                kind: 0,
                index: 4
            }
        );
        assert_eq!(
            rec[10],
            Command::ResolveQueries {
                heap: 0x60,
                kind: 2,
                first: 1,
                count: 2,
                buffer: 0x61,
                offset: 16
            }
        );
    }
}

#[test]
fn upload_memory_and_enhanced_barriers() {
    unsafe {
        let l = new_test_list();
        let p = call!(l, 148, (u64, u64) -> *mut u8, 100, 256);
        assert!(!p.is_null());
        assert_eq!(p as usize % 256, 0);
        std::ptr::write_bytes(p, 0xab, 100);
        assert!(call!(l, 148, (u64, u64) -> *mut u8, 100, 24).is_null());
        assert!(call!(l, 148, (u64, u64) -> *mut u8, u64::MAX, 16).is_null());
        let mut groups = [[2u32, 3, 0, 0], [0, 1, 0, 0]];
        call!(l, 146, (u32, *const [u32; 4]), 2, groups.as_ptr());
        groups[0][0] = 9;
        std::hint::black_box(&groups);
        assert_eq!(
            recorded(l),
            vec![Command::BarrierGroups(vec![(2, 3), (0, 1)])]
        );
    }
}
