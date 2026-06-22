use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

type XtaskResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const DEFAULT_REAL_DATA_DIR: &str =
    "/Users/josephthomasthachil/Desktop/Domyn/uda/KG_VIEWS/Qwen2.5-72B-Instruct/reflection";

fn main() -> XtaskResult<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("verify-internal-beta") => verify_internal_beta(VerifyOptions::parse(args)?),
        Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!("unknown xtask command: {other}").into()),
    }
}

#[derive(Debug, Clone, Default)]
struct VerifyOptions {
    quick: bool,
    skip_real_data_bench: bool,
    skip_docker: bool,
    skip_neo4j_driver: bool,
    run_http_smoke: bool,
}

impl VerifyOptions {
    fn parse<I>(args: I) -> XtaskResult<Self>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let mut options = Self::default();
        for arg in args.into_iter().map(Into::into) {
            match arg.as_str() {
                "--quick" => options.quick = true,
                "--skip-real-data-bench" => options.skip_real_data_bench = true,
                "--skip-docker" => options.skip_docker = true,
                "--skip-neo4j-driver" => options.skip_neo4j_driver = true,
                "--run-http-smoke" => options.run_http_smoke = true,
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown verify flag: {other}").into()),
            }
        }
        Ok(options)
    }
}

fn verify_internal_beta(options: VerifyOptions) -> XtaskResult<()> {
    let root = repo_root()?;
    env::set_current_dir(&root)?;

    println!("== Domyn Nexus internal beta verification ==");
    println!("repo: {}", root.display());
    if options.quick {
        println!("mode: quick (release benchmarks, Docker, and optional driver smoke are skipped)");
    }

    run(CommandSpec::new("cargo").args(["fmt", "--check"]))?;
    run(CommandSpec::new("cargo").args(["test", "--workspace"]))?;
    run(CommandSpec::new("cargo").args([
        "test",
        "-p",
        "nexus-cypher",
        "--test",
        "tck_runner",
        "--",
        "--nocapture",
    ]))?;
    run(CommandSpec::new("cargo")
        .env("TCK_INCLUDE_UPSTREAM_SKIPPED", "1")
        .args([
            "test",
            "-p",
            "nexus-cypher",
            "--test",
            "tck_runner",
            "--",
            "--nocapture",
        ]))?;
    run(CommandSpec::new("cargo").args([
        "test",
        "-p",
        "nexus-cypher",
        "--test",
        "graphrag_readiness",
    ]))?;
    run(CommandSpec::new("cargo").args(["check", "-p", "nexus-server", "--bin", "nexus-server"]))?;

    if options.quick {
        println!("skip: release benchmark smoke (--quick)");
    } else {
        run(CommandSpec::new("cargo").args([
            "run",
            "-p",
            "nexus-bench",
            "--release",
            "--bin",
            "graphrag_retrieval_bench",
            "--",
            "--assert-internal-beta",
        ]))?;
        maybe_run_real_data_bench(&options)?;
    }

    if options.run_http_smoke {
        run(CommandSpec::new("bash").args(["scripts/smoke-dev-single-node.sh"]))?;
    } else {
        println!("skip: HTTP single-node smoke (pass --run-http-smoke to enable)");
    }

    maybe_run_neo4j_driver_smoke(&options)?;
    maybe_run_docker_build(&options)?;

    println!("internal beta verification passed");
    Ok(())
}

fn maybe_run_real_data_bench(options: &VerifyOptions) -> XtaskResult<()> {
    if options.skip_real_data_bench {
        println!("skip: real-data benchmark (--skip-real-data-bench)");
        return Ok(());
    }

    let real_data_dir = env::var("DOMYN_NEXUS_REAL_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_REAL_DATA_DIR));
    let required = env_flag("DOMYN_NEXUS_REAL_DATA_REQUIRED");
    if !real_data_dir.exists() {
        let msg = format!(
            "real-data benchmark dataset not found at {}",
            real_data_dir.display()
        );
        if required {
            return Err(msg.into());
        }
        println!("skip: {msg}");
        println!("      set DOMYN_NEXUS_REAL_DATA_DIR or DOMYN_NEXUS_REAL_DATA_REQUIRED=1");
        return Ok(());
    }

    run(CommandSpec::new("cargo").args([
        "run",
        "-p",
        "nexus-bench",
        "--release",
        "--bin",
        "real_data_bench",
        "--",
        "--assert-internal-beta",
    ]))
}

fn maybe_run_neo4j_driver_smoke(options: &VerifyOptions) -> XtaskResult<()> {
    if options.quick || options.skip_neo4j_driver {
        println!("skip: Neo4j Python driver smoke");
        return Ok(());
    }
    if !env_flag("DOMYN_NEXUS_RUN_NEO4J_DRIVER_SMOKE") {
        println!("skip: Neo4j Python driver smoke (set DOMYN_NEXUS_RUN_NEO4J_DRIVER_SMOKE=1)");
        return Ok(());
    }
    run(CommandSpec::new("bash").args([
        "scripts/smoke-dev-single-node.sh",
        "python3",
        "scripts/neo4j-driver-smoke.py",
    ]))
}

fn maybe_run_docker_build(options: &VerifyOptions) -> XtaskResult<()> {
    if options.quick || options.skip_docker {
        println!("skip: Docker build and container smoke");
        return Ok(());
    }
    if !command_available("docker") {
        println!("skip: Docker build and container smoke (docker not found)");
        return Ok(());
    }
    run(CommandSpec::new("docker").args(["build", "-t", "domyn-nexus:internal-beta", "."]))?;
    run(CommandSpec::new("bash").args(["scripts/smoke-docker-single-node.sh"]))
}

fn repo_root() -> XtaskResult<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| "could not locate repository root from CARGO_MANIFEST_DIR".into())
}

fn command_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

#[derive(Debug, Clone)]
struct CommandSpec {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl CommandSpec {
    fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

fn run(spec: CommandSpec) -> XtaskResult<()> {
    println!("+ {} {}", spec.program, spec.args.join(" "));
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "command failed with status {status}: {} {}",
            spec.program,
            spec.args.join(" ")
        )
        .into())
    }
}

fn print_help() {
    println!(
        "Usage:\n\
           cargo xtask verify-internal-beta [--quick] [--skip-real-data-bench] [--skip-docker] [--skip-neo4j-driver] [--run-http-smoke]\n\n\
         Environment:\n\
           DOMYN_NEXUS_REAL_DATA_DIR             real-data benchmark dataset root\n\
           DOMYN_NEXUS_REAL_DATA_REQUIRED=1      fail if real-data benchmark dataset is missing\n\
           DOMYN_NEXUS_RUN_NEO4J_DRIVER_SMOKE=1  run optional Neo4j driver smoke\n\
           DOMYN_NEXUS_HTTP_ADDR                  dev HTTP bind for self-start smoke, default 127.0.0.1:18080\n\
           DOMYN_NEXUS_BOLT_ADDR                  dev Bolt bind for self-start smoke, default 127.0.0.1:17687\n\
           DOMYN_NEXUS_DOCKER_HTTP_PORT          host HTTPS port for Docker smoke, default 18443\n\
           DOMYN_NEXUS_DOCKER_BOLT_PORT          host Bolt port for Docker smoke, default 17687"
    );
}
