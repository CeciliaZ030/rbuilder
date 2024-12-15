//! `rbuilder` running in-process with vanilla reth.
//!
//! Usage: `cargo run -r --bin reth-rbuilder -- node --rbuilder.config <path-to-your-config-toml>`
//!
//! Note this method of running rbuilder is not quite ready for production.
//! See <https://github.com/flashbots/rbuilder/issues/229> for more information.

use gwyneth::{
    cli::{create_gwyneth_nodes, GwynethArgs}, exex::GwynethFullNode,
};
use rbuilder::{
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
    telemetry,
};
use reth::rpc::api::NetApiClient;
use reth_db_api::Database;
use reth_node_builder::{EngineNodeLauncher, NodeConfig};
use reth_provider::{
    providers::{BlockchainProvider, BlockchainProvider2},
    DatabaseProviderFactory, HeaderProvider, StateProviderFactory,
};
use std::{path::PathBuf, process};
use tokio::task;
use tracing::error;

// Prefer jemalloc for performance reasons.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> eyre::Result<()> {
    use clap::Parser;
    use reth::cli::Cli;
    use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};

    reth_cli_util::sigsegv_handler::install();

    if let Err(err) = Cli::<GwynethArgs>::parse().run(|builder, arg| async move {
        let l1_node_config = builder.config().clone();
        let task_executor = builder.task_executor().clone();
        let gwyneth_nodes = create_gwyneth_nodes(&arg, task_executor.clone(), &l1_node_config).await;

        let enable_engine2 = arg.experimental;
        match enable_engine2 {
            true => {
                let l2_providers = gwyneth_nodes
                    .iter()
                    .map(|node| match node {
                        GwynethFullNode::Provider1(_) => {
                            panic!("Unexpected Provider: expect BlockchainProvider2")
                        }
                        GwynethFullNode::Provider2(n) => n.provider.clone(),
                    })
                    .collect::<Vec<_>>();
                
                let handle = builder
                    .with_types_and_provider::<EthereumNode, BlockchainProvider2<_>>()
                    .with_components(EthereumNode::components())
                    .with_add_ons::<EthereumAddOns>()
                    .on_rpc_started(move |ctx, _| {
                        spawn_rbuilder(&arg, &l1_node_config, ctx.provider().clone(), l2_providers)
                    })
                    .install_exex("Rollup", move |ctx| async {
                        Ok(gwyneth::exex::Rollup::new(ctx, gwyneth_nodes)
                            .await?
                            .start())
                    })
                    .launch_with_fn(|builder| {
                        let launcher = EngineNodeLauncher::new(
                            task_executor,
                            builder.config().datadir(),
                        );
                        builder.launch_with(launcher)
                    })
                    .await?;
                handle.node_exit_future.await
            }
            false => {
                let l2_providers = gwyneth_nodes
                    .iter()
                    .map(|node| match node {
                        GwynethFullNode::Provider1(n) => n.provider.clone(),
                        GwynethFullNode::Provider2(_) => {
                            panic!("Unexpected Provider: expect BlockchainProvider")
                        }
                    })
                    .collect::<Vec<_>>();

                let handle = builder
                    .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
                    .with_components(EthereumNode::components())
                    .with_add_ons::<EthereumAddOns>()
                    .install_exex("Rollup", move |ctx| async {
                        Ok(gwyneth::exex::Rollup::new(ctx, gwyneth_nodes)
                            .await?
                            .start())
                    })
                    .on_rpc_started(move |ctx, _| {
                        spawn_rbuilder(&arg, &l1_node_config, ctx.provider().clone(), l2_providers)
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
    Ok(())
}

/// Spawns a tokio rbuilder task.
///
/// Takes down the entire process if the rbuilder errors or stops.
fn spawn_rbuilder<P, DB>(
    arg: &GwynethArgs,
    l1_node_config: &NodeConfig,
    provider: P,
    l2_providers: Vec<P>,
) -> eyre::Result<()>
where
    DB: Database + Clone + 'static,
    P: DatabaseProviderFactory<DB> + StateProviderFactory + HeaderProvider + Clone + 'static,
{        
    let arg = arg.clone();
    let l1_node_config = l1_node_config.clone();
    let _handle = task::spawn(async move {
        let result = async {
            let mut config: Config = load_config_toml_and_env(
                arg.rbuilder_config.clone().expect("Gwyneth-rbuilder needs config path")
            )?;
            // Where we set L1 rpc, proposer pk and rollup contract address
            config.l1_config.update_in_process_setting(&l1_node_config);
            config.base_config.update_in_process_setting(arg, l1_node_config);

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
            let builder = config
                .new_builder(provider, l2_providers, Default::default())
                .await?;

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
    Ok(())
}
