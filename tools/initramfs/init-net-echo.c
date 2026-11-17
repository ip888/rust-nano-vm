/*
 * tools/initramfs/init-net-echo.c
 *
 * Tiny ELF /init wrapper for the NANOVM_INIT=net-echo variant.
 *
 * Why not use a `#!/bin/busybox sh` shebang on /init directly:
 * on at least one host (Omarchy / Arch, kernel-tiny 6.12) the
 * shebang path through binfmt_script → recursive exec of
 * `/bin/busybox` ends with init exiting 1 within 2 ms and zero
 * userspace output on serial. The same busybox + same script run
 * fine on the host, and the agent variant's ELF /init works fine
 * in the same guest kernel — so the issue is specifically in
 * the shebang path.
 *
 * This wrapper sidesteps it entirely: it IS an ELF binary (same
 * code path as the agent variant), prints a wrapper-alive marker
 * so we can confirm /init launched successfully, then execve's
 * `/bin/busybox` with `argv = ["sh", "/init.sh"]`. From busybox's
 * point of view it's a direct exec, not a shebang recursion.
 *
 * Build flags match init.c (the `test` variant):
 *   gcc -static -no-pie -Os -s
 *
 * `-no-pie` is load-bearing: tinyconfig kernels can't exec
 * static-PIE (ET_DYN) binaries as PID 1 (returns ENOENT). The
 * comment on init.c has the full story.
 */

#define _GNU_SOURCE
#include <sys/types.h>
#include <unistd.h>

static void write_str(const char *s) {
    const char *p = s;
    while (*p) {
        p++;
    }
    /* write(2) is a direct syscall; no libc stdio machinery that
     * could silently buffer or fail on a broken /dev/console.
     * Partial writes on a 70-byte payload to a char device are
     * fine to ignore — this is a best-effort diagnostic line, not
     * the production data path. The explicit result assignment
     * suppresses glibc's `warn_unused_result` on write(). */
    ssize_t rc = write(1, s, (size_t)(p - s));
    (void) rc;
}

int main(int argc, char **argv) {
    (void) argc;
    (void) argv;

    write_str("[wrapper] /init C wrapper alive, about to exec busybox sh /init.sh\n");

    char *new_argv[] = { "sh", "/init.sh", (char *)0 };
    char *new_envp[] = { (char *)0 };
    execve("/bin/busybox", new_argv, new_envp);

    /* If we reach this, execve failed. The wrapper-alive line
     * above already landed on serial, so the operator sees exec
     * failed too. Exit non-zero to make the kernel panic loudly. */
    write_str("[wrapper] FATAL: execve(/bin/busybox) failed\n");
    return 1;
}
