//! Phase 1 success path: Option<T> field with single exposed subfield.

use diff_macros::DifferentialSubject;

struct Header {
    version: u8,
}

#[derive(DifferentialSubject)]
struct Log {
    #[diff(private, durable, expose = [version: u8])]
    header: Option<Header>,
}

fn main() {
    let with = Log {
        header: Some(Header { version: 7 }),
    };
    let without = Log { header: None };
    assert_eq!(with.diff_header_version(), Some(7));
    assert_eq!(without.diff_header_version(), None);
}
