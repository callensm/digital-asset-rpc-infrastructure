mod backfiller;
mod error;
mod metrics;
mod program_transformers;
mod tasks;

use crate::{
    backfiller::backfiller,
    error::IngesterError,
    metrics::safe_metric,
    program_transformers::ProgramTransformer,
    tasks::{common::task::DownloadMetadataTask, BgTask, TaskData, TaskManager},
};
use blockbuster::instruction::{order_instructions, InstructionBundle, IxPair};
use cadence::{BufferedUdpMetricSink, QueuingMetricSink, StatsdClient};
use cadence_macros::{set_global_default, statsd_count, statsd_gauge, statsd_time};
use chrono::Utc;
use figment::{providers::Env, value::Value, Figment};
use futures_util::TryFutureExt;
use plerkle_messenger::{
    redis_messenger::RedisMessenger, Messenger, MessengerConfig, RecvData, ACCOUNT_STREAM,
    TRANSACTION_STREAM,
};
use plerkle_serialization::{root_as_account_info, root_as_transaction_info, Pubkey as FBPubkey};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde::Deserialize;

use sqlx::{self, postgres::PgPoolOptions, Pool, Postgres};
use std::fmt::{Display, Formatter};
use std::net::UdpSocket;
use tokio::{sync::mpsc::UnboundedSender, task::JoinSet};
// Types and constants used for Figment configuration items.
pub type DatabaseConfig = figment::value::Dict;

pub const DATABASE_URL_KEY: &str = "url";
pub const DATABASE_LISTENER_CHANNEL_KEY: &str = "listener_channel";

pub type RpcConfig = figment::value::Dict;

pub const RPC_URL_KEY: &str = "url";
pub const RPC_COMMITMENT_KEY: &str = "commitment";

#[derive(Deserialize, PartialEq, Debug, Clone)]
pub enum IngesterRole {
    All,
    Backfiller,
    BackgroundTaskRunner,
    Ingester,
}

impl Display for IngesterRole {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            IngesterRole::All => write!(f, "all"),
            IngesterRole::Backfiller => write!(f, "backfiller"),
            IngesterRole::BackgroundTaskRunner => write!(f, "background_task_runner"),
            IngesterRole::Ingester => write!(f, "ingester"),
        }
    }
}

// Struct used for Figment configuration items.
#[derive(Deserialize, PartialEq, Debug, Clone)]
pub struct IngesterConfig {
    pub database_config: DatabaseConfig,
    pub messenger_config: MessengerConfig,
    pub env: Option<String>,
    pub rpc_config: RpcConfig,
    pub metrics_port: Option<u16>,
    pub metrics_host: Option<String>,
    pub backfiller: Option<bool>,
    pub role: Option<IngesterRole>,
    pub max_postgres_connections: Option<u32>,
}

fn setup_metrics(config: &IngesterConfig) {
    let uri = config.metrics_host.clone();
    let port = config.metrics_port;
    let env = config.env.clone().unwrap_or("dev".to_string());
    if uri.is_some() || port.is_some() {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let host = (uri.unwrap(), port.unwrap());
        let udp_sink = BufferedUdpMetricSink::from(host, socket).unwrap();
        let queuing_sink = QueuingMetricSink::from(udp_sink);

        let builder = StatsdClient::builder("das_ingester", queuing_sink);
        let client = builder.with_tag("env", env).build();
        set_global_default(client);
    }
}

fn rand_string() -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(30)
        .map(char::from)
        .collect()
}


#[tokio::main]
async fn main() {
    // Read config.
    println!("Starting DASgester");
    let mut config: IngesterConfig = Figment::new()
        .join(Env::prefixed("INGESTER_"))
        .extract()
        .map_err(|config_error| IngesterError::ConfigurationError {
            msg: format!("{}", config_error),
        })
        .unwrap();
    config
        .messenger_config
        .connection_config
        .insert("consumer_id".to_string(), Value::from(rand_string()));

    setup_metrics(&config);

    let url = config
        .database_config
        .get(DATABASE_URL_KEY)
        .and_then(|u| u.clone().into_string())
        .ok_or(IngesterError::ConfigurationError {
            msg: format!("Database connection string missing: {}", DATABASE_URL_KEY),
        })
        .unwrap();

    let pool = PgPoolOptions::new()
        .max_connections(config.max_postgres_connections.unwrap_or(100))
        .connect(&url)
        .await
        .unwrap();

    let backfiller = backfiller::<RedisMessenger>(pool.clone(), config.clone()).await;

    let bg_task_definitions: Vec<Box<dyn BgTask>> = vec![Box::new(DownloadMetadataTask {})];
    let mut background_task_manager =
        TaskManager::new(rand_string(), pool.clone(), bg_task_definitions);
    let background_task_manager_handle = background_task_manager.start_listener();
    let backgroun_task_sender = background_task_manager.get_sender().unwrap();

    let txn_stream = service_transaction_stream::<RedisMessenger>(
        pool.clone(),
        backgroun_task_sender.clone(), // This is allowed because we must
        config.messenger_config.clone(),
    );
    let account_stream = service_account_stream::<RedisMessenger>(
        pool.clone(),
        backgroun_task_sender,
        config.messenger_config.clone(),
    );

    let mut tasks = JoinSet::new();

    let role = config.role.unwrap_or(IngesterRole::All);

    match role {
        IngesterRole::All => {
            tasks.spawn(backfiller);
            tasks.spawn(txn_stream.await);
            tasks.spawn(account_stream.await);
            tasks.spawn(background_task_manager_handle);
            tasks.spawn(background_task_manager.start_runner());
        }
        IngesterRole::Backfiller => {
            tasks.spawn(backfiller);
        }
        IngesterRole::BackgroundTaskRunner => {
            tasks.spawn(background_task_manager.start_runner());
        }
        IngesterRole::Ingester => {
            tasks.spawn(background_task_manager_handle);
            tasks.spawn(txn_stream.await);
            tasks.spawn(account_stream.await);
        }
    }
    let roles_str = role.to_string();
    safe_metric(|| {
        statsd_count!("ingester.startup", 1, "role" => &roles_str);
    });

    // Wait for ctrl-c.
    match tokio::signal::ctrl_c().await {
        Ok(()) => {}
        Err(err) => {
            println!("Unable to listen for shutdown signal: {}", err);
        }
    }

    tasks.shutdown().await;
}

async fn service_transaction_stream<T: Messenger>(
    pool: Pool<Postgres>,
    tasks: UnboundedSender<TaskData>,
    messenger_config: MessengerConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let pool_cloned = pool.clone();
            let tasks_cloned = tasks.clone();
            let messenger_config_cloned = messenger_config.clone();

            let result = tokio::spawn(async {
                let manager = ProgramTransformer::new(pool_cloned, tasks_cloned);
                let mut messenger = T::new(messenger_config_cloned).await.unwrap();
                println!("Setting up transaction listener");

                loop {
                    if let Ok(data) = messenger.recv(TRANSACTION_STREAM).await {
                        let mut ids = handle_transaction(&manager, data).await;
                        if !ids.is_empty() {
                            if let Err(e) = messenger.ack_msg(TRANSACTION_STREAM, &ids).await {
                                println!("Error ACK-ing messages {:?}", e);
                            }
                        }
                    }
                }
            })
            .await;

            match result {
                Ok(_) => break,
                Err(err) if err.is_panic() => {
                    statsd_count!("ingester.service_transaction_stream.task_panic", 1);
                }
                Err(err) => {
                    let err = err.to_string();
                    statsd_count!("ingester.service_transaction_stream.task_error", 1, "error" => &err);
                }
            }
        }
    })
}

async fn service_account_stream<T: Messenger>(
    pool: Pool<Postgres>,
    tasks: UnboundedSender<TaskData>,
    messenger_config: MessengerConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let pool_cloned = pool.clone();
            let tasks_cloned = tasks.clone();
            let messenger_config_cloned = messenger_config.clone();

            let result = tokio::spawn(async {
                let manager = ProgramTransformer::new(pool_cloned, tasks_cloned);
                let mut messenger = T::new(messenger_config_cloned).await.unwrap();
                println!("Setting up account listener");

                loop {
                    if let Ok(data) = messenger.recv(ACCOUNT_STREAM).await {
                        let mut ids = handle_account(&manager, data).await;
                        if !ids.is_empty() {
                            if let Err(e) = messenger.ack_msg(ACCOUNT_STREAM, &ids).await {
                                println!("Error ACK-ing messages {:?}", e);
                            }
                        }
                    }
                }
            })
            .await;

            match result {
                Ok(_) => break,
                Err(err) if err.is_panic() => {
                    statsd_count!("ingester.service_account_stream.task_panic", 1);
                }
                Err(err) => {
                    let err = err.to_string();
                    statsd_count!("ingester.service_account_stream.task_error", 1, "error" => &err);
                }
            }
        }
    })
}

async fn handle_account(manager: &ProgramTransformer, data: Vec<RecvData<'_>>) -> Vec<String> {
    statsd_gauge!("ingester.account_batch_size", data.len() as u64);
    let mut ids = Vec::new();
    for item in data {
        let id = item.id.to_string();
        let data = item.data;
        // Get root of account info flatbuffers object.
        let account_update = match root_as_account_info(data) {
            Err(err) => {
                println!("Flatbuffers AccountInfo deserialization error: {err}");
                continue;
            }
            Ok(account_update) => account_update,
        };
        let seen_at = Utc::now();
        let str_program_id =
            bs58::encode(account_update.owner().unwrap().0.as_slice()).into_string();
        safe_metric(|| {
            statsd_count!("ingester.account_update_seen", 1, "owner" => &str_program_id);
        });
        safe_metric(|| {
            statsd_time!(
                "ingester.account_bus_ingest_time",
                (seen_at.timestamp_millis() - account_update.seen_at()) as u64
            );
        });
        let begin_processing = Utc::now();
        let res = manager.handle_account_update(account_update).await;
        let finish_processing = Utc::now();
        match res {
            Ok(_) => {
                safe_metric(|| {
                    let proc_time = (finish_processing.timestamp_millis()
                        - begin_processing.timestamp_millis())
                        as u64;
                    statsd_time!("ingester.account_proc_time", proc_time);
                });
                safe_metric(|| {
                    statsd_count!("ingester.account_update_success", 1, "owner" => &str_program_id);
                });
                ids.push(id);
            }
            Err(err) => {
                println!("Error handling account update: {:?}", err);
                safe_metric(|| {
                    statsd_count!("ingester.account_update_error", 1, "owner" => &str_program_id);
                });
            }
        };
    }
    ids
}

async fn process_instruction<'i>(
    manager: &ProgramTransformer,
    slot: u64,
    keys: &[FBPubkey],
    outer_ix: IxPair<'i>,
    inner_ix: Option<Vec<IxPair<'i>>>,
) -> Result<(), IngesterError> {
    let (program, instruction) = outer_ix;
    let ix_accounts = instruction.accounts().unwrap_or(&[]);
    let ix_account_len = ix_accounts.len();
    let max = ix_accounts.iter().max().copied().unwrap_or(0) as usize;
    if keys.len() < max {
        return Err(IngesterError::DeserializationError(
            "Missing Accounts in Serialized Ixn/Txn".to_string(),
        ));
    }
    let ix_accounts = ix_accounts
        .iter()
        .fold(Vec::with_capacity(ix_account_len), |mut acc, a| {
            if let Some(key) = keys.get(*a as usize) {
                acc.push(*key);
            }
            //else case here is handled on 272
            acc
        });
    let bundle = InstructionBundle {
        txn_id: "",
        program,
        instruction,
        inner_ix,
        keys: ix_accounts.as_slice(),
        slot,
    };
    manager.handle_instruction(&bundle).await
}

async fn handle_transaction(manager: &ProgramTransformer, data: Vec<RecvData<'_>>) -> Vec<String> {
    statsd_gauge!("ingester.txn_batch_size", data.len() as u64);
    let mut ids = Vec::new();
    for item in data {
        let id = item.id.to_string();
        let tx_data = item.data;
        //TODO -> Dedupe the stream, the stream could have duplicates as a way of ensuring fault tolerance if one validator node goes down.
        //  Possible solution is dedup on the plerkle side but this doesnt follow our principle of getting messages out of the validator asd fast as possible.
        //  Consider a Messenger Implementation detail the deduping of whats in this stream so that
        //  1. only 1 ingest instance picks it up, two the stream coming out of the ingester can be considered deduped
        //
        // can we paralellize this : yes

        // Get root of transaction info flatbuffers object.
        if let Ok(tx) = root_as_transaction_info(tx_data) {
            let instructions = manager.break_transaction(&tx);
            let keys = tx.account_keys().unwrap_or(&[]);
            if let Some(si) = tx.slot_index() {
                let slt_idx = format!("{}-{}", tx.slot(), si);
                safe_metric(|| {
                    statsd_count!("ingester.transaction_event_seen", 1, "slot-idx" => &slt_idx);
                });
            }
            let seen_at = Utc::now();
            safe_metric(|| {
                statsd_time!(
                    "ingester.bus_ingest_time",
                    (seen_at.timestamp_millis() - tx.seen_at()) as u64
                );
            });
            for (outer_ix, inner_ix) in instructions {
                let (program, _) = &outer_ix;
                let str_program_id = bs58::encode(program.0.as_slice()).into_string();
                let begin_processing = Utc::now();
                let res = process_instruction(manager, tx.slot(), keys, outer_ix, inner_ix).await;
                let finish_processing = Utc::now();
                match res {
                    Ok(_) => {
                        safe_metric(|| {
                            let proc_time = (finish_processing.timestamp_millis()
                                - begin_processing.timestamp_millis())
                                as u64;
                            statsd_time!("ingester.tx_proc_time", proc_time);
                        });
                        safe_metric(|| {
                            statsd_count!("ingester.tx_ingest_success", 1, "owner" => &str_program_id);
                        });
                        ids.push(id.clone());
                    }
                    Err(err) => {
                        println!("Error handling transaction: {:?}", err);
                        safe_metric(|| {
                            statsd_count!("ingester.tx_ingest_error", 1, "owner" => &str_program_id);
                        });
                    }
                };
            }
        }
    }
    ids
}
