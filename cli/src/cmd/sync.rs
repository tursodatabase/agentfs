use agentfs_sdk::AgentFSOptions;
use anyhow::anyhow;

use crate::cmd::init::open_agentfs;

pub async fn handle_pull_command(id_or_path: String) -> anyhow::Result<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let (db, _) = open_agentfs(options).await?;
    let Some(db) = db else {
        return Err(anyhow!("db is not connected to the remote"));
    };
    db.pull().await?;
    eprintln!("Remote data pulled to local db successfully");
    Ok(())
}

pub async fn handle_push_command(id_or_path: String) -> anyhow::Result<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let (db, _) = open_agentfs(options).await?;
    let Some(db) = db else {
        return Err(anyhow!("db is not connected to the remote"));
    };
    db.push().await?;
    eprintln!("Local data pushed to remote db successfully");
    Ok(())
}

pub async fn handle_checkpoint_command(id_or_path: String) -> anyhow::Result<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let (db, _) = open_agentfs(options).await?;
    let Some(db) = db else {
        return Err(anyhow!("db is not connected to the remote"));
    };
    db.checkpoint().await?;
    eprintln!("Local db checkpoined successfully");
    Ok(())
}

pub async fn handle_stats_command(
    stdout: &mut impl std::io::Write,
    id_or_path: String,
) -> anyhow::Result<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let (db, _) = open_agentfs(options).await?;
    let Some(db) = db else {
        return Err(anyhow!("db is not connected to the remote"));
    };
    let stats = db.stats().await?;
    stdout.write(serde_json::to_string(&stats)?.as_bytes())?;
    Ok(())
}
