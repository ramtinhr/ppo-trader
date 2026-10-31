//! Deprecated entry point — prefer `scan --bucket month` or `dt scan --bucket month`.
//! Keeps the old binary name so existing scripts keep working.

#[path = "scan/month.rs"]
mod month;

fn main() -> anyhow::Result<()> {
    eprintln!("note: `full_scan` is now `scan --bucket month` (or `dt scan --bucket month`)");
    month::run()
}
