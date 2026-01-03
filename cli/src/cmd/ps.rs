use agentfs_sdk::get_mounted_agents;
use anyhow::Result;
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

/// Get the run directory for agentfs sandbox sessions
fn run_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agentfs")
        .join("run")
}

/// Information about a sandbox session
struct SessionInfo {
    id: String,
    mountpoint: Option<String>,
}

/// List sandbox sessions
pub async fn ps<W: Write>(out: &mut W, show_all: bool) -> Result<()> {
    let run_dir = run_dir();

    // Get currently mounted agents from /proc/mounts (authoritative source)
    let mounted: HashSet<String> = get_mounted_agents()
        .into_iter()
        .map(|m| m.agent_id)
        .collect();

    // Collect session info from ~/.agentfs/run/
    let mut sessions: Vec<SessionInfo> = Vec::new();

    if run_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&run_dir) {
            for entry in entries.flatten() {
                let path = entry.path();

                // Only look at directories that contain a delta.db file
                if path.is_dir() {
                    let delta_db = path.join("delta.db");
                    if delta_db.exists() {
                        if let Some(session_id) = path.file_name().and_then(|s| s.to_str()) {
                            let session_id = session_id.to_string();

                            // Check if this session is currently mounted
                            let mountpoint = if mounted.contains(&session_id) {
                                Some(path.join("mnt").to_string_lossy().to_string())
                            } else {
                                None
                            };

                            sessions.push(SessionInfo {
                                id: session_id,
                                mountpoint,
                            });
                        }
                    }
                }
            }
        }
    }

    // Filter to only running sessions unless -a flag is set
    if !show_all {
        sessions.retain(|s| s.mountpoint.is_some());
    }

    // Sort by session ID
    sessions.sort_by(|a, b| a.id.cmp(&b.id));

    if sessions.is_empty() {
        if show_all {
            writeln!(out, "No sandbox sessions found in {}", run_dir.display())?;
        } else {
            writeln!(out, "No running sandbox sessions. Use -a to show all sessions.")?;
        }
        return Ok(());
    }

    // Calculate column widths
    let id_width = sessions
        .iter()
        .map(|s| s.id.len())
        .max()
        .unwrap_or(10)
        .max(10);
    let mount_width = sessions
        .iter()
        .map(|s| s.mountpoint.as_ref().map(|m| m.len()).unwrap_or(1))
        .max()
        .unwrap_or(10)
        .max(10);

    // Print header
    writeln!(
        out,
        "{:<id_width$}  {:<mount_width$}  STATUS",
        "SESSION ID", "MOUNTPOINT",
        id_width = id_width,
        mount_width = mount_width
    )?;

    // Print sessions
    for session in &sessions {
        let mountpoint = session.mountpoint.as_deref().unwrap_or("-");
        let status = if session.mountpoint.is_some() {
            "running"
        } else {
            "stopped"
        };

        writeln!(
            out,
            "{:<id_width$}  {:<mount_width$}  {}",
            session.id,
            mountpoint,
            status,
            id_width = id_width,
            mount_width = mount_width
        )?;
    }

    Ok(())
}
