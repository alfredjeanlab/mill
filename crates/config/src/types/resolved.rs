use bytesize::ByteSize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

use super::EnvValue;

/// Fully resolved and validated cluster configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClusterConfig {
    pub services: HashMap<String, ServiceDef>,
    pub tasks: HashMap<String, TaskDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceDef {
    pub image: String,
    pub port: u16,
    pub replicas: u32,
    pub resources: Resources,
    pub env: HashMap<String, EnvValue>,
    pub command: Option<Vec<String>>,
    pub health: Option<HealthCheck>,
    pub routes: Vec<RouteDef>,
    pub volumes: Vec<VolumeDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskDef {
    pub image: String,
    pub resources: Resources,
    pub timeout: Duration,
    pub env: HashMap<String, EnvValue>,
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resources {
    pub cpu: f64,
    #[serde(with = "super::bytesize_serde")]
    pub memory: ByteSize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthCheck {
    pub path: String,
    pub interval: Duration,
    pub failure_threshold: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteDef {
    pub hostname: String,
    pub path: String,
    pub websocket: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VolumeDef {
    pub name: String,
    pub path: String,
    #[serde(with = "super::bytesize_serde::option")]
    pub size: Option<ByteSize>,
    pub ephemeral: bool,
}
