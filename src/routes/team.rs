//! The Team page (F21): the tenant's administrators run the tenant's
//! own team, from the tenant's own panel.
//!
//! F18 gave the platform the forms that grant the memberships; F20
//! put each organization's content behind its own host. This page
//! hands the second half to the tenant itself: `/admin/team` on a
//! tenant's host lists the organization's members and offers the
//! same add-or-move / remove vocabulary the platform's Tenants page
//! has. The page answers to an `admin` membership of the request's
//! organization — the platform administrator reaches it only through
//! a membership, exactly like the panel's content pages (F20's
//! "explicit membership only" rule, unchanged).
//!
//! Two laws frame every write, on both sides of the `Host` line:
//!
//! - **an organization never loses its last administrator** — the
//!   final demotion and the final removal are refused with an
//!   explanation ([`solo_administrator`]); the platform's Tenants
//!   page answers the same refusal since F21, and deleting an
//!   account that solo-administers a tenant is refused in the Users
//!   page until the team has another administrator;
//! - **the CMS organization is not managed here** — its memberships
//!   mirror the platform roles (F15), so this module is mounted on
//!   the tenant trees only: the CMS host answers `/admin/team` with
//!   its standard 404 (the routes are not mounted there at all, the
//!   same construction that keeps the platform surface off a
//!   tenant's host), and the platform's Users page remains that
//!   team's page.
//!
//! Like the rest of the panel: plain forms, `303` redirects, flash
//! codes in the query string, and re-rendered forms that keep what
//! was typed.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::{MemberRecord, OrganizationRecord, UserRole};
use crate::error::AppError;
use crate::routes::cms::{
    cms_context, render_view, see_other, solo_administrator, CmsAdmin, PageParts,
};
use crate::state::AppState;
use crate::util::{format_timestamp, read_form};

// ─── Route fragment ──────────────────────────────────────────────────

/// Route fragment for this module. Mounted on the **tenant trees**
/// only (see [`crate::routes::tenant_trees`]) — a tenant's host serves
/// it scoped to its own organization, and the platform's hosts do
/// not serve it at all.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/team", get(team_page).post(add_member))
        .route("/admin/team/{user_id}/delete", post(remove_member))
}

// ─── The page ───────────────────────────────────────────────────────

/// `GET /admin/team`: the organization's members and the add-or-move
/// form.
async fn team_page(State(state): State<AppState>, admin: CmsAdmin, request: Request) -> Response {
    let parts = PageParts::of(&request);
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    // The guard resolved the organization (the request's own, this
    // tree's tenant); the row lookup only recovers its id.
    let Some(organization) =
        load_organization(cms.organizations.as_ref(), &admin.organization).await
    else {
        return AppError::internal("storage failure").into_response();
    };

    // A storage failure is an internal error here, not a silently
    // empty team: the page is an authorization surface — showing an
    // empty list would invite re-granting every membership it could
    // not read.
    let members = match cms.organizations.members(organization.id).await {
        Ok(members) => members,
        Err(error) => {
            tracing::error!(%error, "member listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    render_team(
        &state,
        &parts,
        &organization,
        &members,
        &MemberForm::default(),
        None,
    )
    .await
}

// ─── Adding (or moving) a member ────────────────────────────────────

/// `POST /admin/team`: grants (or changes) an account's membership of
/// the organization — the same upsert the platform's Tenants page
/// performs, refused on the last administrator's demotion.
async fn add_member(State(state): State<AppState>, admin: CmsAdmin, request: Request) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let form = match read_form::<MemberForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_team_rejected(
                &state,
                &parts,
                &admin.organization,
                &MemberForm::default(),
                &message,
            )
            .await;
        }
    };

    let Some(organization) =
        load_organization(cms.organizations.as_ref(), &admin.organization).await
    else {
        return AppError::internal("storage failure").into_response();
    };

    let username = form.username.trim().to_owned();
    let Some(role) = UserRole::parse(&form.role).filter(|role| role.is_editor()) else {
        return render_team_rejected(
            &state,
            &parts,
            &admin.organization,
            &form,
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
        return render_team_rejected(
            &state,
            &parts,
            &admin.organization,
            &form,
            "No account answers to that username.",
        )
        .await;
    };

    // F21: the last administrator's demotion is refused — an
    // organization never loses its last administrator while it
    // exists, whichever side of the `Host` line asks.
    if role != UserRole::Admin {
        match solo_administrator(cms.organizations.as_ref(), organization.id, user.id).await {
            Ok(true) => {
                return render_team_rejected(
                    &state,
                    &parts,
                    &admin.organization,
                    &form,
                    "This is the organization's last administrator: the role cannot be removed.",
                )
                .await;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "administrator count failed");
                return AppError::internal("storage failure").into_response();
            }
        }
    }

    match cms
        .organizations
        .upsert_member(organization.id, user.id, role)
        .await
    {
        Ok(()) => see_other("/admin/team?ok=member-added"),
        Err(error) => {
            tracing::error!(%error, "membership grant failed");
            render_team_rejected(
                &state,
                &parts,
                &admin.organization,
                &form,
                "Could not save (internal error).",
            )
            .await
        }
    }
}

// ─── Removing a member ───────────────────────────────────────────────

/// `POST /admin/team/{user_id}/delete`: removes an account's
/// membership of the organization — refused while the account is its
/// last administrator.
async fn remove_member(
    State(state): State<AppState>,
    admin: CmsAdmin,
    Path(user_id): Path<i64>,
    request: Request,
) -> Response {
    let _parts = PageParts::of(&request);
    let cms = match cms_context(&state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };

    let Some(organization) =
        load_organization(cms.organizations.as_ref(), &admin.organization).await
    else {
        return AppError::internal("storage failure").into_response();
    };

    // F21: the last administrator stays — the team grants another
    // administrator first (or the platform does), then this one
    // goes.
    match solo_administrator(cms.organizations.as_ref(), organization.id, user_id).await {
        Ok(true) => return see_other("/admin/team?error=last-admin"),
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, "administrator count failed");
            return AppError::internal("storage failure").into_response();
        }
    }

    match cms
        .organizations
        .remove_member(organization.id, user_id)
        .await
    {
        Ok(_) => see_other("/admin/team?ok=member-removed"),
        Err(error) => {
            tracing::error!(%error, "membership removal failed");
            AppError::internal("storage failure").into_response()
        }
    }
}

// ─── The membership payload ─────────────────────────────────────────

/// The add-or-move form's payload (the same vocabulary the
/// platform's Tenants page speaks).
#[derive(Deserialize, Default)]
struct MemberForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    role: String,
}

// ─── Shared loads and rendering ─────────────────────────────────────

/// Loads the organization by key, mapping storage failures to
/// `None`: the guard resolved the organization from the live vhost
/// snapshot, so a miss is a storage failure in disguise — the
/// callers answer the internal error.
async fn load_organization(
    organizations: &dyn crate::db::OrganizationRepository,
    key: &str,
) -> Option<OrganizationRecord> {
    match organizations.find(key).await {
        Ok(organization) => organization,
        Err(error) => {
            tracing::error!(%error, "organization lookup failed");
            None
        }
    }
}

/// Renders the team page with the current members plus whatever the
/// rejected form submitted.
async fn render_team(
    state: &AppState,
    parts: &PageParts,
    organization: &OrganizationRecord,
    members: &[MemberRecord],
    form: &MemberForm,
    error: Option<&str>,
) -> Response {
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
        "admin/team.jhs",
        vec![
            (
                "tenant",
                json!({
                    "key": organization.key,
                    "name": organization.name,
                }),
            ),
            ("members", Value::Array(member_values)),
            (
                "form_error",
                error
                    .map(|error| Value::String(error.to_owned()))
                    .unwrap_or(Value::Null),
            ),
            (
                "member_form",
                json!({ "username": form.username, "role": form.role }),
            ),
        ],
        StatusCode::OK,
    )
    .await
}

/// Re-renders the team page for a rejected form: the organization
/// and its members are re-loaded so the list stays fresh.
async fn render_team_rejected(
    state: &AppState,
    parts: &PageParts,
    organization_key: &str,
    form: &MemberForm,
    error: &str,
) -> Response {
    let cms = match cms_context(state) {
        Ok(cms) => cms,
        Err(response) => return response,
    };
    let Some(organization) = load_organization(cms.organizations.as_ref(), organization_key).await
    else {
        return AppError::internal("storage failure").into_response();
    };
    let members = match cms.organizations.members(organization.id).await {
        Ok(members) => members,
        Err(error) => {
            tracing::error!(%error, "member listing failed");
            Vec::new()
        }
    };
    render_team(state, parts, &organization, &members, form, Some(error)).await
}
