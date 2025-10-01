use std::{num::NonZero, str::FromStr, sync::Arc};

use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use tonic::{
    IntoRequest,
    transport::{Channel, ClientTlsConfig, Endpoint},
};
use webzjs_common::Network;
use webzjs_wallet::Wallet;
use zcash_client_backend::data_api::WalletRead;
use zcash_client_backend::encoding::AddressCodec;
use zcash_client_backend::proto::service::{
    ChainSpec, compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_client_memory::MemoryWalletDb;
use zcash_protocol::consensus::MAIN_NETWORK;

type AccountId = <MemoryWalletDb<zcash_protocol::consensus::Network> as WalletRead>::AccountId;

struct AppState {
    wallet: Arc<Wallet<MemoryWalletDb<zcash_protocol::consensus::Network>, Channel>>,
    account_id: AccountId,
}

async fn get_address(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.wallet.db();
    let db = db.read().await;

    match db.get_current_address(state.account_id) {
        Ok(Some(address)) => {
            let address_string = address.to_address(zcash_protocol::consensus::NetworkType::Main);
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let light_client_url = "https://zec.rocks:443";

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
    let network = zcash_protocol::consensus::Network::MainNetwork;
    let db = MemoryWalletDb::new(network, max_checkpoints);

    let wallet = Wallet::new(db, channel, Network::MainNetwork, NonZero::from_str("1")?)?;

    let account_name = "My Account";
    let seed_phrase = std::env::var("SEED_PHRASE").unwrap_or_else(|_| {
        eprintln!("Error: SEED_PHRASE environment variable not found");
        std::process::exit(1);
    });
    let account_hd_index = 0;
    let birthday_height = Some(3084472);

    let account_id = wallet
        .create_account(
            account_name,
            seed_phrase.as_str(),
            account_hd_index,
            birthday_height,
            None,
        )
        .await?;

    println!("syncing wallet .....");
    wallet.sync().await?;
    println!("wallet synced");

    let wallet_summary = wallet.get_wallet_summary().await?;
    print!("wallet summary {:?}\n", wallet_summary);

    let app_state = Arc::new(AppState {
        wallet: Arc::new(wallet),
        account_id,
    });

    let app = Router::new()
        .route("/get_address", get(get_address))
        .route("/sync", get(sync_wallet))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    println!("Server listening on http://127.0.0.1:3000");

    axum::serve(listener, app).await?;

    Ok(())
}
