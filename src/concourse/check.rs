use super::*;
use crate::{config::Config, repo::*, workspace::Workspace};
use anyhow::*;
use std::{env, io, path};

const TMPDIR: &str = "TMPDIR";
pub fn exec() -> Result<()> {
    let ResourceConfig {
        source, version, ..
    }: ResourceConfig = serde_json::from_reader(io::stdin()).context("Deserializing stdin")?;
    eprintln!(
        "Last deployment no: '{}', checking if we can deploy a newer version",
        version
            .as_ref()
            .map(|v| v.deployment_no.as_ref())
            .unwrap_or("0")
    );
    let clone_dir = format!(
        "{}/cepler-repo-cache",
        env::var(TMPDIR).unwrap_or_else(|_| "/tmp".to_string())
    );
    let conf = GitConfig {
        url: source.uri,
        branch: source.branch.clone(),
        private_key: source.private_key,
        dir: clone_dir.clone(),
    };
    let path = path::Path::new(&clone_dir);
    let repo = if !path.exists() || path.read_dir()?.next().is_none() {
        eprintln!("Cloning repo");
        let repo = Repo::clone(conf).context("Couldn't clone repo")?;
        std::env::set_current_dir(path)?;
        repo
    } else {
        eprintln!("Pulling latest state");
        std::env::set_current_dir(path)?;
        let repo = Repo::open()?;
        repo.pull(conf)?;
        repo
    };
    eprintln!(
        "HEAD of branch '{}' is now at: '{}'",
        source.branch,
        repo.head_commit_hash()?
    );

    let config = Config::from_file(&source.config)?;
    let ws = Workspace::new(source.config)?;
    let env = config
        .environments
        .get(&source.environment)
        .context(format!(
            "Environment '{}' not found in config",
            source.environment
        ))?;
    eprintln!("Checking equivalence with last deployed state...");
    let mut res = Vec::new();
    if let Some(version) = version {
        res.push(version);
    }
    match ws.check(env)? {
        None => {
            eprintln!("Nothing new to deploy");
        }
        Some(n) => {
            eprintln!("Found new state to deploy");
            res.push(Version {
                deployment_no: n.to_string(),
            })
        }
    }
    println!("{}", serde_json::to_string(&res)?);
    Ok(())
}
