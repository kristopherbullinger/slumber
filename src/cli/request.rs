use crate::{
    cli::Subcommand,
    collection::{CollectionFile, ProfileId, RecipeId},
    config::Config,
    db::Database,
    http::{HttpEngine, RecipeOptions, Request, RequestBuilder},
    template::{Prompt, Prompter, TemplateContext, TemplateError},
    util::{MaybeStr, ResultExt},
    GlobalArgs,
};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use clap::Parser;
use dialoguer::{console::Style, Input, Password};
use indexmap::IndexMap;
use itertools::Itertools;
use reqwest::header::HeaderMap;
use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
    process::ExitCode,
    str::FromStr,
};
use tracing::warn;

/// Exit code to return when `exit_status` flag is set and the HTTP response has
/// an error status code
const HTTP_ERROR_EXIT_CODE: u8 = 2;

/// Execute a single request, and print its response
#[derive(Clone, Debug, Parser)]
#[clap(aliases=&["req", "rq"])]
pub struct RequestCommand {
    #[clap(flatten)]
    build_request: BuildRequestCommand,

    /// Print HTTP response status
    #[clap(long)]
    status: bool,

    /// Print HTTP request and response headers
    #[clap(long)]
    headers: bool,

    /// Do not print HTTP response body
    #[clap(long)]
    no_body: bool,

    /// Set process exit code based on HTTP response status. If the status is
    /// <400, exit code is 0. If it's >=400, exit code is 2.
    #[clap(long)]
    exit_status: bool,

    /// Just print the generated request, instead of sending it. Triggered
    /// sub-requests will also not be executed.
    #[clap(long)]
    dry_run: bool,
}

/// A helper for any subcommand that needs to build requests. This handles
/// common args, as well as setting up context for rendering requests
#[derive(Clone, Debug, Parser)]
pub struct BuildRequestCommand {
    /// ID of the recipe to render into a request
    recipe_id: RecipeId,

    /// ID of the profile to pull template values from
    #[clap(long = "profile", short)]
    profile: Option<ProfileId>,

    /// List of key=value template field overrides
    #[clap(
        long = "override",
        short = 'o',
        value_parser = parse_key_val::<String, String>,
    )]
    overrides: Vec<(String, String)>,
}

#[async_trait]
impl Subcommand for RequestCommand {
    async fn execute(self, global: GlobalArgs) -> anyhow::Result<ExitCode> {
        let (http_engine, request) = self
            .build_request
            // Don't execute sub-requests in a dry run
            .build_request(global, !self.dry_run)
            .await
            .map_err(|error| {
                // If the build failed because triggered requests are disabled,
                // replace it with a custom error message
                if TemplateError::has_trigger_disabled_error(&error) {
                    error.context(
                        "Triggered requests are disabled with `--dry-run`",
                    )
                } else {
                    error
                }
            })?;

        // HTTP engine will be defined iff dry_run was not enabled
        if let Some(http_engine) = http_engine {
            // Everything other than the body prints to stderr, to make it easy
            // to pipe the body to a file
            if self.headers {
                eprintln!("{}", HeaderDisplay(&request.headers));
            }

            // Run the request
            let record = http_engine.send(request.into()).await?;
            let status = record.response.status;

            // Print stuff!
            if self.status {
                eprintln!("{}", status.as_u16());
            }
            if self.headers {
                eprintln!("{}", HeaderDisplay(&record.response.headers));
            }
            if !self.no_body {
                // If body is not UTF-8, write the raw bytes instead (e.g if
                // downloading an image)
                let body = &record.response.body;
                if let Some(text) = body.text() {
                    print!("{}", text);
                } else {
                    io::stdout()
                        .write(body.bytes())
                        .context("Error writing to stdout")?;
                }
            }

            if self.exit_status && status.as_u16() >= 400 {
                Ok(ExitCode::from(HTTP_ERROR_EXIT_CODE))
            } else {
                Ok(ExitCode::SUCCESS)
            }
        } else {
            println!("{:#?}", request);
            Ok(ExitCode::SUCCESS)
        }
    }
}

impl BuildRequestCommand {
    /// Render the request specified by the user. This returns the HTTP engine
    /// too so it can be re-used if necessary (iff `trigger_dependencies` is
    /// enabled).
    ///
    /// `trigger_dependencies` controls whether chained requests can be executed
    /// if their triggers apply.
    pub async fn build_request(
        self,
        global: GlobalArgs,
        trigger_dependencies: bool,
    ) -> anyhow::Result<(Option<HttpEngine>, Request)> {
        let collection_path = CollectionFile::try_path(global.file)?;
        let database = Database::load()?.into_collection(&collection_path)?;
        let collection_file = CollectionFile::load(collection_path).await?;
        let collection = collection_file.collection;
        // Passing the HTTP engine is how we tell the template renderer that
        // it's ok to execute subrequests during render
        let http_engine = if trigger_dependencies {
            let config = Config::load()?;
            Some(HttpEngine::new(&config, database.clone()))
        } else {
            None
        };

        // Validate profile ID, so we can provide a good error if it's invalid
        if let Some(profile_id) = &self.profile {
            collection.profiles.get(profile_id).ok_or_else(|| {
                anyhow!(
                    "No profile with ID `{profile_id}`; options are: {}",
                    collection.profiles.keys().format(", ")
                )
            })?;
        }

        // Find recipe by ID
        let recipe = collection
            .recipes
            .get_recipe(&self.recipe_id)
            .ok_or_else(|| {
                anyhow!(
                    "No recipe with ID `{}`; options are: {}",
                    self.recipe_id,
                    collection.recipes.recipe_ids().format(", ")
                )
            })?
            .clone();

        // Build the request
        let overrides: IndexMap<_, _> = self.overrides.into_iter().collect();
        let template_context = TemplateContext {
            selected_profile: self.profile,
            collection,
            http_engine: http_engine.clone(),
            database,
            overrides,
            prompter: Box::new(CliPrompter),
            recursion_count: Default::default(),
        };
        let request = RequestBuilder::new(recipe, RecipeOptions::default())
            .build(&template_context)
            .await?;
        Ok((http_engine, request))
    }
}

/// Prompt the user for input on the CLI
#[derive(Debug)]
struct CliPrompter;

impl Prompter for CliPrompter {
    fn prompt(&self, prompt: Prompt) {
        // This will implicitly queue the prompts by blocking the main thread.
        // Since the CLI has nothing else to do while waiting on a response,
        // that's fine.
        let result = if prompt.sensitive {
            // Dialoguer doesn't support default values here so there's nothing
            // we can do
            if prompt.default.is_some() {
                warn!(
                    "Default value not supported for sensitive prompts in CLI"
                );
            }

            Password::new()
                .with_prompt(prompt.message)
                .allow_empty_password(true)
                .interact()
        } else {
            let mut input =
                Input::new().with_prompt(prompt.message).allow_empty(true);
            if let Some(default) = prompt.default {
                input = input.default(default);
            }
            input.interact()
        };

        // If we failed to read the value, print an error and report nothing
        if let Ok(value) =
            result.context("Error reading value from prompt").traced()
        {
            prompt.channel.respond(value);
        }
    }
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
        .ok_or_else(|| format!("invalid key=value: no \"=\" found in `{s}`"))?;
    Ok((key.parse()?, value.parse()?))
}

/// Wrapper making it easy to print a header map
struct HeaderDisplay<'a>(&'a HeaderMap);

impl<'a> Display for HeaderDisplay<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let key_style = Style::new().bold();
        for (key, value) in self.0 {
            writeln!(
                f,
                "{}: {}",
                key_style.apply_to(key),
                MaybeStr(value.as_bytes()),
            )?;
        }
        Ok(())
    }
}
