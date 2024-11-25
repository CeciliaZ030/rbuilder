//! `rbuilder` running in-process with vanilla reth.
//!
//! Usage: `cargo run -r --bin reth-rbuilder -- node --rbuilder.config <path-to-your-config-toml>`
//!
//! Note this method of running rbuilder is not quite ready for production.
//! See <https://github.com/flashbots/rbuilder/issues/229> for more information.

use clap::Args;
use gwyneth::{GwynethFullNode, GwynethNode};
use rbuilder::{
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
    telemetry,
};
use reth::{args::RpcServerArgs, chainspec::ChainSpecBuilder};
use reth_cli_commands::node::L2Args;
use reth_db_api::Database;
use reth_node_builder::{NodeBuilder, NodeConfig, NodeHandle};
use reth_provider::{
    providers::BlockchainProvider, DatabaseProviderFactory, HeaderProvider, StateProviderFactory,
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
    use reth_node_builder::EngineNodeLauncher;
    use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
    use reth_provider::providers::BlockchainProvider2;

    reth_cli_util::sigsegv_handler::install();

    if let Err(err) = Cli::<L2Args>::parse().run(|builder, ext| async move {
        
        let gwyneth_nodes = make_gwyneth_nodes(ext).await?;
        let l2_providers= gwyneth_nodes
            .iter()
            .map(|node| node.provider.clone())
            .collect::<Vec<_>>();
        
        let enable_engine2 = ext.experimental;
        match enable_engine2 {
            true => {
                let handle = builder
                    .with_types_and_provider::<EthereumNode, BlockchainProvider2<_>>()
                    .with_components(EthereumNode::components())
                    .with_add_ons::<EthereumAddOns>()
                    .on_rpc_started(move |ctx, _| {
                        spawn_rbuilder(ctx.provider().clone(), l2_providers, ext.rbuilder_config);
                        Ok(())
                    })
                    .install_exex("Rollup", move |ctx| async {
                        Ok(gwyneth::exex::Rollup::new(ctx, gwyneth_nodes).await?.start())
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

async fn make_gwyneth_nodes(ext: L2Args) -> eyre::Result<Vec<GwynethFullNode>> {
    
    let gwyneth_nodes = Vec::new();

    // Assuming chain_ids & datadirs are mandetory
    // If ports and ipc are not supported we used the default ways to derive 
    assert_eq!(ext.chain_ids.len(), ext.datadirs.len());
    assert!(ext.chain_ids.len() > 0);

    for (idx, (chain_id, datadir)) in ext.chain_ids.into_iter().zip(ext.datadirs).enumerate() {
        let chain_spec = ChainSpecBuilder::default()
            .chain(chain_id.into())
            .genesis(
                serde_json::from_str(include_str!(
                    "../../../crates/ethereum/node/tests/assets/genesis.json"
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
                    .with_ports(ext.ports.get(idx), chain_id)
            );
        

        let NodeHandle { node: gwyneth_node, node_exit_future: _ } =
            NodeBuilder::new(node_config.clone())
                .with_gwyneth_launch_context(exec.clone(), datadir)
                .node(GwynethNode::default())
                .launch()
                .await?;

        gwyneth_nodes.push(gwyneth_node);
    }

    gwyneth_nodes

}

/// Spawns a tokio rbuilder task.
///
/// Takes down the entire process if the rbuilder errors or stops.
fn spawn_rbuilder<P, DB>(provider: P, l2_providers: Vec<P>, config_path: PathBuf)
where
    DB: Database + Clone + 'static,
    P: DatabaseProviderFactory<DB> + StateProviderFactory + HeaderProvider + Clone,
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
