/*
 * Tiny static `/init` for the guest-userspace boot test fixture.
 *
 * The kernel runs this as PID 1. It writes a recognizable marker to
 * /dev/kmsg (the kernel ring buffer), then reboots.
 *
 * Why /dev/kmsg rather than stdout/console: as PID 1 in a minimal
 * initramfs the kernel may not wire stdin/stdout/stderr to a usable
 * tty (the printk console works, but the userspace tty path may not).
 * Writes to /dev/kmsg go straight through printk to the serial
 * console synchronously — so the marker reaches the host's serial
 * capture reliably and isn't lost in a tty buffer when we reboot.
 *
 * reboot(RB_AUTOBOOT) brings the VM down (a triple fault on a minimal
 * KVM guest with no reset device → vm-kvm Shutdown → host sees
 * Stopped), so the test reaches a terminal state quickly. The
 * pause() loop guards against reboot returning (must not fall off
 * the end of init → "kill init" panic).
 *
 * TEST FIXTURE only. The real guest agent (M2) is the Rust
 * static-musl `nanovm-agent` binary; it replaces this init.
 */
#include <fcntl.h>
#include <unistd.h>
#include <sys/reboot.h>

int main(void)
{
	static const char marker[] = "GUEST_USERSPACE_OK\n";
	int fd = open("/dev/kmsg", O_WRONLY);
	if (fd >= 0) {
		(void)write(fd, marker, sizeof(marker) - 1);
		(void)close(fd);
	} else {
		/* Fallback: maybe the kernel wired our stdout to the
		   console after all. */
		(void)write(1, marker, sizeof(marker) - 1);
	}
	reboot(RB_AUTOBOOT);
	for (;;)
		pause();
	return 0;
}
