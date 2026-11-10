use anyhow::{Context as _, Result};
use property::{api, config::Config, store::Store, supervisor::Supervisor, sync::SyncStore};
use std::sync::Arc;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("property_start_or_runtime_failed");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<()> {
    let config = Config::load()?;
    if let Some(path) = &config.openapi {
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&api::openapi()?)?),
        )?;
        return Ok(());
    }
    let store = Store::open(&config.database).await?;
    let syncs = SyncStore::open(config.sync_database()).await?;
    let mut context = api::Context::new(
        store.clone(),
        syncs.clone(),
        config
            .api_token
            .as_deref()
            .context("owner credential required")?,
    );
    if let Some(token) = &config.integration_token {
        context = context.with_integration(token);
    }
    let server = api::start(config.bind, Arc::new(context.clone()))?;
    let ingress = config
        .ingress_bind
        .map(|bind| api::start(bind, Arc::new(context.for_ingress(config.ingress_peer))))
        .transpose()?;
    println!("property_listening={}", server.local_addr());
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let discovery = if let (Some(supervisor), Some(token)) =
        (Supervisor::from_environment()?, config.integration_token)
    {
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {_=stopped.changed()=>break,result=supervisor.announce(&token)=>{if result.is_err(){eprintln!("property_discovery_unavailable");}}}
                tokio::select! {_=stopped.changed()=>break,_=tokio::time::sleep(std::time::Duration::from_secs(60))=>{}}
            }
        }))
    } else {
        None
    };
    shutdown().await?;
    // Dropshot's detached handlers finish before either database is closed.
    let api_result = server.close().await;
    let ingress_result = if let Some(ingress) = ingress {
        ingress.close().await
    } else {
        Ok(())
    };
    let _ = stop.send(true);
    if let Some(discovery) = discovery {
        discovery.await?;
    }
    let sync_result = syncs.close().await;
    let database_result = store.close().await;
    api_result.map_err(|_| anyhow::anyhow!("close API listener"))?;
    ingress_result.map_err(|_| anyhow::anyhow!("close ingress listener"))?;
    sync_result?;
    database_result?;
    Ok(())
}
async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {result=tokio::signal::ctrl_c()=>result?,_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
