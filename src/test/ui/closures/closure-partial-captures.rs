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

fn ref_imm(_arg: &usize) {}
fn ref_mut(_arg: &mut usize) {}

#[rustc_dump_closure_captures]
fn interior_simple() {
    let interior: A = Default::default();
    let _c = || {
        // -^ NOTE Upvar local interior CapturePath([Field(a1), Field(b1), Field(d)]): ByRef immutable
        interior.a1.b1.d + 1
    };
}

#[rustc_dump_closure_captures]
fn interior_full_immutable() {
    let interior: A = Default::default();
    let _c = || {
        // -^ NOTE Upvar local interior CapturePath([Field(a2)]): ByRef immutable
        interior.a2.c1.d + interior.a2.c2.d
    };
}

#[rustc_dump_closure_captures]
fn interior_full_mutable() {
    let mut interior1: A = Default::default();
    let mut interior2: A = Default::default();
    let _c1 = || {
        // -^ NOTE Upvar local interior1 CapturePath([Field(a2)]): ByRef mutable
        ref_imm(&interior1.a2.c1.d);
        ref_mut(&mut interior1.a2.c2.d);
    };
    let _c2 = || {
        // -^ NOTE Upvar local interior1 CapturePath([Field(a2)]): ByRef mutable
        ref_mut(&mut interior2.a2.c2.d);
        ref_imm(&interior2.a2.c1.d);
    };
}

#[rustc_dump_closure_captures]
fn capture_borrow() {
    let cb = 6;
    let _c = || {
        // -^ NOTE Upvar local cb CapturePath([]): ByRef immutable
        ref_imm(&cb);
    };
}

#[rustc_dump_closure_captures]
fn capture_borrow_mut() {
    let mut cbm = 6;
    let _c = || {
        // -^ NOTE Upvar local cbm CapturePath([]): ByRef mutable
        cbm += 1
    };
}

#[rustc_dump_closure_captures]
fn capture_borrow_unique() {
    let mut target = 7;
    let cbu = &mut target;
    let _c = || {
        // -^ NOTE Upvar local cbu CapturePath([]): ByRef unique immutable
        *cbu += 1
    };
}

#[rustc_dump_closure_captures]
fn capture_move() {
    let cm = 7;
    let _c = move || {
        // -^ NOTE Upvar local cm CapturePath([]): ByValue
        cm + 1
    };
}

#[rustc_dump_closure_captures]
fn capture_move_mut() {
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

// The UpvarCapture can be
// - by value
// - by ref, for which the borrow kind may be
//   - Immutable
//   - Unique Immutable
//   - Mutable
fn main() {
    interior_simple();
    // interior_full_immutable();
    // interior_full_mutable();
    // capture_borrow();
    // capture_borrow_mut();
    // capture_borrow_unique();
    // capture_move();
    // capture_move_mut();
    // capture_pat();
}
