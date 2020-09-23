use super::{config::EnvironmentConfig, database::*, repo::*};
use anyhow::*;
use std::path::Path;

pub struct Workspace {
    path_to_config: String,
    db: Database,
}
impl Workspace {
    pub fn new(path_to_config: String) -> Result<Self> {
        Ok(Self {
            db: Database::open(&path_to_config)?,
            path_to_config,
        })
    }

    pub fn ls(&self, env: &EnvironmentConfig) -> Result<Vec<String>> {
        let repo = Repo::open()?;
        let new_env_state = self.construct_env_state(&repo, env, false)?;
        Ok(new_env_state.files.into_iter().map(|(k, _)| k).collect())
    }

    pub fn check(&self, env: &EnvironmentConfig) -> Result<Option<(String, Vec<FileDiff>)>> {
        let repo = Repo::open()?;
        if let Some(previous_env) = env.propagated_from() {
            self.db.get_current_state(&previous_env).context(format!(
                "Previous environment '{}' not deployed yet",
                previous_env
            ))?;
        }
        let new_env_state = self.construct_env_state(&repo, env, false)?;
        let diffs = if let Some(last) = self.db.get_current_state(&env.name) {
            let diffs = new_env_state.diff(&last);
            if diffs.is_empty() {
                return Ok(None);
            }
            diffs
        } else {
            new_env_state
                .files
                .iter()
                .map(|(path, state)| FileDiff {
                    path: path.clone(),
                    current_state: Some(state.clone()),
                    added: true,
                })
                .collect()
        };
        let mut deleted = Vec::new();
        let files: Vec<_> = diffs
            .iter()
            .filter_map(|diff| {
                if diff.current_state.is_some() {
                    Some(Path::new(&diff.path))
                } else {
                    deleted.push(Path::new(&diff.path));
                    None
                }
            })
            .collect();
        let (commit_hash, _) = repo.find_last_changed_commit(files, deleted)?;
        Ok(Some((commit_hash.to_short_ref(), diffs)))
    }

    pub fn prepare(&self, env: &EnvironmentConfig, force_clean: bool) -> Result<()> {
        let repo = Repo::open()?;
        let head_files = if force_clean {
            Some(env.head_filters())
        } else {
            None
        };
        let ignore_list = self.ignore_list();
        repo.checkout_head(head_files, ignore_list.clone())?;
        let head_patterns: Vec<_> = env.head_file_patterns().collect();
        for file_buf in env.propagated_files() {
            let file = file_buf.to_str().unwrap().to_string();
            if !ignore_list.iter().any(|p| p.matches(&file))
                && !head_patterns.iter().any(|p| p.matches(&file))
            {
                std::fs::remove_file(file_buf).expect("Couldn't remove file");
            }
        }
        if let Some(previous_env) = env.propagated_from() {
            if let Some(env_state) = self.db.get_target_propagated_state(&env.name, previous_env) {
                let patterns: Vec<_> = env.propagated_file_patterns().collect();
                for (name, state) in env_state.files.iter() {
                    if patterns.iter().any(|p| p.matches(&name))
                        && !head_patterns.iter().any(|p| p.matches(&name))
                    {
                        repo.checkout_file_from(name, &state.from_commit)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn record_env(
        &mut self,
        env: &EnvironmentConfig,
        commit: bool,
        reset: bool,
        git_config: Option<GitConfig>,
    ) -> Result<(String, Vec<FileDiff>)> {
        eprintln!("Recording current state");
        let repo = Repo::open()?;
        let new_env_state = self.construct_env_state(&repo, env, true)?;
        let diff = if let Some(last_state) = self.db.get_current_state(&env.name) {
            new_env_state.diff(last_state)
        } else {
            new_env_state
                .files
                .iter()
                .map(|(path, state)| FileDiff {
                    path: path.clone(),
                    current_state: Some(state.clone()),
                    added: true,
                })
                .collect()
        };
        let state_file = self.db.set_current_environment_state(
            env.name.clone(),
            env.propagated_from().cloned(),
            new_env_state,
        )?;
        if commit {
            eprintln!("Adding commit to repository to persist state");
            repo.commit_state_file(state_file)?;
        }
        if reset {
            eprintln!("Reseting head to have a clean workspace");
            repo.checkout_head(None, Vec::new())?;
        }
        if let Some(config) = git_config {
            eprintln!("Pushing to remote");
            repo.push(config)?;
        }
        let head_commit = repo.head_commit_hash()?;
        Ok((head_commit.to_short_ref(), diff))
    }

    fn construct_env_state(
        &self,
        repo: &Repo,
        env: &EnvironmentConfig,
        recording: bool,
    ) -> Result<DeployState> {
        let head_commit = repo.head_commit_hash()?;
        let mut new_env_state = DeployState::new(head_commit);

        for file in repo.head_files(env.head_filters(), self.ignore_list()) {
            let dirty = repo.is_file_dirty(&file)?;
            let file_name = file.to_str().unwrap().to_string();
            let (from_commit, message) = repo.find_last_changed_commit(vec![&file], vec![])?;
            let file_hash = hash_file(file);
            let state = FileState {
                file_hash,
                dirty,
                from_commit,
                message,
            };
            new_env_state.files.insert(file_name, state);
        }

        if let Some(previous_env) = env.propagated_from() {
            if let Some(env_state) = self.db.get_target_propagated_state(&env.name, previous_env) {
                new_env_state.propagated_head = Some(env_state.head_commit.clone());
                let patterns: Vec<_> = env.propagated_file_patterns().collect();
                for (name, prev_state) in env_state.files.iter() {
                    if patterns.iter().any(|p| p.matches(&name)) {
                        let (dirty, file_hash) = if recording {
                            let file_hash = hash_file(name);
                            (file_hash != prev_state.file_hash, file_hash)
                        } else {
                            (false, prev_state.file_hash.clone())
                        };
                        let file_state = FileState {
                            dirty,
                            file_hash,
                            from_commit: prev_state.from_commit.clone(),
                            message: prev_state.message.clone(),
                        };
                        new_env_state.files.insert(name.clone(), file_state);
                    }
                }
            }
        }

        Ok(new_env_state)
    }

    fn ignore_list(&self) -> Vec<glob::Pattern> {
        vec![
            glob::Pattern::new(&self.path_to_config).unwrap(),
            glob::Pattern::new(&format!("{}/*", self.db.state_dir)).unwrap(),
            glob::Pattern::new(".git/*").unwrap(),
            glob::Pattern::new(".gitignore").unwrap(),
        ]
    }
}
