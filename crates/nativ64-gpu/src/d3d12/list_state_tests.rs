//! Tests of the methods in `list_state.rs`, recorded through the list's vtable.

use crate::d3d12::command::*;
use crate::d3d12::list::{recorded, test_support::*};
use std::ffi::c_void;

#[test]
fn fixed_function_state_is_copied() {
    unsafe {
        let l = new_test_list();
        let mut vps = [[1.0f32, 2.0, 3.0, 4.0, 0.0, 1.0]; 2];
        call!(l, 21, (u32, *const [f32; 6]), 2, vps.as_ptr());
        vps[0][0] = 99.0;
        std::hint::black_box(&vps);
        let mut rects = [[0i32, 1, 2, 3]];
        call!(l, 22, (u32, *const [i32; 4]), 1, rects.as_ptr());
        rects[0][0] = 7;
        std::hint::black_box(&rects);
        call!(l, 22, (u32, *const [i32; 4]), 0, std::ptr::null());
        call!(l, 20, (u32,), 5);
        let bf = [0.5f32; 4];
        call!(l, 23, (*const [f32; 4],), &bf);
        call!(l, 23, (*const [f32; 4],), std::ptr::null());
        call!(l, 24, (u32,), 3);
        call!(l, 25, (*mut c_void,), 0x40 as *mut c_void);
        call!(l, 11, (*mut c_void,), std::ptr::null_mut());
        call!(l, 31, (*mut c_void, u64, u32), 0x50 as *mut c_void, 8, 1);
        call!(l, 70, (f32, f32), 0.25, 0.75);
        assert_eq!(
            recorded(l),
            vec![
                Command::Viewports(vec![[1.0, 2.0, 3.0, 4.0, 0.0, 1.0]; 2]),
                Command::Scissors(vec![[0, 1, 2, 3]]),
                Command::Scissors(vec![]),
                Command::Topology(5),
                Command::BlendFactor(Some([0.5; 4])),
                Command::BlendFactor(None),
                Command::StencilRef(3),
                Command::SetPipeline { pipeline: 0x40 },
                Command::ClearState { pipeline: 0 },
                Command::Predication {
                    buffer: 0x50,
                    offset: 8,
                    op: 1
                },
                Command::DepthBounds {
                    min: 0.25,
                    max: 0.75
                },
            ]
        );
    }
}

#[test]
fn buffers_and_targets() {
    unsafe {
        let l = new_test_list();
        let mut vbs = [VertexView {
            address: 0x1000,
            size: 64,
            stride: 16,
        }];
        call!(l, 48, (u32, u32, *const VertexView), 1, 1, vbs.as_ptr());
        vbs[0].address = 0;
        std::hint::black_box(&vbs);
        call!(l, 48, (u32, u32, *const VertexView), 2, 3, std::ptr::null());
        let ib = IndexView {
            address: 0x2000,
            size: 12,
            format: 57,
        };
        call!(l, 47, (*const IndexView,), &ib);
        call!(l, 47, (*const IndexView,), std::ptr::null());
        let so = [StreamView {
            address: 1,
            size: 2,
            filled_size_address: 3,
        }];
        call!(l, 49, (u32, u32, *const StreamView), 0, 1, so.as_ptr());
        let handles = [0x10u64, 0x20];
        let depth = 0x30u64;
        call!(
            l,
            50,
            (u32, *const u64, i32, *const u64),
            2,
            handles.as_ptr(),
            0,
            &depth
        );
        call!(
            l,
            50,
            (u32, *const u64, i32, *const u64),
            4,
            handles.as_ptr(),
            1,
            std::ptr::null()
        );
        call!(
            l,
            50,
            (u32, *const u64, i32, *const u64),
            0,
            std::ptr::null(),
            0,
            std::ptr::null()
        );
        assert_eq!(
            recorded(l),
            vec![
                Command::VertexBuffers {
                    start: 1,
                    count: 1,
                    views: vec![VertexView {
                        address: 0x1000,
                        size: 64,
                        stride: 16
                    }]
                },
                Command::VertexBuffers {
                    start: 2,
                    count: 3,
                    views: vec![]
                },
                Command::IndexBuffer(Some(ib)),
                Command::IndexBuffer(None),
                Command::StreamOutput {
                    start: 0,
                    count: 1,
                    views: so.to_vec()
                },
                Command::RenderTargets {
                    count: 2,
                    handles: vec![0x10, 0x20],
                    single_range: false,
                    depth: Some(0x30)
                },
                Command::RenderTargets {
                    count: 4,
                    handles: vec![0x10],
                    single_range: true,
                    depth: None
                },
                Command::RenderTargets {
                    count: 0,
                    handles: vec![],
                    single_range: false,
                    depth: None
                },
            ]
        );
    }
}

#[test]
fn heaps_and_root_arguments() {
    unsafe {
        let l = new_test_list();
        let mut heaps = [0x100usize, 0x200];
        call!(l, 32, (u32, *const usize), 2, heaps.as_ptr());
        heaps[0] = 0;
        std::hint::black_box(&heaps);
        call!(l, 32, (u32, *const usize), 0, std::ptr::null());
        call!(l, 33, (*mut c_void,), 0x300 as *mut c_void);
        call!(l, 34, (*mut c_void,), 0x400 as *mut c_void);
        call!(l, 35, (u32, u64), 1, 0xaa);
        call!(l, 36, (u32, u64), 2, 0xbb);
        call!(l, 37, (u32, u32, u32), 3, 7, 1);
        call!(l, 38, (u32, u32, u32), 4, 8, 0);
        let mut vals = [1u32, 2, 3];
        call!(l, 39, (u32, u32, *const u32, u32), 5, 3, vals.as_ptr(), 2);
        vals[0] = 0;
        std::hint::black_box(&vals);
        call!(
            l,
            40,
            (u32, u32, *const u32, u32),
            6,
            0,
            std::ptr::null(),
            0
        );
        call!(l, 41, (u32, u64), 0, 0x10);
        call!(l, 42, (u32, u64), 0, 0x20);
        call!(l, 43, (u32, u64), 0, 0x30);
        call!(l, 44, (u32, u64), 0, 0x40);
        call!(l, 45, (u32, u64), 0, 0x50);
        call!(l, 46, (u32, u64), 0, 0x60);
        let r = recorded(l);
        assert_eq!(r[0], Command::Heaps(vec![0x100, 0x200]));
        assert_eq!(r[1], Command::Heaps(vec![]));
        assert_eq!(
            r[2],
            Command::RootSignature {
                compute: true,
                signature: 0x300
            }
        );
        assert_eq!(
            r[3],
            Command::RootSignature {
                compute: false,
                signature: 0x400
            }
        );
        assert_eq!(
            r[4],
            Command::RootTable {
                compute: true,
                slot: 1,
                handle: 0xaa
            }
        );
        assert_eq!(
            r[5],
            Command::RootTable {
                compute: false,
                slot: 2,
                handle: 0xbb
            }
        );
        assert_eq!(
            r[6],
            Command::RootConstants {
                compute: true,
                slot: 3,
                offset: 1,
                values: vec![7]
            }
        );
        assert_eq!(
            r[7],
            Command::RootConstants {
                compute: false,
                slot: 4,
                offset: 0,
                values: vec![8]
            }
        );
        assert_eq!(
            r[8],
            Command::RootConstants {
                compute: true,
                slot: 5,
                offset: 2,
                values: vec![1, 2, 3]
            }
        );
        assert_eq!(
            r[9],
            Command::RootConstants {
                compute: false,
                slot: 6,
                offset: 0,
                values: vec![]
            }
        );
        let kinds = [
            (true, RootView::Cbv, 0x10),
            (false, RootView::Cbv, 0x20),
            (true, RootView::Srv, 0x30),
            (false, RootView::Srv, 0x40),
            (true, RootView::Uav, 0x50),
            (false, RootView::Uav, 0x60),
        ];
        for (i, (compute, v, address)) in kinds.into_iter().enumerate() {
            assert_eq!(
                r[10 + i],
                Command::RootView {
                    compute,
                    slot: 0,
                    view: v,
                    address
                }
            );
        }
    }
}
