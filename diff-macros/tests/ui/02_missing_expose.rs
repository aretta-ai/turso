//! Legal: `#[diff(...)]` flags with no `expose = [...]` are metadata-only.
//! No accessor is generated; the derive must still compile cleanly.

use diff_macros::DifferentialSubject;

#[derive(DifferentialSubject)]
struct Cache {
    #[diff(private, scratch)]
    bytes: Option<Vec<u8>>,
}

fn main() {
    let c = Cache { bytes: None };
    let _ = c.bytes; // sanity — struct usable as normal
}
