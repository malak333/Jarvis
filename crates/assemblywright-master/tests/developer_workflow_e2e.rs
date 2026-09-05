//! Runs the same disposable HTTP/file/process boundary on both required CI hosts.
#[test]
fn supervised_developer_workflow_runs_native_processes_and_recovers_checkpoints() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let python = if cfg!(windows) { "python" } else { "python3" };
    let output = std::process::Command::new(python)
        .arg(root.join("scripts/developer-runner-e2e.py"))
        .args(["--binary", env!("CARGO_BIN_EXE_assemblywright-developer")])
        .output()
        .expect("Python is required by the native developer E2E gate");
    assert!(
        output.status.success(),
        "native developer E2E failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
}
