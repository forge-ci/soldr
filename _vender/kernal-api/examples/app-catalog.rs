//! List installed applications matching a query: `app-catalog [QUERY]`.
use kernal_api::app_catalog::list_apps;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let query = std::env::args().nth(1).unwrap_or_default().to_lowercase();
    let apps = list_apps().await;
    let hits: Vec<_> = apps
        .iter()
        .filter(|a| {
            a.name.to_lowercase().contains(&query)
                || a.keywords.iter().any(|k| k.to_lowercase().contains(&query))
        })
        .collect();
    println!("{} apps, {} match {query:?}", apps.len(), hits.len());
    for app in hits.iter().take(10) {
        println!("  {} ({})", app.name, app.id);
    }
}
