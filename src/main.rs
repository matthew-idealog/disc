mod app;
mod config;
mod media_controls;
mod playback;
mod subsonic;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::{model::{is_reserved_server_alias, reserved_server_aliases_label, ServerConfig}, store::ConfigStore};
use subsonic::client::SubsonicClient;

#[derive(Parser, Debug)]
#[command(name = "disc")]
#[command(about = "DISC Is a Subsonic Client: a cross-platform terminal app for Subsonic-compatible music servers", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Tui,
    ServerList,
    ServerAdd {
        name: String,
        alias: String,
        base_url: String,
        username: String,
        password: String,
        #[arg(long, default_value_t = false)]
        primary: bool,
    },
    ServerEdit {
        target: String,
        name: String,
        alias: String,
        base_url: String,
        username: String,
        password: String,
        #[arg(long, default_value_t = false)]
        primary: bool,
    },
    ServerRemove {
        target: String,
    },
    ServerPrimary {
        target: String,
    },
    Ping {
        target: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = ConfigStore::load_default()?;

    match cli.command.unwrap_or(Commands::Tui) {
        Commands::Tui => ui::terminal::run_tui(store).await?,
        Commands::ServerList => {
            let cfg = store.load()?;
            println!("Primary server: {}", cfg.primary_display_name());
            for server in &cfg.servers {
                let marker = if cfg.is_primary(&server.alias) { "*" } else { " " };
                println!("{} {} [{}] -> {}", marker, server.name, server.alias, server.base_url);
            }
        }
        Commands::ServerAdd {
            name,
            alias,
            base_url,
            username,
            password,
            primary,
        } => {
            let mut cfg = store.load()?;
            let alias_lc = alias.trim().to_lowercase();
            if is_reserved_server_alias(&alias_lc) {
                anyhow::bail!("Alias '{}' is reserved for a command. Reserved aliases: {}", alias_lc, reserved_server_aliases_label());
            }
            cfg.add_or_update_server(ServerConfig {
                name,
                alias: alias_lc.clone(),
                base_url,
                username,
                password,
                search_timeout_seconds: crate::config::model::default_search_timeout_seconds(),
            });
            if primary {
                cfg.set_primary_by_target(&alias_lc)?;
            }
            store.save(&cfg)?;
            println!("Saved server. Primary server: {}", cfg.primary_display_name());
        }
        Commands::ServerEdit {
            target,
            name,
            alias,
            base_url,
            username,
            password,
            primary,
        } => {
            let mut cfg = store.load()?;
            let alias_lc = alias.trim().to_lowercase();
            if is_reserved_server_alias(&alias_lc) {
                anyhow::bail!("Alias '{}' is reserved for a command. Reserved aliases: {}", alias_lc, reserved_server_aliases_label());
            }
            cfg.update_server_by_target(
                &target,
                ServerConfig {
                    name,
                    alias: alias_lc.clone(),
                    base_url,
                    username,
                    password,
                    search_timeout_seconds: crate::config::model::default_search_timeout_seconds(),
                },
            )?;
            if primary {
                cfg.set_primary_by_target(&alias_lc)?;
            }
            store.save(&cfg)?;
            println!("Updated server. Primary server: {}", cfg.primary_display_name());
        }
        Commands::ServerRemove { target } => {
            let mut cfg = store.load()?;
            let removed = cfg.remove_server_by_target(&target)?;
            store.save(&cfg)?;
            println!(
                "Removed server {} [{}]. Primary server: {}",
                removed.name,
                removed.alias,
                cfg.primary_display_name()
            );
        }
        Commands::ServerPrimary { target } => {
            let mut cfg = store.load()?;
            cfg.set_primary_by_target(&target)?;
            store.save(&cfg)?;
            println!("Primary server set to {}", cfg.primary_display_name());
        }
        Commands::Ping { target } => {
            let cfg = store.load()?;
            let server = match target {
                Some(t) => cfg.find_server(&t).cloned(),
                None => cfg.primary_server().cloned(),
            }
            .ok_or_else(|| anyhow::anyhow!("No matching server configured."))?;
            let client = SubsonicClient::new(server);
            let response = client.ping().await?;
            println!("Ping OK: {}", response);
        }
    }

    Ok(())
}
