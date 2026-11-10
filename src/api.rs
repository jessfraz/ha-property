//! One typed contract for the native service, HA bridge and command-line client.
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use anyhow::Context as _;
use chrono::Utc;
use dropshot::{
    ApiDescription, Body, ClientErrorStatusCode as Status, ConfigDropshot, HttpError,
    HttpResponseCreated, HttpResponseHeaders, HttpResponseOk, HttpServer, HttpServerStarter, Path,
    Query, RequestContext, TypedBody, endpoint,
};
use http::Response;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{
    store::{ApplyRequest, ApplyResult, ObservationKind, Preview, Store},
    sync::{SyncRun, SyncStore},
    tax::{
        ledger::{Coverage, ImportInput, Observation, PropertyBinding, TaxError},
        reconcile::{BillView, InstallmentView, Reconciliation, reconcile},
    },
};

#[derive(Clone)]
pub struct Context {
    pub store: Store,
    pub syncs: SyncStore,
    owner: [u8; 32],
    integration: Option<[u8; 32]>,
    ingress_peer: Option<IpAddr>,
}
impl Context {
    pub fn new(store: Store, syncs: SyncStore, owner: &str) -> Self {
        Self {
            store,
            syncs,
            owner: Sha256::digest(owner).into(),
            integration: None,
            ingress_peer: None,
        }
    }
    pub fn with_integration(mut self, token: &str) -> Self {
        self.integration = Some(Sha256::digest(token).into());
        self
    }
    pub fn for_ingress(mut self, peer: IpAddr) -> Self {
        self.ingress_peer = Some(peer);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Health {
    pub status: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct System {
    pub service: String,
    pub version: String,
    pub api_version: u32,
    pub schema_version: u32,
    pub role: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PropertyView {
    pub revision: i64,
    pub property: Option<PropertyBinding>,
    pub coverage: Vec<Coverage>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_page: Option<String>,
}
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    pub limit: Option<usize>,
    pub page_token: Option<String>,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ApiError {
    pub error_code: String,
    pub message: String,
}
#[derive(Deserialize, JsonSchema)]
struct BillPath {
    id: String,
}
#[derive(Deserialize, JsonSchema)]
struct SyncPath {
    id: uuid::Uuid,
}
#[derive(Serialize, JsonSchema)]
struct PrivateHeaders {
    #[serde(skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(rename = "cache-control")]
    cache_control: String,
}
type Private<T> = HttpResponseHeaders<HttpResponseOk<T>, PrivateHeaders>;
fn private<T: Serialize + JsonSchema + Send + Sync + 'static>(
    value: T,
    revision: Option<i64>,
) -> Private<T> {
    HttpResponseHeaders::new(
        HttpResponseOk(value),
        PrivateHeaders {
            etag: revision.map(|v| format!("\"{v}\"")),
            cache_control: "no-store".into(),
        },
    )
}
fn fail(status: Status, code: &'static str, message: &'static str) -> HttpError {
    HttpError::for_client_error(Some(code.into()), status, message.into())
}
impl From<TaxError> for HttpError {
    fn from(error: TaxError) -> Self {
        match error {
            TaxError::Invalid(message) => HttpError::for_client_error(
                Some("invalid_input".into()),
                Status::BAD_REQUEST,
                message.into(),
            ),
            TaxError::BindingMismatch => fail(
                Status::CONFLICT,
                "binding_mismatch",
                "Evidence belongs to another county or parcel.",
            ),
            TaxError::PropertyChanged => fail(
                Status::CONFLICT,
                "property_changed",
                "The confirmed property cannot be replaced by an import.",
            ),
            TaxError::ObservationConflict => fail(
                Status::CONFLICT,
                "observation_conflict",
                "An immutable observation changed.",
            ),
            TaxError::RevisionConflict => fail(
                Status::CONFLICT,
                "revision_conflict",
                "The ledger changed. Create a new preview.",
            ),
            TaxError::PreviewMismatch => fail(
                Status::CONFLICT,
                "preview_mismatch",
                "The import does not match its preview.",
            ),
            TaxError::IdempotencyConflict => fail(
                Status::CONFLICT,
                "idempotency_conflict",
                "The idempotency key belongs to another request.",
            ),
            TaxError::Capacity => fail(
                Status::PAYLOAD_TOO_LARGE,
                "capacity",
                "The ledger or import limit was reached.",
            ),
            _ => HttpError::for_unavail(
                Some("database_unavailable".into()),
                "The ledger is unavailable.".into(),
            ),
        }
    }
}
fn header<'a>(request: &'a RequestContext<Arc<Context>>, key: &str) -> Option<&'a str> {
    request
        .request
        .headers()
        .get(key)
        .and_then(|v| v.to_str().ok())
}
fn ingress_access(request: &RequestContext<Arc<Context>>) -> Result<(), HttpError> {
    if request
        .context()
        .ingress_peer
        .is_some_and(|expected| request.request.remote_addr().ip() != expected)
    {
        return Err(fail(
            Status::FORBIDDEN,
            "ingress_peer_required",
            "Only the Supervisor ingress peer is accepted.",
        ));
    }
    Ok(())
}
fn authenticate(
    request: &RequestContext<Arc<Context>>,
    write: bool,
) -> Result<&'static str, HttpError> {
    let context = request.context();
    if context.ingress_peer.is_some() {
        ingress_access(request)?;
        // Supervisor enforces panel_admin. Native requests cannot opt into this
        // path with forwarding headers. Custom request headers block ambient
        // cross-site form submissions on the peer-authenticated listener.
        if header(request, "x-remote-user-id").is_none_or(str::is_empty) {
            return Err(fail(
                Status::UNAUTHORIZED,
                "ha_user_required",
                "Home Assistant authentication is required.",
            ));
        }
        if write && header(request, "x-property-request") != Some("1") {
            return Err(fail(
                Status::FORBIDDEN,
                "browser_request_header_required",
                "Use the Property interface for imports and sync.",
            ));
        }
        return Ok("admin");
    }
    let token = header(request, "authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| v.len() <= 256)
        .ok_or_else(|| {
            fail(
                Status::UNAUTHORIZED,
                "unauthorized",
                "A bearer credential is required.",
            )
        })?;
    let digest: [u8; 32] = Sha256::digest(token).into();
    if bool::from(digest.ct_eq(&context.owner)) {
        return Ok("admin");
    }
    if context
        .integration
        .as_ref()
        .is_some_and(|v| bool::from(digest.ct_eq(v)))
    {
        if write && header(request, "x-property-admin") != Some("true") {
            return Err(fail(
                Status::FORBIDDEN,
                "admin_required",
                "An administrator is required.",
            ));
        }
        return Ok("integration");
    }
    Err(fail(
        Status::UNAUTHORIZED,
        "unauthorized",
        "The bearer credential is invalid.",
    ))
}
fn asset(content: String, mime: &'static str) -> Result<Response<Body>, HttpError> {
    Response::builder().header("content-type", mime).header("cache-control", "no-store")
        .header("x-content-type-options", "nosniff").header("referrer-policy", "no-referrer")
        .header("content-security-policy", "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'self'")
        .body(Body::from(content)).map_err(|_| HttpError::for_internal_error("response unavailable".into()))
}
/// Open the property ledger dashboard.
#[endpoint {method=GET,path="/",tags=["web"]}]
async fn index(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    let base = if request.context().ingress_peer.is_some() {
        header(&request, "x-ingress-path").unwrap_or("")
    } else {
        ""
    };
    if !base.is_empty()
        && (!base.starts_with('/')
            || base.starts_with("//")
            || base.len() > 2048
            || !base
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'/' | b'_' | b'-')))
    {
        return Err(fail(
            Status::BAD_REQUEST,
            "invalid_ingress_path",
            "Invalid ingress prefix.",
        ));
    }
    asset(
        include_str!("../web/index.html").replace(
            "__PROPERTY_BASE__",
            &format!("{}/", base.trim_end_matches('/')),
        ),
        "text/html; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/app.js",tags=["web"]}]
async fn app_js(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    asset(
        include_str!("../web/app.js").into(),
        "text/javascript; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/style.css",tags=["web"]}]
async fn app_css(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    asset(
        include_str!("../web/style.css").into(),
        "text/css; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/kit.js",tags=["web"]}]
async fn kit_js(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    asset(ha_app_ui::SCRIPT.into(), "text/javascript; charset=utf-8")
}
#[endpoint {method=GET,path="/kit.css",tags=["web"]}]
async fn kit_css(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    asset(ha_app_ui::STYLESHEET.into(), "text/css; charset=utf-8")
}
/// Check database readiness without exposing tax evidence or credentials.
#[endpoint {method=GET,path="/health",tags=["system"]}]
async fn health(request: RequestContext<Arc<Context>>) -> Result<Private<Health>, HttpError> {
    ingress_access(&request)?;
    request.context().store.health().await?;
    Ok(private(
        Health {
            status: "ok".into(),
        },
        None,
    ))
}
/// Read the service version, schema version and authenticated credential role.
#[endpoint {method=GET,path="/v1/system",tags=["system"]}]
async fn system(request: RequestContext<Arc<Context>>) -> Result<Private<System>, HttpError> {
    let role = authenticate(&request, false)?;
    Ok(private(
        System {
            service: "property".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            api_version: 1,
            schema_version: 1,
            role: role.into(),
        },
        None,
    ))
}
/// Read the confirmed county/parcel binding and collection coverage.
#[endpoint {method=GET,path="/v1/property",tags=["property"]}]
async fn property(
    request: RequestContext<Arc<Context>>,
) -> Result<Private<PropertyView>, HttpError> {
    authenticate(&request, false)?;
    let snapshot = request.context().store.snapshot().await?;
    Ok(private(
        PropertyView {
            revision: snapshot.revision,
            property: snapshot.property,
            coverage: snapshot.coverage,
        },
        Some(snapshot.revision),
    ))
}
async fn projection(context: &Context) -> Result<Reconciliation, HttpError> {
    Ok(reconcile(&context.store.snapshot().await?, Utc::now()))
}
/// Reconcile notices, scoped balances and explicit payment allocations.
#[endpoint {method=GET,path="/v1/summary",tags=["tax"]}]
async fn summary(
    request: RequestContext<Arc<Context>>,
) -> Result<Private<Reconciliation>, HttpError> {
    authenticate(&request, false)?;
    let value = projection(request.context()).await?;
    let revision = value.revision;
    Ok(private(value, Some(revision)))
}
fn page_limit(limit: Option<usize>) -> Result<usize, HttpError> {
    match limit.unwrap_or(50) {
        value @ 1..=200 => Ok(value),
        _ => Err(fail(
            Status::BAD_REQUEST,
            "invalid_input",
            "limit must be 1..200.",
        )),
    }
}
fn page_cursor(token: Option<&str>) -> Result<Option<(i64, String)>, HttpError> {
    let Some(token) = token else { return Ok(None) };
    if token.len() > 820 {
        return Err(fail(
            Status::BAD_REQUEST,
            "invalid_input",
            "Invalid page token.",
        ));
    }
    let (revision, key) = token
        .split_once(':')
        .ok_or_else(|| fail(Status::BAD_REQUEST, "invalid_input", "Invalid page token."))?;
    let revision = revision
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
        .ok_or_else(|| fail(Status::BAD_REQUEST, "invalid_input", "Invalid page token."))?;
    let key = String::from_utf8(
        hex::decode(key)
            .map_err(|_| fail(Status::BAD_REQUEST, "invalid_input", "Invalid page token."))?,
    )
    .map_err(|_| fail(Status::BAD_REQUEST, "invalid_input", "Invalid page token."))?;
    Ok(Some((revision, key)))
}
fn paginate<T: Clone + Serialize>(
    mut values: Vec<T>,
    revision: i64,
    query: PageQuery,
    key: impl Fn(&T) -> String,
) -> Result<Page<T>, HttpError> {
    let limit = page_limit(query.limit)?;
    let cursor = page_cursor(query.page_token.as_deref())?;
    if cursor
        .as_ref()
        .is_some_and(|(expected, _)| *expected != revision)
    {
        return Err(fail(
            Status::CONFLICT,
            "revision_conflict",
            "The ledger changed. Restart pagination.",
        ));
    }
    let after = cursor.map(|(_, key)| key);
    values.sort_by_key(&key);
    let mut matching = values
        .into_iter()
        .filter(|v| after.as_ref().is_none_or(|after| key(v) > *after))
        .peekable();
    let mut items = Vec::new();
    let mut bytes = 0;
    // The HA bridge has an 8MiB response bound. Limit is a maximum count,
    // not a promise to exceed that transport's capacity for large evidence.
    // Keep 2MiB of headroom for the envelope, cursor and proxy bookkeeping.
    while items.len() < limit {
        let Some(value) = matching.peek() else { break };
        let length = serde_json::to_vec(value)
            .map_err(|_| HttpError::for_internal_error("page encoding failed".into()))?
            .len();
        if length > 6 * 1024 * 1024 {
            return Err(TaxError::Capacity.into());
        }
        if !items.is_empty() && bytes + length + items.len() > 6 * 1024 * 1024 {
            break;
        }
        if let Some(value) = matching.next() {
            bytes += length;
            items.push(value);
        }
    }
    let next_page = if matching.peek().is_some() {
        items
            .last()
            .map(|v| format!("{revision}:{}", hex::encode(key(v))))
    } else {
        None
    };
    Ok(Page { items, next_page })
}
/// List annual and supplemental bills, preserving source versions and missing notices.
#[endpoint {method=GET,path="/v1/tax/bills",tags=["tax"]}]
async fn bills(
    request: RequestContext<Arc<Context>>,
    query: Query<PageQuery>,
) -> Result<Private<Page<BillView>>, HttpError> {
    authenticate(&request, false)?;
    let value = projection(request.context()).await?;
    Ok(private(
        paginate(value.bills, value.revision, query.into_inner(), |v| {
            v.id.clone()
        })?,
        Some(value.revision),
    ))
}
/// Read a bill and its installment-specific evidence.
#[endpoint {method=GET,path="/v1/tax/bills/{id}",tags=["tax"]}]
async fn bill(
    request: RequestContext<Arc<Context>>,
    path: Path<BillPath>,
) -> Result<Private<BillView>, HttpError> {
    authenticate(&request, false)?;
    let value = projection(request.context()).await?;
    let id = path.into_inner().id;
    let bill = value
        .bills
        .into_iter()
        .find(|v| v.id == id)
        .ok_or_else(|| fail(Status::NOT_FOUND, "not_found", "Bill not found."))?;
    Ok(private(bill, Some(value.revision)))
}
/// List installment deadlines, states and separately scoped county balances.
#[endpoint {method=GET,path="/v1/tax/installments",tags=["tax"]}]
async fn installments(
    request: RequestContext<Arc<Context>>,
    query: Query<PageQuery>,
) -> Result<Private<Page<InstallmentView>>, HttpError> {
    authenticate(&request, false)?;
    let value = projection(request.context()).await?;
    Ok(private(
        paginate(
            value.installments,
            value.revision,
            query.into_inner(),
            |v| v.id.clone(),
        )?,
        Some(value.revision),
    ))
}
async fn observations(
    context: &Context,
    query: PageQuery,
    kind: ObservationKind,
) -> Result<Private<Page<Observation>>, HttpError> {
    let limit = page_limit(query.limit)?;
    let cursor = page_cursor(query.page_token.as_deref())?;
    let rows = context
        .store
        .observation_page(
            kind,
            cursor.as_ref().map(|(_, key)| key.as_str()),
            u32::try_from(limit + 1)
                .map_err(|_| fail(Status::BAD_REQUEST, "invalid_input", "Invalid page limit."))?,
            cursor.as_ref().map(|(revision, _)| *revision),
        )
        .await?;
    Ok(private(
        paginate(rows.items, rows.revision, query, |value| value.id.clone())?,
        Some(rows.revision),
    ))
}
/// List assessment evidence without confusing assessed values with tax bills.
#[endpoint {method=GET,path="/v1/tax/assessments",tags=["tax"]}]
async fn assessments(
    request: RequestContext<Arc<Context>>,
    query: Query<PageQuery>,
) -> Result<Private<Page<Observation>>, HttpError> {
    authenticate(&request, false)?;
    observations(
        request.context(),
        query.into_inner(),
        ObservationKind::Assessment,
    )
    .await
}
/// List payment lifecycle evidence. This endpoint never submits a payment.
#[endpoint {method=GET,path="/v1/tax/payments",tags=["tax"]}]
async fn payments(
    request: RequestContext<Arc<Context>>,
    query: Query<PageQuery>,
) -> Result<Private<Page<Observation>>, HttpError> {
    authenticate(&request, false)?;
    observations(
        request.context(),
        query.into_inner(),
        ObservationKind::Payment,
    )
    .await
}
/// Validate a structured evidence import and preview its deterministic reconciliation.
#[endpoint {method=POST,path="/v1/imports/preview",tags=["imports"],request_body_max_bytes=4*1024*1024}]
async fn preview(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<ImportInput>,
) -> Result<Private<Preview>, HttpError> {
    authenticate(&request, true)?;
    let value = request.context().store.preview(body.into_inner()).await?;
    let revision = value.revision;
    Ok(private(value, Some(revision)))
}
/// Apply the exact preview with optimistic revision and durable idempotency protection.
#[endpoint {method=POST,path="/v1/imports",tags=["imports"],request_body_max_bytes=8*1024*1024}]
async fn apply(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<ApplyRequest>,
) -> Result<Private<ApplyResult>, HttpError> {
    authenticate(&request, true)?;
    let input = body.into_inner();
    let expected = format!("\"{}\"", input.if_match);
    if header(&request, "if-match") != Some(expected.as_str()) {
        return Err(fail(
            Status::PRECONDITION_REQUIRED,
            "if_match_required",
            "Supply the preview revision in If-Match.",
        ));
    }
    let value = request.context().store.apply(input).await?;
    let revision = value.revision;
    Ok(private(value, Some(revision)))
}
/// Attempt deterministic county collection and retain explicit partial/unavailable coverage.
#[endpoint {method=POST,path="/v1/sync",tags=["sync"]}]
async fn sync_request(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseHeaders<HttpResponseCreated<SyncRun>, PrivateHeaders>, HttpError> {
    authenticate(&request, true)?;
    let context = request.context();
    let snapshot = context.store.snapshot().await?;
    let binding = snapshot.property.ok_or_else(|| {
        fail(
            Status::CONFLICT,
            "property_required",
            "Confirm the county and parcel with an import first.",
        )
    })?;
    let run = context.syncs.request(&context.store, binding).await?;
    Ok(HttpResponseHeaders::new(
        HttpResponseCreated(run),
        PrivateHeaders {
            etag: None,
            cache_control: "no-store".into(),
        },
    ))
}
/// Read a durable county sync receipt, including failure and coverage details.
#[endpoint {method=GET,path="/v1/sync/{id}",tags=["sync"]}]
async fn sync_get(
    request: RequestContext<Arc<Context>>,
    path: Path<SyncPath>,
) -> Result<Private<SyncRun>, HttpError> {
    authenticate(&request, false)?;
    let run = request
        .context()
        .syncs
        .get(path.into_inner().id)
        .await?
        .ok_or_else(|| fail(Status::NOT_FOUND, "not_found", "Sync not found."))?;
    Ok(private(run, None))
}
/// Read the schema generated from the running service's registered typed endpoints.
#[endpoint {method=GET,path="/openapi.json",tags=["system"]}]
async fn schema(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    let schema = openapi()
        .and_then(|v| serde_json::to_string_pretty(&v).map_err(Into::into))
        .map_err(|_| HttpError::for_internal_error("schema unavailable".into()))?;
    asset(schema, "application/json")
}
pub fn description() -> anyhow::Result<ApiDescription<Arc<Context>>> {
    let mut api = ApiDescription::new();
    let endpoints: [dropshot::ApiEndpoint<Arc<Context>>; 19] = [
        index.into(),
        app_js.into(),
        app_css.into(),
        kit_js.into(),
        kit_css.into(),
        health.into(),
        system.into(),
        property.into(),
        summary.into(),
        bills.into(),
        bill.into(),
        installments.into(),
        assessments.into(),
        payments.into(),
        preview.into(),
        apply.into(),
        sync_request.into(),
        sync_get.into(),
        schema.into(),
    ];
    for endpoint in endpoints {
        api.register(endpoint).context("register API endpoint")?;
    }
    Ok(api)
}
pub fn openapi() -> anyhow::Result<serde_json::Value> {
    description()?.openapi("Property API",semver::Version::parse(env!("CARGO_PKG_VERSION"))?)
        .description("Evidence-backed property tax ledger. Bearer authentication or trusted HA ingress. Imports require exact preview hash, If-Match revision and an idempotency key. No payment submission or mark-paid API.")
        .json().map_err(Into::into)
}
pub fn start(bind: SocketAddr, context: Arc<Context>) -> anyhow::Result<HttpServer<Arc<Context>>> {
    // Do not log URLs, request bodies, parcel IDs or provider responses.
    let logger = slog::Logger::root(slog::Discard, slog::o!());
    HttpServerStarter::new(
        &ConfigDropshot {
            bind_address: bind,
            default_request_body_max_bytes: 8 * 1024 * 1024,
            default_handler_task_mode: dropshot::HandlerTaskMode::Detached,
            ..Default::default()
        },
        description()?,
        context,
        &logger,
    )
    .map_err(|_| anyhow::anyhow!("start HTTP listener"))
    .map(HttpServerStarter::start)
}
