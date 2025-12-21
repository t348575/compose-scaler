use std::{
    fs,
    io::Read,
    net::TcpStream,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use bollard::{service::HealthStatusEnum, Docker};
use deserialize_form_style_query_parameter::option_form_vec_deserialize;
use hashbrown::{HashMap, HashSet};
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
const NO_CONTAINER_FOR_SERVICE: &str = "service has no container (never started or removed)";
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(from = "ParseConfig", into = "ParseConfig")]
pub struct Config {
    pub name: String,
    only: Vec<String>,
    display_name: String,
    session_duration: Duration,
    pub theme: String,
    refresh_frequency: Duration,
    custom_command: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ParseConfig {
    name: String,
    #[serde(deserialize_with = "option_form_vec_deserialize", default)]
    only: Option<Vec<String>>,
    display_name: Option<String>,
    #[serde(
        deserialize_with = "parse_into_duration",
        default = "default_session_duration"
    )]
    session_duration: Duration,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(
        deserialize_with = "parse_into_duration",
        default = "default_refresh_frequency"
    )]
    refresh_frequency: Duration,
    custom_command: Option<String>,
}

fn capitalize_first(s: &str) -> String {
    s.chars()
        .take(1)
        .flat_map(|f| f.to_uppercase())
        .chain(s.chars().skip(1))
        .collect()
}

impl From<ParseConfig> for Config {
    fn from(value: ParseConfig) -> Self {
        Config {
            name: value.name.clone(),
            display_name: value.display_name.unwrap_or(capitalize_first(&value.name)),
            session_duration: value.session_duration,
            theme: value.theme,
            refresh_frequency: value.refresh_frequency,
            custom_command: value.custom_command,
            only: value.only.unwrap_or_default(),
        }
    }
}

impl From<Config> for ParseConfig {
    fn from(value: Config) -> Self {
        ParseConfig {
            name: value.name,
            display_name: Some(value.display_name),
            session_duration: value.session_duration,
            theme: value.theme,
            refresh_frequency: value.refresh_frequency,
            custom_command: value.custom_command,
            only: if value.only.is_empty() {
                None
            } else {
                Some(value.only)
            },
        }
    }
}

fn default_theme() -> String {
    "ghost".to_owned()
}

fn default_session_duration() -> Duration {
    Duration::from_secs(600)
}

fn default_refresh_frequency() -> Duration {
    Duration::from_secs(5)
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

    fn command(&self) -> String {
        self.custom_command
            .clone()
            .unwrap_or(DEFAULT_COMMAND.to_owned())
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

#[derive(Debug, Serialize, Deserialize)]
pub enum OverrideCommand {
    Start,
    Stop,
    Clear,
}

#[derive(Debug)]
struct Project {
    scale_override: bool,
    config: Config,
    last_invoke: Instant,
    handle: Option<JoinHandle<()>>,
    instance_states: Vec<ContainerStatus>,
    status: Status,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct ContainerStatus {
    name: String,
    container_id: Option<String>,
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
    Missing,
}

impl Project {
    fn new(config: Config) -> Project {
        Project {
            scale_override: false,
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
    c: config::Config,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
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
    #[error("Could not override project: {0}")]
    OverrideError(String),
    #[error("Unknown compose services in `only`: {0}")]
    UnknownServices(String),
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

        let mut pm = ProjectManager {
            docker,
            projects: HashMap::new(),
            this: None,
            session,
            c: c.clone(),
        };

        for c in c.projects {
            if let Ok(statuses) = pm.status_project(&c.name, &c.command(), &c.only).await {
                let is_not_running = statuses.len() == 0 || statuses.iter().any(|x| !x.running);
                pm.projects.insert(
                    c.name.clone(),
                    Project {
                        scale_override: false,
                        config: c,
                        last_invoke: if is_not_running {
                            Instant::now() - Duration::from_secs(86400)
                        } else {
                            Instant::now()
                        },
                        handle: None,
                        instance_states: statuses,
                        status: if is_not_running {
                            Status::NotRunning
                        } else {
                            Status::Running
                        },
                    },
                );
            } else {
                error!(
                    "Could not get status for project {} during initialization",
                    c.name
                );
            }
        }
        pm
    }

    pub fn has_override(&self, project: &str) -> bool {
        self.projects
            .get(project)
            .map_or(false, |p| p.scale_override)
    }

    pub fn is_running(&self, project: &str) -> bool {
        self.projects
            .get(project)
            .map_or(false, |p| p.status == Status::Running)
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

    pub async fn override_command(
        &mut self,
        project: String,
        command: OverrideCommand,
    ) -> Result<(), ProjectError> {
        if let Some(p) = self.projects.get_mut(&project) {
            match command {
                OverrideCommand::Start => {
                    p.scale_override = true;
                    self.add_and_start(self.projects.get(&project).unwrap().config.clone())
                        .await?;
                }
                OverrideCommand::Stop => {
                    p.scale_override = true;
                    p.status = Status::NotRunning;
                    let c = &self.projects.get(&project).unwrap().config;
                    if let Err(err) = self.stop_project(&project, &c.command(), &c.only).await {
                        error!("(When stopping for idling) {err}");
                    }
                }
                OverrideCommand::Clear => {
                    p.scale_override = false;
                    if let Status::NotRunning = p.status {
                        p.last_invoke = Instant::now() - (p.config.session_duration * 2);
                        if let Some(h) = p.handle.take() {
                            h.abort();
                        }
                    }
                }
            }
            Ok(())
        } else {
            return Err(ProjectError::OverrideError("Project not found".to_owned()));
        }
    }

    pub async fn add_and_start(
        &mut self,
        config: Config,
    ) -> Result<(Status, Vec<ContainerStatus>), ProjectError> {
        let project = if self.projects.contains_key(&config.name) {
            let item = self.projects.get_mut(&config.name).unwrap();
            item.config = config.clone();
            item
        } else {
            self.projects
                .insert(config.name.clone(), Project::new(config.clone()));
            self.projects.get_mut(&config.name).unwrap()
        };

        if let Status::NotRunning = project.status {
            project.status = Status::Starting;

            let pc = self.this.clone().unwrap();
            let name = config.name.clone();
            spawn(async move {
                if let Err(err) =
                    Self::start_project_sequence(name.clone(), pc.clone(), config).await
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
        config: Config,
    ) -> Result<(), ProjectError> {
        let mut m = this.write().await;

        let cmd = config.command();
        for i in 0..3 {
            if let Err(err) = m.start_project(&name, &cmd, &config.only).await {
                if i == 3 {
                    return Err(err);
                }
            } else {
                break;
            }
        }

        let p = m.projects.get_mut(&name);
        if p.is_none() {
            return Err(ProjectError::StartError("Project not found".to_owned()));
        }
        let p = p.unwrap();
        let this_c = this.clone();
        let name_c = name.clone();

        let start_and_set_handle = |p: &mut Project| {
            let handle = spawn(async move {
                Self::manage_project(name_c, this_c).await;
            });
            p.handle = Some(handle);
        };

        match p.handle.as_ref() {
            Some(h) => {
                if h.is_finished() {
                    start_and_set_handle(p);
                }
            }
            None => start_and_set_handle(p),
        }
        p.status = Status::Running;
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

            let custom_command = p.config.command();
            let sleep_for = p.config.refresh_frequency;
            let only = p.config.only.clone();

            if p.scale_override {
                drop(project_manager);
                sleep(sleep_for).await;
                continue;
            }

            if p.last_invoke.elapsed() >= p.config.session_duration {
                p.status = Status::NotRunning;
                if let Err(err) = project_manager
                    .stop_project(&name, &custom_command, &only)
                    .await
                {
                    error!("(When stopping for idling) {err}");
                }
                break;
            } else {
                match project_manager
                    .status_project(&name, &custom_command, &only)
                    .await
                {
                    Ok(statuses) => {
                        if statuses.len() == 0 || statuses.iter().position(|x| !x.running).is_some()
                        {
                            if let Err(err) = project_manager
                                .start_project(&name, &custom_command, &only)
                                .await
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

    async fn start_project(
        &mut self,
        name: &str,
        command: &str,
        only: &[String],
    ) -> Result<(), ProjectError> {
        self.validate_only_services(name, command, only)?;
        if let Err(err) = self.exec_command(name, &format!("{command} up -d {}", only.join(" "))) {
            match err {
                SESSION_CHANNEL_OPEN_ERR => self.session = Self::connect_ssh(&self.c),
                _ => return Err(ProjectError::StartError(err.to_owned())),
            }
        }
        Ok(())
    }

    async fn stop_project(
        &self,
        name: &str,
        command: &str,
        only: &[String],
    ) -> Result<(), ProjectError> {
        self.validate_only_services(name, command, only)?;
        if let Err(err) = self.exec_command(name, &format!("{command} down {}", only.join(" "))) {
            return Err(ProjectError::StopError(err.to_owned()));
        }
        Ok(())
    }

    async fn status_project(
        &self,
        name: &str,
        command: &str,
        only: &[String],
    ) -> Result<Vec<ContainerStatus>, ProjectError> {
        let declared_services_out = self
            .exec_command(name, &format!("{command} config --services"))
            .map_err(|err| ProjectError::ListError(err.to_string()))?;
        let declared_services = parse_lines(&declared_services_out);
        if !only.is_empty() {
            let declared_set: hashbrown::HashSet<&str> =
                declared_services.iter().map(|s| s.as_str()).collect();
            let unknown: Vec<&str> = only
                .iter()
                .map(|s| s.as_str())
                .filter(|svc| !declared_set.contains(*svc))
                .collect();
            if !unknown.is_empty() {
                return Err(ProjectError::UnknownServices(unknown.join(", ")));
            }
        }

        let mut declared_filtered = declared_services.clone();
        if !only.is_empty() {
            let only_set: HashSet<&str> = only.iter().map(|s| s.as_str()).collect();
            declared_filtered.retain(|svc| only_set.contains(svc.as_str()));
        }
        let container_ids_out = self
            .exec_command(name, &format!("{command} ps -q {}", only.join(" ")))
            .map_err(|err| ProjectError::ListError(err.to_string()))?;
        let container_ids = parse_lines(&container_ids_out);
        let mut by_service = HashMap::new();
        for svc in &declared_filtered {
            by_service.insert(
                svc.clone(),
                ContainerStatus {
                    name: svc.clone(),
                    container_id: None,
                    desired_replicas: 1,
                    current_replicas: 0,
                    running: false,
                    status: ContainerStateStatus::Missing,
                    error: NO_CONTAINER_FOR_SERVICE.to_owned(),
                },
            );
        }

        for id in container_ids {
            let inspect = self
                .docker
                .inspect_container(
                    &id,
                    None::<bollard::query_parameters::InspectContainerOptions>,
                )
                .await
                .map_err(|x| ProjectError::InspectError(x.to_string()))?;

            let service_name = inspect
                .config
                .as_ref()
                .and_then(|c| c.labels.as_ref())
                .and_then(|labels| labels.get(COMPOSE_SERVICE_LABEL))
                .cloned()
                .unwrap_or_else(|| id.clone());
            if !only.is_empty() && !only.iter().any(|s| s == &service_name) {
                continue;
            }
            let entry = by_service
                .entry(service_name.clone())
                .or_insert_with(|| ContainerStatus {
                    name: service_name.clone(),
                    container_id: Some(id.clone()),
                    desired_replicas: 1,
                    ..Default::default()
                });

            entry.name = service_name;
            entry.container_id = Some(id.clone());
            entry.desired_replicas = 1;
            let state = match inspect.state {
                Some(s) => s,
                None => {
                    not_ready_instance_state(entry);
                    entry.error = "container has no state".to_owned();
                    continue;
                }
            };

            use bollard::service::ContainerStateStatusEnum::*;
            match state.status.unwrap() {
                CREATED | PAUSED | RESTARTING | REMOVING => not_ready_instance_state(entry),
                RUNNING => match state.health {
                    Some(h) => match h.status.unwrap() {
                        HealthStatusEnum::HEALTHY => ready_instance_state(entry),
                        HealthStatusEnum::UNHEALTHY => {
                            unrecoverable_instance_state(entry);
                            if let Some(log) = h.log {
                                if !log.is_empty() {
                                    let l = log[0].clone();
                                    entry.error = format!(
                                        "container is unhealthy: {} ({})",
                                        l.output.unwrap_or_default(),
                                        l.exit_code.unwrap_or_default()
                                    );
                                } else {
                                    entry.error = UNHEALTHY_NO_LOG.to_owned();
                                }
                            } else {
                                entry.error = UNHEALTHY_NO_LOG.to_owned();
                            }
                        }
                        _ => not_ready_instance_state(entry),
                    },
                    None => ready_instance_state(entry),
                },
                EXITED => {
                    let exit_code = state.exit_code.unwrap_or_default();
                    if exit_code != 0 {
                        unrecoverable_instance_state(entry);
                        entry.error = format!(r#"container exited with code "{}""#, exit_code);
                    } else {
                        not_ready_instance_state(entry);
                    }
                }
                DEAD => {
                    unrecoverable_instance_state(entry);
                    entry.error = DEAD_STATE.to_owned();
                }
                _ => unreachable!(),
            }
        }

        let mut result = Vec::new();
        for svc in declared_filtered {
            if let Some(s) = by_service.remove(&svc) {
                result.push(s);
            }
        }
        result.extend(by_service.into_values());
        Ok(result)
    }

    pub fn get_all(&self) -> Vec<super::Project> {
        self.projects
            .iter()
            .map(|(_, p)| super::Project {
                config: p.config.clone(),
                scale_override: p.scale_override,
                last_invoke: p.last_invoke.elapsed().as_millis(),
                instance_states: p.instance_states.clone(),
                status: p.status.clone(),
            })
            .collect()
    }

    fn validate_only_services(
        &self,
        dir: &str,
        command: &str,
        only: &[String],
    ) -> Result<(), ProjectError> {
        if only.is_empty() {
            return Ok(());
        }

        let declared_services_out = self
            .exec_command(dir, &format!("{command} config --services"))
            .map_err(|err| ProjectError::ListError(err.to_string()))?;
        let declared = parse_lines(&declared_services_out);
        let declared_set: hashbrown::HashSet<&str> = declared.iter().map(|s| s.as_str()).collect();
        let unknown: Vec<&str> = only
            .iter()
            .map(|s| s.as_str())
            .filter(|svc| !declared_set.contains(*svc))
            .collect();

        if unknown.is_empty() {
            Ok(())
        } else {
            Err(ProjectError::UnknownServices(unknown.join(", ")))
        }
    }

    fn exec_command(&self, dir: &str, command: &str) -> Result<String, &'static str> {
        let mut channel = self
            .session
            .channel_session()
            .map_err(|_| SESSION_CHANNEL_OPEN_ERR)?;
        debug!("cd {}/{dir} && {command}", self.c.projects_dir);
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

fn parse_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_owned())
        .collect()
}
