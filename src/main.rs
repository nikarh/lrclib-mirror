//! lrclib-mirror: LRCLIB's lookup API and web search, answered from a compact store built from LRCLIB's public
//! database dump, which it keeps up to date without restarting.

use lrclib_mirror::{dump, store, web};

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tracing_subscriber::EnvFilter;

use lrclib_mirror::config::{Config, HELP};
use lrclib_mirror::mirror::Mirror;

use lrclib_mirror::VERSION;

fn main() -> ExitCode {
    let mut args = pico_args::Arguments::from_env();
    if args.contains(["-h", "--help"]) {
        let _ = writeln!(std::io::stdout(), "{HELP}");
        return ExitCode::SUCCESS;
    }
    if args.contains(["-V", "--version"]) {
        let _ = writeln!(std::io::stdout(), "lrclib-mirror {VERSION}");
        return ExitCode::SUCCESS;
    }
    let command = match args.subcommand() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let config = match Config::parse(&mut args) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let positional = if command.as_deref() == Some("import") {
        let mut free = Vec::new();
        while let Ok(Some(arg)) = args.opt_free_from_str::<String>() {
            free.push(arg);
        }
        free
    } else {
        Vec::new()
    };
    if let Err(e) = Config::finish(args) {
        return fail(&e);
    }
    let logging = || {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("info,tantivy=warn")),
            )
            .init();
    };

    match command.as_deref() {
        Some("healthcheck") => healthcheck(config.listen),
        Some("import") => {
            let [dump, out] = positional.as_slice() else {
                return fail(&"import needs DUMP and OUT; see --help");
            };
            logging();
            import(dump, std::path::Path::new(out), &config)
        }
        None | Some("serve") => {
            logging();
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => return fail(&e),
            };
            runtime.block_on(serve(config))
        }
        Some(other) => fail(&format!("unknown command {other:?}; see --help")),
    }
}

fn fail(e: &dyn std::fmt::Display) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "lrclib-mirror: {e}");
    ExitCode::FAILURE
}

async fn serve(config: Config) -> ExitCode {
    tracing::info!(version = VERSION, "starting");
    let listen = config.listen;
    let mirror = match Mirror::new(config) {
        Ok(m) => Arc::new(m),
        Err(e) => return fail(&e),
    };
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => return fail(&format!("listening on {listen}: {e}")),
    };
    tracing::info!(%listen, "listening");

    let stop = tokio_util::sync::CancellationToken::new();
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let background = tokio::spawn({
        let (mirror, stop, failed) = (Arc::clone(&mirror), stop.clone(), Arc::clone(&failed));
        async move {
            if let Err(e) = mirror.start().await {
                tracing::error!(error = %e, "can't start");
                failed.store(true, std::sync::atomic::Ordering::Relaxed);
                stop.cancel();
                return;
            }
            mirror.run_updates().await;
        }
    });

    let app = web::router(Arc::clone(&mirror));
    let shutdown = {
        let stop = stop.clone();
        async move {
            tokio::select! {
                () = signal() => tracing::info!("stopping"),
                () = stop.cancelled() => {}
            }
        }
    };
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;
    background.abort();
    mirror.shutdown().await;

    match served {
        Err(e) => fail(&e),
        Ok(()) if failed.load(std::sync::atomic::Ordering::Relaxed) => ExitCode::FAILURE,
        Ok(()) => ExitCode::SUCCESS,
    }
}

async fn signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "no SIGTERM handler");
            let _ = ctrl_c.await;
            return;
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
}

/// Builds a store from a dump file.
fn import(dump: &str, out: &std::path::Path, config: &Config) -> ExitCode {
    if out.exists() {
        return fail(&format!("{} exists", out.display()));
    }
    let input: Box<dyn std::io::Read> = if dump == "-" {
        Box::new(std::io::stdin().lock())
    } else {
        match std::fs::File::open(dump) {
            Ok(f) => Box::new(std::io::BufReader::with_capacity(1 << 20, f)),
            Err(e) => return fail(&format!("{dump}: {e}")),
        }
    };
    let input: Box<dyn std::io::Read> = if std::path::Path::new(dump)
        .extension()
        .is_some_and(|e| e == "gz")
    {
        Box::new(dump::gunzip(input))
    } else {
        input
    };
    let options = store::ImportOptions {
        threads: config.import_threads,
        memory: config.import_memory_bytes,
        ..store::ImportOptions::default()
    };
    let stop = std::sync::atomic::AtomicBool::new(false);
    match store::import(input, out, &options, &stop) {
        Ok(stats) => {
            let _ = serde_json::to_writer_pretty(std::io::stdout(), &stats);
            let _ = writeln!(std::io::stdout());
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

/// For the image's HEALTHCHECK (a scratch image has no curl): GET /healthz on the configured port.
fn healthcheck(listen: SocketAddr) -> ExitCode {
    let addr = SocketAddr::from(([127, 0, 0, 1], listen.port()));
    let check = || -> std::io::Result<bool> {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"))
    };
    match check() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => fail(&"healthcheck: not healthy"),
        Err(e) => fail(&format!("healthcheck: {e}")),
    }
}
