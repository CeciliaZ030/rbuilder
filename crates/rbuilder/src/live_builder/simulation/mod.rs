pub mod sim_worker;
mod simulation_job;

use crate::{
    building::{
        sim::{SimTree, SimulatedResult, SimulationRequest},
        BlockBuildingContext,
    },
    live_builder::order_input::orderpool::OrdersForBlock,
    primitives::{OrderId, SimulatedOrder},
    utils::gen_uid,
};
use ahash::HashMap;
use reth_provider::StateProviderFactory;
use simulation_job::SimulationJob;
use std::{sync::{Arc, Mutex}, marker::PhantomData};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, info_span, Instrument};

#[derive(Debug)]
pub struct SlotOrderSimResults {
    pub orders: mpsc::Receiver<SimulatedOrderCommand>,
}

type BlockContextId = u64;

/// Struct representing the need of order simulation for a particular block.
#[derive(Debug, Clone)]
pub struct SimulationContext {
    pub block_ctx: BlockBuildingContext,
    /// Simulation requests come in through this channel.
    pub requests: flume::Receiver<SimulationRequest>,
    /// Simulation results go out through this channel.
    pub results: mpsc::Sender<SimulatedResult>,
}

/// All active SimulationContexts
#[derive(Debug)]
pub struct CurrentSimulationContexts {
    pub contexts: HashMap<BlockContextId, SimulationContext>,
}

/// Struct that creates several [`sim_worker::run_sim_worker`] threads to allow concurrent simulation for the same block.
/// Usage:
/// 1 Create a single instance via [`OrderSimulationPool::new`] which receives the input.
/// 2 For each block call [`OrderSimulationPool::spawn_simulation_job`] which will spawn a task to run the simulations.
/// 3 Poll the results via the [`SlotOrderSimResults::orders`].
/// 4 IMPORTANT: When done with the simulations signal the provided block_cancellation.

#[derive(Debug)]
pub struct OrderSimulationPool<P> {
    providers: HashMap<u64, P>,
    running_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    current_contexts: Arc<Mutex<CurrentSimulationContexts>>,
    worker_threads: Vec<std::thread::JoinHandle<()>>,
}

/// Result of a simulation.
#[derive(Clone, Debug)]
pub enum SimulatedOrderCommand {
    /// New simulation.
    Simulation(SimulatedOrder),
    /// Forwarded cancellation from the order source.
    Cancellation(OrderId),
}

impl<P> OrderSimulationPool<P>
where
    P: StateProviderFactory + Clone + 'static,
{
    pub fn new(providers: HashMap<u64, P>, num_workers: usize, global_cancellation: CancellationToken) -> Self {
        let mut result = Self {
            providers,
            running_tasks: Arc::new(Mutex::new(Vec::new())),
            current_contexts: Arc::new(Mutex::new(CurrentSimulationContexts {
                contexts: HashMap::default(),
            })),
            worker_threads: Vec::new(),
        };
        for i in 0..num_workers {
            let ctx: Arc<Mutex<CurrentSimulationContexts>> = Arc::clone(&result.current_contexts);
            let providers = result.providers.clone();
            let cancel = global_cancellation.clone();
            let handle = std::thread::Builder::new()
                .name(format!("sim_thread:{}", i))
                .spawn(move || {
                    sim_worker::run_sim_worker(i, ctx, providers, cancel);
                })
                .expect("Failed to start sim worker thread");
            result.worker_threads.push(handle);
        }
        result
    }

    /// Prepares the context to run a SimulationJob and spawns a task with it.
    /// The returned SlotOrderSimResults can be polled to the the simulation stream.
    /// IMPORTANT: By calling spawn_simulation_job we lock some worker threads on the given block.
    ///     When we are done we MUST call block_cancellation so the threads can be freed for the next block.
    /// @Pending: Not properly working to be used with several blocks at the same time (forks!).
    pub fn spawn_simulation_job(
        &self,
        ctx: BlockBuildingContext,
        input: HashMap<u64, OrdersForBlock>,
        block_cancellation: CancellationToken,
    ) -> SlotOrderSimResults {
        let (slot_sim_results_sender, slot_sim_results_receiver) = mpsc::channel(10_000);
    
        // Clone the original providers since we can't convert ProviderFactory<DB> to P
        let providers = self.providers.clone();
    
        let providers = self.providers.clone();
        let current_contexts = Arc::clone(&self.current_contexts);
        let block_context: BlockContextId = gen_uid();
        // let span = info_span!("sim_ctx", block = ctx.block_env.number.to::<u64>(), parent = ?ctx.attributes.parent);
    
        let handle = tokio::spawn(
            async move {
                for (_chain_id, new_order_sub) in input {
                    let sim_tree = SimTree::new(
                        providers.clone(),
                        ctx.chains.iter().map(|(chain_id, ctx)| (*chain_id, ctx.attributes.parent)).collect()
                    );
                    let new_order_sub = new_order_sub.new_order_sub;
                    let (sim_req_sender, sim_req_receiver) = flume::unbounded();
                    let (sim_results_sender, sim_results_receiver) = mpsc::channel(1024);
                    {
                        let mut contexts = current_contexts.lock().unwrap();
                        let sim_context = SimulationContext {
                            block_ctx: ctx.clone(),
                            requests: sim_req_receiver,
                            results: sim_results_sender,
                        };
                        contexts.contexts.insert(block_context, sim_context);
                    }
                    let mut simulation_job = SimulationJob::new(
                        block_cancellation.clone(),
                        new_order_sub,
                        sim_req_sender,
                        sim_results_receiver,
                        slot_sim_results_sender.clone(),
                        sim_tree,
                    );
    
                    simulation_job.run().await;
    
                    // clean up
                    {
                        let mut contexts = current_contexts.lock().unwrap();
                        contexts.contexts.remove(&block_context);
                    }
                }
            }
        );
    
        {
            let mut tasks = self.running_tasks.lock().unwrap();
            tasks.retain(|handle| !handle.is_finished());
            tasks.push(handle);
        }
    
        SlotOrderSimResults {
            orders: slot_sim_results_receiver,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        building::testing::test_chain_state::{BlockArgs, NamedAddr, TestChainState, TxArgs},
        live_builder::order_input::order_sink::OrderPoolCommand,
        primitives::{MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs},
        utils::ProviderFactoryReopener,
    };
    use reth_evm::provider;
    use reth_primitives::U256;

    #[tokio::test]
    async fn test_simulate_order_to_coinbase() {
        let test_context = TestChainState::new(BlockArgs::default().number(11)).unwrap();

        // Create simulation core
        let cancel = CancellationToken::new();
        let provider_factory_reopener =
            ProviderFactoryReopener::new_from_existing(test_context.provider_factory().clone())
                .unwrap();

        let mut providers = HashMap::default();
        providers.insert(test_context.chain_spec.chain.id(), provider_factory_reopener.clone());
        providers.insert(test_context.chain_spec.chain.id() + 1, provider_factory_reopener);

        let sim_pool = OrderSimulationPool::new(providers, 4, cancel.clone());
        let (order_sender, order_receiver) = mpsc::unbounded_channel();
        let orders_for_block = OrdersForBlock {
            new_order_sub: order_receiver,
        };

        let (order_sender2, order_receiver2) = mpsc::unbounded_channel();
        let orders_for_block2 = OrdersForBlock {
            new_order_sub: order_receiver2,
        };

        let mut orders_for_blocks = HashMap::default();
        orders_for_blocks.insert(test_context.chain_spec.chain.id(), orders_for_block);
        orders_for_blocks.insert(test_context.chain_spec.chain.id() + 1, orders_for_block2);
        let mut sim_results = sim_pool.spawn_simulation_job(
            test_context.block_building_context().clone(),
            orders_for_blocks,
            cancel.clone(),
        );

        // Create a simple tx that sends to coinbase 5 wei.
        let coinbase_profit = 5;
        // max_priority_fee will be 0
        let tx_args = TxArgs::new_send_to_coinbase(NamedAddr::User(1), 0, coinbase_profit);
        let tx = test_context.sign_tx(tx_args).unwrap();
        let tx = TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap();
        order_sender
            .send(OrderPoolCommand::Insert(Order::Tx(MempoolTx::new(tx))))
            .unwrap();

        // We expect to receive the simulation giving a profit of coinbase_profit since that's what we sent directly to coinbase.
        // and we are not paying any priority fee
        if let Some(command) = sim_results.orders.recv().await {
            match command {
                SimulatedOrderCommand::Simulation(sim_order) => {
                    assert_eq!(
                        sim_order.sim_value.coinbase_profit,
                        U256::from(coinbase_profit)
                    );
                }
                SimulatedOrderCommand::Cancellation(_) => panic!("Cancellation not expected"),
            };
        }
    }
}
