use anyhow::{Result, ensure};
use atlas_core::Store;
use atlas_server::{App, hash_password};
use std::io::{self, Read};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("ATLAS_DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://atlas.sqlite?mode=rwc".into());
    let retention_days = std::env::var("ATLAS_SYNC_RETENTION_DAYS")
        .unwrap_or_else(|_| "90".into())
        .parse()?;
    let store = Store::connect(&url)
        .await?
        .with_retention_days(retention_days)?;
    store.migrate().await?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "invite") {
        ensure!(args.len() == 1, "usage: atlas-server invite");
        println!(
            "{}",
            serde_json::to_string(&atlas_server::operator_invitation(&store).await?)?
        );
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "revoke-invite") {
        ensure!(args.len() == 2, "usage: atlas-server revoke-invite ID");
        atlas_server::revoke_operator_invitation(&store, &args[1]).await?;
        return Ok(());
    }

    if args
        .first()
        .is_some_and(|a| a == "account" || a == "password")
    {
        ensure!(
            args.len() == 2,
            "usage: atlas-server account|password USERNAME (password on stdin)"
        );
        let mut password = String::new();
        io::stdin().take(1026).read_to_string(&mut password)?;
        let password = password.trim_end_matches(['\r', '\n']).to_owned();
        let hash = hash_password(password).await?;
        if args[0] == "password" {
            atlas_server::reset_password(&store, &args[1], &hash).await?;
            println!("Password changed; all account sessions revoked.");
            return Ok(());
        }
        let id = Uuid::new_v4().to_string();
        store.add_account(&id, &args[1], &hash).await?;
        println!("{id}");
        return Ok(());
    }
    ensure!(
        args.is_empty(),
        "usage: atlas-server [account|password USERNAME|invite|revoke-invite ID]"
    );
    let bind = std::env::var("ATLAS_BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(
        "{}",
        serde_json::json!({"event":"listening","address":listener.local_addr()?.to_string()})
    );
    let proxies = std::env::var("ATLAS_TRUSTED_PROXY_IPS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let maintenance_store = store.clone();
    let maintenance = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            if maintenance_store
                .collect_expired(atlas_server::now())
                .await
                .is_err()
            {
                eprintln!(
                    "{}",
                    serde_json::json!({"event":"maintenance_error","operation":"collect_expired"})
                );
            }
        }
    });
    let directory_enabled = std::env::var("ATLAS_DIRECTORY_ENABLED")
        .unwrap_or_else(|_| "true".into())
        .parse::<bool>()?;
    let mut app = App::new(store)
        .await?
        .trust_proxies(proxies)
        .directory_enabled(directory_enabled);
    if let Ok(origin) = std::env::var("ATLAS_PUBLIC_ORIGIN") {
        app = app.public_origin(&origin)?;
    }
    if let Ok(issuer) = std::env::var("ATLAS_OIDC_ISSUER") {
        app = app.oidc(
            &issuer,
            &std::env::var("ATLAS_OIDC_CLIENT_ID")?,
            std::env::var("ATLAS_OIDC_CLIENT_SECRET").ok(),
            std::env::var("ATLAS_OIDC_AUTO_PROVISION")
                .unwrap_or_else(|_| "false".into())
                .parse()?,
        )?;
    }
    let redirects = std::env::var("ATLAS_OIDC_NATIVE_REDIRECT_URIS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    app = app.oidc_native_redirects(redirects)?;
    let integration_app = app.clone();
    let integrations = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if integration_app.integration_tick().await.is_err() {
                eprintln!(
                    "{}",
                    serde_json::json!({"event":"integration_error","operation":"worker_tick"})
                );
            }
        }
    });
    axum::serve(
        listener,
        app.router()
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    integrations.abort();
    let _ = integrations.await;
    maintenance.abort();
    let _ = maintenance.await;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_ = tokio::signal::ctrl_c()=>{},_ = signal.recv()=>{}}
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
