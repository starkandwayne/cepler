use super::repo::*;
use anyhow::*;
use glob::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fmt,
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

pub struct Database {
    state: DbState,
    pub state_dir: String,
}

const STATE_DIR: &str = ".cepler";

impl Database {
    pub fn open(path_to_config: &str) -> Result<Self> {
        let mut state = DbState::default();
        let path = Path::new(path_to_config);
        let dir = match path.parent() {
            Some(parent) if parent == Path::new("") => STATE_DIR.to_string(),
            None => STATE_DIR.to_string(),
            Some(parent) => format!("{}/{}", parent.to_str().unwrap(), STATE_DIR),
        };
        if Path::new(&dir).is_dir() {
            for path in glob(&format!("{}/*.state", dir))? {
                let path = path?;
                if let Some(name) = path.as_path().file_stem() {
                    let file = File::open(&path)?;
                    let reader = BufReader::new(file);
                    state.environments.insert(
                        name.to_str().expect("Convert name").to_string(),
                        EnvironmentState::from_reader(reader)?,
                    );
                }
            }
        }

        Ok(Self {
            state,
            state_dir: dir,
        })
    }

    pub fn open_env(
        path_to_config: &str,
        env_name: &str,
        propagated_name: Option<&String>,
        commit: CommitHash,
        repo: &Repo,
    ) -> Result<Self> {
        let path = Path::new(path_to_config);
        let dir = match path.parent() {
            Some(parent) if parent == Path::new("") => STATE_DIR.to_string(),
            None => STATE_DIR.to_string(),
            Some(parent) => format!("{}/{}", parent.to_str().unwrap(), STATE_DIR),
        };
        let env_file = format!("{}/{}.state", dir, env_name);
        let env_path = Path::new(&env_file);
        let env_state = repo.get_file_content(commit.clone(), env_path, |bytes| {
            EnvironmentState::from_reader(bytes)
        })?;
        let mut state = DbState::default();
        if let Some(env_state) = env_state {
            state.environments.insert(env_name.to_string(), env_state);
        }
        if let Some(last_env) = propagated_name {
            let env_file = format!("{}/{}.state", dir, last_env);
            let env_path = Path::new(&env_file);
            if let Some(env_state) = repo.get_file_content(commit, env_path, |bytes| {
                EnvironmentState::from_reader(bytes)
            })? {
                state.environments.insert(last_env.to_string(), env_state);
            }
        }
        Ok(Self {
            state,
            state_dir: dir,
        })
    }

    pub fn set_current_environment_state(
        &mut self,
        name: String,
        propagated_from: Option<String>,
        mut env: DeployState,
    ) -> Result<String> {
        let any_dirty = env.files.values().any(|f| f.dirty);
        env.any_dirty = any_dirty;
        let ret = format!("{}/{}.state", self.state_dir, &name);
        if let Some(state) = self.state.environments.get_mut(&name) {
            std::mem::swap(&mut state.current, &mut env);
            state.propagation_queue.push_front(env);
        } else {
            self.state.environments.insert(
                name.clone(),
                EnvironmentState {
                    current: env,
                    propagated_from,
                    propagation_queue: VecDeque::new(),
                },
            );
        }
        self.state.prune_propagation_queue(name);
        self.persist()?;
        Ok(ret)
    }

    pub fn get_target_propagated_state(
        &self,
        env: &str,
        propagated_from: &str,
    ) -> Option<&DeployState> {
        match (
            self.state.environments.get(env),
            self.state.environments.get(propagated_from),
        ) {
            (Some(env), Some(from)) => {
                if let Some(from_head) = env.current.propagated_head.as_ref() {
                    if from_head == &from.current.head_commit || from.propagation_queue.is_empty() {
                        Some(&from.current)
                    } else {
                        match from
                            .propagation_queue
                            .iter()
                            .enumerate()
                            .find(|(_, state)| &state.head_commit == from_head)
                        {
                            Some((idx, _)) if idx == 0 => Some(&from.current),

                            Some((idx, _)) => Some(&from.propagation_queue[idx - 1]),
                            None => Some(&from.propagation_queue[from.propagation_queue.len() - 1]),
                        }
                    }
                } else {
                    Some(&from.current)
                }
            }
            (None, Some(state)) => Some(&state.current),
            _ => None,
        }
    }

    pub fn get_current_state(&self, env: &str) -> Option<&DeployState> {
        self.state.environments.get(env).map(|env| &env.current)
    }

    fn persist(&self) -> Result<()> {
        use std::fs;
        use std::io::Write;
        let _ = fs::remove_dir_all(&self.state_dir);
        fs::create_dir(&self.state_dir)?;
        for (name, env) in self.state.environments.iter() {
            let mut file = File::create(&format!("{}/{}.state", self.state_dir, name))?;
            file.write_all(&serde_yaml::to_vec(&env)?)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DbState {
    environments: BTreeMap<String, EnvironmentState>,
}

impl DbState {
    fn prune_propagation_queue(&mut self, name: String) {
        let mut keep_states = 0;
        let to_prune = self.environments.get(&name).unwrap();
        for commit_hash in self.environments.iter().filter_map(|(env_name, state)| {
            if env_name == &name
                || state.propagated_from.is_none()
                || state.propagated_from.as_ref().unwrap() != &name
            {
                None
            } else {
                state.current.propagated_head.as_ref()
            }
        }) {
            if commit_hash == &to_prune.current.head_commit {
                continue;
            }
            for (idx, old_hash) in to_prune
                .propagation_queue
                .iter()
                .map(|state| &state.head_commit)
                .enumerate()
                .skip(keep_states)
            {
                if old_hash == commit_hash {
                    break;
                }
                keep_states = keep_states.max(idx + 1);
            }
        }
        let to_prune = self.environments.get_mut(&name).unwrap();
        to_prune.propagation_queue.drain(keep_states..);
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EnvironmentState {
    current: DeployState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub propagated_from: Option<String>,
    #[serde(skip_serializing_if = "VecDeque::is_empty")]
    #[serde(default)]
    propagation_queue: VecDeque<DeployState>,
}

impl EnvironmentState {
    fn from_reader(reader: impl Read) -> Result<Self> {
        let state = serde_yaml::from_reader(reader)?;
        Ok(state)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "PersistedDeployState")]
#[serde(into = "PersistedDeployState")]
pub struct DeployState {
    pub head_commit: CommitHash,
    pub propagated_head: Option<CommitHash>,
    any_dirty: bool,
    pub files: BTreeMap<String, FileState>,
}

impl DeployState {
    pub fn new(head_commit: CommitHash) -> Self {
        Self {
            head_commit,
            propagated_head: None,
            any_dirty: false,
            files: BTreeMap::new(),
        }
    }

    pub fn diff(&self, other: &DeployState) -> Vec<FileDiff> {
        let mut removed_files: HashSet<&String> = other.files.keys().collect();
        let mut diffs: Vec<_> = self
            .files
            .iter()
            .filter_map(|(name, state)| {
                if let Some(last_state) = other.files.get(name) {
                    removed_files.remove(&name);
                    if state.file_hash.is_none() && last_state.file_hash.is_none() {
                        None
                    } else if state.dirty
                        || last_state.dirty
                        || state.file_hash != last_state.file_hash
                    {
                        Some(FileDiff {
                            path: name.clone(),
                            current_state: if state.file_hash.is_some() {
                                Some(state.clone())
                            } else {
                                None
                            },
                            added: last_state.file_hash.is_none(),
                        })
                    } else {
                        None
                    }
                } else {
                    removed_files.remove(&name);
                    Some(FileDiff {
                        path: name.clone(),
                        current_state: if state.file_hash.is_some() {
                            Some(state.clone())
                        } else {
                            None
                        },
                        added: true,
                    })
                }
            })
            .collect();
        diffs.extend(removed_files.iter().map(|path| FileDiff {
            path: path.to_string(),
            current_state: None,
            added: false,
        }));
        diffs
    }
}

#[derive(Debug)]
pub struct FileDiff {
    pub path: String,
    pub current_state: Option<FileState>,
    pub added: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub file_hash: Option<FileHash>,
    #[serde(skip_serializing_if = "is_false")]
    #[serde(default)]
    pub dirty: bool,
    pub from_commit: CommitHash,
    pub message: String,
    #[serde(skip)]
    pub propagated: bool,
}

impl fmt::Display for FileState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] - {}",
            self.from_commit.to_short_ref(),
            self.message
        )
    }
}

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedDeployState {
    head_commit: CommitHash,
    #[serde(skip_serializing_if = "Option::is_none")]
    propagated_head: Option<CommitHash>,
    #[serde(skip_serializing_if = "is_false")]
    #[serde(default)]
    any_dirty: bool,
    propagated: BTreeMap<String, FileState>,
    latest: BTreeMap<String, FileState>,
}
impl From<DeployState> for PersistedDeployState {
    fn from(
        DeployState {
            head_commit,
            propagated_head,
            any_dirty,
            files,
        }: DeployState,
    ) -> Self {
        let mut propagated = BTreeMap::new();
        let mut latest = BTreeMap::new();
        for (key, state) in files.into_iter() {
            if state.propagated {
                propagated.insert(key, state);
            } else {
                latest.insert(key, state);
            }
        }
        Self {
            head_commit,
            propagated_head,
            any_dirty,
            propagated,
            latest,
        }
    }
}
impl From<PersistedDeployState> for DeployState {
    fn from(
        PersistedDeployState {
            head_commit,
            propagated_head,
            any_dirty,
            propagated,
            mut latest,
        }: PersistedDeployState,
    ) -> Self {
        latest.extend(propagated.into_iter().map(|(key, mut state)| {
            state.propagated = true;
            (key, state)
        }));
        Self {
            head_commit,
            propagated_head,
            any_dirty,
            files: latest,
        }
    }
}
