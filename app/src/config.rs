use std::net::SocketAddr;

use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// ip:port to listen on
    #[arg(short, long)]
    pub address: SocketAddr,
    /// config file
    #[arg(short, long)]
    pub config_file: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub ssh_host: String,
    pub ssh_username: String,
    pub projects_dir: String,
    pub public_key: String,
    pub private_key: String,
    pub initial_projects: Vec<String>,
}
