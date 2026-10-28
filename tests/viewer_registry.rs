use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn workspace(runtime: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent-workspace-linux"))
        .arg("workspace")
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .env_remove("AGENT_WORKSPACE_ID")
        .env_remove("AGENT_WORKSPACE_RUNTIME_DIR")
        .env_remove("AGENT_WORKSPACE_SOCKET")
        .output()
        .expect("run workspace command")
}

fn json(output: Output) -> serde_json::Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).expect("workspace JSON")
}

#[test]
fn stale_cleanup_preserves_viewer_registry() {
    let runtime = std::env::temp_dir().join(format!("awl-registry-{}", std::process::id()));
    let base = runtime.join("agent-workspace-linux");
    let registry = base.join("viewers");
    let stale = base.join("stale-workspace");
    fs::create_dir_all(&registry).expect("create viewer registry");
    fs::create_dir(&stale).expect("create workspace without a manifest");
    let entry = registry.join("viewer.json");
    let contents = b"{\"viewer_pid\":12345}";
    fs::write(&entry, contents).expect("write viewer registry entry");

    let list = json(workspace(&runtime, &["list"]));
    assert_eq!(list["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(list["workspaces"][0]["id"], "stale-workspace");

    let preview = json(workspace(&runtime, &["cleanup", "--dry-run"]));
    assert_eq!(preview["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(preview["candidates"][0]["id"], "stale-workspace");
    assert!(stale.exists());
    assert_eq!(fs::read(&entry).unwrap(), contents);

    for id in ["viewers", " viewers "] {
        for command in ["start", "status", "stop", "cleanup"] {
            let output = if command == "start" {
                workspace(&runtime, &[command, "--id", id, "--ack-hidden-workspace"])
            } else {
                workspace(&runtime, &[command, "--id", id])
            };
            assert!(!output.status.success(), "{command} accepted {id:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("reserved"),
                "{output:?}"
            );
            assert_eq!(fs::read(&entry).unwrap(), contents);
        }
    }

    let cleanup = json(workspace(&runtime, &["cleanup"]));
    assert_eq!(cleanup["removed"].as_array().unwrap().len(), 1);
    assert_eq!(cleanup["removed"][0]["id"], "stale-workspace");
    assert!(!stale.exists());
    assert_eq!(fs::read(&entry).unwrap(), contents);
    assert!(json(workspace(&runtime, &["list"]))["workspaces"]
        .as_array()
        .unwrap()
        .is_empty());

    fs::remove_dir_all(&runtime).expect("remove test runtime");
}
