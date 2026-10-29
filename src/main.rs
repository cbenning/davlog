use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use davlog::app::{self, AppError, AppExit, Options};
use davlog::bootstrap::{bootstrap_flag, Terminal};
use davlog::brand;
use davlog::config::Config;

/// How long to wait before exiting with the "needs an answer" code, so a container
/// restart loop does not flood the logs.
const NEEDS_ANSWER_DELAY: Duration = Duration::from_secs(5);

fn usage() -> String {
    format!(
        "{name} {version}\n\n\
         Usage: {name} [--config <path>] [--bootstrap clone|init|abort]\n\
         \x20      {name} hash-password     (reads a password on stdin, prints an argon2 hash)\n\n\
         \x20 --config <path>      configuration file (default {config})\n\
         \x20 --bootstrap <choice> answer the first-start question without a prompt\n\
         \x20                      (or set {env}_BOOTSTRAP)\n\
         \x20 -h, --help           show this help\n",
        name = brand::NAME,
        version = env!("CARGO_PKG_VERSION"),
        config = brand::DEFAULT_CONFIG_PATH,
        env = brand::ENV_PREFIX,
    )
}

fn config_path(args: &[String]) -> Result<String, String> {
    let mut path = brand::DEFAULT_CONFIG_PATH.to_string();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--config" {
            path = iter.next().ok_or("--config needs a path")?.clone();
        } else if let Some(v) = arg.strip_prefix("--config=") {
            path = v.to_string();
        }
    }
    Ok(path)
}

async fn termination_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// `davlog hash-password`: read a password from stdin, print an argon2 hash for
/// `auth.password_hash`.
fn hash_password_command() -> ExitCode {
    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        eprintln!("{}: could not read the password from stdin", brand::NAME);
        return ExitCode::FAILURE;
    }
    let password = input.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        eprintln!("{}: the password is empty", brand::NAME);
        return ExitCode::FAILURE;
    }
    match davlog::auth::hash_password(password) {
        Ok(hash) => {
            println!("{hash}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}: could not hash the password: {e}", brand::NAME);
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("hash-password") {
        return hash_password_command();
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", usage());
        return ExitCode::SUCCESS;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let env = |key: &str| std::env::var(key).ok();
    let setup = (|| -> Result<_, String> {
        let path = config_path(&args)?;
        let cfg = Config::load(std::path::Path::new(&path), &env).map_err(|e| e.to_string())?;
        let flag = bootstrap_flag(&args, &env).map_err(|e| format!("{e:#}"))?;
        Ok((cfg, flag))
    })();
    let (cfg, flag) = match setup {
        Ok(pair) => pair,
        Err(message) => {
            eprintln!("{}: {message}\n\n{}", brand::NAME, usage());
            return ExitCode::from(2);
        }
    };

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if cfg.server.worker_threads > 0 {
        builder.worker_threads(cfg.server.worker_threads);
    }
    let runtime = builder.build().expect("build tokio runtime");

    let options = Options {
        bootstrap: flag,
        extra_notifiers: Vec::new(),
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let outcome = runtime.block_on(async {
        if interactive {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            let mut output = std::io::stdout();
            let mut terminal = Terminal::new(&mut input, &mut output);
            app::run(cfg, options, Some(&mut terminal), termination_signal()).await
        } else {
            app::run(cfg, options, None, termination_signal()).await
        }
    });

    match outcome {
        Ok(AppExit::Clean) => ExitCode::SUCCESS,
        Ok(AppExit::Aborted) => {
            eprintln!(
                "{}: aborted at the bootstrap question; nothing was changed",
                brand::NAME
            );
            ExitCode::SUCCESS
        }
        Err(e @ AppError::NeedsAnswer(_)) => {
            eprintln!("{e}");
            std::thread::sleep(NEEDS_ANSWER_DELAY);
            ExitCode::from(e.exit_code() as u8)
        }
        Err(e) => {
            eprintln!("{}: {e}", brand::NAME);
            ExitCode::from(e.exit_code() as u8)
        }
    }
}
