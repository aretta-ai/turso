//! Phase 1 rejection: `expose = [...]` on a non-`Option<T>` field must fail to
//! compile with a clear diagnostic. Phase 2 will lift this for wrapper types.

use diff_macros::DifferentialSubject;

struct Header {
    version: u8,
}

#[derive(DifferentialSubject)]
struct Log {
    #[diff(private, expose = [version: u8])]
    header: Header,
}

fn main() {}
