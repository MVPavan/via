//! The Interface parses CLI requests and serves the VIA API; it never makes
//! session policy decisions or opens the Store.

// The CLI's version output is an intentional stdout response.
#[expect(clippy::print_stdout, reason = "the CLI reports its version on stdout")]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("via {}", env!("CARGO_PKG_VERSION"));
    }
}
