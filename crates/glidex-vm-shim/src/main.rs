//! `glidex-vm-shim --dir <vm runtime dir> [--vm <id>]`: see the library
//! documentation and spec/12-reconciliation.md §8. `glidex-vm-shim
//! --kvm-cpuid [file]` writes what KVM offers guests (spec/32 decision 11).

fn main() {
    // The boot step of glidex-kvm-cpuid.service (spec/32-live-migration.md
    // decision 11): what KVM offers, for the control plane to report.
    let mut a = std::env::args().skip(1);
    if a.next().as_deref() == Some("--kvm-cpuid") {
        let out = a
            .next()
            .unwrap_or_else(|| glidex_hv_client::cpuid::KVM_CPUID_FILE.into());
        let r = glidex_hv_client::cpuid::read_host(std::path::Path::new("/dev/kvm"))
            .and_then(|f| f.write(std::path::Path::new(&out)));
        if let Err(e) = r {
            eprintln!("glidex-vm-shim: KVM's CPUID: {}", e);
            std::process::exit(1);
        }
        return;
    }
    let args = match glidex_vm_shim::supervisor::parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(glidex_vm_shim::supervisor::EXIT_BAD_LAUNCH);
        }
    };
    std::process::exit(glidex_vm_shim::supervisor::run(args));
}
