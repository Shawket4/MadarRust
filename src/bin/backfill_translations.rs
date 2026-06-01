use dotenvy::dotenv;
use sqlx::PgPool;
use std::env;
use sufrix_rust::translation::ensure_translations_json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPool::connect(&db_url).await?;

    tracing::info!("Starting Translation Backfill...");

    // 1. Categories
    tracing::info!("Backfilling Categories...");
    let categories = sqlx::query!("SELECT id, name, name_translations FROM public.categories")
        .fetch_all(&pool).await?;

    for cat in categories {
        let mut tr = cat.name_translations;
        ensure_translations_json(&mut tr, Some(&cat.name)).await?;
        sqlx::query!("UPDATE public.categories SET name_translations = $1 WHERE id = $2", tr, cat.id)
            .execute(&pool).await?;
    }

    // 2. Menu Items
    tracing::info!("Backfilling Menu Items...");
    let menu_items = sqlx::query!("SELECT id, name, description, name_translations, description_translations FROM public.menu_items")
        .fetch_all(&pool).await?;

    for item in menu_items {
        let mut name_tr = item.name_translations;
        ensure_translations_json(&mut name_tr, Some(&item.name)).await?;

        let mut desc_tr = item.description_translations;
        if let Some(desc) = &item.description {
            ensure_translations_json(&mut desc_tr, Some(desc)).await?;
        }

        sqlx::query!(
            "UPDATE public.menu_items SET name_translations = $1, description_translations = $2 WHERE id = $3",
            name_tr, desc_tr, item.id
        ).execute(&pool).await?;
    }

    // 3. Addon Items
    tracing::info!("Backfilling Addon Items...");
    let addons = sqlx::query!("SELECT id, name, name_translations FROM public.addon_items")
        .fetch_all(&pool).await?;

    for addon in addons {
        let mut tr = addon.name_translations;
        ensure_translations_json(&mut tr, Some(&addon.name)).await?;
        sqlx::query!("UPDATE public.addon_items SET name_translations = $1 WHERE id = $2", tr, addon.id)
            .execute(&pool).await?;
    }

    // 4. Bundles
    tracing::info!("Backfilling Bundles...");
    let bundles = sqlx::query!("SELECT id, name, description, name_translations, description_translations FROM public.bundles")
        .fetch_all(&pool).await?;

    for bundle in bundles {
        let mut name_tr = bundle.name_translations;
        ensure_translations_json(&mut name_tr, Some(&bundle.name)).await?;

        let mut desc_tr = bundle.description_translations;
        if let Some(desc) = &bundle.description {
            ensure_translations_json(&mut desc_tr, Some(desc)).await?;
        }

        sqlx::query!(
            "UPDATE public.bundles SET name_translations = $1, description_translations = $2 WHERE id = $3",
            name_tr, desc_tr, bundle.id
        ).execute(&pool).await?;
    }

    tracing::info!("Translation Backfill Complete!");
    Ok(())
}
