/*
 * Tiny static `/init` for the guest-userspace boot test fixture.
 *
 * The kernel runs this as PID 1. Because the initramfs ships a
 * /dev/console device node (see build-initramfs.sh), the kernel
 * wires this process's stdin/stdout/stderr to the serial console
 * (console=ttyS0) before exec'ing us — so a plain write(1, ...)
 * reaches the host's serial capture.
 *
 * Behaviour: print a recognizable marker, then reboot. On a minimal
 * KVM guest with no reset device, reboot(RB_AUTOBOOT) becomes a
 * triple fault, which vm-kvm's vCPU loop surfaces as a Shutdown exit
 * and the host observes the VM transition to Stopped — so the test
 * reaches a terminal state quickly instead of waiting out its
 * timeout. The pause() loop is a belt-and-braces guard: if reboot
 * ever returns, we must not fall off the end of init (that triggers
 * a "kill init" kernel panic).
 *
 * This is a TEST FIXTURE only. The real guest agent (M2/D2) is the
 * Rust static-musl `nanovm-agent` binary; it replaces this init.
 */
#include <unistd.h>
#include <sys/reboot.h>

int main(void)
{
	static const char marker[] = "GUEST_USERSPACE_OK\n";
	(void)write(1, marker, sizeof(marker) - 1);
	reboot(RB_AUTOBOOT);
	for (;;)
		pause();
	return 0;
}
