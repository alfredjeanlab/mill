use std::collections::HashMap;

use mill_config::{SpawnTaskRequest, SpawnTaskResponse, TaskResponse};

use crate::client::MillClient;
use crate::error::CliError;
use crate::output::table;
use crate::output::{self, OutputMode};

pub struct SpawnArgs {
    pub task: Option<String>,
    pub image: Option<String>,
    pub cpu: Option<f64>,
    pub memory: Option<String>,
    pub timeout: Option<String>,
    pub envs: Vec<String>,
}

pub async fn spawn(client: &MillClient, args: SpawnArgs, mode: OutputMode) -> Result<(), CliError> {
    if args.task.is_none() && args.image.is_none() {
        return Err(CliError::Arg(
            "either a task template name or --image is required".to_string(),
        ));
    }

    let env = parse_env_pairs(&args.envs)?;

    let body = SpawnTaskRequest {
        task: args.task,
        image: args.image,
        cpu: args.cpu.unwrap_or(mill_config::default_cpu()),
        memory: args.memory.unwrap_or_else(mill_config::default_memory),
        env,
        timeout: args.timeout,
    };

    let resp: SpawnTaskResponse = client.post_json("/v1/tasks/spawn", &body).await?;

    output::print(mode, &resp, |r| {
        println!("  id:      {}", r.id);
        if let Some(ref addr) = r.address {
            println!("  address: {addr}");
        }
        println!("  status:  {}", r.status);
    });

    Ok(())
}

pub async fn ps(client: &MillClient, mode: OutputMode) -> Result<(), CliError> {
    let tasks: Vec<TaskResponse> = client.get("/v1/tasks").await?;

    output::print_list(mode, &tasks, |tasks| {
        if tasks.is_empty() {
            println!("no running tasks");
        } else {
            table::print_tasks(tasks);
        }
    });

    Ok(())
}

pub async fn kill(client: &MillClient, task_id: &str) -> Result<(), CliError> {
    let path = format!("/v1/tasks/{task_id}");
    client.delete(&path).await?;
    eprintln!("task {task_id} killed");
    Ok(())
}

fn parse_env_pairs(envs: &[String]) -> Result<HashMap<String, String>, CliError> {
    let mut map = HashMap::new();
    for pair in envs {
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            CliError::Arg(format!("invalid env format: {pair} (expected KEY=VALUE)"))
        })?;
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}
