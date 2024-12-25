use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Router,
};
use clap::Parser;
use config::Args;
use lazy_static::lazy_static;
use sessions::{Config, OverrideCommand, SharedProjectManager};
use tera::{Context, Tera};
use tokio::{signal, sync::RwLock};
use tracing::{debug, error};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::sessions::ProjectManager;

mod config;
mod sessions;

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_env("LOG"))
        .init();

    let args = Args::parse();
    debug!("{args:#?}");

    let pm = ProjectManager::new(&args).await;

    let state = Arc::new(RwLock::new(pm));
    state.write().await.set_this(state.clone());

    let api_router = Router::new()
        .route("/scale", get(api_handler))
        .route("/override/:project/:scale", post(scale_commands));

    let app = Router::new()
        .nest("/api", api_router)
        .route("/projects", get(project_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.address)
        .await
        .expect("Could not listen");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Could not start app");
}

lazy_static! {
    pub static ref TEMPLATES: Tera = {
        let tera = match Tera::new("themes/*.html") {
            Ok(t) => t,
            Err(e) => {
                error!("Parsing error(s): {}", e);
                ::std::process::exit(1);
            }
        };
        tera
    };
}

async fn api_handler(
    Query(config): Query<Config>,
    State(manager): State<SharedProjectManager>,
) -> impl IntoResponse {
    debug!("{config:#?}");

    if manager.read().await.has_override(&config.name) {
        return (StatusCode::CONFLICT, "Project has an override").into_response();
    }

    match manager.write().await.add_and_start(config.clone()).await {
        Ok((s, instance_states)) => match s {
            sessions::Status::Running => (
                StatusCode::OK,
                [("X-Compose-Scaler-Session-Status", "ready")],
            )
                .into_response(),
            sessions::Status::Starting | sessions::Status::NotRunning => {
                let mut ctx = Context::new();
                config.insert_ctx(&mut ctx);
                ctx.insert("instance_states", &instance_states);

                let theme = match TEMPLATES.render(&format!("{}.html", config.theme), &ctx) {
                    Ok(t) => t,
                    Err(err) => {
                        return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
                    }
                };

                Html(theme).into_response()
            }
        },
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn scale_commands(
    Path((project, override_command)): Path<(String, OverrideCommand)>,
    State(manager): State<SharedProjectManager>,
) -> impl IntoResponse {
    debug!("{project} {override_command:?}");
    let res = manager
        .write()
        .await
        .override_command(project, override_command)
        .await;
    match res {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[derive(serde::Serialize)]
struct Project {
    config: Config,
    last_invoke: u128,
    scale_override: bool,
    instance_states: Vec<sessions::ContainerStatus>,
    status: sessions::Status,
}

async fn project_handler(
    State(manager): State<SharedProjectManager>,
) -> axum::extract::Json<Vec<Project>> {
    axum::extract::Json(manager.read().await.get_all())
}
