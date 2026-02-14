use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Raw HCL-deserialized config. Strings for sizes/durations — not yet validated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RawConfig {
    #[serde(default)]
    pub service: HashMap<String, RawServiceDef>,
    #[serde(default)]
    pub task: HashMap<String, RawTaskDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RawServiceDef {
    pub image: String,
    pub port: u16,
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    #[serde(default = "default_cpu")]
    pub cpu: f64,
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub health: Option<RawHealthCheck>,
    #[serde(default)]
    pub route: HashMap<String, RawRouteDef>,
    #[serde(default)]
    pub volume: HashMap<String, RawVolumeDef>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RawTaskDef {
    pub image: String,
    #[serde(default = "default_cpu")]
    pub cpu: f64,
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_timeout")]
    pub timeout: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RawHealthCheck {
    pub path: String,
    #[serde(default = "default_health_interval")]
    pub interval: String,
    #[serde(default)]
    pub failure_threshold: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RawRouteDef {
    pub path: String,
    #[serde(default)]
    pub websocket: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RawVolumeDef {
    pub path: String,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub ephemeral: bool,
}

fn default_replicas() -> u32 {
    1
}

pub fn default_cpu() -> f64 {
    0.25
}

pub fn default_memory() -> String {
    "256M".to_owned()
}

fn default_timeout() -> String {
    "1h".to_owned()
}

fn default_health_interval() -> String {
    "10s".to_owned()
}
