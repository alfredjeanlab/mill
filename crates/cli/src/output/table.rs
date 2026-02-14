use comfy_table::{ContentArrangement, Table};

use mill_config::{NodeResponse, SecretListItem, ServiceResponse, TaskResponse, VolumeResponse};

/// Format bytes as a human-readable string (e.g. "340M", "2.1G").
fn format_bytes(bytes: u64) -> String {
    let b = bytesize::ByteSize::b(bytes);
    b.to_string()
}

/// Format CPU as a string (e.g. "0.3", "2.1").
fn format_cpu(cpu: f64) -> String {
    if cpu == cpu.floor() { format!("{:.0}", cpu) } else { format!("{:.1}", cpu) }
}

fn new_table() -> Table {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.load_preset(comfy_table::presets::NOTHING);
    table
}

pub fn print_nodes(nodes: &[NodeResponse]) {
    let mut table = new_table();
    table.set_header(vec!["NAME", "STATUS", "CPU", "MEM", "ALLOCS"]);

    for n in nodes {
        let cpu =
            format!("{}/{}", format_cpu(n.cpu_total - n.cpu_available), format_cpu(n.cpu_total));
        let mem = format!(
            "{}/{}",
            format_bytes(n.memory_total_bytes - n.memory_available_bytes),
            format_bytes(n.memory_total_bytes)
        );
        table.add_row(vec![&n.id, &n.status, &cpu, &mem, &n.alloc_count.to_string()]);
    }

    println!("{table}");
}

pub fn print_services(services: &[ServiceResponse]) {
    let mut table = new_table();
    table.set_header(vec!["NAME", "REPLICAS", "IMAGE"]);

    for s in services {
        let replicas = format!("{}/{}", s.replicas.healthy, s.replicas.desired);
        table.add_row(vec![&s.name, &replicas, &s.image]);
    }

    println!("{table}");
}

pub fn print_tasks(tasks: &[TaskResponse]) {
    let mut table = new_table();
    table.set_header(vec!["ID", "NAME", "NODE", "STATUS", "CPU", "MEM"]);

    for t in tasks {
        table.add_row(vec![
            &t.id,
            &t.name,
            &t.node,
            &t.status,
            &format_cpu(t.cpu),
            &format_bytes(t.memory),
        ]);
    }

    println!("{table}");
}

pub fn print_secrets(secrets: &[SecretListItem]) {
    let mut table = new_table();
    table.set_header(vec!["NAME"]);

    for s in secrets {
        table.add_row(vec![&s.name]);
    }

    println!("{table}");
}

pub fn print_volumes(volumes: &[VolumeResponse]) {
    let mut table = new_table();
    table.set_header(vec!["NAME", "STATE", "NODE", "CLOUD ID"]);

    for v in volumes {
        let node = v.node.as_deref().unwrap_or("--");
        table.add_row(vec![&v.name, &v.state, node, &v.cloud_id]);
    }

    println!("{table}");
}
