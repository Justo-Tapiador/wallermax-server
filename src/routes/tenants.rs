//! The Tenants pages (F18): the management surface for the model
//! F15–F17 built — organizations, domains and memberships as forms
//! instead of SQL.
//!
//! Everything lives under `/admin/tenants` and answers only to a
//! [`CmsAdmin`] (an administrator of the CMS organization, which the
//! F15 mirror keeps equal to the platform administrators): managing
//! tenants is operating the server, not editing one site's content.
//!
//! The pages follow the same no-JavaScript contract as the rest of
//! the panel — plain forms, `303` redirects, flash codes in the
//! query string and re-rendered forms with the error and the
//! submitted values — and the same provenance rules the seeders
//! live by:
//!
//! - the **bootstrap organizations** (`main`, `cms`) are the
//!   seeders' own: their name and document root follow
//!   `wallermax.toml`, so the panel shows them read-only (the seed
//!   would heal any edit on the next boot anyway) — but their host
//!   names are as manageable as any tenant's, because a `manual`
//!   domain row is data whoever it points at;
//! - a **`config`-sourced domain row** (one the `[cms] hosts` list
//!   seeded) cannot be deleted here — it would come back on the
//!   next boot; the configuration is where it must be removed;
//! - the **CMS organization's memberships** are the F15 mirror's
//!   territory (they follow the platform roles, healed on every
//!   boot and every role write) — the panel refuses to touch them
//!   and points at the Users page instead. Memberships of every
//!   other organization are the first sanctioned divergence: data
//!   that mirrors nothing.
//!
//! Every write that can move a host or change a root ends with
//! [`refresh_vhost_state`] swapping the live vhost snapshot, so the
//! change is live for the very next request — the end of
//! "restarting to move a host".

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::{
    normalize_hostname, DomainRecord, MemberRecord, OrganizationRecord, OrganizationRepository,
    RepositoryError, UserRole, CMS_ORGANIZATION_KEY, MAIN_ORGANIZATION_KEY,
};
use crate::error::AppError;
use crate::routes::cms::{cms_context, render_view, see_other, CmsAdmin, PageParts};
use crate::routes::refresh_vhost_state;
use crate::state::AppState;
use crate::util::{format_timestamp, read_form};

/// Longest tenant name the form accepts — the same budget a menu
/// title gets.
const MAX_TENANT_NAME: usize = 200;

/// Longest document root the form accepts — enough for any path an
/// operator types, while keeping the row a bounded string.
const MAX_DOCUMENT_ROOT: usize = 500;

/// The organization keys the panel refuses to create: the seeders'
/// two (`main`, `cms` — creating them would collide with the seed)
/// and the one static route segment the URLs need
/// (`new` — `/admin/tenants/new` would shadow it).
const RESERVED_KEYS: [&str; 3] = [MAIN_ORGANIZATION_KEY, CMS_ORGANIZATION_KEY, "new"];

// ─── Route fragment ──────────────────────────────────────────────────

/// Route fragment for this module (merged while the CMS is enabled,
/// like the panel it manages).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/tenants", get(list_tenants))
        .route("/admin/tenants/new", get(new_tenant_form))
        .route("/admin/tenants", post(create_tenant))
        // Static segment first: axum's matcher prefers it over
        // `{key}`, so the creation form never shadows a tenant.
        .route(
            "/admin/tenants/{key}",
            get(tenant_detail).post(update_tenant),
        )
        .route("/admin/tenants/{key}/delete", post(delete_tenant))
        .route("/admin/tenants/{key}/domains", post(add_tenant_domain))
        .route(
            "/admin/tenants/{key}/domains/{hostname}/delete",
            post(remove_tenant_domain),
        )
        .route("/admin/tenants/{key}/members", post(add_tenant_member))
        .route(
            "/admin/tenants/{key}/members/{user_id}/delete",
            post(remove_tenant_member),
        )
}

// ─── The listing ─────────────────────────────────────────────────────

/// `GET /admin/tenants`: every organization with its badges.
async fn list_tenants(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let listings = match cms.organizations.list().await {
        Ok(listings) => listings,
        Err(error) => {
            tracing::error!(%error, "tenant listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let tenants: Vec<Value> = listings
        .iter()
        .map(|listing| tenant_value(&listing.record, listing.domain_count, listing.member_count))
        .collect();

    render_view(
        &state,
        &parts,
        "admin/tenants.jhs",
        vec![
            ("tenants", Value::Array(tenants)),
            ("form_error", Value::Null),
        ],
        StatusCode::OK,
    )
    .await
}

// ─── Creation ────────────────────────────────────────────────────────

/// `GET /admin/tenants/new`: the creation form.
async fn new_tenant_form(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    render_view(
        &state,
        &parts,
        "admin/tenant_form.jhs",
        vec![
            ("form", tenant_form_data("", "", "")),
            ("form_error", Value::Null),
        ],
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/tenants`: creates a tenant organization.
async fn create_tenant(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let form = match read_form::<NewTenantForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_tenant_error(&state, &parts, "", "", "", &message).await;
        }
    };

    let key = form.key.trim().to_owned();
    let name = form.name.trim().to_owned();
    let document_root = form.document_root.trim().to_owned();

    if let Err(error) = validate_tenant_key(&key) {
        return render_tenant_error(&state, &parts, &key, &name, &document_root, &error).await;
    }
    if let Err(error) = validate_tenant_name(&name) {
        return render_tenant_error(&state, &parts, &key, &name, &document_root, &error).await;
    }
    if let Err(error) = validate_document_root(&document_root) {
        return render_tenant_error(&state, &parts, &key, &name, &document_root, &error).await;
    }

    match cms.organizations.create(&key, &name, &document_root).await {
        Ok(_) => see_other(&format!("/admin/tenants/{key}?ok=created")),
        Err(RepositoryError::Duplicate) => {
            render_tenant_error(
                &state,
                &parts,
                &key,
                &name,
                &document_root,
                "That key is already taken.",
            )
            .await
        }
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "tenant creation failed");
            render_tenant_error(
                &state,
                &parts,
                &key,
                &name,
                &document_root,
                "Could not create (internal error).",
            )
            .await
        }
    }
}

/// The creation payload.
#[derive(Deserialize, Default)]
struct NewTenantForm {
    #[serde(default)]
    key: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    document_root: String,
}

/// Template data for the creation form (submitted values survive
/// errors).
fn tenant_form_data(key: &str, name: &str, document_root: &str) -> Value {
    json!({
        "key": key,
        "name": name,
        "document_root": document_root,
    })
}

/// Re-renders the creation form with an error, keeping the submitted
/// values.
async fn render_tenant_error(
    state: &AppState,
    parts: &PageParts,
    key: &str,
    name: &str,
    document_root: &str,
    error: &str,
) -> Response {
    render_view(
        state,
        parts,
        "admin/tenant_form.jhs",
        vec![
            ("form", tenant_form_data(key, name, document_root)),
            ("form_error", Value::String(error.to_owned())),
        ],
        StatusCode::OK,
    )
    .await
}

// ─── The detail page (and the shared loads) ──────────────────────────

/// What the detail page re-renders with after a rejected form: the
/// error itself plus the submitted values, so the operator edits
/// instead of retyping.
#[derive(Default)]
struct DetailForms {
    error: Option<String>,
    hostname: String,
    username: String,
    role: String,
}

/// `GET /admin/tenants/{key}`: the tenant's page — settings, host
/// names and members.
async fn tenant_detail(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let Some(organization) = load_organization(cms.organizations.as_ref(), &key).await else {
        return see_other("/admin/tenants");
    };
    let domains = load_domains(cms.organizations.as_ref(), organization.id).await;
    let members = load_members(cms.organizations.as_ref(), organization.id).await;

    render_tenant_detail(
        &state,
        &parts,
        &organization,
        &domains,
        &members,
        DetailForms::default(),
    )
    .await
}

/// Loads an organization by key, mapping storage failures to
/// "missing" (the detail page redirects to the listing).
async fn load_organization(
    organizations: &dyn OrganizationRepository,
    key: &str,
) -> Option<OrganizationRecord> {
    match organizations.find(key).await {
        Ok(organization) => organization,
        Err(error) => {
            tracing::error!(%error, "tenant lookup failed");
            None
        }
    }
}

/// Loads the organization's domains, mapping storage failures to an
/// empty list (the detail page still renders).
async fn load_domains(
    organizations: &dyn OrganizationRepository,
    organization_id: i64,
) -> Vec<DomainRecord> {
    match organizations.domains(organization_id).await {
        Ok(domains) => domains,
        Err(error) => {
            tracing::error!(%error, "domain listing failed");
            Vec::new()
        }
    }
}

/// Loads the organization's members, mapping storage failures to an
/// empty list (the detail page still renders).
async fn load_members(
    organizations: &dyn OrganizationRepository,
    organization_id: i64,
) -> Vec<MemberRecord> {
    match organizations.members(organization_id).await {
        Ok(members) => members,
        Err(error) => {
            tracing::error!(%error, "member listing failed");
            Vec::new()
        }
    }
}

/// The template's shape of one organization (shared by the listing
/// and the detail page).
fn tenant_value(organization: &OrganizationRecord, domain_count: i64, member_count: i64) -> Value {
    json!({
        "key": organization.key,
        "name": organization.name,
        "document_root": organization.document_root,
        "created_at_h": format_timestamp(organization.created_at),
        "domain_count": domain_count,
        "member_count": member_count,
        "is_seeded": is_seeded(organization),
        "is_cms": organization.key == CMS_ORGANIZATION_KEY,
    })
}

/// Whether the organization is one of the seeders' own — its name
/// and document root follow `wallermax.toml`, so the panel treats
/// them as read-only.
fn is_seeded(organization: &OrganizationRecord) -> bool {
    organization.key == MAIN_ORGANIZATION_KEY || organization.key == CMS_ORGANIZATION_KEY
}

/// Renders the detail page with the current lists plus whatever the
/// rejected form submitted.
async fn render_tenant_detail(
    state: &AppState,
    parts: &PageParts,
    organization: &OrganizationRecord,
    domains: &[DomainRecord],
    members: &[MemberRecord],
    forms: DetailForms,
) -> Response {
    let domain_values: Vec<Value> = domains
        .iter()
        .map(|domain| {
            json!({
                "hostname": domain.hostname,
                "is_config": domain.source == "config",
            })
        })
        .collect();
    let member_values: Vec<Value> = members
        .iter()
        .map(|member| {
            json!({
                "user_id": member.user_id,
                "username": member.username,
                "role": member.role.as_str(),
                "created_at_h": format_timestamp(member.created_at),
            })
        })
        .collect();

    render_view(
        state,
        parts,
        "admin/tenant_detail.jhs",
        vec![
            (
                "tenant",
                tenant_value(
                    organization,
                    domain_values.len() as i64,
                    member_values.len() as i64,
                ),
            ),
            ("domains", Value::Array(domain_values)),
            ("members", Value::Array(member_values)),
            (
                "form_error",
                forms.error.map(Value::String).unwrap_or(Value::Null),
            ),
            ("domain_form", json!({ "hostname": forms.hostname })),
            (
                "member_form",
                json!({ "username": forms.username, "role": forms.role }),
            ),
        ],
        StatusCode::OK,
    )
    .await
}

// ─── Settings ────────────────────────────────────────────────────────

/// `POST /admin/tenants/{key}`: saves a tenant's name and document
/// root, then refreshes the serving snapshot — the new root is live
/// for the next request.
async fn update_tenant(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let form = match read_form::<TenantForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return update_rejected(
                &state,
                &parts,
                cms.organizations.as_ref(),
                &key,
                "",
                "",
                &message,
            )
            .await;
        }
    };

    let Some(organization) = load_organization(cms.organizations.as_ref(), &key).await else {
        return see_other("/admin/tenants");
    };
    if is_seeded(&organization) {
        // The startup seed would overwrite the edit on the next
        // boot: wallermax.toml is where a bootstrap organization's
        // data lives.
        return see_other(&format!("/admin/tenants/{key}?error=seeded"));
    }

    let name = form.name.trim().to_owned();
    let document_root = form.document_root.trim().to_owned();
    if let Err(error) = validate_tenant_name(&name) {
        return update_rejected(
            &state,
            &parts,
            cms.organizations.as_ref(),
            &key,
            &name,
            &document_root,
            &error,
        )
        .await;
    }
    if let Err(error) = validate_document_root(&document_root) {
        return update_rejected(
            &state,
            &parts,
            cms.organizations.as_ref(),
            &key,
            &name,
            &document_root,
            &error,
        )
        .await;
    }

    match cms.organizations.update(&key, &name, &document_root).await {
        Ok(true) => {}
        Ok(false) => return see_other("/admin/tenants"),
        Err(error) => {
            tracing::error!(%error, "tenant update failed");
            return update_rejected(
                &state,
                &parts,
                cms.organizations.as_ref(),
                &key,
                &name,
                &document_root,
                "Could not save (internal error).",
            )
            .await;
        }
    }

    refresh_or_flag(&state, cms.organizations.as_ref(), &key, "saved").await
}

/// The settings payload.
#[derive(Deserialize, Default)]
struct TenantForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    document_root: String,
}

/// Re-renders the detail page for a rejected settings edit (the
/// organization is re-loaded so the lists stay fresh).
async fn update_rejected(
    state: &AppState,
    parts: &PageParts,
    organizations: &dyn crate::db::OrganizationRepository,
    key: &str,
    name: &str,
    document_root: &str,
    error: &str,
) -> Response {
    let Some(organization) = load_organization(organizations, key).await else {
        return see_other("/admin/tenants");
    };
    let domains = load_domains(organizations, organization.id).await;
    let members = load_members(organizations, organization.id).await;
    let mut edited = organization.clone();
    edited.name = name.to_owned();
    edited.document_root = document_root.to_owned();
    render_tenant_detail(
        state,
        parts,
        &edited,
        &domains,
        &members,
        DetailForms {
            error: Some(error.to_owned()),
            ..DetailForms::default()
        },
    )
    .await
}

/// `POST /admin/tenants/{key}/delete`: removes the tenant and its
/// rows (host names and memberships go with it, explicitly), then
/// refreshes the snapshot so its host names fall back to the main
/// tree on the next request.
async fn delete_tenant(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let _parts = PageParts::of(&request);
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let Some(organization) = load_organization(cms.organizations.as_ref(), &key).await else {
        return see_other("/admin/tenants");
    };
    if is_seeded(&organization) {
        return see_other(&format!("/admin/tenants/{key}?error=seeded"));
    }

    // F20: a tenant that still owns content is not deletable — the
    // operator moves or deletes the content first. Content is never
    // silently destroyed with its tenant (the same doctrine that
    // reparents children and keeps pages whose author was deleted).
    match cms.organizations.content_counts(&key).await {
        Ok(counts) if !counts.is_empty() => {
            return see_other(&format!("/admin/tenants/{key}?error=content"));
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(%error, "tenant content count failed");
            return AppError::internal("storage failure").into_response();
        }
    }

    match cms.organizations.delete(&key).await {
        Ok(true) => {}
        Ok(false) => return see_other("/admin/tenants"),
        Err(error) => {
            tracing::error!(%error, "tenant deletion failed");
            return AppError::internal("storage failure").into_response();
        }
    }

    // The organization is gone; a failed refresh can only mean its
    // host names keep serving the old tree until a restart — flagged
    // on the listing, not hidden.
    match refresh_vhost_state(&state, cms.organizations.as_ref()).await {
        Ok(()) => see_other("/admin/tenants?ok=deleted"),
        Err(message) => {
            tracing::error!(%message, "vhost refresh failed after a tenant deletion");
            see_other("/admin/tenants?ok=deleted&error=refresh")
        }
    }
}

// ─── Host names ──────────────────────────────────────────────────────

/// `POST /admin/tenants/{key}/domains`: maps a host name to the
/// organization as data, then refreshes the snapshot — the host is
/// served by the tenant's tree on the next request.
async fn add_tenant_domain(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let form = match read_form::<DomainForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return domain_rejected(
                &state,
                &parts,
                cms.organizations.as_ref(),
                &key,
                "",
                &message,
            )
            .await;
        }
    };

    let Some(organization) = load_organization(cms.organizations.as_ref(), &key).await else {
        return see_other("/admin/tenants");
    };

    let hostname = normalize_hostname(&form.hostname);
    if let Err(error) = validate_hostname(&hostname) {
        return domain_rejected(
            &state,
            &parts,
            cms.organizations.as_ref(),
            &key,
            &hostname,
            &error,
        )
        .await;
    }

    // The F16 boot rule, mirrored: a CMS host borrows the shared
    // stylesheets from the static tree, so the CMS organization only
    // takes another host name while the static file server is on.
    if organization.key == CMS_ORGANIZATION_KEY && !state.config().static_files.enabled {
        return domain_rejected(
            &state,
            &parts,
            cms.organizations.as_ref(),
            &key,
            &hostname,
            "The CMS host borrows the shared /assets from the static tree — enable \
             [static] serving before mapping more host names to the CMS organization.",
        )
        .await;
    }

    match cms
        .organizations
        .add_domain(organization.id, &hostname)
        .await
    {
        Ok(()) => {}
        Err(RepositoryError::Duplicate) => {
            // A host name maps to exactly one organization; the
            // error names the current owner, and adds the escape
            // hatch when that owner is the configuration's list.
            let owner = cms
                .organizations
                .find_domain_owner(&hostname)
                .await
                .ok()
                .flatten();
            let message = match owner.as_deref() {
                Some(CMS_ORGANIZATION_KEY)
                    if state
                        .config()
                        .cms
                        .hosts
                        .iter()
                        .any(|configured| normalize_hostname(configured) == hostname) =>
                {
                    format!(
                        "`{hostname}` is bootstrapped by the [cms] hosts list in wallermax.toml \
                         — remove it there (and restart) before mapping it to another \
                         organization."
                    )
                }
                Some(owner) => {
                    format!("`{hostname}` is already mapped to the `{owner}` organization.")
                }
                None => format!("`{hostname}` is already mapped."),
            };
            return domain_rejected(
                &state,
                &parts,
                cms.organizations.as_ref(),
                &key,
                &hostname,
                &message,
            )
            .await;
        }
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "domain mapping failed");
            return domain_rejected(
                &state,
                &parts,
                cms.organizations.as_ref(),
                &key,
                &hostname,
                "Could not map the host name (internal error).",
            )
            .await;
        }
    }

    refresh_or_flag(&state, cms.organizations.as_ref(), &key, "domain-added").await
}

/// `POST /admin/tenants/{key}/domains/{hostname}/delete`: unmaps a
/// host name — `manual` rows only; a `config` row follows
/// wallermax.toml and is refused with the pointer to it.
async fn remove_tenant_domain(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    Path((key, hostname)): Path<(String, String)>,
    request: Request,
) -> Response {
    let _parts = PageParts::of(&request);
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let Some(organization) = load_organization(cms.organizations.as_ref(), &key).await else {
        return see_other("/admin/tenants");
    };
    let hostname = normalize_hostname(&hostname);

    let outcome = match cms
        .organizations
        .remove_domain(organization.id, &hostname)
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::error!(%error, "domain unmapping failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let detail = format!("/admin/tenants/{key}");
    match outcome {
        crate::db::DomainRemoval::Removed => {}
        crate::db::DomainRemoval::ConfigProtected => {
            return see_other(&format!("{detail}?error=domain-config"));
        }
        crate::db::DomainRemoval::Missing => {
            return see_other(&format!("{detail}?error=domain-missing"));
        }
    }

    match refresh_vhost_state(&state, cms.organizations.as_ref()).await {
        Ok(()) => see_other(&format!("{detail}?ok=domain-removed")),
        Err(message) => {
            tracing::error!(%message, "vhost refresh failed after a domain removal");
            see_other(&format!("{detail}?ok=domain-removed&error=refresh"))
        }
    }
}

/// The host-name payload.
#[derive(Deserialize, Default)]
struct DomainForm {
    #[serde(default)]
    hostname: String,
}

/// Re-renders the detail page for a rejected host-name form.
async fn domain_rejected(
    state: &AppState,
    parts: &PageParts,
    organizations: &dyn crate::db::OrganizationRepository,
    key: &str,
    hostname: &str,
    error: &str,
) -> Response {
    let Some(organization) = load_organization(organizations, key).await else {
        return see_other("/admin/tenants");
    };
    let domains = load_domains(organizations, organization.id).await;
    let members = load_members(organizations, organization.id).await;
    render_tenant_detail(
        state,
        parts,
        &organization,
        &domains,
        &members,
        DetailForms {
            error: Some(error.to_owned()),
            hostname: hostname.to_owned(),
            ..DetailForms::default()
        },
    )
    .await
}

// ─── Members ─────────────────────────────────────────────────────────

/// `POST /admin/tenants/{key}/members`: grants (or changes) an
/// account's membership of the organization.
async fn add_tenant_member(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let form = match read_form::<MemberForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return member_rejected(
                &state,
                &parts,
                cms.organizations.as_ref(),
                &key,
                "",
                "editor",
                &message,
            )
            .await;
        }
    };

    let Some(organization) = load_organization(cms.organizations.as_ref(), &key).await else {
        return see_other("/admin/tenants");
    };

    // The F15 mirror owns the CMS organization's memberships (they
    // follow the platform roles, healed on every boot and every role
    // write) — a panel edit here would be undone at the next boot,
    // so the page refuses and points at the platform roles instead.
    if organization.key == CMS_ORGANIZATION_KEY {
        return see_other(&format!("/admin/tenants/{key}?error=mirror"));
    }

    let username = form.username.trim().to_owned();
    let Some(role) = UserRole::parse(&form.role).filter(|role| role.is_editor()) else {
        return member_rejected(
            &state,
            &parts,
            cms.organizations.as_ref(),
            &key,
            &username,
            &form.role,
            "Memberships are administrator or editor — a plain user simply has none.",
        )
        .await;
    };

    let Some(auth) = state.auth_context() else {
        return AppError::internal("authentication is not initialized").into_response();
    };
    let user = match auth.repository.find_by_username(&username).await {
        Ok(user) => user,
        Err(error) => {
            tracing::error!(%error, "user lookup failed");
            return AppError::internal("storage failure").into_response();
        }
    };
    let Some(user) = user else {
        return member_rejected(
            &state,
            &parts,
            cms.organizations.as_ref(),
            &key,
            &username,
            &form.role,
            "No account answers to that username.",
        )
        .await;
    };

    match cms
        .organizations
        .upsert_member(organization.id, user.id, role)
        .await
    {
        Ok(()) => see_other(&format!("/admin/tenants/{key}?ok=member-added")),
        Err(error) => {
            tracing::error!(%error, "membership grant failed");
            member_rejected(
                &state,
                &parts,
                cms.organizations.as_ref(),
                &key,
                &username,
                &form.role,
                "Could not save (internal error).",
            )
            .await
        }
    }
}

/// `POST /admin/tenants/{key}/members/{user_id}/delete`: removes an
/// account's membership of the organization.
async fn remove_tenant_member(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    Path((key, user_id)): Path<(String, i64)>,
    request: Request,
) -> Response {
    let _parts = PageParts::of(&request);
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let Some(organization) = load_organization(cms.organizations.as_ref(), &key).await else {
        return see_other("/admin/tenants");
    };
    if organization.key == CMS_ORGANIZATION_KEY {
        return see_other(&format!("/admin/tenants/{key}?error=mirror"));
    }

    match cms
        .organizations
        .remove_member(organization.id, user_id)
        .await
    {
        Ok(_) => see_other(&format!("/admin/tenants/{key}?ok=member-removed")),
        Err(error) => {
            tracing::error!(%error, "membership removal failed");
            AppError::internal("storage failure").into_response()
        }
    }
}

/// The membership payload.
#[derive(Deserialize, Default)]
struct MemberForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    role: String,
}

/// Re-renders the detail page for a rejected membership form.
async fn member_rejected(
    state: &AppState,
    parts: &PageParts,
    organizations: &dyn crate::db::OrganizationRepository,
    key: &str,
    username: &str,
    role: &str,
    error: &str,
) -> Response {
    let Some(organization) = load_organization(organizations, key).await else {
        return see_other("/admin/tenants");
    };
    let domains = load_domains(organizations, organization.id).await;
    let members = load_members(organizations, organization.id).await;
    render_tenant_detail(
        state,
        parts,
        &organization,
        &domains,
        &members,
        DetailForms {
            error: Some(error.to_owned()),
            username: username.to_owned(),
            role: role.to_owned(),
            ..DetailForms::default()
        },
    )
    .await
}

// ─── Shared tail: the refresh after a write ──────────────────────────

/// Refreshes the live vhost snapshot after a write that can move a
/// host or change a root, answering the redirect. A failed refresh
/// keeps the previous snapshot serving (the change waits for a
/// restart) and is flagged on the page instead of hidden.
async fn refresh_or_flag(
    state: &AppState,
    organizations: &dyn crate::db::OrganizationRepository,
    key: &str,
    ok: &str,
) -> Response {
    let detail = format!("/admin/tenants/{key}");
    match refresh_vhost_state(state, organizations).await {
        Ok(()) => see_other(&format!("{detail}?ok={ok}")),
        Err(message) => {
            tracing::error!(%message, "vhost refresh failed; the change waits for a restart");
            see_other(&format!("{detail}?ok={ok}&error=refresh"))
        }
    }
}

// ─── Validation ──────────────────────────────────────────────────────

/// Validates a tenant key: 2-32 characters of lowercase letters,
/// digits and hyphens, hyphen placement like a slug, and none of the
/// reserved keys (the seeders' two, plus `new` — the creation form's
/// URL).
///
/// # Errors
///
/// Returns a human-readable explanation for the form.
fn validate_tenant_key(key: &str) -> Result<(), String> {
    if !(2..=32).contains(&key.len()) {
        return Err("The key must be between 2 and 32 characters.".to_owned());
    }
    if !key.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    }) {
        return Err("The key may only contain lowercase letters, digits and hyphens.".to_owned());
    }
    if key.starts_with('-') || key.ends_with('-') || key.contains("--") {
        return Err(
            "The key cannot start or end with a hyphen, or contain two in a row.".to_owned(),
        );
    }
    if RESERVED_KEYS.contains(&key) {
        return Err(format!(
            "The key `{key}` is reserved — the seeders own `main` and `cms`, and `new` is the \
             creation form's address."
        ));
    }
    Ok(())
}

/// Validates a tenant name: non-empty, at most
/// [`MAX_TENANT_NAME`] characters.
///
/// # Errors
///
/// Returns a human-readable explanation for the form.
fn validate_tenant_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("The name cannot be empty.".to_owned());
    }
    if name.len() > MAX_TENANT_NAME {
        return Err(format!(
            "The name must be at most {MAX_TENANT_NAME} characters."
        ));
    }
    Ok(())
}

/// Validates a document root: non-empty, at most
/// [`MAX_DOCUMENT_ROOT`] characters. The same freedom
/// `[static] root_dir` has — relative paths resolve against the
/// working directory, the documented convention.
///
/// # Errors
///
/// Returns a human-readable explanation for the form.
fn validate_document_root(document_root: &str) -> Result<(), String> {
    if document_root.is_empty() {
        return Err("The document root cannot be empty.".to_owned());
    }
    if document_root.len() > MAX_DOCUMENT_ROOT {
        return Err(format!(
            "The document root must be at most {MAX_DOCUMENT_ROOT} characters."
        ));
    }
    Ok(())
}

/// Validates a host name in the table's canonical shape: the same
/// bare-hostname rules `[cms] hosts` enforces (no scheme, port,
/// path, userinfo or whitespace, and dot placement rules out empty
/// labels).
///
/// # Errors
///
/// Returns a human-readable explanation for the form.
fn validate_hostname(hostname: &str) -> Result<(), String> {
    if hostname.is_empty() {
        return Err("The host name cannot be empty.".to_owned());
    }
    if hostname.len() > 253 {
        return Err("The host name is too long.".to_owned());
    }
    if let Some(bad) = hostname
        .chars()
        .find(|c| matches!(c, ':' | '/' | '\\' | '@' | '#' | '?' | ' ' | '\t'))
    {
        return Err(format!(
            "invalid host name `{hostname}` (character `{bad}`): expected a bare host name such \
             as `acme.example.com` — no scheme, port, path or whitespace"
        ));
    }
    if hostname.starts_with('.') || hostname.ends_with('.') || hostname.contains("..") {
        return Err(format!(
            "invalid host name `{hostname}`: expected a bare host name such as \
             `acme.example.com` (no leading/trailing dot, no empty labels)"
        ));
    }
    Ok(())
}
