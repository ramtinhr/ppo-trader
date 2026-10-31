//! Retired: use the Go product CLI instead.
//!
//!   go build -o dt ./cmd/dt
//!   ./dt browse --scan DIR --layout month

fn main() {
    eprintln!(
        "browse_full_scan has moved to the Go CLI:\n\n  go build -o dt ./cmd/dt\n  ./dt browse --scan <dir> --layout month\n\nSee docs/DT.md."
    );
    std::process::exit(2);
}
