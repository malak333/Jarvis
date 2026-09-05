# Supervised Developer Build

The owner requested a working end-to-end application before further security
hardening on 2026-09-05. This build implements that choice under the existing owner
accounts. It runs actual model-generated changes and validation on Windows and
presents the queue and controls in the Mac app.

## Use it

Keep the existing Windows SSH session open. The first build is:

```sh
./scripts/developer-build.py --build
```

Afterward, double-click `Open Assemblywright Developer.command` to reconnect and
open the app. You can also run `./scripts/developer-build.py`, or open
`target/developer/Assemblywright Developer.app` when the runner is already connected. The launcher
starts the configured local model if its API is unavailable. It uses the existing
`local-ai-mac` controller and model; it does not install a new model configuration.
Run `--help` to change the Windows host, SSH control socket, remote root, or model
controller. Default connection values reflect the owner's current two-machine setup.

1. Enter a simple project folder name. A new folder is created under the displayed
   Windows workspace root; later features with the same name use that project.
2. Describe the feature and enter the command that should validate it. For Python
   projects, `python -m unittest discover -s tests -v` is a useful starting point.
3. Choose **Add to queue**, then **Start**. The confirmation identifies the Windows
   execution and automatic advancement behavior.
4. Review each feature's status, checkpoint, changed files, and validation result.
   Files remain in the Windows project folder.

This build does not clone, commit, push, or publish repositories. It is a supervised
implementation-and-validation loop. The existing production setup/planning surface
remains available in the original application.

## Controls and checkpoints

- **Stop** cancels model waiting or terminates the active validation process tree.
  It preserves applied files and leaves the feature paused.
- Windows starts validation suspended and assigns it to a kill-on-close Job before
  allowing it to run. Stop and Emergency Pause wait for the tree to exit; a runner
  crash also closes that Job. An unconfirmed termination is reported as a failure
  with Emergency Pause latched, rather than a successfully paused feature.
- **Emergency Pause** also latches a pause. **Clear Emergency Pause** only clears
  that latch; **Resume** remains a separate action.
- **Resume** reuses a prepared change set and skips files whose new digest already
  matches. Once the applied checkpoint exists, it runs validation without asking the
  model again or rewriting those files. An owner edit conflicting with the model input or a prepared
  change is preserved and reported. Changed files use synced temporary files and
  atomic replacement to protect existing bytes from interrupted writes.
- **Auto-run on** advances within the queue present at Start/Resume after the actual
  validation command exits successfully. New features added during execution wait
  for another explicit Start.
  **Auto-run off** leaves the next feature queued for **Start next feature**.
- A failed model response or validation stops advancement. A retry with no prepared
  change set asks the model again. A retry after application reruns validation; fix
  failing code in the project before resuming if needed.
- On runner restart, interrupted runs become paused. Completed results and saved
  change sets remain durable. Validation commands can run again after interruption,
  so use repeatable test/build commands.

The portable Mac runner exists for native test coverage and uses process groups;
it does not provide the Windows runner-crash Job guarantee. The launcher executes
owner projects on Windows.

The Windows runner owns a separate SQLite database and an exclusive state-directory
lock. It binds only to loopback. The launcher forwards the runner to Mac port 17796
and the Mac model to Windows port 18080 through the existing SSH connection. The
loopback token is stored in the local developer configuration, not printed by the
launcher. The installed `AssemblywrightMaster` service and owner checkouts remain
separate from this build.

The model URL must also be loopback HTTP; use SSH forwarding for the Mac model.
Validation logs stay in the developer state directory for owner debugging and are
not automatically redacted. The developer snapshot is not a production append-only
audit ledger. If an Emergency Pause cannot be saved, the request reports failure
and the current runner keeps its emergency latch until a successful explicit clear.
Storage failure is not reported as a durable acknowledgement; restart never
automatically resumes work.

The runner accepts at most 100 features, 40 generated files per change set, and a
1 MiB model change set. Project context is bounded and skips hidden directories,
common dependency/build directories, and symbolic links. Validation has a 15-minute
limit and a 2 MiB log limit. These are developer usability limits, not a claim of
hostile-process containment or protected production execution.

## Native validation

```sh
cargo test -p assemblywright-master --bin assemblywright-developer
cargo build -p assemblywright-master --bin assemblywright-developer
python3 scripts/developer-runner-e2e.py --binary target/debug/assemblywright-developer
swift build --disable-sandbox --package-path apps/mac --product AssemblywrightMacApp
```

On Windows use `python` and the `.exe` executable path. The E2E script starts a
separate temporary runner and a labeled fixture model, then exercises authenticated
HTTP, real file writes, real command execution, cancellation, checkpoint reuse,
auto-run, and restart. It does not contact the owner's live queue or local model.
Live-model evidence is recorded separately below.

## Real local-model evidence

On 2026-09-05, local Qwen generated three actual Windows features in
`C:\a\aw-developer-20260905\projects\temperature-demo`: temperature conversion
and tests (10 passing), Kelvin support (20 passing), and documentation (20 passing).
The live control run stopped validation in 0.265 seconds at its applied-files
checkpoint, confirmed Emergency Pause blocked Resume, resumed saved work, and
automatically advanced to the README feature. All three ended `succeeded`.

These live-model results are separate from disposable fixture-model E2E. The
publication closeout adds regression coverage for owner edits during generation,
atomic file replacement, starting queue boundaries, failed validation, database
failure, and Windows runner-crash cleanup. See
[developer-build-testing.md](developer-build-testing.md) for the current coverage
matrix and verification evidence.

An opt-in native Swift test instantiates the app's real `DeveloperRunnerModel`,
reads its saved connection configuration, and decodes the live Windows queue.
Set `ASSEMBLYWRIGHT_DEVELOPER_LIVE_CONFIG` to the developer `runtime.json` and run
`swift test --disable-sandbox --package-path apps/mac --filter DeveloperRunnerTests`.
The ordinary package gate skips this test without a live configuration. The
computer-use bridge could launch the app but closed its native pipe while reading
accessibility state, so this does not claim visual UI automation.

The working developer phase is published separately from unfinished production
integration drafts. Signed production installation and hostile containment remain
separate work.
