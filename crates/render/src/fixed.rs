//! 16.16 fixed-point arithmetic (0027c-D3). Every geometric quantity in the
//! crate is a 16.16 value unless documented otherwise, so renders are
//! byte-exact on every host.

/// A 16.16 fixed-point number.
pub type Fixed = i32;

/// 1.0 in 16.16.
pub const ONE: Fixed = 1 << 16;

/// The path flattening tolerance: 1/16 of a pixel (0027c-D4).
pub const TOLERANCE: Fixed = ONE / 16;

/// Whole pixels to 16.16.
pub const fn px(value: i32) -> Fixed {
    value << 16
}

/// Truncate to whole pixels, toward negative infinity.
pub const fn floor(value: Fixed) -> i32 {
    value >> 16
}

/// `a * b` in 16.16, truncating toward zero (i64 intermediate, so no
/// overflow for in-range inputs).
pub const fn mul(a: Fixed, b: Fixed) -> Fixed {
    ((a as i64 * b as i64) >> 16) as i32
}

/// `a / b` in 16.16, truncating toward zero. Dividing by zero returns 0;
/// callers bound their geometry so zero means a degenerate segment or
/// radius, which draws nothing.
pub const fn div(a: Fixed, b: Fixed) -> Fixed {
    if b == 0 {
        return 0;
    }
    (((a as i64) << 16) / b as i64) as i32
}

/// The integer square root of a 32.32 value, as 16.16, by Newton's method.
/// Exact and deterministic.
pub const fn sqrt32(value: i64) -> Fixed {
    if value <= 0 {
        return 0;
    }
    // Start above the root.
    let mut v = value as u64;
    let mut bits = 0u32;
    while v > 0 {
        bits += 1;
        v >>= 1;
    }
    let mut x: u64 = 1 << (bits / 2 + 1);
    loop {
        let next = (x + value as u64 / x) / 2;
        if next >= x {
            return x as Fixed;
        }
        x = next;
    }
}

/// The Euclidean length of a 16.16 vector, as 16.16.
pub const fn length(x: Fixed, y: Fixed) -> Fixed {
    sqrt32(x as i64 * x as i64 + y as i64 * y as i64)
}

/// Scale a 16.16 vector to the given 16.16 length. A zero vector stays zero.
pub const fn scale_to(x: Fixed, y: Fixed, len: Fixed) -> (Fixed, Fixed) {
    let current = length(x, y);
    if current == 0 {
        return (0, 0);
    }
    (div(mul(x, len), current), div(mul(y, len), current))
}
