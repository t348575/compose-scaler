use std::{
    fs,
    io::Read,
    net::TcpStream,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use bollard::{
    service::{ContainerStateStatusEnum, HealthStatusEnum},
    Docker,
};
use hashbrown::HashMap;
use serde::{Deserialize, Deserializer, Serialize};
use ssh2::Session;
use tera::Context;
use thiserror::Error;
use tokio::{
    task::JoinHandle,
    time::sleep,
    {spawn, sync::RwLock},
};
use tracing::{debug, error, warn};

use crate::config::{self, Args};

const UNHEALTHY_NO_LOG: &str = "container is unhealthy: no log available";
const DEAD_STATE: &str = "container in dead state";
const DEFAULT_COMMAND: &str = "docker compose";
const SESSION_CHANNEL_OPEN_ERR: &str = "Could not open session channel";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    name: String,
    display_name: String,
    #[serde(deserialize_with = "parse_into_duration")]
    session_duration: Duration,
    pub theme: String,
    #[serde(deserialize_with = "parse_into_duration")]
    refresh_frequency: Duration,
    custom_command: Option<String>,
}

impl Config {
    pub fn insert_ctx(&self, ctx: &mut Context) {
        ctx.insert("display_name", &self.display_name);
        ctx.insert(
            "session_duration",
            &human_duration(self.session_duration.as_millis()),
        );
        ctx.insert("refresh_frequency", &self.refresh_frequency.as_secs());
    }
}

fn parse_into_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let mut s: String = Deserialize::deserialize(deserializer)?;

    let unit = s.pop().unwrap();
    let number = u64::from_str_radix(&s, 10)
        .map_err(|_| serde::de::Error::custom("Could not parse number"))?;
    Ok(Duration::from_millis(match unit {
        's' | 'S' => number * 1000,
        'm' | 'M' => number * 1000 * 60,
        'h' | 'H' => number * 1000 * 60 * 60,
        _ => return Err(serde::de::Error::custom("Unit does not exist")),
    }))
}

fn human_duration(dur: u128) -> String {
    match dur {
        0..=59999 => format!("{}s", dur / 1000),
        60000..=3599999 => format!("{}m", dur / (1000 * 60)),
        3600000.. => format!("{}h", dur / (1000 * 60 * 60)),
    }
}

#[derive(Debug)]
struct Project {
    config: Config,
    last_invoke: Instant,
    handle: Option<JoinHandle<()>>,
    instance_states: Vec<ContainerStatus>,
    status: Status,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct ContainerStatus {
    name: String,
    running: bool,
    current_replicas: u8,
    desired_replicas: u8,
    status: ContainerStateStatus,
    error: String,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub enum ContainerStateStatus {
    Ready,
    #[default]
    NotReady,
    Unrecoverable,
}

impl Project {
    fn new(config: Config) -> Project {
        Project {
            config,
            handle: None,
            last_invoke: Instant::now(),
            instance_states: Vec::new(),
            status: Status::NotRunning,
        }
    }
}

pub type SharedProjectManager = Arc<RwLock<ProjectManager>>;
pub struct ProjectManager {
    projects: HashMap<String, Project>,
    docker: Docker,
    this: Option<SharedProjectManager>,
    session: Session,
    c: config::Config
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Status {
    Running,
    NotRunning,
    Starting,
}

#[derive(Debug, Clone, Serialize, Deserialize, Error)]
pub enum ProjectError {
    #[error("Could not start compose project: {0}")]
    StartError(String),
    #[error("Could not stop compose project: {0}")]
    StopError(String),
    #[error("Could not list project containers: {0}")]
    ListError(String),
    #[error("Could not inspect container: {0}")]
    InspectError(String),
}

impl ProjectManager {
    pub async fn new(args: &Args) -> ProjectManager {
        let docker =
            Docker::connect_with_socket_defaults().expect("Could not connect to docker socket");

        let c: config::Config = serde_yaml::from_str(
            &fs::read_to_string(&args.config_file).expect("Could not read config file"),
        )
        .expect("Could not parse config file");

        let session = Self::connect_ssh(&c);

        let pm = ProjectManager {
            docker,
            projects: HashMap::new(),
            this: None,
            session,
            c: c.clone()
        };

        for p in c.initial_projects {
            pm.stop_project(&p, "docker compose")
                .await
                .expect(&format!("Could not stop project {p}"));
        }
        pm
    }

    fn connect_ssh(c: &config::Config) -> Session {
        let tcp = TcpStream::connect(c.ssh_host.clone()).unwrap();
        let mut session = Session::new().unwrap();
        session.set_tcp_stream(tcp);
        session.handshake().unwrap();

        session
            .userauth_pubkey_file(
                &c.ssh_username,
                Some(Path::new(&c.public_key)),
                Path::new(&c.private_key),
                None,
            )
            .unwrap();
        assert!(session.authenticated());
        session
    }

    pub fn set_this(&mut self, self_instance: SharedProjectManager) {
        self.this = Some(self_instance)
    }

    pub async fn add_and_start(
        &mut self,
        config: Config,
    ) -> Result<(Status, Vec<ContainerStatus>), ProjectError> {
        let mut already_exists = true;
        let project = if self.projects.contains_key(&config.name) {
            let item = self.projects.get_mut(&config.name).unwrap();
            item.config = config.clone();
            item
        } else {
            already_exists = false;
            self.projects
                .insert(config.name.clone(), Project::new(config.clone()));
            self.projects.get_mut(&config.name).unwrap()
        };

        if let Status::NotRunning = project.status {
            project.status = Status::Starting;

            let pc = self.this.clone().unwrap();
            let name = config.name.clone();
            spawn(async move {
                if let Err(err) = Self::start_project_sequence(
                    name.clone(),
                    pc.clone(),
                    already_exists,
                    config.custom_command,
                )
                .await
                {
                    error!("Could not startup project: {err}");

                    if let Some(p) = pc.write().await.projects.get_mut(&name) {
                        p.status = Status::NotRunning;
                        if let Some(h) = &p.handle {
                            h.abort();
                        }
                    }
                }
            });
        }

        project.last_invoke = Instant::now();
        Ok((project.status.clone(), project.instance_states.clone()))
    }

    async fn start_project_sequence(
        name: String,
        this: SharedProjectManager,
        already_exists: bool,
        custom_command: Option<String>,
    ) -> Result<(), ProjectError> {
        let mut m = this.write().await;

        let cmd = custom_command.unwrap_or(DEFAULT_COMMAND.to_owned());
        for i in 0..3 {
            if let Err(err) = m.start_project(&name, &cmd).await {
                if i == 3 {
                    return Err(err);
                }
            } else {
                break;
            }
        }

        if let Some(p) = m.projects.get_mut(&name) {
            if !already_exists {
                let this_c = this.clone();
                let name_c = name.clone();
                let handle = spawn(async move {
                    Self::manage_project(name_c, this_c).await;
                });
                p.handle = Some(handle);
            }
            p.status = Status::Running;
        }
        Ok(())
    }

    async fn manage_project(name: String, this: SharedProjectManager) {
        loop {
            debug!("Checking project {name}");
            let mut project_manager = this.write().await;
            let p = match project_manager.projects.get_mut(&name) {
                Some(p) => p,
                None => break,
            };

            let custom_command = p
                .config
                .custom_command
                .clone()
                .unwrap_or(DEFAULT_COMMAND.to_owned());
            let sleep_for = p.config.refresh_frequency;

            if p.last_invoke.elapsed() >= p.config.session_duration {
                if let Err(err) = project_manager.stop_project(&name, &custom_command).await {
                    error!("(When stopping for idling) {err}");
                }
                _ = project_manager.projects.remove(&name);
                break;
            } else {
                match project_manager.status_project(&name, &custom_command).await {
                    Ok(statuses) => {
                        debug!("{statuses:?}");
                        if statuses.len() == 0 || statuses.iter().position(|x| !x.running).is_some()
                        {
                            if let Err(err) =
                                project_manager.start_project(&name, &custom_command).await
                            {
                                error!("(When restarting) {err}")
                            } else {
                                warn!("One or more containers in project {name} stopped");
                            }
                        } else {
                            if let Some(p) = project_manager.projects.get_mut(&name) {
                                p.status = Status::Running;
                            }
                        }

                        if let Some(p) = project_manager.projects.get_mut(&name) {
                            p.instance_states = statuses;
                        }
                    }
                    Err(err) => error!("(When refreshing) {err}"),
                }
            }
            drop(project_manager);
            sleep(sleep_for).await;
        }
    }

    async fn start_project(&mut self, name: &str, command: &str) -> Result<(), ProjectError> {
        if let Err(err) = self.exec_command(name, &format!("{command} up -d")) {
            match err {
                SESSION_CHANNEL_OPEN_ERR => self.session = Self::connect_ssh(&self.c),
                _ => return Err(ProjectError::StartError(err.to_owned()))
            }
        }
        Ok(())
    }

    async fn stop_project(&self, name: &str, command: &str) -> Result<(), ProjectError> {
        if let Err(err) = self.exec_command(name, &format!("{command} down")) {
            return Err(ProjectError::StopError(err.to_owned()));
        }
        Ok(())
    }

    async fn status_project(
        &self,
        name: &str,
        command: &str,
    ) -> Result<Vec<ContainerStatus>, ProjectError> {
        match self.exec_command(name, &format!("{command} ps -q")) {
            Ok(output) => {
                let mut statuses = Vec::new();
                for item in output.split('\n') {
                    let item = item.trim();
                    if item.len() == 0 {
                        continue;
                    }

                    let status = self
                        .docker
                        .inspect_container(item, None)
                        .await
                        .map_err(|x| ProjectError::InspectError(x.to_string()))?;

                    statuses.push(ContainerStatus {
                        name: item.to_owned(),
                        desired_replicas: 1,
                        ..Default::default()
                    });
                    let last = statuses.last_mut().unwrap();
                    let state = status.state.unwrap();
                    use ContainerStateStatusEnum::*;
                    match state.status.unwrap() {
                        CREATED | PAUSED | RESTARTING | REMOVING => not_ready_instance_state(last),
                        RUNNING => match state.health {
                            Some(h) => match h.status.unwrap() {
                                HealthStatusEnum::HEALTHY => ready_instance_state(last),
                                HealthStatusEnum::UNHEALTHY => {
                                    unrecoverable_instance_state(last);
                                    if let Some(log) = h.log {
                                        if log.len() > 0 {
                                            let l = log[0].clone();
                                            last.error = format!(
                                                "container is unhealthy: {} ({})",
                                                l.output.unwrap_or_default(),
                                                l.exit_code.unwrap_or_default()
                                            )
                                        } else {
                                            last.error = UNHEALTHY_NO_LOG.to_owned();
                                        }
                                    } else {
                                        last.error = UNHEALTHY_NO_LOG.to_owned();
                                    }
                                }
                                _ => not_ready_instance_state(last),
                            },
                            None => ready_instance_state(last),
                        },
                        EXITED => {
                            let exit_code = state.exit_code.unwrap_or_default();
                            if exit_code != 0 {
                                unrecoverable_instance_state(last);
                                last.error =
                                    format!(r#"container exited with code "{}""#, exit_code)
                            } else {
                                not_ready_instance_state(last);
                            }
                        }
                        DEAD => {
                            unrecoverable_instance_state(last);
                            last.error = DEAD_STATE.to_owned();
                        }
                        _ => unreachable!(),
                    }
                }
                Ok(statuses)
            }
            Err(err) => Err(ProjectError::ListError(err.to_string())),
        }
    }

    pub fn get_all(&self) -> Vec<super::Project> {
        self.projects
            .iter()
            .map(|(_, p)| super::Project {
                config: p.config.clone(),
                last_invoke: p.last_invoke.elapsed().as_millis(),
                instance_states: p.instance_states.clone(),
                status: p.status.clone(),
            })
            .collect()
    }

    fn exec_command(&self, dir: &str, command: &str) -> Result<String, &'static str> {
        let mut channel = self
            .session
            .channel_session()
            .map_err(|_| SESSION_CHANNEL_OPEN_ERR)?;
        channel
            .exec(&format!("cd {}/{dir} && {command}", self.c.projects_dir))
            .map_err(|_| "Could not execute command")?;
        let mut s = String::new();
        channel
            .read_to_string(&mut s)
            .map_err(|_| "could not read command output to string")?;
        Ok(s)
    }
}

fn not_ready_instance_state(last: &mut ContainerStatus) {
    last.current_replicas = 0;
    last.status = ContainerStateStatus::NotReady;
    last.running = false;
}

fn ready_instance_state(last: &mut ContainerStatus) {
    last.current_replicas = 1;
    last.status = ContainerStateStatus::Ready;
    last.running = true;
}

fn unrecoverable_instance_state(last: &mut ContainerStatus) {
    not_ready_instance_state(last);
    last.status = ContainerStateStatus::Unrecoverable;
}

#[cfg(test)]
mod tests {
    #[test]
    fn duration_parse() {
        use crate::sessions::parse_into_duration;
        use serde::Deserialize;
        use std::time::Duration;

        #[derive(Deserialize)]
        struct DTest {
            #[serde(deserialize_with = "parse_into_duration")]
            value: Duration,
        }

        let test_str = r#"{ "value": "3m" }"#;
        let t: DTest = serde_json::from_str(test_str).expect("Could not parse json");
        assert_eq!(t.value, Duration::from_secs(3 * 60));

        let test_str = r#"{ "value": "10s" }"#;
        let t: DTest = serde_json::from_str(test_str).expect("Could not parse json");
        assert_eq!(t.value, Duration::from_secs(10));

        let test_str = r#"{ "value": "6h" }"#;
        let t: DTest = serde_json::from_str(test_str).expect("Could not parse json");
        assert_eq!(t.value, Duration::from_secs(6 * 60 * 60));
    }
}
