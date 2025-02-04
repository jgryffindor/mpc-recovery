use crate::gcp::error::DatastoreStorageError;
use crate::gcp::GcpService;
use crate::kdf;
use crate::protocol::{SignQueue, SignRequest};
use crate::types::LatestBlockHeight;
use anyhow::Context as _;
use crypto_shared::{derive_epsilon, ScalarExt};
use k256::Scalar;
use near_account_id::AccountId;
use near_lake_framework::{LakeBuilder, LakeContext};
use near_lake_primitives::actions::ActionMetaDataExt;
use near_lake_primitives::receipts::ExecutionStatus;

use near_primitives::types::BlockHeight;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Configures indexer.
#[derive(Debug, Clone, clap::Parser)]
#[group(id = "indexer_options")]
pub struct Options {
    /// AWS S3 bucket name for NEAR Lake Indexer
    #[clap(
        long,
        env("MPC_INDEXER_S3_BUCKET"),
        default_value = "near-lake-data-testnet"
    )]
    pub s3_bucket: String,

    /// AWS S3 region name for NEAR Lake Indexer
    #[clap(long, env("MPC_INDEXER_S3_REGION"), default_value = "eu-central-1")]
    pub s3_region: String,

    /// AWS S3 URL for NEAR Lake Indexer (can be used to point to LocalStack)
    #[clap(long, env("MPC_INDEXER_S3_URL"))]
    pub s3_url: Option<String>,

    /// The block height to start indexing from.
    // Defaults to the latest block on 2023-11-14 07:40:22 AM UTC
    #[clap(
        long,
        env("MPC_INDEXER_START_BLOCK_HEIGHT"),
        default_value = "145964826"
    )]
    pub start_block_height: u64,
}

impl Options {
    pub fn into_str_args(self) -> Vec<String> {
        let mut opts = vec![
            "--s3-bucket".to_string(),
            self.s3_bucket,
            "--s3-region".to_string(),
            self.s3_region,
            "--start-block-height".to_string(),
            self.start_block_height.to_string(),
        ];

        if let Some(s3_url) = self.s3_url {
            opts.extend(vec!["--s3-url".to_string(), s3_url]);
        }

        opts
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct SignArguments {
    request: UnvalidatedContractSignRequest,
}

/// What is recieved when sign is called
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct UnvalidatedContractSignRequest {
    pub payload: [u8; 32],
    pub path: String,
    pub key_version: u32,
}

/// A validated version of the sign request
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ContractSignRequest {
    pub payload: Scalar,
    pub path: String,
    pub key_version: u32,
}

#[derive(Debug, Clone)]
pub struct Indexer {
    latest_block_height: Arc<RwLock<LatestBlockHeight>>,
    last_updated_timestamp: Arc<RwLock<Instant>>,
}

impl Indexer {
    const BEHIND_THRESHOLD: Duration = Duration::from_secs(60);

    fn new(latest_block_height: LatestBlockHeight) -> Self {
        Self {
            latest_block_height: Arc::new(RwLock::new(latest_block_height)),
            last_updated_timestamp: Arc::new(RwLock::new(Instant::now())),
        }
    }

    /// Get the latest block height from the chain.
    pub async fn latest_block_height(&self) -> BlockHeight {
        self.latest_block_height.read().await.block_height
    }

    /// Check whether the indexer is on track with the latest block height from the chain.
    pub async fn is_on_track(&self) -> bool {
        self.last_updated_timestamp.read().await.elapsed() <= Self::BEHIND_THRESHOLD
    }

    /// Check whether the indexer is behind with the latest block height from the chain.
    pub async fn is_behind(&self) -> bool {
        self.last_updated_timestamp.read().await.elapsed() > Self::BEHIND_THRESHOLD
    }

    async fn update_block_height(
        &self,
        block_height: BlockHeight,
        gcp: &GcpService,
    ) -> Result<(), DatastoreStorageError> {
        *self.last_updated_timestamp.write().await = Instant::now();
        self.latest_block_height
            .write()
            .await
            .set(block_height)
            .store(gcp)
            .await
    }
}

#[derive(LakeContext)]
struct Context {
    mpc_contract_id: AccountId,
    node_account_id: AccountId,
    gcp_service: GcpService,
    queue: Arc<RwLock<SignQueue>>,
    indexer: Indexer,
}

async fn handle_block(
    mut block: near_lake_primitives::block::Block,
    ctx: &Context,
) -> anyhow::Result<()> {
    let mut pending_requests = Vec::new();
    for action in block.actions().cloned().collect::<Vec<_>>() {
        if action.receiver_id() == ctx.mpc_contract_id {
            let receipt =
                anyhow::Context::with_context(block.receipt_by_id(&action.receipt_id()), || {
                    format!(
                        "indexer unable to find block for receipt_id={}",
                        action.receipt_id()
                    )
                })?;
            let ExecutionStatus::SuccessReceiptId(receipt_id) = receipt.status() else {
                continue;
            };
            if let Some(function_call) = action.as_function_call() {
                if function_call.method_name() == "sign" {
                    if let Ok(arguments) =
                        serde_json::from_slice::<'_, SignArguments>(function_call.args())
                    {
                        if receipt.logs().is_empty() {
                            tracing::warn!("`sign` did not produce entropy");
                            continue;
                        }
                        let payload = Scalar::from_bytes(arguments.request.payload)
                            .context("Payload cannot be converted to scalar, not in k256 field")?;
                        let Ok(entropy) = serde_json::from_str::<'_, [u8; 32]>(&receipt.logs()[1])
                        else {
                            tracing::warn!(
                                "`sign` did not produce entropy correctly: {:?}",
                                receipt.logs()[0]
                            );
                            continue;
                        };
                        let epsilon =
                            derive_epsilon(&action.predecessor_id(), &arguments.request.path);
                        let delta = kdf::derive_delta(receipt_id, entropy);
                        tracing::info!(
                            receipt_id = %receipt_id,
                            caller_id = receipt.predecessor_id().to_string(),
                            our_account = ctx.node_account_id.to_string(),
                            payload = hex::encode(arguments.request.payload),
                            key_version = arguments.request.key_version,
                            entropy = hex::encode(entropy),
                            "indexed new `sign` function call"
                        );
                        let request = ContractSignRequest {
                            payload,
                            path: arguments.request.path,
                            key_version: arguments.request.key_version,
                        };
                        pending_requests.push(SignRequest {
                            receipt_id,
                            request,
                            epsilon,
                            delta,
                            entropy,
                            time_added: Instant::now(),
                        });
                    }
                }
            }
        }
    }

    ctx.indexer
        .update_block_height(block.block_height(), &ctx.gcp_service)
        .await?;

    crate::metrics::LATEST_BLOCK_HEIGHT
        .with_label_values(&[ctx.gcp_service.account_id.as_str()])
        .set(block.block_height() as i64);

    // Add the requests after going through the whole block to avoid partial processing if indexer fails somewhere.
    // This way we can revisit the same block if we failed while not having added the requests partially.
    let mut queue = ctx.queue.write().await;
    for request in pending_requests {
        queue.add(request);
        crate::metrics::NUM_SIGN_REQUESTS
            .with_label_values(&[ctx.gcp_service.account_id.as_str()])
            .inc();
    }
    drop(queue);

    if block.block_height() % 1000 == 0 {
        tracing::info!(block_height = block.block_height(), "indexed block");
    }
    Ok(())
}

pub fn run(
    options: &Options,
    mpc_contract_id: &AccountId,
    node_account_id: &AccountId,
    queue: &Arc<RwLock<SignQueue>>,
    gcp_service: &crate::gcp::GcpService,
    rt: &tokio::runtime::Runtime,
) -> anyhow::Result<(JoinHandle<anyhow::Result<()>>, Indexer)> {
    tracing::info!(
        s3_bucket = options.s3_bucket,
        s3_region = options.s3_region,
        s3_url = options.s3_url,
        start_block_height = options.start_block_height,
        %mpc_contract_id,
        "starting indexer"
    );

    let latest_block_height = rt.block_on(async {
        match LatestBlockHeight::fetch(gcp_service).await {
            Ok(latest) => latest,
            Err(err) => {
                tracing::warn!(%err, "failed to fetch latest block height; using start_block_height={} instead", options.start_block_height);
                LatestBlockHeight {
                    account_id: node_account_id.clone(),
                    block_height: options.start_block_height,
                }
            }
        }
    });

    let indexer = Indexer::new(latest_block_height);
    let context = Context {
        mpc_contract_id: mpc_contract_id.clone(),
        node_account_id: node_account_id.clone(),
        gcp_service: gcp_service.clone(),
        queue: queue.clone(),
        indexer: indexer.clone(),
    };

    let options = options.clone();
    let join_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        // If indexer fails for whatever reason, let's spin it back up:
        let mut i = 0;
        loop {
            if i > 0 {
                tracing::info!("restarting indexer after failure: restart count={i}");
            }
            i += 1;

            let Ok(lake) = rt.block_on(async {
                let latest = context.indexer.latest_block_height().await;
                let mut lake_builder = LakeBuilder::default()
                    .s3_bucket_name(&options.s3_bucket)
                    .s3_region_name(&options.s3_region)
                    .start_block_height(latest);

                if let Some(s3_url) = &options.s3_url {
                    let aws_config = aws_config::from_env().load().await;
                    let s3_config = aws_sdk_s3::config::Builder::from(&aws_config)
                        .endpoint_url(s3_url)
                        .build();
                    lake_builder = lake_builder.s3_config(s3_config);
                }
                let lake = lake_builder.build()?;
                anyhow::Ok(lake)
            }) else {
                tracing::error!(?options, "indexer failed to build");
                backoff(i);
                continue;
            };

            // TODO/NOTE: currently indexer does not have any interrupt handlers and will never yield back
            // as successful. We can add interrupt handlers in the future but this is not important right
            // now since we managing nodes through integration tests that can kill it or through docker.
            let Err(err) = lake.run_with_context(handle_block, &context) else {
                break;
            };
            tracing::error!(%err, "indexer failed");
            backoff(i)
        }
        Ok(())
    });

    Ok((join_handle, indexer))
}

fn backoff(i: u32) {
    // Exponential backoff with max delay of 2 minutes
    let delay = if i <= 7 {
        2u64.pow(i)
    } else {
        // Max out at 2 minutes
        120
    };
    std::thread::sleep(std::time::Duration::from_secs(delay));
}
