# Control-plane restart recovery

This is the stage-3, one-node recovery model for the control plane. It is
intentionally limited to one control-plane replica attached to one persistent
local volume. It does not provide multi-node coordination, Kubernetes operator
reconciliation, or database-specific Java application recovery.

## Configure it

Set all of the following before starting `nanovm-control-plane`:

```sh
export NANOVM_BACKEND=fleet
export NANOVM_RECOVERY_STATE_PATH=/var/lib/nanovm/control-plane-state.json
export NANOVM_SNAPSHOT_STORE=file:///var/lib/nanovm/snapshots
```

`NANOVM_SNAPSHOT_STORE` may also be an S3-compatible URI supported by the
snapshot store. The recovery journal and snapshot store must use durable
storage that survives a pod/container/process restart.

For multi-tenant ownership, also configure `NANOVM_OWNERSHIP_STORE` on a
SQLite-enabled build. The recovery journal independently carries the owner
for every VM and snapshot and restores that ownership into the control-plane
ownership map at startup.

The control plane writes the journal with same-directory temporary files,
file sync, and atomic rename. Do not point two control-plane replicas at the
same journal or try to share one mutable journal between nodes. Replicated
coordination is outside this recovery model.

## What survives

The journal contains:

- the VM creation config and desired `created`, `running`, or `stopped` state;
- a restored VM's source snapshot;
- snapshot ownership and the durable-store key selected during export;
- the owning organization for VMs and snapshots.

The journal does **not** treat backend ids, worker process ids, sockets, or
allocated host ports as durable. On recovery, the backend assigns fresh VM
and snapshot ids. Every configured `host_port` is reset to `0`, asking the
OS/backend for a fresh port instead of reusing an address that may now belong
to another process. Endpoint proxy state is populated only from the newly
created backend handle.

Clients must rediscover VM and snapshot ids with the list endpoints after a
restart. A prior numeric id is not a stable external identity.

## Recovery order

Reconciliation runs before the control plane starts accepting traffic:

1. Inventory and verify the durable snapshot store.
2. Download and adopt each exported snapshot into a fresh backend process.
3. Revalidate the manifest's id against the durable-store key and require
   all backend state sidecars needed by KVM adoption.
4. Recreate cold VMs from their stored configs or restore snapshot-backed
   VMs from the newly adopted snapshot.
5. Apply the desired lifecycle state.
6. Re-register ownership and fresh endpoint mappings.
7. Atomically replace the journal with the fresh backend ids.

A snapshot only becomes recoverable after a successful
`POST /v1/snapshots/{id}/export`. Durable store ids are immutable: a second
publication to the same id returns `409 snapshot_store_conflict` rather than
mixing old sidecars with a new guest. Choose a new `as_snapshot_id` for a new
artifact. A local snapshot that was never exported is reported as
unrecoverable instead of being represented as restored data.

## Decisions and operator action

Each journal item produces one startup decision:

| Decision | Meaning | Operator action |
| --- | --- | --- |
| `recreated` | The resource and current backend state were rebuilt. | None. |
| `expired` | A VM/snapshot depends on a durable snapshot key that no longer exists in the store. | Restore the snapshot artifact or remove the obsolete intent; restart to retry. |
| `unrecoverable` | The intent was never exported, the store could not be read, the artifact is invalid, or the backend rejected recreation. | Fix the named storage/configuration/backend problem, then restart. |

The server emits one structured log line per item and a final summary with
`recreated`, `expired`, and `unrecoverable` counts. Per-item failures do not
hide failures for other resources: reconciliation continues so one bad
artifact cannot prevent independent cold VMs from recovering.

## Recovery-safe operations

- A fresh endpoint is required after restart; never publish or cache the old
  loopback host port.
- VM stop/start intent is persisted after the backend transition.
- VM destroy removes the VM journal intent and prunes any private snapshot
  intent no longer referenced by another VM.
- Export records the exact durable-store key, including `as_snapshot_id`.
- Import records both ownership and the source durable-store key.

Deleting a snapshot that still backs a live VM removes it from the public
inventory but retains its internal durable-store intent. Recovery adopts that
artifact transiently, recreates dependent VMs, and removes the temporary
local snapshot before serving traffic.

Snapshot retention is operator-controlled. Do not delete a durable snapshot
while a live VM still declares it as its restart source unless that VM may be
classified as expired at the next recovery.

This facility is Java-framework and JDK-vendor agnostic. It only restores the
guest snapshot and control-plane intent; application databases, schemas, data
masking, and database licensing remain customer-owned external systems.
