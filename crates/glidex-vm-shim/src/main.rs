//! `glidex-vm-shim --dir <vm runtime dir> [--vm <id>]`: see the library
//! documentation and spec/reconciliation.md §8.

fn main() {
    let args = match glidex_vm_shim::supervisor::parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(glidex_vm_shim::supervisor::EXIT_BAD_LAUNCH);
        }
    };
    std::process::exit(glidex_vm_shim::supervisor::run(args));
}
