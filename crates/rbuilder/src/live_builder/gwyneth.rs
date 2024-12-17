use ahash::HashMap;
use alloy_eips::{BlockHashOrNumber, BlockId};
use alloy_primitives::U256;
use alloy_provider::{IpcConnect, Provider, ProviderBuilder, RootProvider};
use alloy_pubsub::PubSubFrontend;
use alloy_rpc_types::{Block, BlockTransactionsKind};
use eyre::Result;
use futures::future::IntoStream;
use futures::stream::TakeUntil;
use futures::{FutureExt, Stream, StreamExt};
use reth::network::NetworkInfo;
use reth::rpc::eth::pubsub::EthPubSubInner;
use reth::transaction_pool::{EthPoolTransaction, EthPooledTransaction, NewTransactionEvent};
use reth_db::{Database, DatabaseEnv};
use reth_evm::provider;
use reth_node_core::args::utils::chain_value_parser;
use reth_primitives::{Header, TransactionSignedEcRecovered};
use reth_provider::{BlockReader, CanonStateSubscriptions, DatabaseProviderFactory, EvmEnvProvider, HeaderProvider, StateProvider, StateProviderFactory};
use revm_primitives::B256;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};
use std::future::Future;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::Poll;
use std::time::Duration;
use tracing::{event, warn};

use crate::primitives::TransactionSignedEcRecoveredWithBlobs;

use super::order_input::OrderInputConfig;

pub type GwynethMempoolReciever = Receiver<NewTransactionEvent<EthPooledTransaction>>;

#[derive(Debug)]
pub struct GwynethNode<P> {
    pub mempool_listener: MempoolListener,
    pub provider: P,
    pub order_input_config: OrderInputConfig,
}

#[derive(Debug, Default)]
pub struct GwynethNodes<P> {
    pub nodes: HashMap<u64, GwynethNode<P>>,
}

impl<P> GwynethNodes<P> 
where 
    P: StateProviderFactory + HeaderProvider + Clone + 'static,

{
    pub fn new(
        providers: Vec<P>,
        mempools: Vec<GwynethMempoolReciever>,
        server_ports: Vec<u16>,
    ) -> Result<Self> {
        println!("Cecilia ==> GwynethNodes::new {:?}", server_ports);
        let mut nodes = HashMap::default();
        for ((provider, mempool), port) in providers
            .into_iter()
            .zip(mempools.into_iter())
            .zip(server_ports.iter())
        {
            nodes.insert(
                0,
                GwynethNode::<P> {
                    mempool_listener: MempoolListener { inner: Arc::new(Mutex::new(mempool)) },
                    provider: provider,
                    order_input_config: OrderInputConfig::new(
                        true,
                        false,
                        PathBuf::new(),
                        *port,
                        Ipv4Addr::new(0, 0, 0, 0),
                        4096,
                        Duration::from_millis(50),
                        10_000,
                    ),
                },
            );
        }
        Ok(Self {
            nodes,
        })
    }

    pub async fn get_latest_header(&self, chain_id: u64) -> Result<Option<Header>> {
        println!("Cecilia ==> GwynethNodes::get_latest_header {:?}", chain_id);
        if let Some(provider) = self.nodes.get(&chain_id).map(|n| n.provider.clone()) {
            let number = provider.last_block_number()?;
            provider
                .header_by_number(number)
                .map_err(|e| eyre::eyre!("Error getting latest header for chain_id {}: {:?}", chain_id, e))
                
        } else {
            eyre::bail!("No provider found for chain_id: {}", chain_id)
        }
    }
}

#[derive(Debug, Clone)]
pub struct MempoolListener {
    inner: Arc<Mutex<GwynethMempoolReciever>>
}

impl MempoolListener {
    pub fn new(mempool: GwynethMempoolReciever) -> Self {
        Self {
            inner: Arc::new(Mutex::new(mempool))
        }
    }
}

impl Future for MempoolListener {
    type Output = TransactionSignedEcRecoveredWithBlobs;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        println!("Cecilia ==> MempoolListener::poll");
        let mut this = self
            .get_mut()
            .inner
            .try_lock()
            .expect("Mempool listener mutex poisoned");
        match this.poll_recv(cx) {
            std::task::Poll::Ready(Some(event)) => {
                println!("New transaction: {:?}", event);

                let mut pooled_tx = event.transaction.transaction.clone();
                let tx = pooled_tx.transaction().clone();
                
                let tx_with_blobs = if let Some(blob) = pooled_tx.take_blob().maybe_sidecar() {
                    TransactionSignedEcRecoveredWithBlobs {
                        tx,
                        blobs_sidecar: Arc::new(blob.clone()),
                        metadata: Default::default(),
                    }
                } else {
                    TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap()
                };
                std::task::Poll::Ready(tx_with_blobs)
            },
            std::task::Poll::Ready(None) => {
                panic!("Mempool listener closed")
            },
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

pub trait EthApiStream {
    pub fn new_headers_stream(&self) -> impl Stream<Item = Header>;
    pub fn full_pending_transaction_stream(
        &self,
    ) -> impl Stream<Item = NewTransactionEvent<EthPooledTransaction>;
}

impl<Provider, Events, Network> EthApiStream for EthPubSubInner<Provider, EthPooledTransaction, Events, Network>
where
    Provider: BlockReader + EvmEnvProvider + 'static,
    Events: CanonStateSubscriptions + 'static,
    Network: NetworkInfo + 'static,
{
    pub fn new_headers_stream(&self) -> impl Stream<Item = Header> {
        self.events.canonical_state_stream()
    }

    pub fn full_pending_transaction_stream(
        &self,
    ) -> impl Stream<Item = NewTransactionEvent<EthPooledTransaction> {
        self.events.new_transaction_stream()
    }
}