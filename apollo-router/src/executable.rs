//! Main entry point for CLI command to start server.

use crate::{
    configuration::Configuration,
    subscriber::{set_global_subscriber, RouterSubscriber},
    ApolloRouterBuilder, ConfigurationKind, SchemaKind, ShutdownKind,
};
use anyhow::{anyhow, Context, Result};
use clap::{CommandFactory, Parser};
use directories::ProjectDirs;
use once_cell::sync::OnceCell;
use schemars::gen::SchemaSettings;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;
use std::{env, fmt};
use tracing_subscriber::EnvFilter;
use url::Url;

static GLOBAL_ENV_FILTER: OnceCell<String> = OnceCell::new();

/// Options for the router
#[derive(Parser, Debug)]
#[structopt(name = "router", about = "Apollo federation router")]
pub struct Opt {
    /// Log level (off|error|warn|info|debug|trace).
    #[clap(
        long = "log",
        default_value = "apollo_router=info,router=info,apollo_router_core=info,apollo_spaceport=info,tower_http=info,reqwest_tracing=info",
        alias = "log-level",
        env = "RUST_LOG"
    )]
    log_level: String,

    /// Reload configuration and schema files automatically.
    #[clap(short, long)]
    watch: bool,

    /// Configuration location relative to the project directory.
    #[clap(short, long = "config", parse(from_os_str), env)]
    configuration_path: Option<PathBuf>,

    /// Schema location relative to the project directory.
    #[clap(short, long = "supergraph", parse(from_os_str), env)]
    supergraph_path: Option<PathBuf>,

    /// Prints the configuration schema.
    #[clap(long)]
    schema: bool,

    /// Your Apollo key
    #[clap(long, env)]
    apollo_key: Option<String>,

    /// Your Apollo graph reference
    #[clap(long, env)]
    apollo_graph_ref: Option<String>,

    /// The endpoint polled to fetch the latest supergraph schema.
    #[clap(long, env)]
    apollo_schema_config_delivery_endpoint: Option<Url>,

    /// The time between polls to Apollo uplink. Minimum 10s.
    #[clap(long, default_value = "10s", parse(try_from_str = humantime::parse_duration), env)]
    apollo_schema_poll_interval: Duration,
}

/// Wrapper so that structop can display the default config path in the help message.
/// Uses ProjectDirs to get the default location.
#[derive(Debug)]
struct ProjectDir {
    path: Option<PathBuf>,
}

impl Default for ProjectDir {
    fn default() -> Self {
        let dirs = ProjectDirs::from("com", "Apollo", "Federation");
        Self {
            path: dirs.map(|dirs| dirs.config_dir().to_path_buf()),
        }
    }
}

impl From<&OsStr> for ProjectDir {
    fn from(s: &OsStr) -> Self {
        Self {
            path: Some(PathBuf::from(s)),
        }
    }
}

impl fmt::Display for ProjectDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            None => {
                write!(f, "Unknown, -p option must be used.")
            }
            Some(path) => {
                write!(f, "{}", path.to_string_lossy())
            }
        }
    }
}

/// This is the main router entrypoint.
///
/// It effectively builds a tokio runtime and runs `rt_main()`.
///
/// Refer to the examples if you would like how to run your own router with plugins.
pub fn main() -> Result<()> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Some(nb) = std::env::var("ROUTER_NUM_CORES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        builder.worker_threads(nb);
    }
    let runtime = builder.build()?;
    runtime.block_on(rt_main())
}

/// If you already have a tokio runtime, you can spawn the router like this:
///
/// ```no_run
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///   apollo_router::rt_main().await
/// }
/// ```
pub async fn rt_main() -> Result<()> {
    let opt = Opt::parse();

    copy_args_to_env();

    if opt.schema {
        let settings = SchemaSettings::draft2019_09().with(|s| {
            s.option_nullable = true;
            s.option_add_null_type = false;
            s.inline_subschemas = true;
        });
        let gen = settings.into_generator();
        let schema = gen.into_root_schema_for::<Configuration>();
        println!("{}", serde_json::to_string_pretty(&schema)?);
        return Ok(());
    }

    // This is more complex than I'd like it to be. Really, we just want to pass
    // a FmtSubscriber to set_global_subscriber(), but we can't because of the
    // generic nature of FmtSubscriber. See: https://github.com/tokio-rs/tracing/issues/380
    // for more details.
    let builder = tracing_subscriber::fmt::fmt()
        .with_env_filter(EnvFilter::try_new(&opt.log_level).context("could not parse log")?);

    let subscriber: RouterSubscriber = if atty::is(atty::Stream::Stdout) {
        RouterSubscriber::TextSubscriber(builder.finish())
    } else {
        RouterSubscriber::JsonSubscriber(builder.json().finish())
    };

    set_global_subscriber(subscriber)?;

    GLOBAL_ENV_FILTER.set(opt.log_level).unwrap();

    tracing::info!(
        "{}@{}",
        std::env!("CARGO_PKG_NAME"),
        std::env!("CARGO_PKG_VERSION")
    );

    let current_directory = std::env::current_dir()?;

    let configuration = opt
        .configuration_path
        .as_ref()
        .map(|path| {
            let path = if path.is_relative() {
                current_directory.join(path)
            } else {
                path.to_path_buf()
            };

            ConfigurationKind::File {
                path,
                watch: opt.watch,
                delay: None,
            }
        })
        .unwrap_or_else(|| ConfigurationKind::Instance(Configuration::builder().build().boxed()));

    let schema = match (opt.supergraph_path, opt.apollo_key) {
        (Some(supergraph_path), _) => {
            let supergraph_path = if supergraph_path.is_relative() {
                current_directory.join(supergraph_path)
            } else {
                supergraph_path
            };
            SchemaKind::File {
                path: supergraph_path,
                watch: opt.watch,
                delay: None,
            }
        }
        (None, Some(apollo_key)) => {
            let apollo_graph_ref = opt.apollo_graph_ref.ok_or_else(||anyhow!("cannot fetch the supergraph from Apollo Studio without setting the APOLLO_GRAPH_REF environment variable"))?;
            if opt.apollo_schema_poll_interval < Duration::from_secs(10) {
                return Err(anyhow!("Apollo poll interval must be at least 10s"));
            }

            SchemaKind::Registry {
                apollo_key,
                apollo_graph_ref,
                url: opt.apollo_schema_config_delivery_endpoint,
                poll_interval: opt.apollo_schema_poll_interval,
            }
        }
        _ => {
            return Err(anyhow!(
                r#"
    💫 Apollo Router requires a supergraph to be set using '--supergraph':

        $ ./router --supergraph <file>`

        Alternatively, to retrieve the supergraph from Apollo Studio, set the APOLLO_KEY
        and APOLLO_GRAPH_REF environment variables to your graph's settings.
        
          $ APOLLO_KEY="..." APOLLO_GRAPH_REF="..." ./router
          
        For more on Apollo Studio and Managed Federation, see our documentation:
        
          https://www.apollographql.com/docs/router/managed-federation/

    🪐 The supergraph can be built or downloaded from the Apollo Registry
       using the Rover CLI. To find out how, see:

        https://www.apollographql.com/docs/rover/supergraphs/.

    🧪 If you're just experimenting, you can download and use an example
       supergraph with pre-deployed subgraphs:

        $ curl -L https://supergraph.demo.starstuff.dev/ > starstuff.graphql

       Then run the Apollo Router with that supergraph:

        $ ./router --supergraph starstuff.graphql

    "#,
            ));
        }
    };

    // Create your text map propagator & assign it as the global propagator.
    //
    // This is required in order to create the header traceparent used in http_subgraph to
    // propagate the trace id to the subgraph services.
    //
    // /!\ If this is not called, there will be no warning and no header will be sent to the
    //     subgraphs!
    let propagator = opentelemetry::sdk::propagation::TraceContextPropagator::new();
    opentelemetry::global::set_text_map_propagator(propagator);

    let server = ApolloRouterBuilder::default()
        .configuration(configuration)
        .schema(schema)
        .shutdown(ShutdownKind::CtrlC)
        .build();
    let mut server_handle = server.serve();
    server_handle.with_default_state_receiver().await;

    if let Err(err) = server_handle.await {
        tracing::error!("{}", err);
        return Err(err.into());
    }

    Ok(())
}

fn copy_args_to_env() {
    // Copy all the args to env.
    // This way, Clap is still responsible for the definitive view of what the current options are.
    // But if we have code that relies on env variable then it will still work.
    // Env variables should disappear over time as we move to plugins.
    let matches = Opt::command().get_matches();
    Opt::command().get_arguments().for_each(|a| {
        if let Some(env) = a.get_env() {
            if a.is_allow_invalid_utf8_set() {
                if let Some(value) = matches.value_of_os(a.get_id()) {
                    env::set_var(env, value);
                }
            } else if let Some(value) = matches.value_of(a.get_id()) {
                env::set_var(env, value);
            }
        }
    });
}