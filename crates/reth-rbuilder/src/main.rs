//! `rbuilder` running in-process with vanilla reth.
//!
//! Usage: `cargo run -r --bin reth-rbuilder -- node --rbuilder.config <path-to-your-config-toml>`
//!
//! Note this method of running rbuilder is not quite ready for production.
//! See <https://github.com/flashbots/rbuilder/issues/229> for more information.

use clap::Args;
use gwyneth::{engine_api::RpcServerArgsExEx, GwynethFullNode, GwynethNode};
use rbuilder::{
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
    telemetry, utils::{ProviderFactoryReopener, ProviderFactoryUnchecked, ConsistencyReopener}
};
use reth::{args::{DiscoveryArgs, NetworkArgs, RpcServerArgs}, chainspec::{ChainSpec, SEPOLIA}, dirs::{DataDirPath, MaybePlatformPath}};
use reth_db_api::Database;
use reth::args::DatadirArgs;
use reth_db::{DatabaseEnv, mdbx::{DatabaseArguments, init_db}}; 
use reth_node_builder::{NodeBuilder, NodeConfig, NodeHandle};
use reth_provider::{
    providers::{BlockchainProvider, StaticFileProvider}, DatabaseProviderFactory, HeaderProvider, StateProviderFactory, ProviderFactory,
};
use reth_cli_commands::node::L2Args;
use std::{path::PathBuf, process, sync::Arc};
use tokio::task;
use tracing::error;

// Prefer jemalloc for performance reasons.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// #[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
// pub struct ExtraArgs {
//     /// Path of the rbuilder config to use
//     #[arg(long = "rbuilder.config")]
//     pub rbuilder_config: PathBuf,
//     /// Enable the engine2 experimental features on reth binary
//     #[arg(long = "engine.experimental", default_value = "false")]
//     pub experimental: bool,
// }

fn main() {
    use clap::Parser;
    use reth::cli::Cli;
    use reth_node_builder::EngineNodeLauncher;
    use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
    use reth_provider::providers::BlockchainProvider2;

    reth_cli_util::sigsegv_handler::install();

    if let Err(err) = Cli::<L2Args>::parse().run(|builder, l2_args| async move {
        // Setup paths
        let db_path = PathBuf::from("/tmp/rbuilder-test/db");
        let static_files_path = PathBuf::from("/tmp/rbuilder-test/static_files");

        // Create directories
        std::fs::create_dir_all(&db_path)?;
        std::fs::create_dir_all(&static_files_path)?;

        // Create our components holder
        let components = DbComponents {
            db: Arc::new(init_db(&db_path, DatabaseArguments::default())?),
            chain_spec: Arc::new(ChainSpec::builder().build()),
            static_file_provider: StaticFileProvider::read_write(&static_files_path)?,
        };

        let gwyneth_nodes = create_gwyneth_nodes(&l2_args).await?;

        let enable_engine2 = l2_args.experimental;
        match enable_engine2 {
            true => {
                let handle = builder
                    .with_types_and_provider::<EthereumNode, BlockchainProvider2<_>>()
                    .with_components(EthereumNode::components())
                    .with_add_ons::<EthereumAddOns>()
                    .install_exex("Gwyneth", move |ctx| async {
                        Ok(gwyneth::exex::Rollup::new(ctx, gwyneth_nodes).await?.start())
                    })                    
                    .on_rpc_started({
                        let components = components.clone();
                        let config_path = l2_args.rbuilder_config.clone();
                        
                        move |_ctx, _| {
                            let provider_factory = ProviderFactory::new(
                                components.db,
                                components.chain_spec,
                                components.static_file_provider,
                            );
                            let provider = ProviderFactoryReopener::new_from_existing(provider_factory)
                                .expect("failed to create provider reopener");
                            spawn_rbuilder(provider, config_path);
                            Ok(())
                        }
                    })
                    .launch_with_fn(|builder| {
                        let launcher = EngineNodeLauncher::new(
                            builder.task_executor().clone(),
                            builder.config().datadir(),
                        );
                        builder.launch_with(launcher)
                    })
                    .await?;
                handle.node_exit_future.await
            }
            false => {
                let handle = builder
                    .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
                    .with_components(EthereumNode::components())
                    .with_add_ons::<EthereumAddOns>()
                    .on_rpc_started(move |ctx, _| {
                        spawn_rbuilder(ctx.provider().clone(), l2_args.rbuilder_config);
                        Ok(())
                    })
                    .launch()
                    .await?;
                handle.node_exit_future.await
            }
        }
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

async fn create_gwyneth_nodes(l2_args: &L2Args) -> eyre::Result<Vec<GwynethFullNode>> {
    // Assuming chain_ids & datadirs are mandetory
    // If ports and ipc are not supported we used the default ways to derive 
    assert_eq!(l2_args.chain_ids.len(), l2_args.datadirs.len());
    assert!(l2_args.chain_ids.len() > 0);

    println!("Starting reth node with custom exex \n {:?}", l2_args);
    let tasks = reth::tasks::TaskManager::current();
    let exec = tasks.executor();
    let network_config = NetworkArgs {
        discovery: DiscoveryArgs { disable_discovery: true, ..DiscoveryArgs::default() },
        ..NetworkArgs::default()
    };

    let mut gwyneth_nodes = Vec::new();

    for (idx, (chain_id, datadir)) in l2_args.chain_ids.iter().zip(l2_args.datadirs.clone()).enumerate() {
        let chain_spec = reth::chainspec::ChainSpecBuilder::default()
            .chain(chain_id.clone().into())
            .genesis(
                serde_json::from_str(include_str!(
                    "../../../../reth/crates/ethereum/node/tests/assets/genesis.json"
                ))
                .unwrap(),
            )
            .cancun_activated()
            .build();
        
        let node_config = NodeConfig::test()
            .with_chain(chain_spec.clone())
            .with_network(network_config.clone())
            .with_unused_ports()
            .with_rpc(
                RpcServerArgs::default()
                    .with_unused_ports()
                    .with_ports(l2_args.ports.get(idx), chain_id.clone())
            );
        
        let NodeHandle { node: gwyneth_node, node_exit_future: _ } =
            NodeBuilder::new(node_config.clone())
                .gwyneth_node(exec.clone(), datadir)
                .node(GwynethNode::default())
                .launch()
                .await?;

        gwyneth_nodes.push(gwyneth_node);
    }

    Ok(gwyneth_nodes)
}

/// Spawns a tokio rbuilder task.
///
/// Takes down the entire process if the rbuilder errors or stops.
fn spawn_rbuilder<P, DB>(provider: P, config_path: PathBuf)
where
    DB: Database + Clone + 'static,
    P: DatabaseProviderFactory<DB> + StateProviderFactory + HeaderProvider + Clone + 'static,
{
    let _handle = task::spawn(async move {
        let result = async {
            let config: Config = load_config_toml_and_env(config_path)?;

            // TODO: Check removing this is OK. It seems reth already sets up the global tracing
            // subscriber, so this fails
            // config.base_config.setup_tracing_subscriber().expect("Failed to set up rbuilder tracing subscriber");

            // Spawn redacted server that is safe for tdx builders to expose
            telemetry::servers::redacted::spawn(
                config.base_config().redacted_telemetry_server_address(),
            )
            .await?;

            // Spawn debug server that exposes detailed operational information
            telemetry::servers::full::spawn(
                config.base_config.full_telemetry_server_address(),
                config.version_for_telemetry(),
                config.base_config.log_enable_dynamic,
            )
            .await?;
            let builder = config.new_builder(provider, Default::default()).await?;

            builder.run().await?;

            Ok::<(), eyre::Error>(())
        }
        .await;

        if let Err(e) = result {
            error!("Fatal rbuilder error: {}", e);
            process::exit(1);
        }

        error!("rbuilder stopped unexpectedly");
        process::exit(1);
    });
}

