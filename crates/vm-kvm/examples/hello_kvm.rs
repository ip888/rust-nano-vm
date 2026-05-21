//! `hello_kvm` — smallest-possible KVM round-trip on the host.
//!
//! What it does:
//!
//! 1. Opens `/dev/kvm` and creates a VM.
//! 2. Allocates one 4 KiB page of host memory via `mmap` and registers it
//!    as guest physical RAM at GPA `0`.
//! 3. Writes a 3-byte real-mode program at GPA `0`:
//!
//!    ```asm
//!    mov al, 0x42
//!    out 0x10, al      ; KVM_EXIT_IO with data=0x42
//!    hlt               ; KVM_EXIT_HLT
//!    ```
//!
//!    Each byte is hand-coded — no assembler needed.
//! 4. Creates a vCPU, sets CS:IP = 0x0000:0x0000 so the vCPU starts at the
//!    real-mode reset vector pointing into our mapped page.
//! 5. Runs the vCPU and asserts the two exits arrive in order: `Io { port:
//!    0x10, data: [0x42] }`, then `Hlt`.
//!
//! Why: gives us a single command we can paste into a fresh KVM host to
//! prove the kernel module, `/dev/kvm` perms, host RAM mmap, vCPU run loop,
//! and PIO exit-handling all work — without depending on any of the more
//! ambitious vm-kvm code paths. If this passes, the surrounding crate's
//! `create_vm` / `start` (M1) is the next thing to exercise; if it fails,
//! we have a precise error and a 100-line file to bisect against.
//!
//! Run:
//!
//! ```sh
//! cargo run -p vm-kvm --example hello_kvm --features kvm
//! ```
//!
//! Expected output:
//!
//! ```text
//! vcpu exit 1: Io { port: 0x10, data: [0x42] }
//! vcpu exit 2: Hlt
//! hello_kvm: OK
//! ```

#![allow(clippy::missing_safety_doc)]

#[cfg(not(feature = "kvm"))]
fn main() {
    eprintln!("This example requires the `kvm` feature. Run with:");
    eprintln!("  cargo run -p vm-kvm --example hello_kvm --features kvm");
    std::process::exit(2);
}

#[cfg(feature = "kvm")]
fn main() -> std::io::Result<()> {
    use std::io::{self, Error};
    use std::ptr;

    use kvm_bindings::{kvm_segment, kvm_userspace_memory_region};
    use kvm_ioctls::{Kvm, VcpuExit};

    const MEM_SIZE: usize = 0x1000; // one page is enough for the program below.

    // ------------------------------------------------------------------
    // Step 1: open /dev/kvm.
    // ------------------------------------------------------------------
    let kvm = Kvm::new().map_err(|e| Error::other(format!("Kvm::new failed: {e}")))?;
    println!("kvm api version: {}", kvm.get_api_version());

    // ------------------------------------------------------------------
    // Step 2: create the VM file descriptor.
    // ------------------------------------------------------------------
    let vm = kvm
        .create_vm()
        .map_err(|e| Error::other(format!("create_vm failed: {e}")))?;

    // ------------------------------------------------------------------
    // Step 3: allocate one page of host RAM and register it as guest
    // physical RAM at GPA 0.
    //
    // SAFETY: mmap() with MAP_ANONYMOUS | MAP_PRIVATE returns a freshly
    // mapped region we own. We check for MAP_FAILED before any use.
    // The pointer is then handed to KVM_SET_USER_MEMORY_REGION which
    // requires that the host_addr / memory_size pair remain valid for
    // the VM's lifetime — true here because `mem_ptr` outlives every
    // KVM ioctl below.
    // ------------------------------------------------------------------
    let mem_ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            MEM_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if mem_ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    // ------------------------------------------------------------------
    // Step 4: write the program into guest RAM at GPA 0.
    //
    // Bytes hand-assembled for 16-bit real mode:
    //   b0 42        mov  al, 0x42
    //   e6 10        out  0x10, al
    //   f4           hlt
    //
    // SAFETY: `mem_ptr` points to MEM_SIZE bytes of mmap'd writable
    // memory owned by this process; PROGRAM.len() (5) < MEM_SIZE.
    // ------------------------------------------------------------------
    const PROGRAM: &[u8] = &[0xb0, 0x42, 0xe6, 0x10, 0xf4];
    unsafe {
        ptr::copy_nonoverlapping(PROGRAM.as_ptr(), mem_ptr as *mut u8, PROGRAM.len());
    }

    // ------------------------------------------------------------------
    // Step 5: register the host mapping with KVM.
    //
    // SAFETY: We're handing KVM a host_addr that points at a valid
    // mmap'd region of `memory_size` bytes (see Step 3). The region is
    // unique within the VM (slot 0) and not reused below.
    // ------------------------------------------------------------------
    let region = kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: 0,
        memory_size: MEM_SIZE as u64,
        userspace_addr: mem_ptr as u64,
        flags: 0,
    };
    unsafe {
        vm.set_user_memory_region(region)
            .map_err(|e| Error::other(format!("set_user_memory_region failed: {e}")))?;
    }

    // ------------------------------------------------------------------
    // Step 6: create a vCPU.
    // ------------------------------------------------------------------
    let mut vcpu = vm
        .create_vcpu(0)
        .map_err(|e| Error::other(format!("create_vcpu failed: {e}")))?;

    // ------------------------------------------------------------------
    // Step 7: set CS:IP so the vCPU starts executing at GPA 0.
    //
    // At reset, x86 vCPUs are in real mode with CS:IP = F000:FFF0 (the
    // BIOS reset vector). We override to CS={base=0, selector=0} and
    // RIP=0 so the vCPU enters our hand-written program directly.
    // Requires VT-x unrestricted-guest support, which every Intel CPU
    // from ~Nehalem onwards has.
    // ------------------------------------------------------------------
    let mut sregs = vcpu
        .get_sregs()
        .map_err(|e| Error::other(format!("get_sregs failed: {e}")))?;
    sregs.cs = kvm_segment {
        base: 0,
        selector: 0,
        ..sregs.cs
    };
    vcpu.set_sregs(&sregs)
        .map_err(|e| Error::other(format!("set_sregs failed: {e}")))?;

    let mut regs = vcpu
        .get_regs()
        .map_err(|e| Error::other(format!("get_regs failed: {e}")))?;
    regs.rip = 0;
    regs.rflags = 0x2; // mandatory reserved bit
    vcpu.set_regs(&regs)
        .map_err(|e| Error::other(format!("set_regs failed: {e}")))?;

    // ------------------------------------------------------------------
    // Step 8: run the vCPU. Expect two exits in order: an IO write of
    // 0x42 to port 0x10, then HLT.
    // ------------------------------------------------------------------
    let mut io_seen = false;
    for round in 1..=8 {
        match vcpu.run() {
            Ok(VcpuExit::IoOut(port, data)) => {
                println!("vcpu exit {round}: Io {{ port: {port:#x}, data: {data:?} }}");
                if port != 0x10 || data != [0x42] {
                    return Err(Error::other(format!(
                        "unexpected IO: port={port:#x} data={data:?}"
                    )));
                }
                io_seen = true;
            }
            Ok(VcpuExit::Hlt) => {
                println!("vcpu exit {round}: Hlt");
                if !io_seen {
                    return Err(Error::other(
                        "HLT before IO — program executed in the wrong order",
                    ));
                }
                println!("hello_kvm: OK");
                return Ok(());
            }
            Ok(other) => {
                return Err(Error::other(format!(
                    "vcpu exit {round}: unexpected reason: {other:?}"
                )));
            }
            Err(e) => {
                return Err(Error::other(format!(
                    "vcpu.run() failed (round {round}): {e}"
                )));
            }
        }
    }
    Err(Error::other(
        "vcpu produced 8 exits without hitting HLT — likely loop somewhere",
    ))
}
