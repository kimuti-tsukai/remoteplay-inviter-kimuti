use anyhow::{Context as _, Result};
use dotenvy_macro::dotenv;
use futures::SinkExt;
use futures_util::stream::StreamExt;
use std::sync::Arc;
use steam_stuff::SteamStuff;
use tokio::{
    sync::Mutex,
    time::{self, timeout, Duration},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        http::{uri::Builder, Uri},
        protocol::Message,
    },
};
use uuid::Uuid;

mod config;
mod console;
mod handlers;
mod models;
mod retry;
mod ws_error_handler;

use config::{read_or_generate_config, Config};
use handlers::Handler;
use models::*;
use retry::RetrySec;
use ws_error_handler::handle_ws_error;

// Version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Endpoint URL
const DEFAULT_URL: &str = dotenv!("ENDPOINT_URL");

#[tokio::main]
async fn main() -> Result<()> {
    // Event loop
    'main: {
        console::printdoc! {"
            ------------------------------------------------------------------------------
                        ╦═╗┌─┐┌┬┐┌─┐┌┬┐┌─┐┌─┐┬  ┌─┐┬ ┬  ╦┌┐┌┬  ┬┬┌┬┐┌─┐┬─┐
                        ╠╦╝├┤ ││││ │ │ ├┤ ├─┘│  ├─┤└┬┘  ║│││└┐┌┘│ │ ├┤ ├┬┘
                        ╩╚═└─┘┴ ┴└─┘ ┴ └─┘┴  ┴─┘┴ ┴ ┴   ╩┘└┘ └┘ ┴ ┴ └─┘┴└─
                           Version: {VERSION}                   by Kamesuta
                                                            
                Invite your friends via Discord and play Steam games together for free! 
            ------------------------------------------------------------------------------
        
        "};

        // Version command
        if std::env::args().any(|arg| arg == "--version" || arg == "-v") {
            console::println!("✓ Version: {}", VERSION);
            return Ok(());
        }

        // Help command
        if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
            let program = std::env::current_exe()
                .ok()
                .and_then(|f| f.file_name().map(|f| f.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "remoteplay-inviter".to_owned());
            console::printdoc! {"
                Usage: {program} [options]

                Options:
                    -v, --version    Display the version of the program
                    -h, --help       Display this help message
            "};
            return Ok(());
        }

        // Initialize SteamStuff
        let steam = match SteamStuff::new()
            .context("Failed to connect to Steam Client. Please make sure Steam is running.")
        {
            Ok(steam) => Arc::new(Mutex::new(steam)),
            Err(err) => {
                console::eprintln!("☓ {}", err);
                break 'main;
            }
        };

        // Create a Handler
        let mut handler = Handler::new(steam.clone());

        // Set up Steam callbacks
        handler.setup_steam_callbacks().await;
        // Start a task to periodically call Steam callbacks
        handler.run_steam_callbacks();

        // Reconnection flag
        let mut reconnect = false;
        // Retry seconds
        let mut retry_sec = RetrySec::new();

        // URL to connect to
        let result: Result<String> = (|| {
            // Read the endpoint configuration file
            let endpoint_config = config::read_endpoint_config()?;

            // Read or generate the configuration file (if it doesn't exist)
            let config = read_or_generate_config(|| Config {
                uuid: Uuid::new_v4().to_string(),
            })?;

            // Session ID
            let session_id: u32 = rand::random();

            // Endpoint URL
            let endpoint_url = match endpoint_config {
                Some(e) => {
                    console::println!("✓ Using custom endpoint URL: {}", e.url);
                    e.url
                }
                None => DEFAULT_URL.to_string(),
            };

            // Create the URL
            let uri: Uri = endpoint_url.parse().context("Failed to parse URL")?;
            let uri = Builder::from(uri)
                .path_and_query(format!(
                    "/ws?v={VERSION}&token={0}&session={session_id}",
                    config.uuid
                ))
                .build()
                .context("Failed to build URL")?;
            Ok(uri.to_string())
        })();

        let url = match result {
            Ok(url) => url,
            Err(err) => {
                console::eprintln!("☓ {}", err);
                break 'main;
            }
        };

        enum ResultConfig {
            Success,
            Break,
        }

        loop {
            let result: Result<ResultConfig> = {
                let retry_sec = Mutex::new(&mut retry_sec);
                let handler = Mutex::new(&mut handler);
                #[allow(clippy::redundant_closure_call)]
                (|| async {
                    // Display the reconnection message
                    if reconnect {
                        console::println!("↪ Reconnecting to the server...");
                    }

                    // Create a WebSocket client
                    let connect_result = timeout(Duration::from_secs(10), connect_async(&url))
                        .await
                        .context("Connection timed out to the server")?;
                    let ws_stream = match connect_result {
                        Ok((ws_stream, _)) => ws_stream,
                        Err(err) => {
                            handle_ws_error(err)?;
                            // If OK is returned, break the loop and exit
                            return Ok(ResultConfig::Break);
                        }
                    };

                    // Stream and sink for communicating with the server
                    let (mut write, mut read) = ws_stream.split();

                    // Display the reconnection message
                    if reconnect {
                        console::println!("✓ Reconnected!");
                    } else {
                        console::println!("✓ Connected to the server!");
                    }

                    // Loop to process messages received from the server
                    while let Some(message) = timeout(Duration::from_secs(60), read.next())
                        .await
                        .context("Connection timed out")?
                    {
                        // Process each message
                        match message.context("Failed to receive message from the server")? {
                            Message::Close(_) => break,
                            Message::Ping(ping) => {
                                // Send a Pong message
                                write
                                    .send(Message::Pong(ping))
                                    .await
                                    .context("Failed to send pong message to the server")?;

                                // Reset the retry seconds
                                retry_sec.lock().await.reset();
                            }
                            Message::Text(text) => {
                                // Parse the JSON data
                                let msg: ServerMessage = serde_json::from_str(&text).context(
                                    "Failed to deserialize JSON message from the server",
                                )?;

                                // Process the message
                                if handler.lock().await.handle_server_message(msg, &mut write).await? {
                                    // If the exit flag is set, break the loop and exit
                                    return Ok(ResultConfig::Break);
                                }

                                // Reset the retry seconds
                                retry_sec.lock().await.reset();
                            }
                            _ => (),
                        }
                    }

                    Ok(ResultConfig::Success)
                })()
                .await
            };
            if let Ok(ResultConfig::Break) = result {
                break 'main;
            }
            if let Err(err) = result {
                console::eprintln!("☓ {}", err);
            }

            // Reconnect to the server if the connection is lost
            let sec = retry_sec.next();
            console::println!("↪ Connection lost. Reconnecting in {sec} seconds...");
            time::sleep(Duration::from_secs(sec)).await;
            reconnect = true;
        }
    }

    // Wait for input before exiting
    console::println!("□ Press Ctrl+C to exit...");
    let _ = tokio::signal::ctrl_c().await;

    Ok(())
}
