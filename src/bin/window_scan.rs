//! Deprecated entry point — prefer `scan --bucket window` or `dt scan --bucket window`.
//! Keeps the old binary name so existing scripts keep working.

#[path = "scan/window.rs"]
mod window;

fn main() -> anyhow::Result<()> {
    eprintln!("note: `window_scan` is now `scan --bucket window` (or `dt scan --bucket window`)");
    window::run()
}
