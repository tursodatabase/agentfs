The snapshot functionality has been implemented. This adds:

- `snapshot()` method to the SDK
- `agentfs snapshot <ID_OR_PATH> <DEST_PATH>` CLI command

You can snapshot an active session's database by pointing to its delta.db file:

```
agentfs snapshot ~/.agentfs/run/<session>/delta.db /path/to/snapshot.db
```

The implementation uses WAL checkpoint + file copy. Note that there's a small race window between checkpoint and copy where concurrent writes from the FUSE process might not be captured. For fully consistent snapshots of an active mount, a future improvement could have the FUSE process handle the snapshot internally.
