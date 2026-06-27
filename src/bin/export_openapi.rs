//! Stand-alone binary that writes the OpenAPI spec to disk.
//!
//! Usage:
//!     cargo run --bin export-openapi -- [path]
//!
//! Defaults to `./openapi.json` if no path is given. Intended for CI so
//! the Flutter and React projects can consume a versioned contract
//! artifact without having to boot the live server.

use std::{env, fs, process::ExitCode};

use madar_rust::openapi::ApiDoc;
use utoipa::OpenApi;

fn main() -> ExitCode {
    let path = env::args().nth(1).unwrap_or_else(|| "openapi.json".into());

    let spec = match ApiDoc::openapi().to_pretty_json() {
        Ok(json) => json,
        Err(err) => {
            eprintln!("failed to serialize OpenAPI spec: {err}");
            return ExitCode::from(1);
        }
    };

    if let Err(err) = fs::write(&path, &spec) {
        eprintln!("failed to write {path}: {err}");
        return ExitCode::from(1);
    }

    eprintln!("wrote {} ({} bytes)", path, spec.len());
    ExitCode::SUCCESS
}