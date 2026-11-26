//! Xbox Direct3D 12 objects, one module per object kind.

/// Test helper: calls vtable slot `$slot` of list `$l` with argument types `($t..)`.
#[cfg(test)]
macro_rules! call {
    ($l:expr, $slot:expr, ($($t:ty),* $(,)?), $($a:expr),* $(,)?) => {
        call!($l, $slot, ($($t),*) -> (), $($a),*)
    };
    ($l:expr, $slot:expr, ($($t:ty),* $(,)?) -> $r:ty, $($a:expr),* $(,)?) => {{
        let vtbl = *($l as *const *const *const ());
        let f: unsafe extern "win64" fn(*mut std::ffi::c_void, $($t),*) -> $r =
            std::mem::transmute(*vtbl.add($slot));
        f($l, $($a),*)
    }};
}

pub mod allocator;
pub mod blob;
pub mod command;
pub mod descriptor;
pub mod device;
pub mod fence;
pub mod frame;
pub mod info;
pub mod list;
pub mod list_record;
pub mod list_state;
#[cfg(test)]
mod list_state_tests;
pub mod list_work;
#[cfg(test)]
mod list_work_tests;
pub mod object;
pub mod pipeline;
pub mod pipeline_state;
pub mod present;
pub mod query;
pub mod queue;
pub mod resource;
pub mod rootobj;
pub mod rootsig;
pub mod signature;
