use std::{
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use property::{
    api::{ApiError, Health, Page, PropertyView, System},
    store::{ApplyRequest, ApplyResult, Preview},
    sync::SyncRun,
    tax::{
        ledger::{ImportInput, Observation},
        reconcile::{BillView, InstallmentView, Reconciliation},
    },
};
use reqwest::{Client, Method, RequestBuilder, Url, header::HeaderValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Parser)]
#[command(
    name = "ha-property",
    version,
    about = "Typed JSON client for the Property ledger"
)]
struct Cli {
    #[arg(long, value_enum, default_value = "native", global = true)]
    transport: Transport,
    /// Origin only. Defaults to PROPERTY_API_URL or HOME_ASSISTANT_URL.
    #[arg(long, global = true)]
    url: Option<String>,
    /// Owner-only credential file instead of PROPERTY_API_TOKEN/HOME_ASSISTANT_TOKEN.
    #[arg(long, global = true)]
    token_file: Option<PathBuf>,
    #[arg(long, default_value = "30", value_parser = clap::value_parser!(u64).range(1..=300), global = true)]
    timeout: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Transport {
    Native,
    HomeAssistant,
}

#[derive(Subcommand)]
enum Command {
    Health,
    System,
    Show,
    Summary,
    #[command(subcommand)]
    Tax(Tax),
    #[command(subcommand)]
    Imports(Imports),
    #[command(subcommand)]
    Sync(Sync),
}
#[derive(Subcommand)]
enum Tax {
    #[command(subcommand)]
    Bills(Bills),
    #[command(subcommand)]
    Installments(List),
    #[command(subcommand)]
    Payments(List),
    #[command(subcommand)]
    Assessments(List),
}
#[derive(Subcommand)]
enum Bills {
    List(Paging),
    Get { id: String },
}
#[derive(Subcommand)]
enum List {
    List(Paging),
}
#[derive(Args)]
struct Paging {
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=200))]
    limit: Option<u16>,
    #[arg(long)]
    page_token: Option<String>,
}
impl Paging {
    fn apply(&self, mut request: RequestBuilder) -> RequestBuilder {
        if let Some(limit) = self.limit {
            request = request.query(&[("limit", limit)]);
        }
        if let Some(token) = &self.page_token {
            request = request.query(&[("page_token", token)]);
        }
        request
    }
}
#[derive(Subcommand)]
enum Imports {
    Preview {
        #[arg(long)]
        input: PathBuf,
    },
    Apply {
        /// JSON file written by imports preview. No automatic re-preview.
        #[arg(long)]
        preview: PathBuf,
        /// Exact quoted ETag from preview, including revision zero.
        #[arg(long)]
        if_match: String,
        #[arg(long)]
        idempotency_key: String,
    },
}
#[derive(Subcommand)]
enum Sync {
    Request,
    Get { id: uuid::Uuid },
}

#[derive(Serialize, Deserialize)]
struct Output<T> {
    status: u16,
    etag: Option<String>,
    body: T,
}
#[derive(Serialize)]
struct Failure {
    status: Option<u16>,
    error_code: &'static str,
}
impl Failure {
    fn local(code: &'static str) -> Self {
        Self {
            status: None,
            error_code: code,
        }
    }
}

struct Api {
    client: Client,
    origin: Url,
    token: Option<HeaderValue>,
    transport: Transport,
}
impl Api {
    fn new(cli: &Cli) -> Result<Self, Failure> {
        let (url_env, token_env) = match cli.transport {
            Transport::Native => ("PROPERTY_API_URL", "PROPERTY_API_TOKEN"),
            Transport::HomeAssistant => ("HOME_ASSISTANT_URL", "HOME_ASSISTANT_TOKEN"),
        };
        let value = cli
            .url
            .clone()
            .or_else(|| std::env::var(url_env).ok())
            .ok_or(Failure::local("url_required"))?;
        let origin = Url::parse(&value).map_err(|_| Failure::local("invalid_url"))?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(Failure::local("invalid_url"));
        }
        let token = if let Some(path) = &cli.token_file {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::symlink_metadata(path)
                    .map_err(|_| Failure::local("token_file_unavailable"))?;
                if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
                    return Err(Failure::local("token_file_not_private"));
                }
            }
            let mut content = String::new();
            std::fs::File::open(path)
                .map_err(|_| Failure::local("token_file_unavailable"))?
                .take(4097)
                .read_to_string(&mut content)
                .map_err(|_| Failure::local("invalid_token_file"))?;
            if content.len() > 4096 {
                return Err(Failure::local("invalid_token_file"));
            }
            Some(content.trim().to_owned())
        } else {
            std::env::var(token_env).ok()
        };
        let token = token
            .map(|token| {
                if token.is_empty()
                    || token.len() > 4096
                    || !token.bytes().all(|v| v.is_ascii_graphic())
                {
                    return Err(Failure::local("invalid_token"));
                }
                let mut header = HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| Failure::local("invalid_token"))?;
                header.set_sensitive(true);
                Ok(header)
            })
            .transpose()?;
        if token.is_none()
            && (!matches!(cli.command, Command::Health)
                || cli.transport == Transport::HomeAssistant)
        {
            return Err(Failure::local("token_required"));
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(cli.timeout))
            .build()
            .map_err(|_| Failure::local("client_failed"))?;
        Ok(Self {
            client,
            origin,
            token,
            transport: cli.transport,
        })
    }
    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, Failure> {
        let path = match self.transport {
            Transport::Native => path.to_owned(),
            Transport::HomeAssistant => format!(
                "/api/property/v1/{}",
                path.strip_prefix("/v1/")
                    .unwrap_or(path.trim_start_matches('/'))
            ),
        };
        let target = self
            .origin
            .join(&path)
            .map_err(|_| Failure::local("invalid_path"))?;
        if target.origin() != self.origin.origin() {
            return Err(Failure::local("invalid_path"));
        }
        let mut request = self.client.request(method, target);
        if let Some(token) = &self.token {
            request = request.header(reqwest::header::AUTHORIZATION, token.clone());
        }
        Ok(request)
    }
}

fn read<T: DeserializeOwned>(path: PathBuf, max_bytes: usize) -> Result<T, Failure> {
    let mut data = Vec::new();
    let input: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(path).map_err(|_| Failure::local("input_unavailable"))?)
    };
    input
        .take(u64::try_from(max_bytes).map_err(|_| Failure::local("input_too_large"))? + 1)
        .read_to_end(&mut data)
        .map_err(|_| Failure::local("input_unavailable"))?;
    if data.len() > max_bytes {
        return Err(Failure::local("input_too_large"));
    }
    serde_json::from_slice(&data).map_err(|_| Failure::local("invalid_input"))
}
async fn send<T: DeserializeOwned + Serialize>(request: RequestBuilder) -> Result<(), Failure> {
    let mut response = request
        .send()
        .await
        .map_err(|_| Failure::local("request_failed"))?;
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure {
        status: Some(status),
        error_code: "response_read_failed",
    })? {
        if chunk.len() > MAX_BYTES.saturating_sub(bytes.len()) {
            return Err(Failure {
                status: Some(status),
                error_code: "response_too_large",
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    if !response.status().is_success() {
        let code = serde_json::from_slice::<ApiError>(&bytes).ok();
        let error_code = match code.as_ref().map(|v| v.error_code.as_str()) {
            Some("unauthorized") => "unauthorized",
            Some("invalid_input") => "invalid_input",
            Some("revision_conflict") => "revision_conflict",
            Some("preview_mismatch") => "preview_mismatch",
            Some("idempotency_conflict") => "idempotency_conflict",
            Some("binding_mismatch") => "binding_mismatch",
            Some("observation_conflict") => "observation_conflict",
            Some("property_changed") => "property_changed",
            Some("not_found") => "not_found",
            Some("property_required") => "property_required",
            Some("if_match_required") => "if_match_required",
            Some("database_unavailable") => "database_unavailable",
            Some("capacity") => "capacity",
            _ if (300..400).contains(&status) => "redirect_refused",
            _ => "http_error",
        };
        return Err(Failure {
            status: Some(status),
            error_code,
        });
    }
    let body: T = serde_json::from_slice(&bytes).map_err(|_| Failure {
        status: Some(status),
        error_code: "invalid_json_response",
    })?;
    write(std::io::stdout().lock(), &Output { status, etag, body })
}
fn write(mut writer: impl Write, value: &impl Serialize) -> Result<(), Failure> {
    serde_json::to_writer(&mut writer, value).map_err(|_| Failure::local("output_failed"))?;
    writer
        .write_all(b"\n")
        .map_err(|_| Failure::local("output_failed"))
}

async fn run(cli: Cli) -> Result<(), Failure> {
    let api = Api::new(&cli)?;
    match cli.command {
        Command::Health => send::<Health>(api.request(Method::GET, "/health")?).await,
        Command::System => send::<System>(api.request(Method::GET, "/v1/system")?).await,
        Command::Show => send::<PropertyView>(api.request(Method::GET, "/v1/property")?).await,
        Command::Summary => send::<Reconciliation>(api.request(Method::GET, "/v1/summary")?).await,
        Command::Tax(Tax::Bills(Bills::List(page))) => {
            send::<Page<BillView>>(page.apply(api.request(Method::GET, "/v1/tax/bills")?)).await
        }
        Command::Tax(Tax::Bills(Bills::Get { id })) => {
            // IDs are hashes from the API, never URL paths or a provider string.
            if id.len() != 64 || !id.bytes().all(|v| v.is_ascii_hexdigit()) {
                return Err(Failure::local("invalid_bill_id"));
            }
            send::<BillView>(api.request(Method::GET, &format!("/v1/tax/bills/{id}"))?).await
        }
        Command::Tax(Tax::Installments(List::List(page))) => {
            send::<Page<InstallmentView>>(
                page.apply(api.request(Method::GET, "/v1/tax/installments")?),
            )
            .await
        }
        Command::Tax(Tax::Payments(List::List(page))) => {
            send::<Page<Observation>>(page.apply(api.request(Method::GET, "/v1/tax/payments")?))
                .await
        }
        Command::Tax(Tax::Assessments(List::List(page))) => {
            send::<Page<Observation>>(page.apply(api.request(Method::GET, "/v1/tax/assessments")?))
                .await
        }
        Command::Imports(Imports::Preview { input }) => {
            send::<Preview>(
                api.request(Method::POST, "/v1/imports/preview")?
                    .json(&read::<ImportInput>(input, 4 * 1024 * 1024)?),
            )
            .await
        }
        Command::Imports(Imports::Apply {
            preview,
            if_match,
            idempotency_key,
        }) => {
            #[derive(Deserialize)]
            #[serde(untagged)]
            enum Saved {
                Raw(Preview),
                Output(Output<Preview>),
            }
            let preview = match read::<Saved>(preview, 8 * 1024 * 1024)? {
                Saved::Raw(v) => v,
                Saved::Output(v) => v.body,
            };
            let revision = if_match
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|v| *v >= 0)
                .ok_or(Failure::local("invalid_revision"))?;
            if revision != preview.revision {
                return Err(Failure::local("revision_mismatch"));
            }
            if idempotency_key.is_empty()
                || idempotency_key.len() > 200
                || !idempotency_key.bytes().all(|v| v.is_ascii_graphic())
            {
                return Err(Failure::local("invalid_idempotency_key"));
            }
            send::<ApplyResult>(
                api.request(Method::POST, "/v1/imports")?
                    .header("If-Match", if_match)
                    .json(&ApplyRequest {
                        preview,
                        if_match: revision,
                        idempotency_key,
                    }),
            )
            .await
        }
        Command::Sync(Sync::Request) => {
            send::<SyncRun>(api.request(Method::POST, "/v1/sync")?).await
        }
        Command::Sync(Sync::Get { id }) => {
            send::<SyncRun>(api.request(Method::GET, &format!("/v1/sync/{id}"))?).await
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            return if error.print().is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Err(_) => {
            let _ = write(
                std::io::stderr().lock(),
                &Failure::local("invalid_arguments"),
            );
            return ExitCode::from(2);
        }
    };
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = write(std::io::stderr().lock(), &error);
            ExitCode::FAILURE
        }
    }
}
