import re

with open("src/bundles/handlers.rs", "r") as f:
    code = f.read()

# 1. Update create_bundle
create_old = """pub async fn create_bundle(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateBundleRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    require_same_org(&claims, Some(body.org_id))?;

    if body.components.is_empty() {
        return Err(AppError::BadRequest("A bundle must contain components".into()));
    }

    let mut tx = pool.begin().await?;

    let name_translations = body.name_translations.clone().unwrap_or_else(|| serde_json::json!({}));
    let description_translations = body.description_translations.clone().unwrap_or_else(|| serde_json::json!({}));"""

create_new = """pub async fn create_bundle(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateBundleRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    require_same_org(&claims, Some(body.org_id))?;

    if body.components.is_empty() {
        return Err(AppError::BadRequest("A bundle must contain components".into()));
    }

    let mut mut_body = body.into_inner();
    let mut tx = pool.begin().await?;

    let mut name_translations = mut_body.name_translations.clone().unwrap_or_else(|| serde_json::json!({}));
    crate::translation::ensure_translations_json(&mut name_translations, Some(&mut_body.name))
        .await
        .map_err(|_| AppError::Internal)?;

    let mut description_translations = mut_body.description_translations.clone().unwrap_or_else(|| serde_json::json!({}));
    if let Some(desc) = &mut_body.description {
        crate::translation::ensure_translations_json(&mut description_translations, Some(desc))
            .await
            .map_err(|_| AppError::Internal)?;
    }"""
code = code.replace(create_old, create_new)

# Update references to `body` in `create_bundle` after the new block
create_bind_old = """    .bind(body.org_id)
    .bind(&body.name)
    .bind(name_translations)
    .bind(&body.description)
    .bind(description_translations)
    .bind(body.price)
    .bind(&body.image_url)
    .bind(body.display_order.unwrap_or(0))
    .bind(body.available_from_time)
    .bind(body.available_until_time)
    .bind(body.available_from_date)
    .bind(body.available_until_date)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;

    // Insert components
    for (i, c) in body.components.iter().enumerate() {"""
create_bind_new = """    .bind(mut_body.org_id)
    .bind(&mut_body.name)
    .bind(&name_translations)
    .bind(&mut_body.description)
    .bind(&description_translations)
    .bind(mut_body.price)
    .bind(&mut_body.image_url)
    .bind(mut_body.display_order.unwrap_or(0))
    .bind(mut_body.available_from_time)
    .bind(mut_body.available_until_time)
    .bind(mut_body.available_from_date)
    .bind(mut_body.available_until_date)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;

    // Insert components
    for (i, c) in mut_body.components.iter().enumerate() {"""
code = code.replace(create_bind_old, create_bind_new)

create_branch_old = """let Some(branch_ids) = body.branch_ids"""
create_branch_new = """let Some(branch_ids) = mut_body.branch_ids"""
code = code.replace(create_branch_old, create_branch_new)


# 2. Update update_bundle
update_old = """    let mut tx = pool.begin().await?;

    // Prepare updated values
    let name = body.name.as_ref().unwrap_or(&original.bundle.name);
    let name_translations = body.name_translations.as_ref().unwrap_or(&original.bundle.name_translations);
    let description = body.description.as_ref().or(original.bundle.description.as_ref());
    let description_translations = body.description_translations.as_ref().unwrap_or(&original.bundle.description_translations);
    let price = body.price.unwrap_or(original.bundle.price);
    let image_url = body.image_url.as_ref().or(original.bundle.image_url.as_ref());
    let display_order = body.display_order.unwrap_or(original.bundle.display_order);"""

update_new = """    let mut mut_body = body.into_inner();
    let mut tx = pool.begin().await?;

    let mut name_translations = original.bundle.name_translations;
    if let Some(new_name) = &mut_body.name {
        crate::translation::ensure_translations_json(&mut name_translations, Some(new_name))
            .await
            .map_err(|_| AppError::Internal)?;
    } else if let Some(new_tr) = mut_body.name_translations {
        name_translations = new_tr;
        crate::translation::ensure_translations_json(&mut name_translations, Some(&original.bundle.name))
            .await
            .map_err(|_| AppError::Internal)?;
    }

    let mut description_translations = original.bundle.description_translations;
    if let Some(new_desc) = &mut_body.description {
        crate::translation::ensure_translations_json(&mut description_translations, Some(new_desc))
            .await
            .map_err(|_| AppError::Internal)?;
    } else if let Some(new_tr) = mut_body.description_translations {
        description_translations = new_tr;
        if let Some(desc) = &original.bundle.description {
            crate::translation::ensure_translations_json(&mut description_translations, Some(desc))
                .await
                .map_err(|_| AppError::Internal)?;
        }
    }

    // Prepare updated values
    let name = mut_body.name.as_ref().unwrap_or(&original.bundle.name);
    let price = mut_body.price.unwrap_or(original.bundle.price);
    let image_url = mut_body.image_url.as_ref().or(original.bundle.image_url.as_ref());
    let display_order = mut_body.display_order.unwrap_or(original.bundle.display_order);"""

code = code.replace(update_old, update_new)

# Since `mut_body.description` takes ownership, let's just make sure we only access references if we need it
update_bind_old = """    .bind(name)
    .bind(name_translations)
    .bind(description)
    .bind(description_translations)
    .bind(price)
    .bind(body.status.unwrap_or(original.bundle.status))
    .bind(image_url)
    .bind(display_order)
    .bind(body.available_from_time.or(original.bundle.available_from_time))
    .bind(body.available_until_time.or(original.bundle.available_until_time))
    .bind(body.available_from_date.or(original.bundle.available_from_date))
    .bind(body.available_until_date.or(original.bundle.available_until_date))
    .bind(*id)
    .fetch_one(&mut *tx)
    .await?;

    if let Some(comps) = body.components {"""

# For description, we need a special logic
desc_logic = """    let description = if mut_body.description.is_some() { mut_body.description.as_ref() } else { original.bundle.description.as_ref() };"""
code = code.replace("    let image_url", desc_logic + "\n    let image_url")

update_bind_new = """    .bind(name)
    .bind(&name_translations)
    .bind(description)
    .bind(&description_translations)
    .bind(price)
    .bind(mut_body.status.unwrap_or(original.bundle.status))
    .bind(image_url)
    .bind(display_order)
    .bind(mut_body.available_from_time.or(original.bundle.available_from_time))
    .bind(mut_body.available_until_time.or(original.bundle.available_until_time))
    .bind(mut_body.available_from_date.or(original.bundle.available_from_date))
    .bind(mut_body.available_until_date.or(original.bundle.available_until_date))
    .bind(*id)
    .fetch_one(&mut *tx)
    .await?;

    if let Some(comps) = mut_body.components {"""
code = code.replace(update_bind_old, update_bind_new)

update_branch_old = """if let Some(branch_ids) = body.branch_ids {"""
update_branch_new = """if let Some(branch_ids) = mut_body.branch_ids {"""
code = code.replace(update_branch_old, update_branch_new)


with open("src/bundles/handlers.rs", "w") as f:
    f.write(code)

print("Patch applied to src/bundles/handlers.rs")
