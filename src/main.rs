#![deny(clippy::all)]
#![feature(iterator_try_collect)]
#![feature(try_blocks)]

mod config;
#[cfg(test)]
mod factory;
mod http;
mod repository;
mod template;
mod tui;
mod util;

use crate::{
    config::RequestCollection,
    http::{HttpEngine, Request},
    repository::Repository,
    template::TemplateContext,
    tui::Tui,
    util::{find_by, ResultExt},
};
use anyhow::Context;
use clap::Parser;
use indexmap::IndexMap;
use std::{
    error::Error,
    path::{Path, PathBuf},
    str::FromStr,
};
use tracing_subscriber::{filter::EnvFilter, prelude::*};

#[derive(Debug, Parser)]
#[clap(
    author,
    version,
    about,
    long_about = "Configurable REST client with both TUI and CLI interfaces"
)]
struct Args {
    /// Collection file, which defines your environments and recipes. If
    /// omitted, check for the following files in the current directory
    /// (first match will be used): slumber.yml, slumber.yaml, .slumber.yml,
    /// .slumber.yaml
    #[clap(long, short)]
    collection: Option<PathBuf>,

    /// Subcommand to execute. If omitted, run the TUI
    #[command(subcommand)]
    subcommand: Option<Subcommand>,
}

#[derive(Clone, Debug, clap::Subcommand)]
enum Subcommand {
    /// Execute a single request
    #[clap(aliases=&["req", "rq"])]
    Request {
        /// ID of the request to execute
        request_id: String,

        /// ID of the environment to pull template values from
        #[clap(long = "env", short)]
        environment: Option<String>,

        /// List of key=value overrides
        #[clap(
            long = "override",
            short = 'o',
            value_parser = parse_key_val::<String, String>,
        )]
        overrides: Vec<(String, String)>,

        /// Just print the generated request, instead of sending it
        #[clap(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() {
    // Global initialization
    initialize_tracing().unwrap();
    let args = Args::parse();
    // This won't panic at the failure site because it can also be called
    // mid-TUI execution
    let (collection_file, collection) =
        RequestCollection::load(args.collection.as_deref())
            .await
            .expect("Error loading collection");

    // Select mode based on whether request ID(s) were given
    match args.subcommand {
        // Run the TUI
        None => {
            Tui::start(collection_file.to_owned(), collection);
        }

        // Execute one request without a TUI
        Some(subcommand) => {
            if let Err(err) = execute_subcommand(collection, subcommand).await {
                eprintln!("{err:#}");
            }
        }
    }
}

/// Execute a non-TUI command
async fn execute_subcommand(
    collection: RequestCollection,
    subcommand: Subcommand,
) -> anyhow::Result<()> {
    match subcommand {
        Subcommand::Request {
            request_id,
            environment,
            overrides,
            dry_run,
        } => {
            // Find environment and recipe by ID
            let environment = environment
                .map(|environment| {
                    Ok::<_, anyhow::Error>(
                        find_by(
                            collection.environments,
                            |e| &e.id,
                            &environment,
                            "No environment with ID",
                        )?
                        .data,
                    )
                })
                .transpose()?
                .unwrap_or_default();
            let recipe = find_by(
                collection.requests,
                |r| &r.id,
                &request_id,
                "No request recipe with ID",
            )?;

            // Build the request
            let mut repository = Repository::load()?;
            let overrides: IndexMap<_, _> = overrides.into_iter().collect();
            let request = Request::build(
                &recipe,
                &TemplateContext {
                    environment,
                    overrides,
                    chains: collection.chains,
                    repository: repository.clone(),
                },
            )
            .await?;

            if dry_run {
                println!("{:#?}", request);
            } else {
                // Register the request in the repo *before* launching it, in
                // case it fails
                let http_engine = HttpEngine::new();
                let future = http_engine.send(&request);
                let record_id = repository.add_request(request).await?.id();

                // For Ok, we have to move the response, so print it *first*.
                // For Err, we want to return the owned error. Fortunately the
                // record stores it as a string, so we can pass a reference.
                match future.await {
                    Ok(response) => {
                        print!("{}", response.body);
                        repository
                            .add_response(
                                record_id,
                                Ok::<_, anyhow::Error>(response),
                            )
                            .await?;
                    }
                    Err(err) => {
                        // This error shouldn't hide the HTTP error, so trace
                        // it instead of returning it
                        let _ = repository
                            .add_response(record_id, Err(&err))
                            .await
                            .traced();
                        return Err(err);
                    }
                }
            }
            Ok(())
        }
    }
}

/// Set up tracing to log to a file
fn initialize_tracing() -> anyhow::Result<()> {
    let directory = Path::new("./log/");
    std::fs::create_dir_all(directory)
        .context(format!("Error creating log directory {directory:?}"))?;
    let log_path = directory.join("ratatui-app.log");
    let log_file = std::fs::File::create(log_path)?;
    let file_subscriber = tracing_subscriber::fmt::layer()
        .with_file(true)
        .with_line_number(true)
        .with_writer(log_file)
        .with_target(false)
        .with_ansi(false)
        .with_filter(EnvFilter::from_default_env());
    tracing_subscriber::registry().with(file_subscriber).init();
    Ok(())
}

/// Parse a single key=value pair for an argument
fn parse_key_val<T, U>(
    s: &str,
) -> Result<(T, U), Box<dyn Error + Send + Sync + 'static>>
where
    T: FromStr,
    T::Err: Error + Send + Sync + 'static,
    U: FromStr,
    U::Err: Error + Send + Sync + 'static,
{
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("invalid key=value: no \"=\" found in {s:?}"))?;
    Ok((key.parse()?, value.parse()?))
}
