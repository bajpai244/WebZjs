use std::{num::NonZero, str::FromStr};

use tonic::{
    IntoRequest,
    transport::{Channel, ClientTlsConfig, Endpoint},
};
use webzjs_common::Network;
use webzjs_wallet::{Wallet, error::Error};
use zcash_client_backend::encoding::AddressCodec;
use zcash_client_backend::proto::service::{
    self, ChainSpec, compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_client_backend::{
    data_api::{InputSource, WalletRead},
    proto::service::TxFilter,
};
use zcash_client_memory::{MemoryWalletDb, proto::generated::MemoryWallet};
use zcash_protocol::{ShieldedProtocol, TxId};
use zcash_protocol::{consensus::MAIN_NETWORK, value::Zatoshis};

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
    let min_confirmations = 1;
    let network = MAIN_NETWORK;
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

    let db = wallet.db();
    let db = db.read().await;

    let address = db.get_current_address(account_id).unwrap().unwrap();

    let address = address
        .to_address(zcash_protocol::consensus::NetworkType::Main)
        .to_string();
    println!("address: {:?}", address);

    let address = if let Some(address) = db.get_current_address(account_id.into())? {
        Ok(address.transparent().unwrap().encode(&network))
    } else {
        Err(Error::AccountNotFound(1))
    };
    let address = address.unwrap();

    let tx_hash = "5068578ab13a4f09d935e007ac21701cf7a72f09e54454501a28cbdd0a671cec";

    let bytes = hex::decode(tx_hash).expect("Invalid hex");
    let mut array = [0u8; 32];
    array.copy_from_slice(&bytes);
    array.reverse(); // TxId stores bytes in reverse order for display
    let tx_id = TxId::from_bytes(array);

    // let result = db.get_transaction(tx_id);
    // println!("result {:?}", result);
    //

    let notes = db.select_spendable_notes(
        account_id,
        Zatoshis::from_u64(10).unwrap(),
        &[ShieldedProtocol::Orchard],
        birthday_height.unwrap().into(),
        &[],
    );

    let notes = notes.unwrap();

    println!("notes : {:?}", notes.total_value());

    let transparent_address_string = address.as_str();
    println!("transparent address: {}", transparent_address_string);

    Ok(())
}
