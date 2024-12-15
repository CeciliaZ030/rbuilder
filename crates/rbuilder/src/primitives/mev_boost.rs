use crate::mev_boost::{RelayClient, SubmitBlockErr, SubmitBlockRequest};

use crate::proposing::BlockProposer;

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use serde::{Deserialize, Deserializer};
use std::{env, sync::Arc, time::Duration};
use url::Url;

/// Usually human readable id for relays. Not used on anything on any protocol just to identify the relays.
pub type MevBoostRelayID = String;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
    pub name: String,
    pub url: String,
    pub priority: usize,
    // true->ssz false->json
    #[serde(default)]
    pub use_ssz_for_submit: bool,
    #[serde(default)]
    pub use_gzip_for_submit: bool,
    #[serde(default)]
    pub optimistic: bool,
    #[serde(default, deserialize_with = "deserialize_env_var")]
    pub authorization_header: Option<String>,
    #[serde(default, deserialize_with = "deserialize_env_var")]
    pub builder_id_header: Option<String>,
    #[serde(default, deserialize_with = "deserialize_env_var")]
    pub api_token_header: Option<String>,
    #[serde(default)]
    pub interval_between_submissions_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_env_var")]
    pub l1_rpc_url: Option<String>,
    pub l1_proposer_pk: Option<String>,
    pub l1_rollup_contract: Option<String>,
}

impl RelayConfig {
    pub fn with_url(self, url: &str) -> Self {
        Self {
            url: url.to_string(),
            ..self
        }
    }

    pub fn with_name(self, name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..self
        }
    }
}

/// Wrapper over RelayClient that allows to submit blocks and
/// hides the particular configuration (eg: ssz, gip, optimistic).
/// Sometimes the client is used externally.
#[derive(Debug, Clone)]
pub struct MevBoostRelay {
    /// Id for UI
    pub id: MevBoostRelayID,
    pub client: RelayClient,
    /// Lower priority -> more important.
    pub priority: usize,
    /// true -> ssz; false -> json.
    pub use_ssz_for_submit: bool,
    pub use_gzip_for_submit: bool,
    /// Relay accepts optimistic submissions.
    pub optimistic: bool,
    pub submission_rate_limiter: Option<Arc<DefaultDirectRateLimiter>>,
    pub block_proposer: Option<BlockProposer>,
}

impl MevBoostRelay {
    pub fn from_config(config: &RelayConfig) -> eyre::Result<Self> {
        let client = {
            let url: Url = config.url.parse()?;
            RelayClient::from_url(
                url,
                config.authorization_header.clone(),
                config.builder_id_header.clone(),
                config.api_token_header.clone(),
            )
        };

        let submission_rate_limiter = config.interval_between_submissions_ms.map(|d| {
            Arc::new(RateLimiter::direct(
                Quota::with_period(Duration::from_millis(d)).expect("Rate limiter time period"),
            ))
        });

        let block_proposer =
            if let (Some(l1_rpc_url), Some(l1_smart_contract_address), Some(l1_proposer_pk)) = (
                &config.l1_rpc_url,
                &config.l1_rollup_contract,
                &config.l1_proposer_pk,
            ) {
                Some(BlockProposer::new(
                    l1_rpc_url.clone(),
                    l1_smart_contract_address.clone(),
                    l1_proposer_pk.clone(),
                )?)
            } else {
                None
            };

        Ok(MevBoostRelay {
            id: config.name.to_string(),
            client,
            priority: config.priority,
            use_ssz_for_submit: config.use_ssz_for_submit,
            use_gzip_for_submit: config.use_gzip_for_submit,
            optimistic: config.optimistic,
            submission_rate_limiter,
            block_proposer,
        })
    }

    // Brecht: Can make a proposeBlock call here to L1 with the given block
    // Can implement a custom "relay" for gwyneth that has this behaviour
    pub async fn submit_block(&self, data: &SubmitBlockRequest) -> Result<(), SubmitBlockErr> {
        println!("Brecht: L1 propose!");

        // Handle the Option<BlockProposer>
        if let Some(proposer) = &self.block_proposer {
            // Call propose_block on the BlockProposer with the SubmitBlockRequest
            proposer
                .propose_block(data)
                .await
                .map_err(|e| SubmitBlockErr::SimError(e.to_string()))?;
        } else {
            // Handle the case where there's no BlockProposer
            println!("No L1 block proposer configured");
        }

        self.client
            .submit_block(data, self.use_ssz_for_submit, self.use_gzip_for_submit)
            .await
    }
}

fn deserialize_env_var<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    Ok(match s {
        Some(val) if val.starts_with("env:") => {
            let env_var = &val[4..];
            env::var(env_var).ok()
        }
        _ => s,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_deserialize_relay_config() {
        let example = "
        name = 'relay1'
        url = 'url'
        priority = 0
        authorization_header = 'env:XXX'
        builder_id_header = 'env:YYY'
        api_token_header = 'env:ZZZ'
        ";

        std::env::set_var("XXX", "AAA");
        std::env::set_var("YYY", "BBB");
        std::env::set_var("ZZZ", "CCC");

        let config: RelayConfig = toml::from_str(example).unwrap();
        assert_eq!(config.name, "relay1");
        assert_eq!(config.url, "url");
        assert_eq!(config.priority, 0);
        assert_eq!(config.authorization_header.unwrap(), "AAA");
        assert_eq!(config.builder_id_header.unwrap(), "BBB");
        assert_eq!(config.api_token_header.unwrap(), "CCC");
    }
}
