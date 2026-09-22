//! Running shell commands for module click/scroll actions.

use tracing::warn;

/// Run `command` via `sh -c`, detaching it from the bar.
///
/// A short-lived thread waits for the child so it does not become a zombie.
pub fn spawn_command(command: &str) {
    let command = command.to_string();
    std::thread::spawn(move || {
        if let Err(err) = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .status()
        {
            warn!("failed to run command `{}`: {}", command, err);
        }
    });
}
