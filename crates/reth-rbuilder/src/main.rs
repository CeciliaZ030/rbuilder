//! `rbuilder` running in-process with vanilla reth.
//!
//! Usage: `cargo run -r --bin reth-rbuilder -- node --rbuilder.config <path-to-your-config-toml>`
//!
//! Note this method of running rbuilder is not quite ready for production.
//! See <https://github.com/flashbots/rbuilder/issues/229> for more information.

use clap::Args;
use gwyneth::{engine_api::RpcServerArgsExEx, exex::{GwynethFullNode}, GwynethNode};
use rbuilder::{
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
    telemetry,
};
use reth::{args::{DiscoveryArgs, NetworkArgs, RpcServerArgs}, chainspec::ChainSpecBuilder, tasks::TaskManager};
use reth_cli_commands::node::L2Args;
use reth_db_api::Database;
use reth_node_builder::{DefaultNodeLauncher, Node, NodeBuilder, NodeConfig, NodeHandle};
use reth_provider::{
    providers::{BlockchainProvider, BlockchainProvider2}, DatabaseProviderFactory, HeaderProvider, StateProviderFactory,
};
use std::{path::PathBuf, process};
use tokio::task;
use tracing::error;

// Prefer jemalloc for performance reasons.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    use clap::Parser;
    use reth::cli::Cli;
    use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};

    reth_cli_util::sigsegv_handler::install();

    if let Err(err) = Cli::<L2Args>::parse().run(|builder, ext| async move {        
        let enable_engine2 = ext.experimental;
        match enable_engine2 {
            true => {
                unimplemented!()
                // let mut gwyneth_nodes = Vec::new();
                // let mut l2_providers = Vec::new();
                // for (idx, (chain_id, datadir)) in ext.chain_ids.into_iter().zip(ext.datadirs).enumerate() {
                //     let NodeHandle { node: gwyneth_node, node_exit_future: _ } =
                //         NodeBuilder::new(make_gwyneth_node_config(ext.clone()))
                //             .with_gwyneth_launch_context(TaskManager::current().executor(), datadir)
                //             .node2(GwynethNode::default())
                //             .launch_with_fn(|builder| {
                //                 let launcher = EngineNodeLauncher::new(
                //                     builder.task_executor().clone(),
                //                     builder.config().datadir(),
                //                 );
                //                 builder.launch_with(launcher)
                //             })                            
                //             .await?;
                //     l2_providers.push(gwyneth_node.provider.clone());
                //     gwyneth_nodes.push(gwyneth_node);
                // }

                // let handle = builder
                //     .with_types_and_provider::<EthereumNode, BlockchainProvider2<_>>()
                //     .with_components(EthereumNode::components())
                //     .with_add_ons::<EthereumAddOns>()
                //     .on_rpc_started(move |ctx, _| {
                //         spawn_rbuilder(ctx.provider().clone(), l2_providers, ext.rbuilder_config);
                //         Ok(())
                //     })
                //     .install_exex("Rollup", move |ctx| async {
                //         Ok(gwyneth::exex::Rollup::new(ctx, gwyneth_nodes).await?.start())
                //     })
                //     .launch_with_fn(|builder| {
                //         let launcher = EngineNodeLauncher::new(
                //             builder.task_executor().clone(),
                //             builder.config().datadir(),
                //         );
                //         builder.launch_with(launcher)
                //     })
                //     .await?;
                // handle.node_exit_future.await
            }
            false => {

                let mut gwyneth_nodes = Vec::new();
                let mut l2_providers = Vec::new();
                for (idx, (chain_id, datadir)) in ext.chain_ids.iter().zip(ext.datadirs.clone()).enumerate() {
                    let NodeHandle { node: gwyneth_node, node_exit_future: _ } =
                        NodeBuilder::new(make_gwyneth_node_config(ext.clone(), idx, *chain_id))
                            .with_gwyneth_launch_context(TaskManager::current().executor(), datadir)
                            .node(GwynethNode::default())
                            .launch()
                            .await?;
                    l2_providers.push(gwyneth_node.provider.clone());
                    gwyneth_nodes.push(gwyneth_node);
                }

                let handle = builder
                    .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
                    .with_components(EthereumNode::components())
                    .with_add_ons::<EthereumAddOns>()
                    .install_exex("Rollup", move |ctx| async {
                        Ok(gwyneth::exex::Rollup::new(ctx, gwyneth_nodes).await?.start())
                    })
                    .on_rpc_started(move |ctx, _| {
                        spawn_rbuilder(ctx.provider().clone(), l2_providers, ext.rbuilder_config);
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


fn make_gwyneth_node_config(ext: L2Args, idx: usize, chain_id: u64) -> NodeConfig {
    // Assuming chain_ids & datadirs are mandetory
    // If ports and ipc are not supported we used the default ways to derive 
    assert_eq!(ext.chain_ids.len(), ext.datadirs.len());
    assert!(ext.chain_ids.len() > 0);

    let network_config = NetworkArgs {
        discovery: DiscoveryArgs { disable_discovery: true, ..DiscoveryArgs::default() },
        ..NetworkArgs::default()
    };

    let chain_spec = ChainSpecBuilder::default()
            .chain(chain_id.into())
            .genesis(
                serde_json::from_str(include_str!(
                    "../../../../reth/crates/ethereum/node/tests/assets/genesis.json"
                ))
                .unwrap(),
            )
            .cancun_activated()
            .build();
        
    NodeConfig::test()
        .with_chain(chain_spec.clone())
        .with_network(network_config.clone())
        .with_unused_ports()
        .with_rpc(
            RpcServerArgs::default()
                .with_unused_ports()
                .with_ports(ext.ports.get(idx), chain_id)
        )

}


/// Spawns a tokio rbuilder task.
///
/// Takes down the entire process if the rbuilder errors or stops.
fn spawn_rbuilder<P, DB>(provider: P, l2_providers: Vec<P>, config_path: PathBuf)
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
            let builder = config.new_builder(provider, l2_providers, Default::default()).await?;

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
