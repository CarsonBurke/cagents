This Linux fixture models a transient address-in-use failure at the selected
candidate daemon's serving-socket bind. It hooks only libc `bind`; every other
bind calls the real function. The updater sees the product's ordinary startup
error and must restore and verify the original service and workload.

`scripts/rehearse_update_retry.py` compiles the shim with the private root and
candidate executable fixed into it. `scripts/rehearse-update` loads it only from
that marked root's `.fixture` directory into its private environment. The armed
sentinel blocks only that exact candidate executable and serving socket. The
retry orchestrator removes it only after real rollback, census verification,
provider activity and run-preservation checks pass. Decisions, including the old
daemon's pass-through bind, are logged with PID and executable.

The installed archive and executables remain unchanged across attempts. The
existing `--fail-new-daemon` archive wrapper remains a separate persistent fault.
Run native retries from a genuine shell on fw16; compilation of the C shim and
native execution belong there. Python-only tests can run elsewhere.

Run the pure contracts with `python3 -B tests/rehearse_update_retry.py`. Run
`python3 -B tests/rehearse_update_retry_bind.py` from the supervised fw16 shell
to prove repeated armed refusal, source/unrelated-socket pass-through, disarmed
success and byte stability using a copied Python executable. This C check does
not qualify the installed app.

For each native retry, use a new root and run the standalone orchestrator from
the frozen checkout:

```sh
python3 -B scripts/rehearse_update_retry.py \
  --root /var/tmp/cr/uff-retry-startup --mode startup \
  --old-dir /var/tmp/cagents-rehearsal/builds/0.1.3+local.991daa43e \
  --old-version 0.1.3+local.991daa43e \
  --old-installer /var/tmp/cagents-rehearsal/builds/0.1.3+local.991daa43e/install.sh \
  --new-dir "$verified_archive_dir" --new-version "$verified_version" \
  --new-installer "$verified_archive_dir/install.sh" \
  --ceditor /var/tmp/cagents-rehearsal/builds/ceditor-07e7efc/bin
```

For cancellation followed by consent, change only the root to
`/var/tmp/cr/uff-retry-cancel` and mode to `cancel`. Defaults seed four fake chats,
two ordinary terminals (one with a long-lived child), and one managed console.
`--no-console` is available only for explicitly recorded compatibility limits.
No real provider is used. The second attempt follows the exact apply command
printed by the stage-only installer. Both attempts retain raw pre-answer
confirmations, controlling-PTY transcripts, job/census/recovery files and native
observations under `retry/` and `records/`. A failure preserves the root; cleanup
stops the private processes but does not remove the evidence. Inspect
`retry-result.json`; an unqualified first restoration never releases the fault.
