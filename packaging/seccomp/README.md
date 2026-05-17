# seccomp profile

This directory ships a seccomp-BPF profile that pares the syscall
surface available to `nanovm-control-plane` down to roughly the
default set Docker / containerd / Kubernetes apply (sans the ones a
tokio HTTP server doesn't need).

Closes the remainder of tracked gap **G6** from
`docs/threat-model.md` — covers the container and raw-binary
deployment paths where `systemd`'s `SystemCallFilter=` isn't
available (the systemd path is closed by
`packaging/systemd/nanovm-control-plane.service`).

## Files

| File | Format | Used by |
| --- | --- | --- |
| `nanovm-control-plane.json` | OCI seccomp JSON | Docker, containerd, podman, Kubernetes |

## What the profile allows

The default action is `SCMP_ACT_ERRNO` (return `EPERM` rather than
`SIGSYS` — quieter on logs, equally effective). A curated allow-list
covers what a tokio HTTP server + glibc need:

- File I/O: `open*`, `read*`, `write*`, `close*`, `stat*`, `getdents`,
  `fcntl`, `fsync`, `lseek`, `rename`, `unlink*`, `*xattr`.
- Sockets: `socket`, `bind`, `listen`, `accept*`, `connect`,
  `getsockopt`, `setsockopt`, `getpeername`, `recvfrom`, `sendto`,
  `sendmsg`, `recvmsg`, `shutdown`.
- Process: `clone*`, `execve*`, `wait*`, `kill`, `tgkill`, `tkill`,
  `rt_sig*`, `getpid`, `gettid`, `prctl`, `arch_prctl`.
- Time: `clock_*`, `gettimeofday`, `nanosleep`, `timer_*`,
  `timerfd_*`.
- Memory: `mmap`, `munmap`, `mprotect`, `mremap`, `brk`, `madvise`.
- Tokio plumbing: `epoll_*`, `eventfd*`, `pipe*`, `futex*`,
  `pselect6`, `signalfd*`, `rseq`, `membarrier`, `restart_syscall`.

## What the profile *denies* (relative to Docker's default)

The blocked surface that matters for blast-radius after a
hypothetical exploit:

- No `bpf`, `perf_event_open`, `kexec_*`, `init_module`,
  `delete_module`, `mount`, `umount*`, `pivot_root`, `chroot`.
- No `reboot`, `swapon`, `swapoff`, `sethostname`, `setdomainname`.
- No `keyctl`, `request_key`, `add_key` — keyring is off-limits.
- No `userfaultfd` — only the future snapshot crate (M5) needs it,
  and that runs in a separate process.
- No `process_vm_readv` / `process_vm_writev` — no peeking at
  sibling processes.

## Use with Docker

```sh
docker run --rm \
    --security-opt seccomp=packaging/seccomp/nanovm-control-plane.json \
    --security-opt no-new-privileges \
    --cap-drop=ALL \
    --read-only \
    --tmpfs /tmp \
    -p 8080:8080 \
    -e NANOVM_API_TOKENS=dev \
    nanovm-control-plane:dev
```

(The Dockerfile already runs as `uid 65532` non-root via distroless.
The profile + `--cap-drop=ALL` + `--read-only` are the
defence-in-depth layer.)

## Use with containerd / Kubernetes

`securityContext.seccompProfile.type = Localhost` and put the JSON
under the node's `/var/lib/kubelet/seccomp/` (configurable):

```yaml
securityContext:
  seccompProfile:
    type: Localhost
    localhostProfile: nanovm-control-plane.json
  capabilities:
    drop: [ALL]
  readOnlyRootFilesystem: true
  allowPrivilegeEscalation: false
  runAsNonRoot: true
  runAsUser: 65532
```

## Troubleshooting

If `nanovm-control-plane` exits with `EPERM` on startup, you're
missing a syscall the binary needs. Reproduce with `strace -f` to
identify which one, then add it via a drop-in:

```jsonc
// derived-profile.json
{
  "defaultAction": "SCMP_ACT_ERRNO",
  "syscalls": [
    { "names": ["the_missing_one"], "action": "SCMP_ACT_ALLOW" },
    // ...rest copy-pasted from nanovm-control-plane.json
  ]
}
```

Then file an issue with the strace excerpt so the profile can grow
upstream.

## Why not in-binary `prctl(PR_SET_SECCOMP, …)`?

Two reasons:

1. The workspace is `#![forbid(unsafe_code)]` and applying a BPF
   program via `prctl` needs unsafe. Pulling in `seccompiler` or
   `libseccomp` to wrap it costs ~10 transitive deps.
2. The OCI / containerd / Kubernetes ecosystem already has a
   well-trodden path for applying a JSON profile *before* the
   binary starts. That's strictly stronger (the binary never gets
   a chance to disable the filter mid-boot).

If a future deployment path needs in-binary enforcement (e.g. a
single-static-binary release with no container runtime), revisit
then.
