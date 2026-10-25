//! An accounts ledger served over HTTP and gRPC from one in-memory store.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

// tonic's generated signatures return the large `tonic::Status` by value.
#[allow(clippy::result_large_err)]
pub mod pb {
    tonic::include_proto!("ledger.v1");
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LedgerError {
    #[error("account {0} already exists")]
    AlreadyExists(String),
    #[error("account {0} not found")]
    NotFound(String),
    #[error("amount must be positive")]
    InvalidAmount,
    #[error("cannot transfer to the same account")]
    SameAccount,
    #[error("insufficient funds")]
    InsufficientFunds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    pub id: String,
    pub balance: i64,
}

pub trait Store: Send + Sync {
    fn create(&self, id: &str) -> Result<Account, LedgerError>;
    fn deposit(&self, id: &str, amount: i64) -> Result<Account, LedgerError>;
    fn withdraw(&self, id: &str, amount: i64) -> Result<Account, LedgerError>;
    fn transfer(
        &self,
        from: &str,
        to: &str,
        amount: i64,
    ) -> Result<(Account, Account), LedgerError>;
    fn get(&self, id: &str) -> Result<Account, LedgerError>;
}

#[derive(Default)]
pub struct MemoryStore {
    balances: Mutex<HashMap<String, i64>>,
}

fn positive(amount: i64) -> Result<(), LedgerError> {
    if amount > 0 {
        Ok(())
    } else {
        Err(LedgerError::InvalidAmount)
    }
}

fn account(id: &str, balance: i64) -> Account {
    Account {
        id: id.to_owned(),
        balance,
    }
}

impl Store for MemoryStore {
    fn create(&self, id: &str) -> Result<Account, LedgerError> {
        let mut balances = self.balances.lock().expect("store lock");
        if balances.contains_key(id) {
            return Err(LedgerError::AlreadyExists(id.to_owned()));
        }
        balances.insert(id.to_owned(), 0);
        Ok(account(id, 0))
    }

    fn deposit(&self, id: &str, amount: i64) -> Result<Account, LedgerError> {
        positive(amount)?;
        let mut balances = self.balances.lock().expect("store lock");
        let balance = balances
            .get_mut(id)
            .ok_or_else(|| LedgerError::NotFound(id.to_owned()))?;
        *balance = balance
            .checked_add(amount)
            .ok_or(LedgerError::InvalidAmount)?;
        Ok(account(id, *balance))
    }

    fn withdraw(&self, id: &str, amount: i64) -> Result<Account, LedgerError> {
        positive(amount)?;
        let mut balances = self.balances.lock().expect("store lock");
        let balance = balances
            .get_mut(id)
            .ok_or_else(|| LedgerError::NotFound(id.to_owned()))?;
        if *balance < amount {
            return Err(LedgerError::InsufficientFunds);
        }
        *balance -= amount;
        Ok(account(id, *balance))
    }

    fn transfer(
        &self,
        from: &str,
        to: &str,
        amount: i64,
    ) -> Result<(Account, Account), LedgerError> {
        positive(amount)?;
        if from == to {
            return Err(LedgerError::SameAccount);
        }
        let mut balances = self.balances.lock().expect("store lock");
        let source = *balances
            .get(from)
            .ok_or_else(|| LedgerError::NotFound(from.to_owned()))?;
        let target = *balances
            .get(to)
            .ok_or_else(|| LedgerError::NotFound(to.to_owned()))?;
        if source < amount {
            return Err(LedgerError::InsufficientFunds);
        }
        let target = target
            .checked_add(amount)
            .ok_or(LedgerError::InvalidAmount)?;
        balances.insert(from.to_owned(), source - amount);
        balances.insert(to.to_owned(), target);
        Ok((account(from, source - amount), account(to, target)))
    }

    fn get(&self, id: &str) -> Result<Account, LedgerError> {
        let balances = self.balances.lock().expect("store lock");
        let balance = balances
            .get(id)
            .ok_or_else(|| LedgerError::NotFound(id.to_owned()))?;
        Ok(account(id, *balance))
    }
}

type Shared = Arc<dyn Store>;

impl IntoResponse for LedgerError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::AlreadyExists(_) => StatusCode::CONFLICT,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::InvalidAmount | Self::SameAccount => StatusCode::BAD_REQUEST,
            Self::InsufficientFunds => StatusCode::UNPROCESSABLE_ENTITY,
        };
        (status, Json(json!({"error": self.to_string()}))).into_response()
    }
}

impl From<LedgerError> for tonic::Status {
    fn from(error: LedgerError) -> Self {
        let message = error.to_string();
        match error {
            LedgerError::AlreadyExists(_) => Self::already_exists(message),
            LedgerError::NotFound(_) => Self::not_found(message),
            LedgerError::InvalidAmount | LedgerError::SameAccount => {
                Self::invalid_argument(message)
            }
            LedgerError::InsufficientFunds => Self::failed_precondition(message),
        }
    }
}

#[derive(Deserialize)]
struct NewAccount {
    id: String,
}

#[derive(Deserialize)]
struct Amount {
    amount: i64,
}

#[derive(Deserialize)]
struct Transfer {
    from: String,
    to: String,
    amount: i64,
}

pub fn router(store: Shared) -> Router {
    Router::new()
        .route("/accounts", post(create))
        .route("/accounts/{id}", get(read))
        .route("/accounts/{id}/deposit", post(deposit))
        .route("/accounts/{id}/withdraw", post(withdraw))
        .route("/transfers", post(transfer))
        .with_state(store)
}

async fn create(
    State(store): State<Shared>,
    Json(body): Json<NewAccount>,
) -> Result<(StatusCode, Json<Account>), LedgerError> {
    Ok((StatusCode::CREATED, Json(store.create(&body.id)?)))
}

async fn read(
    State(store): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<Account>, LedgerError> {
    Ok(Json(store.get(&id)?))
}

async fn deposit(
    State(store): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<Amount>,
) -> Result<Json<Account>, LedgerError> {
    Ok(Json(store.deposit(&id, body.amount)?))
}

async fn withdraw(
    State(store): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<Amount>,
) -> Result<Json<Account>, LedgerError> {
    Ok(Json(store.withdraw(&id, body.amount)?))
}

async fn transfer(
    State(store): State<Shared>,
    Json(body): Json<Transfer>,
) -> Result<Json<serde_json::Value>, LedgerError> {
    let (from, to) = store.transfer(&body.from, &body.to, body.amount)?;
    Ok(Json(json!({"from": from, "to": to})))
}

pub struct Grpc {
    store: Shared,
}

fn wire(account: Account) -> pb::Account {
    pb::Account {
        id: account.id,
        balance: account.balance,
    }
}

#[allow(clippy::result_large_err)]
#[tonic::async_trait]
impl pb::ledger_server::Ledger for Grpc {
    async fn create_account(
        &self,
        request: tonic::Request<pb::CreateAccountRequest>,
    ) -> Result<tonic::Response<pb::Account>, tonic::Status> {
        Ok(tonic::Response::new(wire(
            self.store.create(&request.into_inner().id)?,
        )))
    }

    async fn deposit(
        &self,
        request: tonic::Request<pb::AmountRequest>,
    ) -> Result<tonic::Response<pb::Account>, tonic::Status> {
        let request = request.into_inner();
        Ok(tonic::Response::new(wire(
            self.store.deposit(&request.id, request.amount)?,
        )))
    }

    async fn withdraw(
        &self,
        request: tonic::Request<pb::AmountRequest>,
    ) -> Result<tonic::Response<pb::Account>, tonic::Status> {
        let request = request.into_inner();
        Ok(tonic::Response::new(wire(
            self.store.withdraw(&request.id, request.amount)?,
        )))
    }

    async fn transfer(
        &self,
        request: tonic::Request<pb::TransferRequest>,
    ) -> Result<tonic::Response<pb::TransferReply>, tonic::Status> {
        let request = request.into_inner();
        let (from, to) = self
            .store
            .transfer(&request.from, &request.to, request.amount)?;
        Ok(tonic::Response::new(pb::TransferReply {
            from: Some(wire(from)),
            to: Some(wire(to)),
        }))
    }

    async fn get_balance(
        &self,
        request: tonic::Request<pb::GetBalanceRequest>,
    ) -> Result<tonic::Response<pb::Account>, tonic::Status> {
        Ok(tonic::Response::new(wire(
            self.store.get(&request.into_inner().id)?,
        )))
    }
}

/// Serves both APIs until `shutdown` completes, then stops both.
///
/// # Errors
///
/// Returns an error when either address cannot be bound or a server fails.
pub async fn serve(
    http: SocketAddr,
    grpc: SocketAddr,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let store: Shared = Arc::new(MemoryStore::default());
    let (stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = stop.send(true);
    });
    let wait = |mut stopped: tokio::sync::watch::Receiver<bool>| async move {
        let _ = stopped.wait_for(|stop| *stop).await;
    };

    let listener = tokio::net::TcpListener::bind(http).await?;
    tracing::info!(%http, %grpc, "ledger listening");
    let http_server = axum::serve(listener, router(Arc::clone(&store)))
        .with_graceful_shutdown(wait(stopped.clone()));
    let grpc_server = tonic::transport::Server::builder()
        .add_service(pb::ledger_server::LedgerServer::new(Grpc { store }))
        .serve_with_shutdown(grpc, wait(stopped));
    let (http_result, grpc_result) = tokio::join!(http_server, grpc_server);
    http_result?;
    grpc_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfers_are_checked_and_atomic() {
        let store = MemoryStore::default();
        store.create("a").unwrap();
        store.create("b").unwrap();
        store.deposit("a", 10).unwrap();
        assert_eq!(
            store.transfer("a", "b", 11),
            Err(LedgerError::InsufficientFunds)
        );
        assert_eq!(store.transfer("a", "a", 1), Err(LedgerError::SameAccount));
        let (from, to) = store.transfer("a", "b", 4).unwrap();
        assert_eq!((from.balance, to.balance), (6, 4));
    }
}
