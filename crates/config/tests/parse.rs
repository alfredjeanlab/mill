use std::time::Duration;

use bytesize::ByteSize;

use mill_config::{ClusterConfig, EnvPart, EnvValue};
use yare::parameterized;

fn read_example(name: &str) -> String {
    let path = format!(
        "{}/examples/{name}",
        env!("CARGO_MANIFEST_DIR")
            .strip_suffix("/crates/config")
            .unwrap_or(env!("CARGO_MANIFEST_DIR"))
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        // Try from workspace root
        let alt = format!("{}/../../examples/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&alt).unwrap_or_else(|_| {
            panic!("could not read example {name}: {e} (tried {path} and {alt})")
        })
    })
}

// --- Parse examples ---

#[test]
fn parse_hello_example() -> Result<(), Box<dyn std::error::Error>> {
    let input = read_example("hello.mill");
    let config = mill_config::parse(&input)?;
    assert_eq!(config.services.len(), 1);
    assert!(config.tasks.is_empty());

    let hello = &config.services["hello"];
    assert_eq!(hello.image, "nginx:alpine");
    assert_eq!(hello.port, 80);
    assert_eq!(hello.replicas, 1);
    assert_eq!(hello.resources.cpu, 0.25);
    assert_eq!(hello.resources.memory, ByteSize::mb(128));
    assert_eq!(hello.routes.len(), 1);
    assert_eq!(hello.routes[0].hostname, "hello.example.com");
    assert_eq!(hello.routes[0].path, "/");
    Ok(())
}

#[test]
fn parse_backend_example() -> Result<(), Box<dyn std::error::Error>> {
    let input = read_example("backend.mill");
    let config = mill_config::parse(&input)?;
    assert_eq!(config.services.len(), 1);
    assert_eq!(config.tasks.len(), 2);

    let api = &config.services["api"];
    assert_eq!(api.replicas, 2);
    assert_eq!(api.resources.cpu, 1.0);
    assert_eq!(api.resources.memory, ByteSize::mb(512));
    assert!(api.health.is_some());
    assert_eq!(api.env.len(), 3);

    // Check that ENVIRONMENT is a literal
    assert_eq!(api.env["ENVIRONMENT"], EnvValue::Literal("production".to_owned()));

    // Check secret refs
    assert_eq!(
        api.env["DATABASE_URL"],
        EnvValue::Interpolated {
            parts: vec![EnvPart::Secret { name: "api.database-url".to_owned() }]
        }
    );

    let report = &config.tasks["report-generator"];
    assert_eq!(report.timeout, Duration::from_secs(30 * 60));
    assert_eq!(report.resources.cpu, 2.0);

    let migration = &config.tasks["migration"];
    assert_eq!(migration.timeout, Duration::from_secs(10 * 60));
    Ok(())
}

#[test]
fn parse_frontend_example() -> Result<(), Box<dyn std::error::Error>> {
    let input = read_example("frontend.mill");
    let config = mill_config::parse(&input)?;
    assert_eq!(config.services.len(), 3);
    assert!(config.tasks.is_empty());

    let web = &config.services["web"];
    assert_eq!(
        web.env["REDIS_URL"],
        EnvValue::Interpolated {
            parts: vec![
                EnvPart::Literal("tcp://".to_owned()),
                EnvPart::ServiceAddress { service: "redis".to_owned() },
            ]
        }
    );

    let redis = &config.services["redis"];
    assert_eq!(redis.volumes.len(), 1);
    assert_eq!(redis.volumes[0].name, "redis-data");
    assert_eq!(redis.volumes[0].size, Some(ByteSize::gib(1)));
    Ok(())
}

#[test]
fn parse_config_md_example() -> Result<(), Box<dyn std::error::Error>> {
    let input = read_example("../docs/config.md");
    // Extract the HCL block from markdown
    let start = input.find("```hcl\n").map(|i| i + 7).ok_or("no hcl block found")?;
    let end = input[start..].find("\n```").map(|i| i + start).ok_or("no closing fence found")?;
    let hcl = &input[start..end];
    let config = mill_config::parse(hcl)?;
    assert_eq!(config.services.len(), 3);
    assert_eq!(config.tasks.len(), 1);
    Ok(())
}

// --- Defaults ---

#[test]
fn defaults_applied() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"
        service "minimal" {
            image = "test:latest"
            port  = 8080
        }
        task "minimal" {
            image = "test:latest"
        }
    "#;
    let config = mill_config::parse(input)?;

    let svc = &config.services["minimal"];
    assert_eq!(svc.replicas, 1);
    assert_eq!(svc.resources.cpu, 0.25);
    assert_eq!(svc.resources.memory, ByteSize::mb(256));

    let task = &config.tasks["minimal"];
    assert_eq!(task.resources.cpu, 0.25);
    assert_eq!(task.resources.memory, ByteSize::mb(256));
    assert_eq!(task.timeout, Duration::from_secs(3600));
    Ok(())
}

// --- Validation errors ---

#[parameterized(
    unknown_service_ref = {
        r#"service "web" {
            image = "test:latest"
            port  = 3000
            env { BACKEND = "tcp://${service.nonexistent.address}" }
        }"#,
        "nonexistent",
    },
    invalid_byte_size = {
        r#"service "bad" {
            image  = "test:latest"
            port   = 8080
            memory = "notasize"
        }"#,
        "memory",
    },
    invalid_duration = {
        r#"task "bad" {
            image   = "test:latest"
            timeout = "notaduration"
        }"#,
        "timeout",
    },
    zero_cpu_service = {
        r#"service "bad" {
            image = "test:latest"
            port  = 8080
            cpu   = 0
        }"#,
        "cpu must be greater than 0",
    },
    zero_cpu_task = {
        r#"task "bad" {
            image = "test:latest"
            cpu   = 0
        }"#,
        "cpu must be greater than 0",
    },
    zero_port = {
        r#"service "bad" {
            image = "test:latest"
            port  = 0
        }"#,
        "port must be greater than 0",
    },
    health_path_without_slash = {
        r#"service "bad" {
            image = "test:latest"
            port  = 8080
            health { path = "health" }
        }"#,
        "health.path",
    },
    persistent_volume_missing_size = {
        r#"service "bad" {
            image = "test:latest"
            port  = 8080
            volume "data" { path = "/data" }
        }"#,
        "volume",
    },
)]
fn parse_rejects_invalid_config(input: &str, expected: &str) {
    let err = mill_config::parse(input).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains(expected), "expected error containing {expected:?}, got: {msg}");
}

#[test]
fn ephemeral_volume_no_size_ok() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"
        service "ok" {
            image = "test:latest"
            port  = 8080
            volume "scratch" {
                path      = "/tmp/scratch"
                ephemeral = true
            }
        }
    "#;
    let config = mill_config::parse(input)?;
    let vol = &config.services["ok"].volumes[0];
    assert!(vol.ephemeral);
    assert!(vol.size.is_none());
    Ok(())
}

// --- Round-trip via JSON ---

#[test]
fn round_trip_json() -> Result<(), Box<dyn std::error::Error>> {
    let input = read_example("frontend.mill");
    let config = mill_config::parse(&input)?;
    let json = serde_json::to_string(&config)?;
    let restored: ClusterConfig = serde_json::from_str(&json)?;
    assert_eq!(config, restored);
    Ok(())
}

// --- Env classification ---

#[test]
fn env_literal() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"
        service "test" {
            image = "test:latest"
            port  = 8080
            env {
                MODE = "production"
            }
        }
    "#;
    let config = mill_config::parse(input)?;
    assert_eq!(config.services["test"].env["MODE"], EnvValue::Literal("production".to_owned()));
    Ok(())
}

#[test]
fn env_secret_ref() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"
        service "test" {
            image = "test:latest"
            port  = 8080
            env {
                DB_PASS = "${secret(db.password)}"
            }
        }
    "#;
    let config = mill_config::parse(input)?;
    assert_eq!(
        config.services["test"].env["DB_PASS"],
        EnvValue::Interpolated { parts: vec![EnvPart::Secret { name: "db.password".to_owned() }] }
    );
    Ok(())
}

#[test]
fn env_service_address_interpolation() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"
        service "db" {
            image = "postgres:16"
            port  = 5432
        }
        service "app" {
            image = "test:latest"
            port  = 8080
            env {
                DB_URL = "postgres://${service.db.address}/mydb"
            }
        }
    "#;
    let config = mill_config::parse(input)?;
    assert_eq!(
        config.services["app"].env["DB_URL"],
        EnvValue::Interpolated {
            parts: vec![
                EnvPart::Literal("postgres://".to_owned()),
                EnvPart::ServiceAddress { service: "db".to_owned() },
                EnvPart::Literal("/mydb".to_owned()),
            ]
        }
    );
    Ok(())
}

#[test]
fn env_mixed_interpolation() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"
        service "db" {
            image = "postgres:16"
            port  = 5432
        }
        service "app" {
            image = "test:latest"
            port  = 8080
            env {
                DSN = "postgres://${secret(db.user)}:${secret(db.pass)}@${service.db.address}/mydb"
            }
        }
    "#;
    let config = mill_config::parse(input)?;
    assert_eq!(
        config.services["app"].env["DSN"],
        EnvValue::Interpolated {
            parts: vec![
                EnvPart::Literal("postgres://".to_owned()),
                EnvPart::Secret { name: "db.user".to_owned() },
                EnvPart::Literal(":".to_owned()),
                EnvPart::Secret { name: "db.pass".to_owned() },
                EnvPart::Literal("@".to_owned()),
                EnvPart::ServiceAddress { service: "db".to_owned() },
                EnvPart::Literal("/mydb".to_owned()),
            ]
        }
    );
    Ok(())
}

// --- parse_raw escape hatch ---

// --- Health check failure threshold ---

#[test]
fn health_check_default_failure_threshold() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"
        service "web" {
            image = "test:latest"
            port  = 8080
            health {
                path = "/health"
            }
        }
    "#;
    let config = mill_config::parse(input)?;
    let hc = config.services["web"].health.as_ref().unwrap();
    assert_eq!(hc.failure_threshold, 3);
    Ok(())
}

#[test]
fn health_check_explicit_failure_threshold() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"
        service "web" {
            image = "test:latest"
            port  = 8080
            health {
                path              = "/health"
                failure_threshold = 5
            }
        }
    "#;
    let config = mill_config::parse(input)?;
    let hc = config.services["web"].health.as_ref().unwrap();
    assert_eq!(hc.failure_threshold, 5);
    Ok(())
}

#[test]
fn parse_raw_returns_strings() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"
        service "test" {
            image  = "test:latest"
            port   = 8080
            memory = "1G"
        }
    "#;
    let raw = mill_config::parse_raw(input)?;
    assert_eq!(raw.service["test"].memory, "1G");
    Ok(())
}
