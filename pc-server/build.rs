use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=TOUCHMONITOR_COMMIT");
    // The fallback below reads Git directly. Tell Cargo about the files that
    // change when a new commit is checked out, otherwise it may reuse an old
    // binary and keep displaying the previous commit ID.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    let commit = std::env::var("TOUCHMONITOR_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=10", "HEAD"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let short = commit.chars().take(10).collect::<String>();
    println!("cargo:rustc-env=TOUCHMONITOR_COMMIT={short}");
}
