use std::{collections::HashMap, num::NonZero, str::FromStr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::{process, sync::RwLock};
use tonic::{
    IntoRequest,
    transport::{Channel, ClientTlsConfig, Endpoint},
};
use tower_http::cors::CorsLayer;
use webzjs_common::Network;
use webzjs_wallet::Wallet;
use zcash_client_backend::data_api::WalletRead;
use zcash_client_backend::encoding::AddressCodec;
use zcash_client_backend::proto::service::{
    ChainSpec, compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_client_memory::MemoryWalletDb;

type AccountId = <MemoryWalletDb<zcash_protocol::consensus::Network> as WalletRead>::AccountId;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum JobStatus {
    Pending,
    Completed,
}

#[derive(Debug, Clone)]
struct Job {
    status: JobStatus,
    target_increase: u64,
    initial_balance: u64,
    contract_address: String,
    amount: u64,
}

type JobStore = Arc<RwLock<HashMap<String, Job>>>;

struct AppState {
    wallet: Arc<Wallet<MemoryWalletDb<zcash_protocol::consensus::Network>, Channel>>,
    account_id: AccountId,
    jobs: JobStore,
}

async fn get_address(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.wallet.db();
    let db = db.read().await;

    match db.get_current_address(state.account_id) {
        Ok(Some(address)) => {
            let address_string = address.to_address(zcash_protocol::consensus::NetworkType::Test);
            (StatusCode::OK, address_string.to_string())
        }
        Ok(None) => (StatusCode::NOT_FOUND, "Address not found".to_string()),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to get address".to_string(),
        ),
    }
}

async fn sync_wallet(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.wallet.sync().await {
        Ok(_) => (StatusCode::OK, "Wallet synced successfully"),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to sync wallet"),
    }
}

async fn get_orchard_balance(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.wallet.get_wallet_summary().await {
        Ok(Some(summary)) => {
            if let Some(account_balance) = summary.account_balances().get(&state.account_id) {
                let orchard_balance = u64::from(account_balance.orchard_balance().total());
                (StatusCode::OK, orchard_balance.to_string())
            } else {
                (StatusCode::NOT_FOUND, "Account not found".to_string())
            }
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            "No wallet summary available".to_string(),
        ),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to get wallet summary".to_string(),
        ),
    }
}

async fn get_current_orchard_balance(
    wallet: &Wallet<MemoryWalletDb<zcash_protocol::consensus::Network>, Channel>,
    account_id: AccountId,
) -> Option<u64> {
    if let Ok(Some(summary)) = wallet.get_wallet_summary().await {
        if let Some(account_balance) = summary.account_balances().get(&account_id) {
            return Some(u64::from(account_balance.orchard_balance().total()));
        }
    }
    None
}

#[derive(Deserialize)]
struct CreateJobRequest {
    contract_address: String,
    amount: u64,
}

#[derive(Serialize)]
struct SolveRequest {
    #[serde(rename = "contractAddress")]
    contract_address: String,
    amount: u64,
}

#[derive(Serialize)]
struct CreateJobResponse {
    job_id: String,
}

async fn create_job(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateJobRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Get current balance
    let initial_balance = match get_current_orchard_balance(&state.wallet, state.account_id).await {
        Some(balance) => balance,
        None => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    // Generate job ID
    let job_id = uuid::Uuid::new_v4().to_string();

    // Create job
    let job = Job {
        status: JobStatus::Pending,
        target_increase: 100,
        initial_balance,
        contract_address: request.contract_address,
        amount: request.amount,
    };

    // Store job
    state.jobs.write().await.insert(job_id.clone(), job);

    // Spawn background task to monitor balance
    let wallet = state.wallet.clone();
    let account_id = state.account_id;
    let jobs = state.jobs.clone();
    let job_id_clone = job_id.clone();

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;

            println!("checking shielded balance.....");

            if let Some(current_balance) = get_current_orchard_balance(&wallet, account_id).await {
                let (should_complete, contract_address, amount) = {
                    let mut jobs_lock = jobs.write().await;
                    if let Some(job) = jobs_lock.get_mut(&job_id_clone) {
                        println!("current balance: {}", current_balance);

                        if current_balance >= job.initial_balance + job.target_increase {
                            job.status = JobStatus::Completed;
                            println!("job {:?} completed", job);
                            (true, job.contract_address.clone(), job.amount)
                        } else {
                            (false, String::new(), 0)
                        }
                    } else {
                        break;
                    }
                };

                if should_complete {
                    // Call solve endpoint
                    let client = reqwest::Client::new();
                    let solve_request = SolveRequest {
                        contract_address,
                        amount,
                    };

                    match client
                        .post("http://localhost:4000/solve")
                        .json(&solve_request)
                        .send()
                        .await
                    {
                        Ok(response) => {
                            println!(
                                "Solve endpoint called successfully: {:?}",
                                response.status()
                            );
                        }
                        Err(e) => {
                            eprintln!("Failed to call solve endpoint: {:?}", e);
                        }
                    }
                    break;
                }
            }
        }
    });

    Ok(Json(CreateJobResponse { job_id }))
}

#[derive(Serialize)]
struct JobStatusResponse {
    status: JobStatus,
}

async fn get_job_status(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    let jobs = state.jobs.read().await;

    if let Some(job) = jobs.get(&job_id) {
        (
            StatusCode::OK,
            Json(JobStatusResponse {
                status: job.status.clone(),
            }),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(JobStatusResponse {
                status: JobStatus::Pending,
            }),
        )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let light_client_url = "https://testnet.zec.rocks:443";

    let tls_config = ClientTlsConfig::new().with_native_roots();

    let channel = Endpoint::from_static(light_client_url)
        .tls_config(tls_config)
        .unwrap()
        .connect()
        .await?;

    let mut test_client = CompactTxStreamerClient::new(channel.clone());

    let request = ChainSpec::default();
    let latest_block = test_client.get_latest_block(request.into_request()).await?;

    println!("latest block: {:?}", latest_block);

    let max_checkpoints = 100;
    let network = zcash_protocol::consensus::Network::TestNetwork;
    let db = MemoryWalletDb::new(network, max_checkpoints);

    let wallet = Wallet::new(db, channel, Network::TestNetwork, NonZero::from_str("1")?)?;

    let account_name = "My Account";
    let seed_phrase = std::env::var("SEED_PHRASE").unwrap_or_else(|_| {
        eprintln!("Error: SEED_PHRASE environment variable not found");
        std::process::exit(1);
    });
    let account_hd_index = 0;

    let birthday_height: u32 = std::env::var("BIRTHDAY_HEIGHT")
        .unwrap_or_else(|_| {
            eprintln!("Error: BIRTHDAY_HEIGHT environment variable not found");
            std::process::exit(1);
        })
        .parse()
        .expect("Error: failed to parse string to u32");

    let account_id = wallet
        .create_account(
            account_name,
            seed_phrase.as_str(),
            account_hd_index,
            Some(birthday_height),
            None,
        )
        .await?;

    println!("syncing wallet .....");
    wallet.sync().await?;
    println!("wallet synced");

    let wallet_summary = wallet.get_wallet_summary().await?;
    print!("wallet summary {:?}\n", wallet_summary);

    let jobs: JobStore = Arc::new(RwLock::new(HashMap::new()));

    let app_state = Arc::new(AppState {
        wallet: Arc::new(wallet),
        account_id,
        jobs: jobs.clone(),
    });

    // Spawn background task to sync wallet every 15 seconds
    let wallet_for_sync = app_state.wallet.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if let Err(e) = wallet_for_sync.sync().await {
                eprintln!("Background sync error: {:?}", e);
            } else {
                println!("Background sync completed");
            }
        }
    });

    let app = Router::new()
        .route("/get_address", get(get_address))
        .route("/sync", get(sync_wallet))
        .route("/orchard_balance", get(get_orchard_balance))
        .route("/create_job", post(create_job))
        .route("/job_status/{job_id}", get(get_job_status))
        .with_state(app_state)
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    println!("Server listening on http://127.0.0.1:3000");

    axum::serve(listener, app).await?;

    Ok(())
}
