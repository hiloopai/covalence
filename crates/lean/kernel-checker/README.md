# Lean kernel checker

`cov-kernel-checker` replays built [Lean](https://lean-lang.org) modules in the
Lean kernel. It keeps a receipt for each successful module check, and reuses the
receipt when a later run has the same complete inputs. Receipts move between CI
runs through S3-compatible storage, using Covalence's S3 transport.

```sh
# Replay the listed modules of a built Lake project, eight workers at a time.
cov-kernel-checker lean-check replay-built --jobs 8 --project . --modules modules.txt

# Restore the receipts of the first parent revision that has them, then publish.
cov-kernel-checker receipt-cache --remote s3://bucket/prefix --scope "$SCOPE" \
  --ledger previous.tsv --current-inputs current.tsv pull --revision "$PARENT"
cov-kernel-checker receipt-cache --remote s3://bucket/prefix --scope "$SCOPE" \
  --ledger current.tsv push --revision "$REVISION"
```

- `lean-check replay-built` checks each module's theorems in the kernel. It
  reuses a theorem's earlier result only when the theorem's statement, its
  dependencies, the toolchain and the checker are all unchanged.
- `receipt-cache pull` reads the receipt pack of the first listed revision that
  has one. It imports only the modules whose input row has changed. Revisions
  are only hints; the caller's scope and ledger decide what is reused.
- `receipt-cache push` publishes a pack and then its index. An index never
  points at an incomplete pack.

Write access to the S3 prefix is trust. Give it only to CI that runs on
reviewed code, never to pull requests.

The release archive is for Linux x86-64. Consumers pin the release revision and
the archive's SHA-256, and should key stored results on both.
