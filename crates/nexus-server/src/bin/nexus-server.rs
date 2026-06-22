use nexus_core::graph::Graph;
use nexus_server::bolt::BoltServer;
use nexus_server::config::{ConfigValidationError, NexusServerConfig};
use nexus_server::engine::{MultiTenantEngine, NexusEngine};
use nexus_server::http::{
    create_multi_tenant_router_with_config, run_http_server, run_https_server,
};
use nexus_storage::persistence::NexusStore;
use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::oneshot;

const DEV_CONFIG_TEMPLATE: &str = include_str!("../../../../docs/examples/dev-single-node.json");
const PRODUCTION_CONFIG_TEMPLATE: &str =
    include_str!("../../../../docs/examples/production-single-node.json");

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    init_tracing();

    match parse_cli_args(env::args().skip(1))? {
        CliCommand::Start { config_path } => run_server(config_path).await,
        CliCommand::CheckConfig { config_path } => run_check_config(&config_path),
        CliCommand::Init { profile, output } => run_init(profile, output.as_deref()),
        CliCommand::Backup {
            config_path,
            output,
        } => run_backup(&config_path, &output),
        CliCommand::Restore { backup, data_dir } => run_restore(&backup, &data_dir),
        CliCommand::Help => {
            print_help();
            Ok(())
        }
    }
}

async fn run_server(config_path: Option<String>) -> Result<(), Box<dyn Error + Send + Sync>> {
    let config = load_config(config_path.as_deref())?;
    config.validate_for_production()?;

    let engine = Arc::new(load_engine(&config)?);
    let tenants = Arc::new(MultiTenantEngine::new_with_arc(
        "default",
        Arc::clone(&engine),
    ));
    let router = create_multi_tenant_router_with_config(tenants, config.server_config());

    if let Some(bolt) =
        BoltServer::from_config(Arc::clone(&engine), &config.bolt, config.auth_token.clone())
    {
        tokio::spawn(async move {
            if let Err(err) = bolt.run().await {
                tracing::error!(error = %err, "Bolt server stopped");
            }
        });
    }

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(());
        }
    });

    tracing::info!(
        bind_addr = %config.http.bind_addr,
        tls = config.http.tls.is_some(),
        "starting Domyn Nexus HTTP server"
    );

    if let Some(tls) = &config.http.tls {
        run_https_server(router, &config.http.bind_addr, tls, shutdown_rx).await?;
    } else {
        run_http_server(router, &config.http.bind_addr, shutdown_rx).await?;
    }

    Ok(())
}

fn run_check_config(config_path: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    let config = load_config(Some(config_path))?;
    match config.validate_for_production() {
        Ok(()) => {
            println!(
                "configuration ok: production_mode={}, http={}, bolt_enabled={}",
                config.production_mode, config.http.bind_addr, config.bolt.enabled
            );
            Ok(())
        }
        Err(err) => {
            print_config_validation_error(&err);
            Err(Box::new(err))
        }
    }
}

fn run_init(
    profile: InitProfile,
    output: Option<&str>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let template = match profile {
        InitProfile::Dev => DEV_CONFIG_TEMPLATE,
        InitProfile::Production => PRODUCTION_CONFIG_TEMPLATE,
    };

    if let Some(path) = output {
        let path = Path::new(path);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, template)?;
        println!("wrote {}", path.display());
    } else {
        print!("{template}");
    }
    Ok(())
}

fn run_backup(config_path: &str, output: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    let config = load_config(Some(config_path))?;
    config.validate_for_production()?;
    let Some(storage_path) = config.storage_path.as_deref() else {
        return Err("backup requires storage_path in config".into());
    };

    let mut store =
        NexusStore::open_with_options(storage_path, config.server_config().store_options())?;
    let manifest = store.backup_to(output)?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}

fn run_restore(backup: &str, data_dir: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    NexusStore::restore_backup(backup, data_dir)?;
    println!("restored backup from {backup} into {data_dir}");
    Ok(())
}

fn load_config(path: Option<&str>) -> Result<NexusServerConfig, Box<dyn Error + Send + Sync>> {
    let path = path
        .map(str::to_string)
        .or_else(|| env::var("DOMYN_NEXUS_CONFIG").ok());
    let config = if let Some(path) = path {
        NexusServerConfig::load_json_file(&path)?
    } else {
        NexusServerConfig::default()
    };
    Ok(config.with_env_overrides())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Start {
        config_path: Option<String>,
    },
    CheckConfig {
        config_path: String,
    },
    Init {
        profile: InitProfile,
        output: Option<String>,
    },
    Backup {
        config_path: String,
        output: String,
    },
    Restore {
        backup: String,
        data_dir: String,
    },
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitProfile {
    Dev,
    Production,
}

fn parse_cli_args<I, S>(args: I) -> Result<CliCommand, Box<dyn Error + Send + Sync>>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into).peekable();
    let Some(first) = args.next() else {
        return Ok(CliCommand::Start {
            config_path: env::var("DOMYN_NEXUS_CONFIG").ok(),
        });
    };

    match first.as_str() {
        "--help" | "-h" => Ok(CliCommand::Help),
        "check-config" => {
            let config_path = parse_required_flag(&mut args, "--config")?;
            ensure_no_extra_args(&mut args)?;
            Ok(CliCommand::CheckConfig { config_path })
        }
        "init" => {
            let mut profile = None;
            let mut output = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--profile" => {
                        let Some(value) = args.next() else {
                            return Err("--profile requires dev or production".into());
                        };
                        profile = Some(match value.as_str() {
                            "dev" => InitProfile::Dev,
                            "production" => InitProfile::Production,
                            other => {
                                return Err(format!("unknown init profile: {other}").into());
                            }
                        });
                    }
                    "--output" | "-o" => {
                        let Some(value) = args.next() else {
                            return Err("--output requires a path".into());
                        };
                        output = Some(value);
                    }
                    other => return Err(format!("unknown init argument: {other}").into()),
                }
            }
            Ok(CliCommand::Init {
                profile: profile.ok_or("init requires --profile dev|production")?,
                output,
            })
        }
        "backup" => {
            let mut config_path = None;
            let mut output = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--config" | "-c" => {
                        config_path = Some(
                            args.next()
                                .ok_or("backup --config requires a config path")?,
                        );
                    }
                    "--output" | "-o" => {
                        output = Some(args.next().ok_or("backup --output requires a directory")?);
                    }
                    other => return Err(format!("unknown backup argument: {other}").into()),
                }
            }
            Ok(CliCommand::Backup {
                config_path: config_path.ok_or("backup requires --config PATH")?,
                output: output.ok_or("backup requires --output DIR")?,
            })
        }
        "restore" => {
            let mut backup = None;
            let mut data_dir = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--backup" => {
                        backup = Some(args.next().ok_or("restore --backup requires a directory")?);
                    }
                    "--data-dir" => {
                        data_dir = Some(
                            args.next()
                                .ok_or("restore --data-dir requires a directory")?,
                        );
                    }
                    other => return Err(format!("unknown restore argument: {other}").into()),
                }
            }
            Ok(CliCommand::Restore {
                backup: backup.ok_or("restore requires --backup DIR")?,
                data_dir: data_dir.ok_or("restore requires --data-dir DIR")?,
            })
        }
        other => {
            let mut path = env::var("DOMYN_NEXUS_CONFIG").ok();
            let mut pending = Some(other.to_string());
            loop {
                let Some(arg) = pending.take().or_else(|| args.next()) else {
                    break;
                };
                match arg.as_str() {
                    "--config" | "-c" => {
                        let Some(value) = args.next() else {
                            return Err("--config requires a path".into());
                        };
                        path = Some(value);
                    }
                    "--help" | "-h" => {
                        return Ok(CliCommand::Help);
                    }
                    other => {
                        return Err(format!("unknown argument: {other}").into());
                    }
                }
            }

            Ok(CliCommand::Start { config_path: path })
        }
    }
}

fn parse_required_flag<I>(
    args: &mut std::iter::Peekable<I>,
    flag: &str,
) -> Result<String, Box<dyn Error + Send + Sync>>
where
    I: Iterator<Item = String>,
{
    match args.next().as_deref() {
        Some("--config") | Some("-c") if flag == "--config" => args
            .next()
            .ok_or_else(|| format!("{flag} requires a value").into()),
        Some(other) => Err(format!("expected {flag}, got {other}").into()),
        None => Err(format!("{flag} is required").into()),
    }
}

fn ensure_no_extra_args<I>(
    args: &mut std::iter::Peekable<I>,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
    I: Iterator<Item = String>,
{
    if let Some(extra) = args.next() {
        Err(format!("unexpected argument: {extra}").into())
    } else {
        Ok(())
    }
}

fn print_help() {
    println!(
        "Usage:\n\
           nexus-server [--config PATH]\n\
           nexus-server check-config --config PATH\n\
           nexus-server init --profile dev|production [--output PATH]\n\
           nexus-server backup --config PATH --output DIR\n\
           nexus-server restore --backup DIR --data-dir DIR\n\n\
         Environment:\n\
           DOMYN_NEXUS_CONFIG              config path fallback\n\
           DOMYN_NEXUS_PRODUCTION_MODE     true/false override\n\
           DOMYN_NEXUS_HTTP_BIND_ADDR      HTTP bind override\n\
           DOMYN_NEXUS_BOLT_BIND_ADDR      Bolt bind override\n\
           DOMYN_NEXUS_AUTH_TOKEN          legacy admin token override"
    );
}

fn print_config_validation_error(err: &ConfigValidationError) {
    eprintln!("configuration is not production-ready:");
    for issue in &err.issues {
        eprintln!("- [{}] {}: {}", issue.code, issue.field, issue.message);
        eprintln!("  fix: {}", issue.remediation);
    }
}

fn load_engine(config: &NexusServerConfig) -> Result<NexusEngine, Box<dyn Error + Send + Sync>> {
    if let Some(path) = &config.storage_path {
        let store = NexusStore::open_with_options(path, config.server_config().store_options())?;
        let graph = store.load_graph(0, 0)?;
        let engine = NexusEngine::with_store(graph, store);
        let loaded_vectors = engine.load_all_vector_indexes()?;
        if !loaded_vectors.is_empty() {
            tracing::info!(
                count = loaded_vectors.len(),
                "loaded persisted vector indexes"
            );
        }
        Ok(engine)
    } else {
        let mut graph = Graph::new(0, 0);
        graph.build();
        Ok(NexusEngine::new(graph))
    }
}

fn init_tracing() {
    let filter =
        env::var("RUST_LOG").unwrap_or_else(|_| "nexus_server=info,tower_http=info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::properties::PropertyType;
    use nexus_core::types::{Value, VertexId};
    use tempfile::TempDir;

    #[test]
    fn parses_start_with_config() {
        assert_eq!(
            parse_cli_args(["--config", "dev.json"]).unwrap(),
            CliCommand::Start {
                config_path: Some("dev.json".into())
            }
        );
    }

    #[test]
    fn parses_check_config() {
        assert_eq!(
            parse_cli_args(["check-config", "--config", "prod.json"]).unwrap(),
            CliCommand::CheckConfig {
                config_path: "prod.json".into()
            }
        );
    }

    #[test]
    fn parses_init_with_output() {
        assert_eq!(
            parse_cli_args(["init", "--profile", "production", "--output", "config.json"]).unwrap(),
            CliCommand::Init {
                profile: InitProfile::Production,
                output: Some("config.json".into())
            }
        );
    }

    #[test]
    fn parses_backup_and_restore() {
        assert_eq!(
            parse_cli_args(["backup", "--config", "prod.json", "--output", "backups/one"]).unwrap(),
            CliCommand::Backup {
                config_path: "prod.json".into(),
                output: "backups/one".into()
            }
        );
        assert_eq!(
            parse_cli_args(["restore", "--backup", "backups/one", "--data-dir", "data"]).unwrap(),
            CliCommand::Restore {
                backup: "backups/one".into(),
                data_dir: "data".into()
            }
        );
    }

    #[test]
    fn backup_and_restore_cli_helpers_roundtrip() -> Result<(), Box<dyn Error + Send + Sync>> {
        let dir = TempDir::new()?;
        let data_dir = dir.path().join("data");
        let backup_dir = dir.path().join("backup");
        let restore_dir = dir.path().join("restore");
        let config_path = dir.path().join("config.json");

        {
            let mut graph = Graph::new(4, 4);
            graph.register_vertex_property("name", PropertyType::String, true, false);
            let vertex = graph.add_vertex("Entity");
            graph.set_vertex_property(vertex, "name", Value::String("cli-backup".into()));
            graph.build();

            let mut store = NexusStore::open(&data_dir)?;
            store.save_snapshot(&graph)?;
        }

        let config = serde_json::json!({
            "production_mode": false,
            "storage_path": data_dir,
            "http": {
                "backup_root": dir.path().join("allowed-backups")
            },
            "bolt": {
                "enabled": false
            }
        });
        fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;

        run_backup(config_path.to_str().unwrap(), backup_dir.to_str().unwrap())?;
        assert!(backup_dir.join("backup-manifest.json").exists());

        run_restore(backup_dir.to_str().unwrap(), restore_dir.to_str().unwrap())?;
        let restored = NexusStore::open(&restore_dir)?;
        let graph = restored.load_graph(4, 4)?;
        assert_eq!(
            graph.get_vertex_property(VertexId(0), "name"),
            Value::String("cli-backup".into())
        );

        Ok(())
    }
}
