use crossbeam::channel::{unbounded, Sender};
use digital_asset_types::dao::cl_audits;
use log::{trace, warn};
use plerkle_messenger::{MessengerConfig, TRANSACTION_STREAM};
use plerkle_serialization::serializer::seralize_encoded_transaction_with_status;
use sea_orm::{QueryOrder, Value};
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use tokio::runtime::Builder;

use {
    anchor_client::anchor_lang::AnchorDeserialize,
    anyhow::Context,
    clap::{arg, Parser, Subcommand},
    figment::util::map,
    futures::{
        future::{try_join, try_join_all, BoxFuture, FutureExt, TryFutureExt},
        stream::{self, StreamExt},
    },
    log::{debug, error, info},
    sea_orm::{
        sea_query::Expr, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, DbErr,
        EntityTrait, FromQueryResult, QueryFilter, QuerySelect, QueryTrait, SqlxPostgresConnector,
        Statement,
    },
    // plerkle_serialization::serializer::seralize_encoded_transaction_with_status,
    // solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config,
    solana_client::{
        nonblocking::rpc_client::RpcClient, rpc_config::RpcTransactionConfig,
        rpc_request::RpcRequest,
    },
    solana_sdk::{
        commitment_config::{CommitmentConfig, CommitmentLevel},
        pubkey::{ParsePubkeyError, Pubkey},
        signature::Signature,
        transaction::VersionedTransaction,
    },
    solana_transaction_status::{
        option_serializer::OptionSerializer, EncodedConfirmedTransactionWithStatusMeta,
        UiTransactionEncoding, UiTransactionStatusMeta,
    },
    // solana_sdk::signature::Signature,
    // solana_transaction_status::UiTransactionEncoding,
    spl_account_compression::{
        state::{
            merkle_tree_get_size, ConcurrentMerkleTreeHeader, CONCURRENT_MERKLE_TREE_HEADER_SIZE_V1,
        },
        AccountCompressionEvent, ChangeLogEvent,
    },
    sqlx::postgres::{PgConnectOptions, PgPoolOptions},
    std::{
        cmp,
        collections::HashMap,
        env,
        num::NonZeroUsize,
        pin::Pin,
        str::FromStr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    },
    tokio::{
        fs::OpenOptions,
        io::{stdout, AsyncWrite, AsyncWriteExt},
        sync::{mpsc, Mutex},
    },
    txn_forwarder::{find_signatures, read_lines, rpc_tx_with_retries},
};

const RPC_GET_TXN_RETRIES: u8 = 5;
const RPC_TXN_CONFIG: RpcTransactionConfig = RpcTransactionConfig {
    encoding: Some(UiTransactionEncoding::Base64),
    commitment: Some(CommitmentConfig {
        commitment: CommitmentLevel::Finalized,
    }),
    max_supported_transaction_version: Some(0),
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("failed to load Transaction Meta")]
    TransactionMeta,
    #[error("failed to decode Transaction")]
    Transaction,
    #[error("failed to decode instruction data: {0}")]
    Instruction(#[from] bs58::decode::Error),
    #[error("failed to parse pubkey: {0}")]
    Pubkey(#[from] ParsePubkeyError),
}

#[derive(Debug, FromQueryResult, Clone)]
struct MaxSeqItem {
    max_seq: i64,
    cnt_seq: i64,
}

#[allow(dead_code)]
#[derive(Debug, FromQueryResult, Clone)]
struct MissingSeq {
    missing_seq: i64,
}

#[derive(Debug, FromQueryResult)]
struct AssetMaxSeq {
    leaf_idx: i64,
    seq: i64,
}

#[derive(Debug)]
struct LeafNode {
    leaf: Vec<u8>,
    index: i64,
}

type MaybeLeafNode = Option<LeafNode>;

#[derive(Parser)]
#[command(next_line_help = true, author, version, about)]
struct Args {
    /// Solana RPC endpoint.
    #[arg(long, short, alias = "rpc-url")]
    rpc: String,

    /// Number of concurrent requests for fetching transactions.
    #[arg(long, short, default_value_t = 25)]
    concurrency: usize,

    /// Maximum number of retries for transaction fetching.
    #[arg(long, short, default_value_t = 3)]
    max_retries: u8,

    #[command(subcommand)]
    action: Action,
}

impl Args {
    async fn get_pg_conn(&self) -> anyhow::Result<DatabaseConnection> {
        match &self.action {
            Action::CheckTree { pg_url, .. }
            | Action::CheckTrees { pg_url, .. }
            | Action::CheckTreeLeafs { pg_url, .. }
            | Action::CheckTreesLeafs { pg_url, .. }
            | Action::FixTree { pg_url, .. } => {
                let options: PgConnectOptions = pg_url.parse().unwrap();

                // Create postgres pool
                let pool = PgPoolOptions::new()
                    .min_connections(2)
                    .max_connections(10)
                    .connect_with(options)
                    .await?;

                // Create new postgres connection
                Ok(SqlxPostgresConnector::from_sqlx_postgres_pool(pool))
            }
            Action::ShowTree { .. } | Action::ShowTrees { .. } => {
                anyhow::bail!("show-tree and show-tress do not have connection to database")
            }
        }
    }
    async fn get_messenger_config(&self) -> anyhow::Result<MessengerConfig> {
        match &self.action {
            Action::FixTree { redis_url, .. } => {
                let config_wrapper = figment::value::Value::from(map! {
                    "redis_connection_str" => redis_url.to_string(),
                    "pipeline_size_bytes" => 1u128.to_string(),
                });
                let config = config_wrapper.into_dict().unwrap();

                let messenenger_config = MessengerConfig {
                    messenger_type: plerkle_messenger::MessengerType::Redis,
                    connection_config: config,
                };
                Ok(messenenger_config)
            }
            _ => {
                anyhow::bail!("No redis client supported")
            }
        }
    }
}

#[derive(Subcommand, Clone)]
enum Action {
    /// Checks a single merkle tree to check if it's fully indexed
    CheckTree {
        #[arg(short, long)]
        pg_url: String,
        #[arg(short, long, help = "Tree pubkey")]
        tree: String,
    },
    /// Checks a list of merkle trees to check if they're fully indexed
    CheckTrees {
        #[arg(short, long)]
        pg_url: String,
        #[arg(short, long, help = "Path to file with trees pubkeys")]
        file: String,
    },
    /// Checks leafs from a single merkle tree with assets from database
    CheckTreeLeafs {
        #[arg(short, long)]
        pg_url: String,
        #[arg(short, long)]
        output: Option<String>,
        #[arg(short, long, help = "Tree pubkey")]
        tree: String,
    },
    /// Checks leafs from merkle tree from a file with assets from database
    CheckTreesLeafs {
        #[arg(short, long)]
        pg_url: String,
        #[arg(short, long)]
        output: Option<String>,
        #[arg(short, long, help = "Path to file with trees pubkeys")]
        file: String,
    },
    /// Show a tree
    ShowTree {
        #[arg(short, long, help = "Takes a single tree as a parameter to check")]
        tree: String,
    },
    /// Shows a list of trees
    ShowTrees {
        #[arg(short, long, help = "Path to file with trees pubkeys")]
        file: String,
    },
    /// Submits txns for the missing gaps in a Merkle tree.
    FixTree {
        #[arg(short, long)]
        pg_url: String,
        #[arg(short, long)]
        redis_url: String,
        #[arg(short, long, help = "Tree pubkey")]
        tree: String,
        #[arg(
            short,
            long,
            help = "Concurrency for fetching signatures for sequence batches"
        )]
        get_sigs_concurrency: Option<usize>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // RUST_LOG=info,sqlx=warn,tree_status=debug
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info,sqlx=warn".into()),
    );
    env_logger::init();

    let args = Args::parse();

    let concurrency = NonZeroUsize::new(args.concurrency)
        .ok_or_else(|| anyhow::anyhow!("invalid concurrency: {}", args.concurrency))?;

    // Set up RPC interface
    let pubkeys_str = match &args.action {
        Action::CheckTree { tree, .. }
        | Action::CheckTreeLeafs { tree, .. }
        | Action::FixTree { tree, .. }
        | Action::ShowTree { tree } => {
            let tree = tree.to_string();
            stream::once(async move { Ok(tree) }).boxed()
        }
        Action::CheckTrees { file, .. }
        | Action::CheckTreesLeafs { file, .. }
        | Action::ShowTrees { file } => read_lines(file).await?.boxed(),
    };

    let mut pubkeys = pubkeys_str.map(|maybe_pubkey_str| {
        maybe_pubkey_str.map_err(Into::into).and_then(|pubkey_str| {
            pubkey_str
                .parse::<Pubkey>()
                .with_context(|| format!("failed to parse pubkey: {}", &pubkey_str))
        })
    });

    match &args.action {
        Action::CheckTree { .. } | Action::CheckTrees { .. } => {
            let client = RpcClient::new(args.rpc.clone());
            let conn = args.get_pg_conn().await?;
            while let Some(maybe_pubkey) = pubkeys.next().await {
                let pubkey = maybe_pubkey?;
                info!("checking tree {pubkey}, hex: {}", hex::encode(pubkey));
                if let Err(error) = check_tree(pubkey, &client, &conn).await {
                    error!("{:?}", error);
                }
            }
        }
        Action::CheckTreeLeafs { output, .. } | Action::CheckTreesLeafs { output, .. } => {
            let conn = args.get_pg_conn().await?;
            let mut output: Option<Pin<Box<dyn AsyncWrite>>> = if let Some(output) = output {
                Some(if output == "-" {
                    Box::pin(stdout())
                } else {
                    Box::pin(
                        OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .open(output)
                            .await?,
                    )
                })
            } else {
                None
            };
            while let Some(maybe_pubkey) = pubkeys.next().await {
                let pubkey = maybe_pubkey?;
                info!("checking tree leafs {pubkey}, hex: {}", hex::encode(pubkey));
                if let Err(error) = check_tree_leafs(
                    pubkey,
                    &args.rpc,
                    concurrency,
                    args.max_retries,
                    &conn,
                    output.as_mut(),
                )
                .await
                {
                    error!("{:?}", error);
                }
            }
            if let Some(mut output) = output {
                output.flush().await?;
            }
        }
        Action::ShowTree { .. } | Action::ShowTrees { .. } => {
            while let Some(maybe_pubkey) = pubkeys.next().await {
                let pubkey = maybe_pubkey?;
                info!("showing tree {pubkey}, hex: {}", hex::encode(pubkey));
                if let Err(error) =
                    read_tree(pubkey, &args.rpc, concurrency, args.max_retries).await
                {
                    error!("{:?}", error);
                }
            }
        }
        Action::FixTree {
            get_sigs_concurrency,
            pg_url: _,
            redis_url: _,
            tree: _,
        } => {
            let client = RpcClient::new(args.rpc.clone());
            let conn = args.get_pg_conn().await?;
            let messenger_config = args.get_messenger_config().await?;
            if let Some(maybe_pubkey) = pubkeys.next().await {
                let pubkey: Pubkey = maybe_pubkey?;
                info!("fixing tree {pubkey}, hex: {}", hex::encode(pubkey));
                if let Err(error) = fix_tree(
                    pubkey,
                    client,
                    conn,
                    messenger_config,
                    Some(args.concurrency),
                    get_sigs_concurrency.to_owned(),
                )
                .await
                {
                    error!("{:?}", error);
                }
            }
        }
    }

    Ok(())
}

async fn check_tree(
    pubkey: Pubkey,
    client: &RpcClient,
    conn: &DatabaseConnection,
) -> anyhow::Result<()> {
    let onchain_seq: i64 = get_onchain_tree_seq(pubkey, client)
        .await
        .with_context(|| format!("[{pubkey}] tree is missing from chain or error occured"))?
        .try_into()
        .unwrap();

    let indexed_seq = get_tree_max_seq(pubkey, conn)
        .await
        .with_context(|| format!("[{pubkey:?}] counldn't query tree from index"))?
        .ok_or_else(|| anyhow::anyhow!("[{pubkey}] tree missing from index"))?;

    // Check tip
    match indexed_seq.max_seq.cmp(&onchain_seq) {
        cmp::Ordering::Less => {
            warn!(
                "[{pubkey}] Tree not fully indexed. On-chain seq: {}. Indexed seq: {}",
                onchain_seq, indexed_seq.max_seq
            );
        }
        cmp::Ordering::Equal => {
            info!("[{pubkey}] Tree is up-to-date! Seq: {}", onchain_seq)
        }
        cmp::Ordering::Greater => {
            error!(
                "[{pubkey}] Something went wrong. Indexer is ahead of the chain? On-chain seq: {}. Indexed seq: {}",
                onchain_seq, indexed_seq.max_seq
            );
        }
    }

    // Check completeness
    if indexed_seq.max_seq != indexed_seq.cnt_seq {
        warn!(
            "[{pubkey}] Tree has gaps. Max indexed seq: {}. Distinct seqs: {}",
            indexed_seq.max_seq, indexed_seq.cnt_seq
        );
        let missing_seqs = get_missing_seq(pubkey, onchain_seq, conn).await?;
        warn!(
            "[{pubkey}] missing seq ranges: {:?}",
            build_seq_ranges(missing_seqs)
        );
    } else {
        info!("[{:?}] Tree has no gaps!", pubkey)
    }
    Ok(())
}

async fn fix_tree(
    pubkey: Pubkey,
    client: RpcClient,
    conn: DatabaseConnection,
    messenger_config: MessengerConfig,
    get_txn_concurrency: Option<usize>,
    get_sigs_concurrency: Option<usize>,
) -> anyhow::Result<()> {
    let onchain_seq: i64 = get_onchain_tree_seq(pubkey, &client)
        .await
        .with_context(|| format!("[{pubkey}] tree is missing from chain or error occured"))?
        .try_into()
        .unwrap();

    let indexed_seq = get_tree_max_seq(pubkey, &conn)
        .await
        .with_context(|| format!("[{pubkey:?}] counldn't query tree from index"))?
        .ok_or_else(|| anyhow::anyhow!("[{pubkey}] tree missing from index"))?;

    match indexed_seq.max_seq.cmp(&onchain_seq) {
        cmp::Ordering::Less => {
            warn!(
                "[{pubkey}] Tree not fully indexed. On-chain seq: {}. Indexed seq: {}",
                onchain_seq, indexed_seq.max_seq
            );
        }
        cmp::Ordering::Equal => {
            info!("[{pubkey}] Tree is up-to-date! Seq: {}", onchain_seq)
        }
        cmp::Ordering::Greater => {
            error!(
                "[{pubkey}] Something went wrong. Indexer is ahead of the chain? On-chain seq: {}. Indexed seq: {}",
                onchain_seq, indexed_seq.max_seq
            );
        }
    }

    // Check completeness
    if indexed_seq.max_seq != indexed_seq.cnt_seq {
        warn!(
            "[{pubkey}] Tree has gaps. Max indexed seq: {}. Distinct seqs: {}",
            indexed_seq.max_seq, indexed_seq.cnt_seq
        );
        let missing_seqs = get_missing_seq(pubkey, onchain_seq, &conn).await?;
        trace!("[{pubkey}] missing seq: {:?}", missing_seqs);
        find_and_forward_txns_for_missing_seqs(
            pubkey,
            missing_seqs,
            client,
            conn,
            messenger_config,
            get_txn_concurrency,
            get_sigs_concurrency,
        )
        .await?;
    } else {
        info!(
            "[{:?}] Tree has no gaps! Indexed Seq: {:?}",
            pubkey, indexed_seq.max_seq
        )
    }
    Ok(())
}

async fn find_and_forward_txns_for_missing_seqs(
    tree: Pubkey,
    seqs: Vec<i64>,
    client: RpcClient,
    conn: DatabaseConnection,
    messenger_config: MessengerConfig,
    get_txn_concurrency: Option<usize>,
    get_sigs_concurrency: Option<usize>,
) -> anyhow::Result<()> {
    // Concurrency config
    let get_txn_concurrency: usize = get_txn_concurrency.unwrap_or(20);
    let get_sigs_concurrency: usize = get_sigs_concurrency.unwrap_or(3);

    let (r_sender, r_recv) = unbounded();
    let (s_sender, s_recv) = unbounded();

    let client = Arc::new(client);
    let conn = Arc::new(conn);
    let messenger = init_redis_messenger(messenger_config).await?;

    crossbeam::scope(|s| {
        let runtime = Arc::new(
            Builder::new_multi_thread()
                .enable_all()
                .worker_threads(4)
                .build()
                .unwrap(),
        );

        s.spawn(|_| {
            let ranges = build_seq_ranges(seqs);
            info!("Processing seq ranges: {:?}", ranges);
            for range in ranges {
                r_sender.send(range).unwrap();
            }
            drop(r_sender);
        });

        for _ in 0..get_sigs_concurrency {
            let (s_sender, r_recv) = (s_sender.clone(), r_recv.clone());
            let client = client.clone();
            let conn = conn.clone();
            let runtime = runtime.clone();
            // Spawn workers in separate threads
            s.spawn(move |_| {
                for range in r_recv.iter() {
                    info!("Processing seq range: {:?}", range);
                    match runtime.block_on(find_signatures_for_missing_seq_range(
                        tree, range, &client, &conn, &s_sender,
                    )) {
                        Ok(_) => {}
                        Err(err) => {
                            warn!("error processing seq range: {:?}, error: {:?}", range, err)
                        }
                    }
                }
            });
        }
        drop(s_sender);

        for _ in 0..get_txn_concurrency {
            let s_recv = s_recv.clone();
            let client = client.clone();
            let messenger = messenger.clone();
            let runtime = runtime.clone();
            s.spawn(move |_| {
                for sig in s_recv.iter() {
                    trace!("Attempting to send signature to redis: {:?}", sig);
                    runtime
                        .block_on(send_txn(sig, &client, &messenger))
                        .unwrap();
                }
            });
        }
    })
    .unwrap();

    anyhow::Ok(())
}

async fn init_redis_messenger(
    config: MessengerConfig,
) -> anyhow::Result<Arc<Mutex<Box<dyn plerkle_messenger::Messenger>>>> {
    let mut messenger = plerkle_messenger::select_messenger(config).await?;
    messenger.add_stream(TRANSACTION_STREAM).await?;
    messenger
        .set_buffer_size(TRANSACTION_STREAM, 10000000000000000)
        .await;
    anyhow::Ok(Arc::new(Mutex::new(messenger)))
}

async fn send_txn(
    signature: Signature,
    client: &RpcClient,
    messenger: &Mutex<Box<dyn plerkle_messenger::Messenger>>,
) -> anyhow::Result<()> {
    let txn: EncodedConfirmedTransactionWithStatusMeta = rpc_tx_with_retries(
        &client,
        RpcRequest::GetTransaction,
        serde_json::json!([signature.to_string(), RPC_TXN_CONFIG,]),
        RPC_GET_TXN_RETRIES,
        signature,
    )
    .await?;

    // Ignore if tx failed or meta is missed
    let meta = txn.transaction.meta.as_ref();
    if meta.map(|meta| meta.status.is_err()).unwrap_or(true) {
        info!("Dropping failed transaction: {:?}", signature);
        return Ok(());
    }

    let fbb = flatbuffers::FlatBufferBuilder::new();
    let fbb = seralize_encoded_transaction_with_status(fbb, txn)
        .with_context(|| format!("failed to serialize transaction with {}", signature))?;
    let bytes = fbb.finished_data();

    let mut locked = messenger.lock().await;
    locked.send(TRANSACTION_STREAM, bytes).await?;
    drop(locked);
    info!("Successfully pushed transaction to redis: {:?}", signature);
    Ok(())
}

fn build_seq_ranges(seqs: Vec<i64>) -> Vec<(i64, i64)> {
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    if seqs.is_empty() {
        return ranges;
    }

    let mut current_start = seqs[0];
    let mut current_end = seqs[0];
    for &num in seqs.iter().skip(1) {
        if current_end + 1 == num {
            current_end = num;
        } else {
            ranges.push((current_start, current_end));
            current_start = num;
            current_end = num;
        }
    }
    ranges.push((current_start, current_end));

    // Two ranges will be joined if within this gap.
    // This will reduce the calls to GetSignaturesForAddress which will improve overall performance.
    let maximum_join_gap: i32 = 10;
    let mut joined_ranges: Vec<(i64, i64)> = Vec::new();
    let (mut current_start, mut current_end) = ranges[0];
    for (start, end) in ranges.iter().skip(1) {
        if current_end + maximum_join_gap as i64 >= *start {
            current_end = *end;
        } else {
            joined_ranges.push((current_start, current_end));
            current_start = *start;
            current_end = *end;
        }
    }
    joined_ranges.push((current_start, current_end));

    joined_ranges
}

// TODO: Txns submitted not be the right ones! We need a more complex search algo.
// Add the following:
//   1 – Keep searching until finding a successful transaction.
//   2 – Parse txns and extract seq, keep searching until the seq is found (can use Helius for this).
async fn find_signatures_for_missing_seq_range(
    tree: Pubkey,
    range: (i64, i64),
    client: &RpcClient,
    conn: &DatabaseConnection,
    sender: &Sender<Signature>,
) -> anyhow::Result<()> {
    let (start, end) = range;
    trace!("Filling gap for range: [{:?}, {:?}]", start, end);

    // Find the next indexed after the end of the range.
    let before_txn = cl_audits::Entity::find()
        .filter(cl_audits::Column::Tree.eq(tree.as_ref()))
        .filter(cl_audits::Column::Seq.gte(end))
        .order_by_asc(cl_audits::Column::Seq)
        .limit(1)
        .all(conn)
        .await?;
    let before_txn = before_txn.first();

    // Find the indexed seq before the start of the range.
    let until_txn = cl_audits::Entity::find()
        .filter(cl_audits::Column::Tree.eq(tree.as_ref()))
        .filter(cl_audits::Column::Seq.lte(start))
        .order_by_desc(cl_audits::Column::Seq)
        .limit(1)
        .all(conn)
        .await?;
    let until_txn = until_txn.first();

    trace!(
        "Txns for missing seq range [{:?}, {:?}]. Until (start): {:?}. Before (end): {:?}.",
        start,
        end,
        until_txn
            .as_ref()
            .map_or("None".to_string(), |txn| txn.tx.clone()),
        before_txn
            .as_ref()
            .map_or("None".to_string(), |txn| txn.tx.clone()),
    );

    let mut before = before_txn
        .map(|txn| Signature::from_str(&txn.tx).ok())
        .flatten();

    let until = until_txn
        .map(|txn| Signature::from_str(&txn.tx).ok())
        .flatten();
    let limit: usize = 1000;
    loop {
        let config = GetConfirmedSignaturesForAddress2Config {
            before: before,
            until: until,
            limit: Some(limit),
            ..Default::default()
        };
        let sigs = client
            .get_signatures_for_address_with_config(&tree, config)
            .await?;
        for sig in sigs.clone() {
            let o = Signature::from_str(&sig.signature)?;
            sender.send(o)?;
            before = Some(o);
        }
        if sigs.len() == 0 {
            break;
        }
    }

    return anyhow::Ok(());
}

async fn get_onchain_tree_seq(address: Pubkey, client: &RpcClient) -> anyhow::Result<u64> {
    // get account info
    let account_info = client
        .get_account_with_commitment(&address, CommitmentConfig::confirmed())
        .await?;

    let mut account = account_info
        .value
        .ok_or_else(|| anyhow::anyhow!("No account found"))?;

    let (header_bytes, rest) = account
        .data
        .split_at_mut(CONCURRENT_MERKLE_TREE_HEADER_SIZE_V1);
    let header = ConcurrentMerkleTreeHeader::try_from_slice(header_bytes)?;

    // let auth = Pubkey::find_program_address(&[address.as_ref()], &mpl_bubblegum::id()).0;

    let merkle_tree_size = merkle_tree_get_size(&header)?;
    let (tree_bytes, _canopy_bytes) = rest.split_at_mut(merkle_tree_size);

    let seq_bytes = tree_bytes[0..8].try_into().context("Error parsing bytes")?;
    Ok(u64::from_le_bytes(seq_bytes))
}

async fn get_tree_max_seq(
    tree: Pubkey,
    conn: &DatabaseConnection,
) -> Result<Option<MaxSeqItem>, DbErr> {
    let query = cl_audits::Entity::find()
        .select_only()
        .filter(cl_audits::Column::Tree.eq(tree.as_ref()))
        .column_as(Expr::col(cl_audits::Column::Seq).max(), "max_seq")
        .column_as(Expr::cust("count(distinct seq)"), "cnt_seq")
        .build(DbBackend::Postgres);

    MaxSeqItem::find_by_statement(query).one(conn).await
}

// TODO: Break checks into batches for larger trees.
async fn get_missing_seq(
    tree: Pubkey,
    max_seq: i64,
    conn: &DatabaseConnection,
) -> Result<Vec<i64>, DbErr> {
    let query = Statement::from_string(
        DbBackend::Postgres,
        format!(
            "
SELECT
    s.seq AS missing_seq
FROM
    generate_series(1::bigint, {}::bigint) s(seq)
WHERE
    NOT EXISTS (
        SELECT 1 FROM cl_audits WHERE seq = s.seq AND tree='\\x{}'
    )",
            max_seq,
            hex::encode(tree.as_ref())
        ),
    );

    let res: Vec<MissingSeq> = conn
        .query_all(query)
        .await?
        .iter()
        .map(|q| MissingSeq::from_query_result(q, "").unwrap())
        .collect();
    Ok(res.iter().map(|m| m.missing_seq).collect::<Vec<i64>>())
}

async fn check_tree_leafs(
    pubkey: Pubkey,
    client_url: &str,
    concurrency: NonZeroUsize,
    max_retries: u8,
    conn: &DatabaseConnection,
    mut output: Option<&mut Pin<Box<dyn AsyncWrite>>>,
) -> anyhow::Result<()> {
    let (fetch_fut, mut leafs_rx) = read_tree_start(pubkey, client_url, concurrency, max_retries);
    try_join(fetch_fut, async move {
        // collect max seq per leaf index from transactions
        let mut leafs = HashMap::new();
        while let Some((_id, signature, vec)) = leafs_rx.recv().await {
            for (seq, maybe_leaf) in vec.unwrap_or_default() {
                if let Some(LeafNode {
                    index: leaf_idx,
                    leaf: _leaf,
                }) = maybe_leaf
                {
                    let entry = leafs.entry(leaf_idx).or_insert((signature, seq));
                    if entry.1 < seq {
                        *entry = (signature, seq);
                    }
                }
            }
        }

        info!("Found {:?} leaves", leafs.len());

        // fetch from database in chunks
        let query = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "
SELECT
    cl_items.leaf_idx, MAX(asset.seq) AS seq
FROM
    asset
INNER JOIN
    cl_items ON
        cl_items.tree = asset.tree_id AND
        cl_items.seq = asset.seq
WHERE
    asset.tree_id = $1 AND
    cl_items.leaf_idx IS NOT NULL
GROUP BY
    cl_items.leaf_idx
",
            [Value::Bytes(Some(Box::new(pubkey.as_ref().to_vec())))],
        );

        debug!("send query to database...");
        let leafs_db = conn.query_all(query).await?;

        for leaf_db in leafs_db.iter() {
            let leaf_db = AssetMaxSeq::from_query_result(leaf_db, "").unwrap();
            match leafs.remove(&leaf_db.leaf_idx) {
                Some((signature, seq)) => {
                    if leaf_db.seq != seq as i64 {
                        error!(
                            "leaf index {}: invalid seq {} vs {} (db vs blockchain, tx={:?})",
                            leaf_db.leaf_idx, leaf_db.seq, seq, signature
                        );
                    }
                }
                None => {
                    error!("leaf index {}: not found in blockchain", leaf_db.leaf_idx);
                }
            }
        }
        for (leaf_idx, (signature, seq)) in leafs.into_iter() {
            error!("leaf index {leaf_idx}: not found in db, seq {seq} tx={signature:?}");
            if let Some(output) = output.as_mut() {
                let _ = output.write(format!("{signature}\n").as_bytes()).await?;
            }
        }

        Ok(())
    })
    .await
    .map(|_| ())
}

// Fetches all the transactions referencing a specific trees
async fn read_tree(
    pubkey: Pubkey,
    client_url: &str,
    concurrency: NonZeroUsize,
    max_retries: u8,
) -> anyhow::Result<()> {
    fn print_seqs(id: usize, sig: Signature, seqs: Option<Vec<(u64, MaybeLeafNode)>>) {
        for (seq, leaf_idx) in seqs.unwrap_or_default() {
            let leaf_idx = leaf_idx.map(|v| v.index.to_string()).unwrap_or_default();
            info!("{seq} {leaf_idx} {sig} {id}");
        }
    }

    let (fetch_fut, mut print_rx) = read_tree_start(pubkey, client_url, concurrency, max_retries);
    try_join(fetch_fut, async move {
        let mut next_id = 0;
        let mut map = HashMap::new();

        while let Some((id, sig, seqs)) = print_rx.recv().await {
            map.insert(id, (sig, seqs));

            if let Some((sig, seqs)) = map.remove(&next_id) {
                print_seqs(next_id, sig, seqs);
                next_id += 1;
            }
        }

        let mut vec = map.into_iter().collect::<Vec<_>>();
        vec.sort_by_key(|(id, _)| *id);
        for (id, (sig, seqs)) in vec.into_iter() {
            print_seqs(id, sig, seqs);
        }

        Ok(())
    })
    .await
    .map(|_| ())
}

#[allow(clippy::type_complexity)]
fn read_tree_start(
    pubkey: Pubkey,
    client_url: &str,
    concurrency: NonZeroUsize,
    max_retries: u8,
) -> (
    BoxFuture<'static, anyhow::Result<()>>,
    mpsc::UnboundedReceiver<(usize, Signature, Option<Vec<(u64, MaybeLeafNode)>>)>,
) {
    let sig_id = Arc::new(AtomicUsize::new(0));
    let rx_sig = Arc::new(Mutex::new(find_signatures(
        pubkey,
        RpcClient::new(client_url.to_owned()),
        None,
        None,
        2_000,
        false,
    )));

    let (tx, rx) = mpsc::unbounded_channel();
    let tx = Arc::new(tx);

    let fetch_futs = (0..concurrency.get())
        .map(|_| {
            let sig_id = Arc::clone(&sig_id);
            let rx_sig = Arc::clone(&rx_sig);
            let client = RpcClient::new(client_url.to_owned());
            let tx = Arc::clone(&tx);
            async move {
                loop {
                    let mut lock = rx_sig.lock().await;
                    let maybe_msg = lock.recv().await;
                    let id = sig_id.fetch_add(1, Ordering::SeqCst);
                    if id > 0 && id % 10 == 0 {
                        debug!("received {} transactions", id);
                    }
                    drop(lock);
                    match maybe_msg {
                        Some(maybe_sig) => {
                            let signature = maybe_sig?;
                            let mut map = process_tx(signature, &client, max_retries).await?;
                            let _ = tx.send((id, signature, map.remove(&pubkey)));
                        }
                        None => return Ok::<(), anyhow::Error>(()),
                    }
                }
            }
        })
        .collect::<Vec<_>>();
    drop(tx);

    (try_join_all(fetch_futs).map_ok(|_| ()).boxed(), rx)
}

// Process and individual transaction, fetching it and reading out the sequence numbers
async fn process_tx(
    signature: Signature,
    client: &RpcClient,
    max_retries: u8,
) -> anyhow::Result<HashMap<Pubkey, Vec<(u64, MaybeLeafNode)>>> {
    const CONFIG: RpcTransactionConfig = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Base64),
        commitment: Some(CommitmentConfig {
            commitment: CommitmentLevel::Finalized,
        }),
        max_supported_transaction_version: Some(0),
    };

    let tx: EncodedConfirmedTransactionWithStatusMeta = rpc_tx_with_retries(
        client,
        RpcRequest::GetTransaction,
        serde_json::json!([signature.to_string(), CONFIG]),
        max_retries,
        signature,
    )
    .await?;
    parse_tx_sequence(tx).map_err(Into::into)
}

// Parse the trasnaction data
fn parse_tx_sequence(
    tx: EncodedConfirmedTransactionWithStatusMeta,
) -> Result<HashMap<Pubkey, Vec<(u64, MaybeLeafNode)>>, ParseError> {
    let mut seq_updates = HashMap::<Pubkey, Vec<(u64, MaybeLeafNode)>>::new();

    // ignore if tx failed or meta is missed
    let meta = tx.transaction.meta.as_ref();
    if meta.map(|meta| meta.status.is_err()).unwrap_or(true) {
        return Ok(seq_updates);
    }

    // Get `UiTransaction` out of `EncodedTransactionWithStatusMeta`.
    let meta: UiTransactionStatusMeta = tx.transaction.meta.ok_or(ParseError::TransactionMeta)?;

    // See https://github.com/ngundotra/spl-ac-seq-parse/blob/main/src/main.rs
    if let OptionSerializer::Some(inner_instructions_vec) = meta.inner_instructions.as_ref() {
        let transaction: VersionedTransaction = tx
            .transaction
            .transaction
            .decode()
            .ok_or(ParseError::Transaction)?;

        // Add the account lookup stuff
        let mut account_keys = transaction.message.static_account_keys().to_vec();
        if let OptionSerializer::Some(loaded_addresses) = meta.loaded_addresses {
            for pubkey in loaded_addresses.writable.iter() {
                account_keys.push(Pubkey::from_str(pubkey)?);
            }
            for pubkey in loaded_addresses.readonly.iter() {
                account_keys.push(Pubkey::from_str(pubkey)?);
            }
        }

        for inner_ixs in inner_instructions_vec.iter() {
            for inner_ix in inner_ixs.instructions.iter() {
                if let solana_transaction_status::UiInstruction::Compiled(instr) = inner_ix {
                    if let Some(program) = account_keys.get(instr.program_id_index as usize) {
                        if *program == spl_noop::id() {
                            let data = bs58::decode(&instr.data)
                                .into_vec()
                                .map_err(ParseError::Instruction)?;

                            if let Ok(AccountCompressionEvent::ChangeLog(cl_data)) =
                                AccountCompressionEvent::try_from_slice(&data)
                            {
                                let ChangeLogEvent::V1(cl_data) = cl_data;
                                let leaf = cl_data.path.get(0).map(|node| LeafNode {
                                    leaf: node.node.to_vec(),
                                    index: node_idx_to_leaf_idx(
                                        node.index as i64,
                                        cl_data.path.len() as u32 - 1,
                                    ),
                                });
                                seq_updates
                                    .entry(cl_data.id)
                                    .or_default()
                                    .push((cl_data.seq, leaf));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(seq_updates)
}

fn node_idx_to_leaf_idx(index: i64, tree_height: u32) -> i64 {
    index - 2i64.pow(tree_height)
}
