//! The `stella-serve` binary — run the engine service from environment config.
//!
//! ```text
//! STELLA_SERVE_BIND   address to bind (default 127.0.0.1:8080; container: 0.0.0.0:8080)
//! STELLA_SERVE_TOKEN  bearer token every request must present (required)
//! STELLA_SERVE_TOOLS  must be `remote` (the default) — all tool execution is
//!                     remoted to the host; a local tool surface is never served
//! ```
//!
//! `stella-serve healthcheck` probes `/healthz` on the bind port and exits 0/1,
//! so a container HEALTHCHECK needs no extra tooling in the runtime image.

use std::process::ExitCode;

use stella_serve::{ServeConfig, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEFAULT_BIND: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => run().await,
        Some("healthcheck") => healthcheck().await,
        Some(other) => {
            eprintln!("stella-serve: unknown argument `{other}` (expected none, or `healthcheck`)");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> ExitCode {
    let bind = std::env::var("STELLA_SERVE_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let addr = match bind.parse() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("stella-serve: invalid STELLA_SERVE_BIND `{bind}`: {err}");
            return ExitCode::FAILURE;
        }
    };
    let token = match std::env::var("STELLA_SERVE_TOKEN") {
        Ok(token) if !token.is_empty() => token,
        _ => {
            eprintln!(
                "stella-serve: STELLA_SERVE_TOKEN is required — the bearer token every request must present"
            );
            return ExitCode::FAILURE;
        }
    };
    // Server mode is remote-only: the engine holds no local tool surface, so any
    // other value is a misconfiguration we refuse rather than silently ignore.
    if let Ok(tools) = std::env::var("STELLA_SERVE_TOOLS")
        && tools != "remote"
    {
        eprintln!(
            "stella-serve: STELLA_SERVE_TOOLS=`{tools}` is unsupported; only `remote` is served — every tool and model call is remoted to the host"
        );
        return ExitCode::FAILURE;
    }

    let config = ServeConfig { bind: addr, token };
    match serve(config, |bound| {
        println!("stella-serve listening on {bound}")
    })
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("stella-serve: server error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn healthcheck() -> ExitCode {
    let bind = std::env::var("STELLA_SERVE_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let port = bind
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    match probe_health(port).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("stella-serve: healthcheck: /healthz did not return 200");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("stella-serve: healthcheck: could not reach the server: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn probe_health(port: u16) -> std::io::Result<bool> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response.lines().next().is_some_and(|l| l.contains("200")))
}
