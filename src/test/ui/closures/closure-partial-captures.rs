#![feature(thread_local)]
// compile-pass
// ignore-test
#![feature(rustc_attrs)]

#[derive(Default)]
struct D {
    d: usize,
}

#[derive(Default)]
struct B {
    b1: D,
}

#[derive(Default)]
struct C {
    c1: D,
    c2: D,
}

#[derive(Default)]
struct A {
    a1: B,
    a2: C,
}

fn ref_imm<T>(_arg: &T) {}
fn ref_mut<T>(_arg: &mut T) {}

#[rustc_dump_closure_captures]
fn interior_simple() {
    let interior: A = Default::default();
    let _c = || {
        // -^ NOTE Upvar local interior CapturePath([Field(a1), Field(b1), Field(d)]): ByRef immutable
        interior.a1.b1.d + 1
    };
}

#[rustc_dump_closure_captures]
fn interior_byref_full_immutable_merge() {
    let interior: A = Default::default();
    let _c = || {
        // -^ NOTE Upvar local interior CapturePath([Field(a2)]): ByRef immutable
        interior.a2.c1.d + interior.a2.c2.d
    };
}

#[rustc_dump_closure_captures]
fn interior_byref_subpath_merge() {
    let mut interior1: A = Default::default();
    let mut interior2: A = Default::default();
    // If
    // - we are capturing paths A and B by reference and
    // - A is a prefix of B,
    // then we want to be capturing A by ref, with a borrow kind that
    // is appropriate for both.
    let _c1 = || {
        // -^ NOTE Upvar local interior1 CapturePath([Field(a2), Field(c2)]): ByRef mutable
        ref_imm(&interior1.a2.c2);
        ref_mut(&mut interior1.a2.c2.d);
    };
    let _c2 = || {
        // -^ NOTE Upvar local interior1 CapturePath([Field(a2), Field(c2)]): ByRef mutable
        ref_mut(&mut interior2.a2.c2.d);
        ref_imm(&interior2.a2.c2);
    };
}

#[rustc_dump_closure_captures]
fn interior_byref_no_merge_incompatible() {
    let mut interior1: A = Default::default();
    let mut interior2: A = Default::default();
    // If
    // - we're capturing pathA/fld1 and pathA/fld2 by ref and
    // - fld1 and fld2 are the only fields in the struct,
    // - but the two captures are not of the same borrow kind
    // then we do not want to merge; otherwise the capture would
    // no longer be minimal.
    let _c1 = || {
        // -^ NOTE Upvar local interior1 CapturePath([Field(a2), Field(c2)]): ByRef mutable
        ref_imm(&interior1.a2.c1.d);
        ref_mut(&mut interior1.a2.c2.d);
    };
    let _c2 = || {
        // -^ NOTE Upvar local interior1 CapturePath([Field(a2), Field(c2)]): ByRef mutable
        ref_mut(&mut interior2.a2.c2.d);
        ref_imm(&interior2.a2.c2);
    };
}

#[rustc_dump_closure_captures]
fn empty_path_borrow() {
    let cb = 6;
    let _c = || {
        // -^ NOTE Upvar local cb CapturePath([]): ByRef immutable
        ref_imm(&cb);
    };
}

#[rustc_dump_closure_captures]
fn empty_path_borrow_mut() {
    let mut cbm = 6;
    let _c = || {
        // -^ NOTE Upvar local cbm CapturePath([]): ByRef mutable
        cbm += 1
    };
}

#[rustc_dump_closure_captures]
fn empty_path_borrow_unique() {
    let mut target = 7;
    let cbu = &mut target;
    let _c = || {
        // -^ NOTE Upvar local cbu CapturePath([]): ByRef unique immutable
        *cbu += 1
    };
}

#[rustc_dump_closure_captures]
fn empty_path_move() {
    let cm = 7;
    let _c = move || {
        // -^ NOTE Upvar local cm CapturePath([]): ByValue
        cm + 1
    };
}

#[rustc_dump_closure_captures]
fn empty_path_move_mut() {
    let mut cmm = 8;
    let _c = move || {
        // -^ NOTE Upvar local cb CapturePath([]): ByValue
        cmm += 1
    };
}

#[rustc_dump_closure_captures]
fn capture_pat() {
    let interior1: A = Default::default();
    let _c = || {
        // -^ NOTE Upvar local interior1 CapturePath([Field(a1)]): ByValue
        match interior1 {
            A {a1, ..} => { // This hits `upvar::consume_pat`
                ref_imm(&a1.b1.d);
            }
        }
    };
    let interior2: A = Default::default();
    let _c = || {
        // -^ NOTE Upvar local interior2 CapturePath([Field(a2)]): ByRef immutable
        match interior2 {
            A {ref a2, ..} => {
                ref_imm(&a2.c1.d);
            }
        }
    };
    let mut interior3: A = Default::default();
    let _c = || {
        // -^ NOTE Upvar local mut interior3 CapturePath([Field(a1)]): ByRef mutable
        match interior3 {
            A {ref mut a1, ..} => {
                ref_mut(&mut a1.b1.d);
            }
        }
    };
}

static GLOBAL: usize = 7;

#[rustc_dump_closure_captures]
fn no_capture_static() {
    let _ = || {
        ref_imm(&GLOBAL); // Should not be captured
    };
}

use std::cell::RefCell;

#[thread_local]
static TLS: RefCell<u32> = RefCell::new(1);

#[rustc_dump_closure_captures]
fn no_capture_thread_local() {
    let _ = || {
        ref_imm(&TLS); // Should not be captured
    };
}

#[rustc_dump_closure_captures]
fn no_capture_local() {
    let _ = || {
        let x = 7; // Should not be captured
        ref_imm(&x)
    };
}

// The UpvarCapture can be
// - by value
// - by ref, for which the borrow kind may be
//   - Immutable
//   - Unique Immutable
//   - Mutable
fn main() {
    interior_simple();
    interior_byref_full_immutable_merge();
    interior_byref_subpath_merge();
    interior_byref_no_merge_incompatible();
    empty_path_borrow();
    empty_path_borrow_mut();
    empty_path_borrow_unique();
    empty_path_move();
    empty_path_move_mut();
    capture_pat();
    no_capture_static();
    no_capture_thread_local();
    no_capture_local();
}
