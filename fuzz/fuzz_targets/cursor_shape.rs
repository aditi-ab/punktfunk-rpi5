//! A host's cursor shape as the client reads it. `core_wire` reaches this decoder only through
//! a well-formed frame header, which the fuzzer rarely builds unseeded; here the body is the
//! input. The pixel invariant every client renderer trusts, `rgba.len() == w * h * 4`, is
//! asserted on the way out.
#![no_main]

use libfuzzer_sys::fuzz_target;
use punktfunk_core::quic::v2::msg::V2Message;
use punktfunk_core::quic::CursorShape;

fuzz_target!(|data: &[u8]| {
    let Ok(shape) = CursorShape::from_body(data) else {
        return;
    };
    assert!(shape.w > 0 && shape.h > 0);
    assert_eq!(
        shape.rgba.len(),
        usize::from(shape.w) * usize::from(shape.h) * 4
    );
    let wire = shape.fields().into_body();
    let again = CursorShape::from_body(&wire).expect("a decoder refuses its own encoding");
    assert_eq!(
        again.fields().into_body(),
        wire,
        "an encoding does not settle"
    );
});
